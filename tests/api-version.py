#!/usr/bin/env python3
"""Check the advertised API range and rejection before an upload body is sent."""
import http.client
import json
import socket
import sys


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=5)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path):
    def request(target):
        connection = Connection(path)
        try:
            connection.request("GET", target)
            reply = connection.getresponse()
            return reply.status, reply.read()
        finally:
            connection.close()

    # A client older than 1.24 expects an error as plain text. Docker 29 answers
    # it so; Docker 28 and Kiri answer every client in JSON.
    def message(version, data):
        if tuple(int(number or 0) for number in version.split(".")) < (1, 24) and not data.startswith(b"{"):
            return data.decode().rstrip("\n")
        return json.loads(data)["message"]

    cases = 0
    status, version = request("/version")
    assert status == 200, (status, version)
    version = json.loads(version)
    # Docker 28 and Kiri start at 1.24, Docker 29 at 1.40.
    oldest, newest = version["MinAPIVersion"], version["ApiVersion"]
    older = "1.%d" % (int(oldest.split(".")[1]) - 1)
    too_old = "client version %s is too old. Minimum supported API version is " + oldest + ", please upgrade your client to a newer version"
    too_new = "client version %s is too new. Maximum supported API version is " + newest
    # A version is compared number by number: a third number of 0 changes nothing.
    for supported in (oldest, "1.47", newest, oldest + ".0", newest + ".0"):
        status, data = request("/v" + supported + "/version")
        assert status == 200 and json.loads(data)["ApiVersion"] == newest, (supported, status, data)
        print("PASS supported", supported)
        cases += 1
    # A missing or empty number counts as 0.
    for unsupported, expected in {older: too_old, "1.23": too_old, "1": too_old, "1.": too_old, ".47": too_old,
                                  ".": too_old, "1..47": too_old, "99.0": too_new, "2": too_new,
                                  newest + ".1": too_new, "1.99999999999999999999": too_new}.items():
        status, data = request("/v" + unsupported + "/info")
        assert status == 400 and message(unsupported, data) == expected % unsupported, (unsupported, status, data)
        print("PASS rejected", unsupported)
        cases += 1
    # Not a version prefix at all: no such route.
    for target in ("/v/_ping", "/v1.x/_ping", "/V1.47/_ping"):
        status, data = request(target)
        assert status == 404, (target, status, data)
        print("PASS not found", target)
        cases += 1
    for target in ("/v99.0/containers/create", "/v99.0/images/load"):
        connection = Connection(path)
        try:
            connection.putrequest("POST", target)
            connection.putheader("Content-Length", "100")
            connection.putheader("Expect", "100-continue")
            connection.endheaders()
            # Send no body: version negotiation must finish before an upload.
            reply = connection.getresponse()
            data = json.loads(reply.read())
            assert reply.status == 400 and data["message"] == too_new % "99.0", (reply.status, data)
            print("PASS reject before body", target)
            cases += 1
        finally:
            connection.close()
    # A rejected request without a body leaves the connection usable, and a
    # rejected HEAD has no body to get in the way of the next response.
    for method in ("GET", "HEAD"):
        connection = Connection(path)
        try:
            connection.request(method, "/v99.0/_ping")
            used = connection.sock
            reply = connection.getresponse()
            data = reply.read()
            assert reply.status == 400 and reply.getheader("Connection") != "close", (method, reply.status, reply.getheaders())
            assert data == b"" if method == "HEAD" else json.loads(data)["message"] == too_new % "99.0", (method, data)
            connection.request("GET", "/_ping")
            assert connection.sock is used, method
            reply = connection.getresponse()
            assert reply.status == 200 and reply.read() == b"OK", (method, reply.status)
            print("PASS connection kept after rejected", method)
            cases += 1
        finally:
            connection.close()
    print(cases, "API version cases passed")


if __name__ == "__main__":
    check(sys.argv[1])
