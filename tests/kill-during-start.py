#!/usr/bin/env python3
"""Check that a kill, stop or forced removal accepted while a container starts takes effect."""
import http.client
import json
import socket
import sys
import threading
import time

ROUNDS = 10
# Containers started at once: they queue behind each other, which leaves
# time to signal one while its own start is still under way.
AT_ONCE = 4


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    owned = []

    def request(method, target, expected, body=None):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status in expected, (method, target, response.status, data)
            return response.status, json.loads(data) if data else None
        finally:
            connection.close()

    def create():
        _, created = request("POST", "/containers/create", (201,), {
            "Image": image, "Cmd": ["sleep", "120"],
            "HostConfig": {"NetworkMode": "none"}})
        owned.append(created["Id"])
        return created["Id"]

    def reported_running(cid):
        deadline = time.monotonic() + 30
        while not request("GET", "/containers/" + cid + "/json", (200,))[1]["State"]["Running"]:
            assert time.monotonic() < deadline, "the container did not start"

    def kill(cid):
        # 409 until the engine takes the container for running.
        deadline = time.monotonic() + 30
        while request("POST", "/containers/" + cid + "/kill", (204, 409))[0] != 204:
            assert time.monotonic() < deadline, "no kill was accepted"
        _, state = request("GET", "/containers/" + cid + "/json", (200,))
        assert not state["State"]["Running"], ("running after an accepted kill", state["State"])
        _, waited = request("POST", "/containers/" + cid + "/wait", (200,))
        assert waited["StatusCode"] == 137, waited

    def stop(cid):
        reported_running(cid)
        request("POST", "/containers/" + cid + "/stop?t=0", (204,))
        # The answer can come before the exit is recorded: wait for that.
        request("POST", "/containers/" + cid + "/wait", (200,))
        _, state = request("GET", "/containers/" + cid + "/json", (200,))
        assert state["State"]["Status"] == "exited", ("not exited after an accepted stop", state["State"])

    def remove(cid):
        reported_running(cid)
        request("DELETE", "/containers/" + cid + "?force=true", (204,))
        request("GET", "/containers/" + cid + "/json", (404,))

    def rounds(signal):
        for _ in range(ROUNDS):
            failures = []

            def run(step, *args):
                try:
                    step(*args)
                except Exception as error:
                    failures.append(error)

            threads = []
            for cid in [create() for _ in range(AT_ONCE)]:
                threads.append(threading.Thread(target=run, args=(request, "POST", "/containers/" + cid + "/start", (204,))))
                threads.append(threading.Thread(target=run, args=(signal, cid)))
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            if failures:
                raise failures[0]

    try:
        count = str(ROUNDS * AT_ONCE)
        rounds(kill)
        print("PASS " + count + " kills during start: exited with 137", flush=True)
        rounds(stop)
        print("PASS " + count + " zero-timeout stops during start: exited", flush=True)
        rounds(remove)
        print("PASS " + count + " forced removals during start: gone", flush=True)
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(owned):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", (204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove all kill-during-start containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
