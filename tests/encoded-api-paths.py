#!/usr/bin/env python3
"""Inspect a tagged image through the escaped URL paths used by SDKs."""
import http.client
import json
import socket
import sys
import urllib.parse
import uuid


def check(path, image):
    def request(method, target):
        connection = http.client.HTTPConnection("localhost", timeout=5)
        connection.sock = socket.socket(socket.AF_UNIX)
        connection.sock.settimeout(5)
        connection.sock.connect(path)
        try:
            connection.request(method, target)
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    repository = "kiri-path-test/" + uuid.uuid4().hex
    name = repository + ":sdk"
    source = urllib.parse.quote(image, safe="/:")
    status, data = request("POST", "/v1.47/images/" + source + "/tag?repo="
                           + urllib.parse.quote(repository, safe="") + "&tag=sdk")
    # Tagging is setup; upstream currently returns 204 instead of Docker's 201.
    assert status in (201, 204), (status, data)
    try:
        status, data = request("GET", "/v1.47/images/" + name + "/json")
        assert status == 200, (status, data)
        expected = json.loads(data)["Id"]
        for version in ("", "/v1.47"):
            escaped = urllib.parse.quote(name, safe="").replace("%2F", "%2f")
            status, data = request("GET", version + "/images/" + escaped + "/json")
            assert status == 200, (status, data)
            assert json.loads(data)["Id"] == expected
            print("PASS", version or "unversioned", escaped, flush=True)
    finally:
        status, data = request("DELETE", "/v1.47/images/" + name)
        if status != 200:
            if sys.exc_info()[0] is None:
                raise RuntimeError((status, data))
            print("Cleanup failed:", status, data, file=sys.stderr)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
