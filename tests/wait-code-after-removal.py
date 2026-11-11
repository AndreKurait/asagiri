#!/usr/bin/env python3
"""Check that a wait reports the exit code of a container that is removed by force."""
import http.client
import json
import socket
import sys
import threading
import time

ROUNDS = 20
KILLED = 137


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
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

    def waited(auto_remove, condition):
        """The code a wait reports for a running container that is then removed by force."""
        _, created = request("POST", "/containers/create", (201,), {
            "Image": image, "Cmd": ["sleep", "120"],
            "HostConfig": {"NetworkMode": "none", "AutoRemove": auto_remove}})
        cid = created["Id"]
        owned.append(cid)
        request("POST", "/containers/" + cid + "/start", (204,))
        answer = {}

        def wait():
            try:
                answer["code"] = request("POST", "/containers/" + cid + "/wait?condition=" + condition, (200,))[1]["StatusCode"]
            except Exception as error:
                answer["error"] = error

        thread = threading.Thread(target=wait)
        thread.start()
        # The wait has to be there before the removal: one that comes after it finds no container.
        time.sleep(0.2)
        request("DELETE", "/containers/" + cid + "?force=true", (204,))
        thread.join()
        if "error" in answer:
            raise answer["error"]
        return answer["code"]

    try:
        for auto_remove in (False, True):
            for condition in ("not-running", "next-exit", "removed"):
                codes = [waited(auto_remove, condition) for _ in range(ROUNDS)]
                assert codes == [KILLED] * ROUNDS, (
                    "a wait reported another code than 137 after a forced removal",
                    {"AutoRemove": auto_remove, "condition": condition, "codes": codes})
        print("PASS %d waits on containers removed by force: all reported 137" % (6 * ROUNDS), flush=True)
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
            raise RuntimeError("Could not remove all wait-code-after-removal containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
