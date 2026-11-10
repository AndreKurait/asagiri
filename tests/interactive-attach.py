#!/usr/bin/env python3
"""Compare attach history, detach keys, stdin lifetime and WebSocket streams."""
import base64
import hashlib
import http.client
import json
import os
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


def check(path, image):
    owned = []

    def request(method, target, body=None, expected=200):
        conn = Connection(path)
        try:
            conn.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                         {"Content-Type": "application/json"})
            response = conn.getresponse()
            data = response.read()
            if target == "/containers/create" and response.status == 201:
                owned.append(json.loads(data)["Id"])
            assert response.status == expected, (target, response.status, data)
            return json.loads(data) if data else None
        finally:
            conn.close()

    def create(command, **config):
        return request("POST", "/containers/create", {
            "Image": image, "Cmd": command, "HostConfig": {"NetworkMode": "none"}, **config}, 201)["Id"]

    def start(cid):
        request("POST", "/containers/" + cid + "/start", expected=204)

    def running(cid):
        return request("GET", "/containers/" + cid + "/json")["State"]["Running"]

    def attach(cid, query, websocket=False, version=47, early=b""):
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(10)
        reader = None
        try:
            client.connect(path)
            key = base64.b64encode(os.urandom(16)).decode()
            target = f"/v1.{version}/containers/{cid}/attach" + ("/ws" if websocket else "") + "?" + query
            headers = (f"Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
                       if websocket else "Upgrade: tcp\r\nContent-Length: 0\r\n")
            client.sendall((f"{'GET' if websocket else 'POST'} {target} HTTP/1.1\r\n"
                            f"Host: localhost\r\nConnection: Upgrade\r\n{headers}\r\n").encode() + early)
            reader = client.makefile("rb")
            status = reader.readline()
            assert b" 101 " in status, status
            received = {}
            while True:
                line = reader.readline()
                assert line, "upgrade headers ended early"
                if line == b"\r\n":
                    break
                name, value = line.decode().split(":", 1)
                received[name.lower()] = value.strip()
            if websocket:
                expected = base64.b64encode(hashlib.sha1(
                    (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
                assert received["sec-websocket-accept"] == expected, received
            return client, reader
        except BaseException:
            if reader:
                reader.close()
            client.close()
            raise

    def frame(data, opcode=2, final=True):
        mask = os.urandom(4)
        length = len(data)
        size = bytes([0x80 | length]) if length < 126 else b"\xfe" + struct.pack("!H", length)
        return bytes([(0x80 if final else 0) | opcode]) + size + mask + bytes(
            value ^ mask[i % 4] for i, value in enumerate(data))

    def receive(reader):
        header = reader.read(2)
        assert len(header) == 2, header
        first, length = header
        assert not length & 0x80, "server frames must not be masked"
        if length == 126:
            length = struct.unpack("!H", reader.read(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", reader.read(8))[0]
        assert length <= 8 * 1024 * 1024, length
        data = reader.read(length)
        assert len(data) == length, (len(data), length)
        return first & 15, data

    def streams(data):
        result = {1: b"", 2: b""}
        while data:
            assert len(data) >= 8, data
            kind, size = data[0], struct.unpack("!I", data[4:8])[0]
            assert kind in result and data[1:4] == b"\0\0\0" and len(data) >= 8 + size, data
            result[kind] += data[8:8 + size]
            data = data[8 + size:]
        return result

    try:
        history = create(["sh", "-c", "printf history; printf hidden >&2"])
        start(history)
        assert request("POST", "/containers/" + history + "/wait")["StatusCode"] == 0
        client, reader = attach(history, "logs=1&stream=0&stdout=1&stderr=0")
        try:
            assert streams(reader.read()) == {1: b"history", 2: b""}
        finally:
            reader.close()
            client.close()
        print("PASS raw attach history and stream filtering", flush=True)

        cid = create(["cat"], OpenStdin=True, Tty=True)
        start(cid)
        client, reader = attach(cid, "stream=1&stdin=1&stdout=1&detachKeys=ctrl-a,ctrl-b")
        try:
            client.sendall(b"\x01")
            client.sendall(b"\x02")
            assert reader.read() == b"", "detach did not close the stream"
            assert running(cid), "detach stopped the container"
        finally:
            reader.close()
            client.close()
        print("PASS custom detach keys preserve the running container", flush=True)

        for once in (False, True):
            cid = create(["cat"], OpenStdin=True, StdinOnce=once)
            start(cid)
            client, reader = attach(cid, "stream=1&stdin=1&stdout=1")
            try:
                client.sendall(b"first\n")
                header = reader.read(8)
                assert len(header) == 8 and header[0] == 1, header
                assert reader.read(struct.unpack("!I", header[4:])[0]) == b"first\n"
                client.shutdown(socket.SHUT_WR)
                if once:
                    assert reader.read() == b""
                    assert request("POST", "/containers/" + cid + "/wait")["StatusCode"] == 0
            finally:
                reader.close()
                client.close()
            if not once:
                # A second connection proves stdin remains usable after EOF.
                client, reader = attach(cid, "stream=1&stdin=1&stdout=1")
                try:
                    client.sendall(b"second\n")
                    header = reader.read(8)
                    assert len(header) == 8 and header[0] == 1, header
                    assert reader.read(struct.unpack("!I", header[4:])[0]) == b"second\n"
                finally:
                    reader.close()
                    client.close()
        print("PASS raw attach StdinOnce and reconnect", flush=True)

        for version, once in ((47, False), (47, True), (41, False), (27, False)):
            cid = create(["cat"], OpenStdin=True, StdinOnce=once)
            start(cid)
            query = "stream=1" + ("&stdin=1&stdout=1&stderr=1" if version >= 42 else "")
            # The first fragment travels in the HTTP request's buffered tail.
            client, reader = attach(cid, query, True, version, frame(b"websocket-", final=False))
            try:
                client.sendall(frame(b"roundtrip\n", opcode=0))
                output = b""
                while len(output) < len(b"websocket-roundtrip\n"):
                    opcode, data = receive(reader)
                    assert opcode == (2 if version >= 28 else 1), opcode
                    output += data
                assert output == b"websocket-roundtrip\n", output
                client.sendall(frame(b"alive", opcode=9))
                assert receive(reader) == (10, b"alive")
                client.sendall(frame(b"\x03\xe8", opcode=8))
                assert receive(reader)[0] == 8
            finally:
                reader.close()
                client.close()
            if once:
                assert request("POST", "/containers/" + cid + "/wait")["StatusCode"] == 0
            else:
                assert running(cid)
            print(f"PASS WebSocket version=1.{version} StdinOnce={once}, fragments, ping and close", flush=True)

        client, reader = attach(history, "logs=1&stream=0&stdout=0&stderr=1", True)
        try:
            output = b""
            while True:
                opcode, data = receive(reader)
                if opcode == 8:
                    break
                assert opcode == 2, opcode
                output += data
            assert output == b"hidden", output
        finally:
            reader.close()
            client.close()
        print("PASS WebSocket log replay and stderr filtering", flush=True)
        print("8 interactive attach scenarios passed")
    finally:
        failed = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(owned):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", expected=204)
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not failed:
            raise RuntimeError("Could not remove all attach-test containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
