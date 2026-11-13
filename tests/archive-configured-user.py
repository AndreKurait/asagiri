#!/usr/bin/env python3
"""Compare copyUIDGID=true to the configured container user, as Docker does."""
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


def payload(directory):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        for name, kind, mode, content in [
            ("", tarfile.DIRTYPE, 0o750, b""),
            ("/secret", tarfile.REGTYPE, 0o400, b"readable by configured user\n"),
            ("/setid", tarfile.REGTYPE, 0o6751, b"setid modes survive chown\n"),
            ("/link", tarfile.SYMTYPE, 0o777, b""),
            ("/hard", tarfile.LNKTYPE, 0o400, b""),
        ]:
            header = tarfile.TarInfo(directory + name)
            header.type, header.mode = kind, mode
            header.uid, header.gid = 1001, 1002
            header.size = len(content)
            if kind == tarfile.SYMTYPE:
                header.linkname = "secret"
            elif kind == tarfile.LNKTYPE:
                header.linkname = directory + "/secret"
            archive.addfile(header, io.BytesIO(content) if content else None)
    return stream.getvalue()


def check(path, image):
    docker = ["docker", "--host", "unix://" + path]

    def cli(*args):
        return subprocess.run(docker + list(args), check=True, capture_output=True,
                              text=True, timeout=60).stdout.strip()

    def request(method, target, data=None):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, data, {"Content-Type": "application/x-tar"})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    users = [
        ("", (1001, 1002)),  # Empty Config.User preserves the archive owners.
        ("1003:1004", (1003, 1004)),
        ("1003", (1003, 0)),
        ("nobody", (65534, 65534)),
        ("root", (0, 0)),
        (":1004", (0, 1004)),
        ("root:nobody", (0, 65534)),
    ]
    for user, configured_owner in users:
        cid = cli("run", "-d", "--network", "none", "--user", user, image, "sleep", "300")
        try:
            for state in ("running", "stopped"):
                if state == "stopped":
                    cli("stop", "-t", "0", cid)
                    cli("wait", cid)
                for compressed in (False, True):
                    for copy_user in (False, True):
                        directory = f"configured-{state}-{int(compressed)}-{int(copy_user)}"
                        query = urlencode({"path": "/tmp", "copyUIDGID": str(copy_user).lower()})
                        raw = payload(directory)
                        status, response = request("PUT", f"/containers/{cid}/archive?{query}",
                                                   gzip.compress(raw) if compressed else raw)
                        assert status == 200, (user, state, status, response)
                        status, data = request("GET", f"/containers/{cid}/archive?" + urlencode({"path": "/tmp/" + directory}))
                        assert status == 200, (status, data)
                        owner = configured_owner if copy_user else (1001, 1002)
                        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                            members = {member.name.rstrip("/"): member for member in archive}
                            for suffix, mode in [("", 0o750), ("/secret", 0o400), ("/setid", 0o6751),
                                                 ("/link", 0o777), ("/hard", 0o400)]:
                                member = members[directory + suffix]
                                assert (member.uid, member.gid, member.mode) == (*owner, mode), (
                                    user, state, compressed, copy_user, suffix,
                                    (member.uid, member.gid, oct(member.mode)), owner, oct(mode))
                        if copy_user and state == "running":
                            assert cli("exec", cid, "cat", "/tmp/" + directory + "/secret") == "readable by configured user"
                print("PASS", repr(user), state, "configured owner, archive owner, gzip, links and modes", flush=True)
        finally:
            cli("rm", "-f", cid)

    cid = cli("create", "--network", "none", "--user", "asagiri-nonexistent-test-user", image, "true")
    try:
        status, response = request("PUT", f"/containers/{cid}/archive?path=/tmp&copyUIDGID=true",
                                   payload("invalid-user"))
        assert status == 400, (status, response)
        status, _ = request("GET", f"/containers/{cid}/archive?path=/tmp/invalid-user")
        assert status == 404, "An unresolved user must not leave extracted files"
        print("PASS unknown configured user fails before extracting files", flush=True)
    finally:
        cli("rm", "-f", cid)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
