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


def response(reader):
    """Use one reader across replies so an unexpected extra reply cannot vanish."""
    line = reader.readline()
    assert line.startswith(b"HTTP/1.1 "), line
    status = int(line.split()[1])
    headers = {}
    while True:
        line = reader.readline()
        if line == b"\r\n":
            break
        assert line.endswith(b"\r\n"), line
        key, value = line.split(b":", 1)
        headers[key.lower()] = value.strip().lower()
    if status in (100, 204, 304):
        return status, b""
    if headers.get(b"transfer-encoding") == b"chunked":
        data = bytearray()
        while True:
            size = int(reader.readline().split(b";", 1)[0], 16)
            if size == 0:
                while True:
                    trailer = reader.readline()
                    assert trailer.endswith(b"\r\n"), trailer
                    if trailer == b"\r\n":
                        break
                return status, bytes(data)
            chunk = reader.read(size)
            assert len(chunk) == size
            data.extend(chunk)
            assert reader.read(2) == b"\r\n"
    size = int(headers[b"content-length"])
    data = reader.read(size)
    assert len(data) == size
    return status, data


def refused(path, wire):
    """Send raw bytes; return every reply's status once the engine has closed."""
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(5)
        sock.connect(path)
        sock.sendall(wire)
        data = bytearray()
        while True:
            try:
                piece = sock.recv(65536)
            except ConnectionResetError:
                break
            if not piece:
                break
            data.extend(piece)
    return [int(line.split()[1]) for line in bytes(data).split(b"\r\n") if line.startswith(b"HTTP/1.1 ")]


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
            for expect in (False, True):
                for variant in variants:
                    connection = Connection(path)
                    payload = archive.getvalue() if streamed else create
                    chunked = variant in ("chunked", "trailers", "extension")
                    method = "PUT" if streamed else "POST"
                    url = ("/v1.47/containers/" + target + "/archive?path=/tmp"
                           if streamed else "/v1.47/containers/create")
                    reader = None
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
                        reader = connection.sock.makefile("rb")
                        if expect:
                            assert response(reader) == (100, b"")
                        if chunked:
                            extension = ";test=" + "x" * 128 if variant == "extension" else ""
                            for piece in (payload[:7], payload[7:]):
                                connection.send((f"{len(piece):x}{extension}\r\n").encode() + piece + b"\r\n")
                            connection.send(b"0\r\n" + (b"X-Checksum: test\r\n" if variant == "trailers" else b"") + b"\r\n")
                        else:
                            connection.send(payload)
                        status, data = response(reader)
                        assert status == (200 if streamed else 201), (streamed, variant, expect, status, data)
                        if not streamed:
                            owned.append(json.loads(data)["Id"])
                            connection.send(b"GET /_ping HTTP/1.1\r\nHost: localhost\r\n\r\n")
                            ping = response(reader)
                            assert ping == (200, b"OK"), (streamed, variant, expect, ping)
                        count += 1
                        print("PASS", "streamed" if streamed else "buffered", variant, "expect" if expect else "immediate", flush=True)
                    finally:
                        if reader is not None:
                            reader.close()
                        connection.close()
        # Framing that cannot be trusted is refused and the connection closed:
        # the request behind it must not be answered.
        for name, framing, want in (
                ("conflicting-length", b"Content-Length: 2\r\nContent-Length: 3\r\n", 400),
                ("signed-length", b"Content-Length: +2\r\n", 400),
                ("padded-length-name", b"Content-Length : 2\r\n", 400),
                ("unsupported-coding", b"Transfer-Encoding: gzip\r\n", 501),
                ("repeated-coding", b"Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n", 501)):
            statuses = refused(path, b"POST /v1.47/containers/create HTTP/1.1\r\nHost: localhost\r\n"
                               b"Content-Type: application/json\r\n" + framing + b"\r\n{}"
                               b"GET /_ping HTTP/1.1\r\nHost: localhost\r\n\r\n")
            assert statuses == [want], (name, statuses)
            count += 1
            print("PASS refused", name, flush=True)
        # An upload refused before its body is read gets no interim reply.
        connection = Connection(path)
        try:
            connection.putrequest("PUT", "/v1.47/containers/http-framing-missing/archive?path=/tmp")
            connection.putheader("Content-Type", "application/x-tar")
            connection.putheader("Content-Length", str(len(archive.getvalue())))
            connection.putheader("Expect", "100-continue")
            connection.endheaders()
            with connection.sock.makefile("rb") as reader:
                status, data = response(reader)
            assert status == 404, (status, data)
            count += 1
            print("PASS refused missing-container expect", flush=True)
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
