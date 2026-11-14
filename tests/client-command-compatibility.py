#!/usr/bin/env python3
"""Check SDK command defaults and real EOF after Connection: close requests."""
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


def check(path, image):
    owned = []

    def request(method, target, expected, body=None, close=False):
        connection = Connection(path)
        closing_socket = None
        try:
            headers = {"Content-Type": "application/json"}
            if close:
                headers["Connection"] = "keep-alive, Close"
            connection.request(method, "/v1.47" + target,
                               None if body is None else json.dumps(body), headers)
            closing_socket = connection.sock.dup() if close else None
            response = connection.getresponse()
            data = response.read()
            assert response.status in ((expected,) if isinstance(expected, int) else expected), (target, response.status, data)
            if close:
                # A bodyless HTTP response can finish before the peer closes.
                # Test the socket itself, as clients waiting for EOF do.
                assert closing_socket.recv(1) == b"", "server did not close"
            return json.loads(data) if data else None
        finally:
            if closing_socket is not None:
                closing_socket.close()
            connection.close()

    try:
        request("GET", "/version", 200, close=True)
        defaults = request("GET", "/images/" + image + "/json", 200)["Config"]
        for overrides in ({"Cmd": []}, {"Cmd": None}, {"Entrypoint": [], "Cmd": []}):
            created = request("POST", "/containers/create", 201, {
                "Image": image, "HostConfig": {"NetworkMode": "none"}, **overrides,
            })
            cid = created["Id"]
            owned.append(cid)
            config = request("GET", "/containers/" + cid + "/json", 200)["Config"]
            assert config["Cmd"] == defaults["Cmd"], (overrides, config)
            assert (config.get("Entrypoint") or []) == ([] if "Entrypoint" in overrides else defaults.get("Entrypoint") or []), config
            request("POST", "/containers/" + cid + "/start", 204, close=True)
            request("POST", "/containers/" + cid + "/stop?t=0", (204, 304), close=True)
            request("DELETE", "/containers/" + cid, 204, close=True)
            owned.remove(cid)
        print("PASS SDK empty Cmd/default Entrypoint and EOF on GET/start/stop/remove", flush=True)
    finally:
        for cid in owned:
            request("DELETE", "/containers/" + cid + "?force=true", 204)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2])
