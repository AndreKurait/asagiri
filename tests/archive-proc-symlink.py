#!/usr/bin/env python3
"""Check that /proc and /sys archive as empty directories when a link leads to them."""
import http.client
import json
import socket
import sys
import tarfile
import time
import urllib.parse

# "root" leads to the container's root, the other two straight to the kernel's directories.
SETUP = "mkdir /data && ln -s / /data/root && ln -s /proc /data/proc && ln -s /sys /data/sys && exec sleep 120"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class Body:
    """A response body that remembers its size and how it ended."""

    def __init__(self, response):
        self.response, self.size, self.end = response, 0, b""

    def read(self, size):
        data = self.response.read(size)
        self.size += len(data)
        self.end = (self.end + data)[-1024:]
        return data


def check(path, image):
    def request(method, target, expected, body=None):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status in expected, (method, target, response.status, data)
            return response.status, json.loads(data) if data else None
        finally:
            connection.close()

    def archive(source):
        connection = Connection(path)
        try:
            connection.request("GET", "/v1.47/containers/" + cid + "/archive?path="
                               + urllib.parse.quote(source, safe=""))
            response = connection.getresponse()
            assert response.status == 200, (source, response.status, response.read())
            body = Body(response)
            with tarfile.open(fileobj=body, mode="r|") as members:
                entries = {member.name: member.type for member in members}
            # tarfile takes a damaged entry for the end of the archive: a whole
            # archive is made of 512-byte blocks and ends with two empty ones.
            while body.read(1 << 16):
                pass
            assert body.size % 512 == 0 and body.end == bytes(1024), (
                source, "damaged archive", body.size, body.end[-80:])
            return entries
        finally:
            connection.close()

    _, created = request("POST", "/containers/create", (201,), {
        "Image": image, "Cmd": ["sh", "-c", SETUP], "HostConfig": {"NetworkMode": "none"}})
    cid = created["Id"]
    try:
        request("POST", "/containers/" + cid + "/start", (204,))
        deadline = time.monotonic() + 30
        while request("HEAD", "/containers/" + cid + "/archive?path=/data/sys", (200, 404))[0] != 200:
            assert time.monotonic() < deadline, "the container did not make its links"
            time.sleep(0.1)
        for state in ("running", "stopped"):
            if state == "stopped":
                request("POST", "/containers/" + cid + "/stop?t=0", (204,))
                request("POST", "/containers/" + cid + "/wait", (200,))
            # In Docker's archive of a container, running or not, /proc and
            # /sys are empty directories, whatever name they were reached by.
            for name in ("proc", "sys"):
                for source in ("/" + name, "/data/root/" + name, "/data/" + name + "/"):
                    entries = archive(source)
                    assert entries == {name: tarfile.DIRTYPE}, (state, source, sorted(entries)[:8])
                    print("PASS", state, source, flush=True)
            # The whole root through the link: both directories are in it,
            # empty, and the walk goes on after them. The link is not followed again.
            entries = archive("/data/root/")
            inside = [entry for entry in entries if entry.startswith(("root/proc/", "root/sys/"))]
            assert not inside, (state, "/data/root/", inside[:8])
            for entry, kind in (("root", tarfile.DIRTYPE), ("root/proc", tarfile.DIRTYPE),
                                ("root/sys", tarfile.DIRTYPE), ("root/var", tarfile.DIRTYPE),
                                ("root/data/root", tarfile.SYMTYPE)):
                assert entries.get(entry) == kind, (state, "/data/root/", entry, entries.get(entry))
            print("PASS", state, "/data/root/", flush=True)
    finally:
        failing = sys.exc_info()[0] is not None
        try:
            request("DELETE", "/containers/" + cid + "?force=true", (204,))
        except Exception as error:
            if not failing:
                raise
            print("Cleanup failed:", error, file=sys.stderr)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
