#!/usr/bin/env python3
"""Exercise SDK upload framing against a running engine with a cached image."""
import http.client
import io
import json
import socket
import sys
import tarfile


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=5)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    owned = []

    def request(method, target, data=b""):
        connection = Connection(path)
        try:
            connection.request(method, target, data, {"Content-Type": "application/json"})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    create = json.dumps({"Image": image, "Cmd": ["true"]}).encode()
    status, data = request("POST", "/v1.47/containers/create", create)
    assert status == 201, (status, data)
    target = json.loads(data)["Id"]
    owned.append(target)
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w") as output:
        entry = tarfile.TarInfo("http-framing-probe")
        entry.size = 7
        output.addfile(entry, io.BytesIO(b"content"))
    count = 0
    try:
        for streamed in (False, True):
            variants = ("fixed", "chunked", "trailers") if streamed else (
                "fixed", "chunked", "trailers", "duplicate-length", "extension")
            for variant in variants:
                for expect in (False, True):
                    connection = Connection(path)
                    payload = archive.getvalue() if streamed else create
                    chunked = variant in ("chunked", "trailers", "extension")
                    method = "PUT" if streamed else "POST"
                    url = ("/v1.47/containers/" + target + "/archive?path=/tmp"
                           if streamed else "/v1.47/containers/create")
                    try:
                        connection.putrequest(method, url)
                        connection.putheader("Content-Type", "application/x-tar" if streamed else "application/json")
                        if chunked:
                            connection.putheader("Transfer-Encoding", "chunked")
                        else:
                            connection.putheader("Content-Length", str(len(payload)))
                            if variant == "duplicate-length":
                                connection.putheader("Content-Length", str(len(payload)))
                        if variant == "trailers":
                            connection.putheader("Trailer", "X-Checksum")
                        if expect:
                            connection.putheader("Expect", "100-continue")
                        connection.endheaders()
                        if expect:
                            interim = b""
                            while len(interim) < 25:
                                received = connection.sock.recv(25 - len(interim))
                                assert received, "closed before 100 Continue"
                                interim += received
                            assert interim == b"HTTP/1.1 100 Continue\r\n\r\n", interim
                        if chunked:
                            extension = ";test=" + "x" * 128 if variant == "extension" else ""
                            for piece in (payload[:7], payload[7:]):
                                connection.send((f"{len(piece):x}{extension}\r\n").encode() + piece + b"\r\n")
                            connection.send(b"0\r\n" + (b"X-Checksum: test\r\n" if variant == "trailers" else b"") + b"\r\n")
                        else:
                            connection.send(payload)
                        response = connection.getresponse()
                        data = response.read()
                        assert response.status == (200 if streamed else 201), (streamed, variant, expect, response.status, data)
                        if not streamed:
                            owned.append(json.loads(data)["Id"])
                            connection.request("GET", "/_ping")
                            ping = connection.getresponse()
                            assert (ping.status, ping.read()) == (200, b"OK")
                        count += 1
                        print("PASS", "streamed" if streamed else "buffered", variant, "expect" if expect else "immediate", flush=True)
                    finally:
                        connection.close()
        print(count, "HTTP upload framing cases passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        cleanup_errors = []
        for cid in reversed(owned):
            try:
                status, data = request("DELETE", "/v1.47/containers/" + cid + "?force=true")
                if status != 204:
                    raise RuntimeError((status, data))
            except Exception as error:
                cleanup_errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if cleanup_errors and not primary_failure:
            raise RuntimeError("Could not remove all test containers") from cleanup_errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
