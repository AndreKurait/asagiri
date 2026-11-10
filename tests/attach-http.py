#!/usr/bin/env python3
"""Exercise attach and exec response negotiation against a running engine.

Usage: python3 tests/attach-http.py /path/to/docker.sock [image]
The image must already be pulled. Only this script's containers are removed.
"""
import http.client
import json
import socket
import struct
import sys


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=10)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def request(path, method, url, body=None, expected=200):
    conn = Connection(path)
    try:
        conn.request(method, "/v1.47" + url,
                     None if body is None else json.dumps(body),
                     {"Content-Type": "application/json"})
        response = conn.getresponse()
        data = response.read()
        assert response.status == expected, (url, response.status, data)
        return json.loads(data) if data else None
    finally:
        conn.close()


def output(data, tty):
    if tty:
        return data
    result = bytearray()
    while data:
        assert len(data) >= 8, data
        stream, length = data[0], struct.unpack(">I", data[4:8])[0]
        assert stream in (1, 2) and data[1:4] == b"\0\0\0", data[:8]
        assert len(data) >= 8 + length, data
        result.extend(data[8:8 + length])
        data = data[8 + length:]
    return bytes(result)


def check(path, image, headers, status, tty, attach):
    create = {"Image": image, "Cmd": ["sh", "-c", "printf out; printf err >&2"]
              if attach else ["sleep", "120"], "Tty": tty,
              "HostConfig": {"NetworkMode": "none"}}
    cid = request(path, "POST", "/containers/create", create, 201)["Id"]
    conn = Connection(path)
    try:
        if attach:
            url = f"/containers/{cid}/attach?stream=1&stdout=1&stderr=1"
            body = None
        else:
            request(path, "POST", f"/containers/{cid}/start", expected=204)
            eid = request(path, "POST", f"/containers/{cid}/exec", {
                "Cmd": ["sh", "-c", "printf out; printf err >&2"],
                "AttachStdout": True, "AttachStderr": True, "Tty": tty,
            }, 201)["Id"]
            url, body = f"/exec/{eid}/start", json.dumps({"Detach": False, "Tty": tty})
        conn.request("POST", "/v1.47" + url, body,
                     {"Content-Type": "application/json", **headers})
        response = conn.getresponse()
        assert response.status == status, (url, headers, response.status)
        assert response.getheader("Content-Length") is None
        assert response.getheader("Transfer-Encoding") is None
        if status == 200:
            assert response.getheader("Content-Type") == "application/vnd.docker.raw-stream"
        else:
            assert response.getheader("Upgrade") == "tcp"
        if attach:
            # SDKs receive a final response before starting the container.
            request(path, "POST", f"/containers/{cid}/start", expected=204)
        # Read the hijacked 101 stream as well as the close-delimited 200 body.
        data = output(response.fp.read(), tty)
        assert data in (b"outerr", b"errout"), data
    finally:
        conn.close()
        request(path, "DELETE", f"/containers/{cid}?force=1", expected=204)


def main():
    path, image = sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine"
    cases = [
        ({}, 200),
        ({"Connection": "Upgrade"}, 200),
        ({"Upgrade": "tcp"}, 101),
        ({"upgrade": "other"}, 101),
        ({"Upgrade": "TCP", "Connection": "keep-alive, Upgrade"}, 101),
    ]
    for attach in (True, False):
        for tty in (False, True):
            for headers, status in cases:
                check(path, image, headers, status, tty, attach)
    print("20 attach/exec HTTP negotiation cases passed")


if __name__ == "__main__":
    main()
