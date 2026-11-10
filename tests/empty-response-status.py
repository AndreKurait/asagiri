#!/usr/bin/env python3
"""Check success codes for operations whose response body is empty."""
import http.client
import json
import socket
import sys
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=10)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    def request(method, target, expected, body=None, empty=False):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, json.dumps(body) if body is not None else None,
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status == expected, (method, target, response.status, data)
            if empty:
                assert not data, data
            return json.loads(data) if data else None
        finally:
            connection.close()

    suffix = uuid.uuid4().hex
    tag = "kiri-empty-status-" + suffix
    cleanup = []
    try:
        network = request("POST", "/networks/create", 201, {"Name": "empty-status-" + suffix})["Id"]
        cleanup.append(("DELETE", "/networks/" + network, 204))
        cid = request("POST", "/containers/create", 201,
                      {"Image": image, "Cmd": ["sleep", "60"]})["Id"]
        cleanup.append(("DELETE", "/containers/" + cid + "?force=true", 204))
        request("POST", "/networks/" + network + "/connect", 200, {"Container": cid}, empty=True)
        request("POST", "/networks/" + network + "/disconnect", 200, {"Container": cid}, empty=True)
        print("PASS network connect/disconnect")
        image_id = request("GET", "/images/" + image + "/json", 200)["Id"]
        # Record cleanup before the request: a wrong status may still tag the image.
        cleanup.append(("DELETE", "/images/" + tag, 200))
        request("POST", "/images/" + image_id + "/tag?repo=" + tag, 201, empty=True)
        print("PASS image tag")
        request("POST", "/containers/" + cid + "/start", 204, empty=True)
        eid = request("POST", "/containers/" + cid + "/exec", 201, {"Cmd": ["true"]})["Id"]
        request("POST", "/exec/" + eid + "/start", 200, {"Detach": True}, empty=True)
        print("PASS detached exec")
        print("5 empty success responses passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for method, target, status in reversed(cleanup):
            try:
                request(method, target, status)
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove all test objects") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
