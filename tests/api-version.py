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
            return reply.status, json.loads(reply.read())
        finally:
            connection.close()

    status, version = request("/version")
    assert status == 200, (status, version)
    for supported in (version["MinAPIVersion"], "1.47", version["ApiVersion"]):
        status, data = request("/v" + supported + "/version")
        assert status == 200, (supported, status, data)
        print("PASS supported", supported)
    for unsupported, reason in (("1.23", "too old"), ("99.0", "too new")):
        status, data = request("/v" + unsupported + "/info")
        assert status == 400 and reason in data["message"], (unsupported, status, data)
        print("PASS rejected", unsupported)
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
            assert reply.status == 400 and "too new" in data["message"], (reply.status, data)
            print("PASS reject before body", target)
        finally:
            connection.close()
    print("7 API version cases passed")


if __name__ == "__main__":
    check(sys.argv[1])
