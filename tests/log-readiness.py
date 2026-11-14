#!/usr/bin/env python3
"""Exercise SDK log readiness before start, delayed output, and stream filters."""
import http.client
import json
import socket
import struct
import sys
import time


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=20)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def frames(data):
    result = []
    while data:
        assert len(data) >= 8 and data[1:4] == b"\0\0\0", data
        length = struct.unpack(">I", data[4:8])[0]
        assert len(data) >= 8 + length, data
        result.append((data[0], data[8:8 + length].decode()))
        data = data[8 + length:]
    return result


def check(path, image):
    def request(method, target, body=None, status=200):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target,
                               None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status == status, (method, target, response.status, data)
            return data
        finally:
            connection.close()

    cid = json.loads(request("POST", "/containers/create", {
        "Image": image, "Cmd": ["sh", "-ec",
            "echo ready-out; echo ready-err >&2; sleep 1; echo ready-out; echo ready-err >&2"],
        "HostConfig": {"NetworkMode": "none"},
    }, 201))["Id"]
    connection = Connection(path)
    try:
        # SDKs can subscribe while the container is still Created. Receiving
        # headers before Start ensures this covers that ordering explicitly.
        connection.request("GET", f"/v1.47/containers/{cid}/logs?follow=1&stdout=1&stderr=1")
        response = connection.getresponse()
        assert response.status == 200, response.status
        request("POST", f"/containers/{cid}/start", status=204)
        got = frames(response.read())
        assert [text for stream, text in got if stream == 1] == ["ready-out\n"] * 2, got
        assert [text for stream, text in got if stream == 2] == ["ready-err\n"] * 2, got
        print("PASS log readiness subscribed before start sees both delayed occurrences", flush=True)

        stdout = frames(request("GET", f"/containers/{cid}/logs?stdout=1&stderr=0"))
        assert stdout == [(1, "ready-out\n")] * 2, stdout
        stderr = frames(request("GET", f"/containers/{cid}/logs?stdout=0&stderr=1"))
        assert stderr == [(2, "ready-err\n")] * 2, stderr
        stamped = frames(request("GET", f"/containers/{cid}/logs?stdout=1&timestamps=1"))
        assert len(stamped) == 2, stamped
        for stream, text in stamped:
            stamp, body = text.split(" ", 1)
            assert stream == 1 and stamp.endswith("Z") and "T" in stamp and body == "ready-out\n", stamped
        assert request("GET", f"/containers/{cid}/logs?stdout=1&stderr=1&since={int(time.time()) + 3600}") == b""
        assert len(frames(request("GET", f"/containers/{cid}/logs?stdout=1&stderr=1&tail=1"))) == 1
        print("PASS stdout/stderr selection, timestamps, since, and tail", flush=True)
    finally:
        connection.close()
        request("DELETE", f"/containers/{cid}?force=1", status=204)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2])
