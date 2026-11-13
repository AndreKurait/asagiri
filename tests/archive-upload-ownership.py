#!/usr/bin/env python3
"""Verify numeric archive owners and restrictive modes on the upload API.

Run beside Docker or Asagiri: archive-upload-ownership.py SOCKET [IMAGE].
Only disposable test containers are modified. No real secrets are used.
"""
import gzip
import http.client
import io
import socket
import subprocess
import sys
import tarfile
from urllib.parse import urlencode


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def payload(directory, root_owned=False):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        entries = [
            (directory, tarfile.DIRTYPE, 0o750, 1001, b""),
            (directory + "/secret", tarfile.REGTYPE, 0o400, 1001, b"test-secret\n"),
            (directory + "/group", tarfile.REGTYPE, 0o040, 1003, b"group-readable\n"),
            (directory + "/link", tarfile.SYMTYPE, 0o777, 1001, b""),
        ]
        for name, kind, mode, uid, data in entries:
            member = tarfile.TarInfo(name)
            member.type, member.mode = kind, mode
            member.uid, member.gid = (0, 0) if root_owned else (uid, 1002)
            member.uname, member.gname = "archive-owner", "archive-group"
            member.size = len(data)
            if kind == tarfile.SYMTYPE:
                member.linkname = "secret"
            archive.addfile(member, io.BytesIO(data) if data else None)
    return stream.getvalue()


def check(path, image):
    docker = ["docker", "--host", "unix://" + path]

    def run(*args):
        return subprocess.run(docker + list(args), capture_output=True, check=True, timeout=60).stdout

    def request(method, target, data=None):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, data, {"Content-Type": "application/x-tar"})
            response = connection.getresponse()
            body = response.read()
            assert response.status == 200, (method, target, response.status, body)
            return body
        finally:
            connection.close()

    cid = run("run", "-d", "--network", "none", image, "sleep", "300").decode().strip()
    try:
        for state in ("running", "stopped"):
            if state == "stopped":
                run("stop", "-t", "0", cid)
                run("wait", cid)
            directories = []
            for compressed in (False, True):
                for explicit_false in (False, True):
                    directory = f"archive-owner-{state}-{int(compressed)}-{int(explicit_false)}"
                    directories.append(directory)
                    query = {"path": "/tmp"}
                    if explicit_false:
                        query["copyUIDGID"] = "false"
                    target = f"/containers/{cid}/archive?" + urlencode(query)
                    # Replacing existing root-owned entries must update ownership too.
                    request("PUT", target, payload(directory, root_owned=True))
                    data = payload(directory)
                    request("PUT", target, gzip.compress(data) if compressed else data)
                    copied = request("GET", f"/containers/{cid}/archive?" + urlencode({"path": "/tmp/" + directory}))
                    with tarfile.open(fileobj=io.BytesIO(copied)) as archive:
                        entries = {entry.name.rstrip("/"): entry for entry in archive}
                        for name, uid, mode in (("", 1001, 0o750), ("/secret", 1001, 0o400),
                                                ("/group", 1003, 0o040), ("/link", 1001, 0o777)):
                            entry = entries[directory + name]
                            assert (entry.uid, entry.gid, entry.mode) == (uid, 1002, mode), (
                                state, compressed, explicit_false, name, entry.uid, entry.gid, oct(entry.mode))
                        assert entries[directory + "/link"].linkname == "secret"
                        assert archive.extractfile(directory + "/secret").read() == b"test-secret\n"
                    print("PASS", state, "gzip" if compressed else "tar",
                          "copyUIDGID=false" if explicit_false else "copyUIDGID omitted", flush=True)
            if state == "stopped":
                run("start", cid)
            for directory in directories:
                output = run("exec", "--user", "1001:1002", cid, "sh", "-c",
                             'cat "$1/secret" "$1/group" "$1/link"', "sh", "/tmp/" + directory)
                assert output == b"test-secret\ngroup-readable\ntest-secret\n", (state, directory, output)
            print("PASS", state, "uploaded files readable as UID 1001, GID 1002", flush=True)
    finally:
        run("rm", "-f", cid)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
