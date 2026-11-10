#!/usr/bin/env python3
"""Check success codes and headers for operations whose response body is empty."""
import http.client
import json
import socket
import sys
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    passed = []

    def request(method, target, expected, body=None, empty=None):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, json.dumps(body) if body is not None else None,
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status == expected, (method, target, response.status, data)
            if empty:
                assert not data, (empty, data)
                # Docker names no type for a body that isn't there, and sends
                # a length only where the status allows a body at all.
                headers = response.getheader("Content-Type"), response.getheader("Content-Length")
                assert headers == (None, None if expected == 204 else "0"), (empty, headers)
                passed.append(empty)
                print("PASS", empty)
            return json.loads(data) if data else None
        finally:
            connection.close()

    suffix = uuid.uuid4().hex
    tag = "kiri-empty-status-" + suffix
    cleanup = []

    def remove(target, what):
        # A checked removal; until it passes, cleanup still removes the object.
        request("DELETE", target, 204, empty=what)
        cleanup.remove(("DELETE", target, 204))

    try:
        network = request("POST", "/networks/create", 201, {"Name": "empty-status-" + suffix})["Id"]
        cleanup.append(("DELETE", "/networks/" + network, 204))
        volume = "empty-status-" + suffix
        request("POST", "/volumes/create", 201, {"Name": volume})
        cleanup.append(("DELETE", "/volumes/" + volume, 204))
        cid = request("POST", "/containers/create", 201,
                      {"Image": image, "Cmd": ["sleep", "60"]})["Id"]
        container = "/containers/" + cid
        cleanup.append(("DELETE", container + "?force=true", 204))
        request("POST", "/networks/" + network + "/connect", 200, {"Container": cid}, empty="network connect")
        request("POST", "/networks/" + network + "/disconnect", 200, {"Container": cid}, empty="network disconnect")
        image_id = request("GET", "/images/" + image + "/json", 200)["Id"]
        # Record cleanup before the request: a wrong status may still tag the image.
        cleanup.append(("DELETE", "/images/" + tag, 200))
        request("POST", "/images/" + image_id + "/tag?repo=" + tag, 201, empty="image tag")
        request("POST", container + "/start", 204, empty="container start")
        eid = request("POST", container + "/exec", 201, {"Cmd": ["true"]})["Id"]
        request("POST", "/exec/" + eid + "/start", 200, {"Detach": True}, empty="detached exec")
        request("POST", container + "/pause", 204, empty="container pause")
        request("POST", container + "/unpause", 204, empty="container unpause")
        request("POST", container + "/rename?name=empty-status-" + suffix, 204, empty="container rename")
        # sleep ignores SIGTERM as PID 1: t=0 goes straight to SIGKILL.
        request("POST", container + "/stop?t=0", 204, empty="container stop")
        # The exit is recorded before the next start (the answer has a body).
        assert "StatusCode" in request("POST", container + "/wait", 200)
        request("POST", container + "/start", 204)
        request("POST", container + "/restart?t=0", 204, empty="container restart")
        request("POST", container + "/kill", 204, empty="container kill")
        assert "StatusCode" in request("POST", container + "/wait", 200)
        remove(container + "?force=true", "container remove")
        remove("/networks/" + network, "network remove")
        remove("/volumes/" + volume, "volume remove")
        print(len(passed), "empty success responses passed")
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
