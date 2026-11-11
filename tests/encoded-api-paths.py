#!/usr/bin/env python3
"""Reach images and containers through escaped URL paths, as SDKs send them."""
import http.client
import json
import socket
import sys
import urllib.parse
import uuid


def check(path, image):
    def request(method, target, body=None):
        connection = http.client.HTTPConnection("localhost", timeout=5)
        connection.sock = socket.socket(socket.AF_UNIX)
        connection.sock.settimeout(5)
        connection.sock.connect(path)
        try:
            connection.request(method, target, body, {"Content-Type": "application/json"} if body else {})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    repository = "asagiri-path-test/" + uuid.uuid4().hex
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

    # A container name, and the version prefix itself, may carry escapes.
    container = "asagiri-path-test-" + uuid.uuid4().hex + "1"
    status, data = request("POST", "/v1.47/containers/create?name=" + container, json.dumps({"Image": image}))
    assert status == 201, (status, data)
    try:
        escaped = container[:-1] + "%31"
        for target in ("/containers/" + escaped + "/json", "/v1.47/containers/" + escaped + "/json",
                       "/v1%2E47/containers/" + escaped + "/json", "/%761.47/containers/" + container + "/json",
                       "/v1.47%2Fcontainers/" + container + "/json", "/v1.47/containers%2f" + escaped + "%2Fjson"):
            status, data = request("GET", target)
            assert status == 200 and json.loads(data)["Name"] == "/" + container, (target, status, data)
            print("PASS", target, flush=True)
    finally:
        status, data = request("DELETE", "/v1.47/containers/" + container)
        if status != 204:
            if sys.exc_info()[0] is None:
                raise RuntimeError((status, data))
            print("Cleanup failed:", status, data, file=sys.stderr)
    for target in ("/v1.47/%5fping", "/v1%2E47/_ping", "/%761.47/_ping", "/v1.47%2f_ping"):
        status, data = request("GET", target)
        assert status == 200 and data == b"OK", (target, status, data)
        print("PASS", target, flush=True)

    # An escape is decoded once, "+" is not a space in a path, and an escaped
    # control character stays part of the name.
    for target, name in (("a%252Fb", "a%2Fb"), ("a+b", "a+b"), ("a%2Bb", "a+b"), ("a%0Db", "a\rb")):
        status, data = request("GET", "/v1.47/containers/" + target + "/json")
        assert status == 404 and json.loads(data)["message"] == "No such container: " + name, (target, status, data)
        print("PASS", target, flush=True)
    # Docker has no route for a name with a line feed; either way it is not found.
    status, data = request("GET", "/v1.47/containers/a%0Ab/json")
    assert status == 404, (status, data)
    print("PASS a%0Ab", flush=True)

    # A "%" that does not start an escape is a bad request, not a name.
    for target in ("/containers/%zz/json", "/v1.47/containers/a%/json", "/networks/net%a", "/v1%2/_ping"):
        status, data = request("GET", target)
        assert status == 400, (target, status, data)
        print("PASS", target, flush=True)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
