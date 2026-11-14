#!/usr/bin/env python3
"""Exercise engine Stop/Start and parallel stop timing in a disposable host."""
import http.client
import json
import os
import socket
import sys
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    # This probe stops the entire engine. CI's private host opts in explicitly.
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "requires a disposable test engine"
    owned = {}
    prefix = "asagiri-power-" + uuid.uuid4().hex[:10] + "-"

    def request(method, target, body=None, statuses=(200,)):
        connection = Connection(path)
        try:
            connection.request(method, target if target.startswith("/asagiri/") else "/v1.47" + target,
                               None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status in statuses, (method, target, response.status, data)
            return json.loads(data) if data else None
        finally:
            connection.close()

    def control(action):
        return request("POST", "/asagiri/" + action, statuses=(204,))

    def states():
        return {name: request("GET", "/containers/" + cid + "/json")["State"]["Running"]
                for name, cid in owned.items()}

    def await_running(expected):
        deadline = time.monotonic() + 40
        while True:
            actual = {name for name, running in states().items() if running}
            if actual == set(expected):
                return
            assert time.monotonic() < deadline, (sorted(actual), sorted(expected))
            time.sleep(.05)

    assert not request("GET", "/containers/json"), "run before other live probes create containers"
    policies = {"unless": "unless-stopped", "always": "always",
                "user": "unless-stopped", "no": "no",
                **{"extra" + str(i): "always" for i in range(5)}}
    try:
        for name, policy in policies.items():
            cid = request("POST", "/containers/create?name=" + prefix + name,
                          {"Image": image, "Cmd": ["sleep", "600"], "StopTimeout": 2,
                           "HostConfig": {"NetworkMode": "none", "RestartPolicy": {"Name": policy}}},
                          (201,))["Id"]
            owned[name] = cid
            request("POST", "/containers/" + cid + "/start", statuses=(204,))
        await_running(policies)

        # PID 1 ignores the default SIGTERM: nine independent two-second
        # timeouts distinguish parallel stop from the previous 18-second loop.
        started = time.monotonic()
        control("stop")
        elapsed = time.monotonic() - started
        await_running([])
        assert elapsed < 12, f"nine two-second stops took {elapsed:.3f}s"
        print(f"PASS nine containers stop together in {elapsed:.3f}s (limit 12s)", flush=True)

        # Stopping a held container is still a user stop, even though it is
        # already down. It cancels its pending restart and answers 204.
        request("POST", "/containers/" + owned["user"] + "/stop?t=0", statuses=(204,))
        refused = request("POST", "/containers/" + owned["no"] + "/start", statuses=(500,))
        assert "Asagiri is stopped" in refused["message"], refused
        control("wake")
        expected = set(policies) - {"user", "no"}
        await_running(expected)
        print("PASS Start restores always/unless-stopped and respects a user cancellation", flush=True)

        # Sleep followed by Wake is the backend restart path. Restart-policy
        # exits must not start new containers while the backend is stopping.
        control("sleep")
        await_running([])
        control("wake")
        await_running(expected)
        print("PASS backend sleep and wake preserve held restart policies", flush=True)

        # Wake starts held containers asynchronously. Stop immediately after
        # the accepted Wake must also stop any start still in flight.
        for _ in range(3):
            control("stop")
            control("wake")
            control("stop")
            await_running([])
            time.sleep(.25)
            assert not any(states().values()), "a held restart ran after Stop completed"
        request("POST", "/containers/" + owned["extra0"] + "/stop?t=0", statuses=(204,))
        control("wake")
        expected.remove("extra0")
        await_running(expected)
        assert not request("GET", "/containers/" + owned["extra0"] + "/json")["State"]["Running"]
        print("PASS three rapid Start/Stop cycles and cancellation of a held restart", flush=True)
        print(json.dumps({"probe": "engine-power", "parallel_stop_seconds": elapsed,
                          "containers": len(owned), "rapid_cycles": 3}), flush=True)
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        try:
            control("wake")
        except Exception as error:
            errors.append(error)
        for cid in owned.values():
            try:
                request("DELETE", "/containers/" + cid + "?force=true", statuses=(204, 404))
            except Exception as error:
                errors.append(error)
        if errors:
            for error in errors:
                print("Cleanup failed:", repr(error), file=sys.stderr)
            if not primary_failure:
                raise RuntimeError("Could not remove all engine-power test containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
