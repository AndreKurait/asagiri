#!/usr/bin/env python3
"""Check that a removal and a start sent together do not both take effect."""
import http.client
import json
import socket
import sys
import threading
import time

ROUNDS = 25
AT_ONCE = 4
# How much later than the start the removal is sent, in steps: the two have
# to meet inside the engine, and where that is differs from run to run.
STEP = 0.00001


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

    def create():
        _, created = request("POST", "/containers/create", (201,), {
            "Image": image, "Cmd": ["sleep", "120"],
            "HostConfig": {"NetworkMode": "none"}})
        owned.append(created["Id"])
        return created["Id"]

    try:
        removed = started = 0
        for number in range(ROUNDS):
            ids = [create() for _ in range(AT_ONCE)]
            answers = {}
            failures = []
            gate = threading.Barrier(2 * AT_ONCE)

            def send(key, delay, method, target, expected):
                try:
                    gate.wait()
                    time.sleep(delay)
                    answers[key] = request(method, target, expected)[0]
                except Exception as error:
                    failures.append(error)

            threads = []
            for index, cid in enumerate(ids):
                delay = (number % 20) * STEP + index * STEP / 4
                # A start that finds the container removed, or being removed: 404 or 409.
                threads.append(threading.Thread(target=send, args=((cid, "start"), 0, "POST", "/containers/" + cid + "/start", (204, 404, 409))))
                # A removal that finds the container running: 409.
                threads.append(threading.Thread(target=send, args=((cid, "remove"), delay, "DELETE", "/containers/" + cid, (204, 409))))
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            if failures:
                raise failures[0]
            for cid in ids:
                pair = (answers[(cid, "start")], answers[(cid, "remove")])
                status, inspected = request("GET", "/containers/" + cid + "/json", (200, 404))
                if pair[1] == 204:
                    assert pair[0] != 204, "a start was answered 204 for a container whose removal was answered 204"
                    assert status == 404, ("still there after its removal", inspected["State"])
                    removed += 1
                else:
                    assert pair[0] == 204, ("neither started nor removed", pair)
                    assert status == 200 and inspected["State"]["Running"], ("not running after its start", pair, inspected)
                    started += 1
                    request("DELETE", "/containers/" + cid + "?force=true", (204,))
        print("PASS %d containers started and removed at once: %d started, %d removed, none both" % (ROUNDS * AT_ONCE, started, removed), flush=True)
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
            raise RuntimeError("Could not remove all remove-during-start containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
