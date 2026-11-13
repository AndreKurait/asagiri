#!/usr/bin/env python3
"""Compare archive replacement and noOverwriteDirNonDir with Docker."""
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


def archive(entries):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w") as tar:
        for path, kind, value in entries:
            header = tarfile.TarInfo(path)
            header.mode = 0o755 if kind == "dir" else 0o644
            header.type = {"dir": tarfile.DIRTYPE, "file": tarfile.REGTYPE,
                           "link": tarfile.SYMTYPE}[kind]
            content = value.encode() if kind == "file" else b""
            header.size = len(content)
            if kind == "link":
                header.linkname = value
            tar.addfile(header, io.BytesIO(content) if content else None)
    return out.getvalue()


def check(path, image):
    docker = ["docker", "--host", "unix://" + path]

    def cli(*args):
        return subprocess.run(docker + list(args), check=True, capture_output=True,
                              text=True, timeout=60).stdout.strip()

    def request(method, target, body=None):
        conn = Connection(path)
        try:
            conn.request(method, "/v1.47" + target, body,
                         {"Content-Type": "application/x-tar"})
            response = conn.getresponse()
            return response.status, response.read()
        finally:
            conn.close()

    def put(destination, entries, option=None, compressed=False):
        query = {"path": destination}
        if option is not None:
            query["noOverwriteDirNonDir"] = str(option).lower()
        data = archive(entries)
        return request("PUT", f"/containers/{cid}/archive?" + urlencode(query),
                       gzip.compress(data) if compressed else data)

    cid = cli("run", "-d", "--network", "none", image, "sleep", "300")
    failures = []
    count = 0
    try:
        for state in ("running", "stopped"):
            if state == "stopped":
                cli("stop", "-t", "0", cid)
                cli("wait", cid)
            for old in ("file", "dir", "link-file", "link-dir"):
                for new in ("file", "dir", "link"):
                    for option in (None, False, True):
                        for absolute in (False, True):
                            count += 1
                            case = f"overwrite-{count}"
                            target = case + "/target"
                            initial = [(case, "dir", ""),
                                       (case + "/referent", "file", "untouched"),
                                       (case + "/referent-dir", "dir", ""),
                                       (case + "/referent-dir/kept", "file", "untouched")]
                            if old == "dir":
                                initial += [(target, "dir", ""), (target + "/kept", "file", "old")]
                            elif old.startswith("link"):
                                initial += [(target, "link", "referent" if old == "link-file" else "referent-dir")]
                            else:
                                initial += [(target, "file", "old")]
                            status, data = put("/tmp", initial)
                            assert status == 200, ("fixture", status, data)
                            name = "/target" if absolute else "target"
                            entries = [(name, new, "referent" if new == "link" else "new")]
                            if new == "dir":
                                entries.append((name + "/added", "file", "new"))
                            status, data = put("/tmp/" + case, entries, option, compressed=absolute)
                            rejected = option is True and ((old == "dir") != (new == "dir"))
                            expected = 500 if rejected else 200
                            label = (state, old, new, option, absolute)
                            if status != expected:
                                failures.append((label, "status", status, expected, data[:180]))
                                continue
                            status, data = request("GET", f"/containers/{cid}/archive?" +
                                                   urlencode({"path": "/tmp/" + case}))
                            assert status == 200, (status, data)
                            with tarfile.open(fileobj=io.BytesIO(data)) as tar:
                                members = {m.name.rstrip("/"): m for m in tar}
                                node = members[target]
                                kind = old.split("-")[0] if rejected else new
                                assert {"file": node.isfile, "dir": node.isdir,
                                        "link": node.issym}[kind](), (label, node.type)
                                for referent in (case + "/referent", case + "/referent-dir/kept"):
                                    assert tar.extractfile(members[referent]).read() == b"untouched", label
                                if kind == "file":
                                    expected_content = b"old" if rejected else b"new"
                                    assert tar.extractfile(node).read() == expected_content, label
                                if not rejected and new == "dir":
                                    assert tar.extractfile(members[target + "/added"]).read() == b"new", label
                                if old == "dir" and (rejected or new == "dir"):
                                    assert tar.extractfile(members[target + "/kept"]).read() == b"old", label
            print("PASS" if not failures else "CHECKED", state, "archive replacement matrix", flush=True)
    finally:
        cli("rm", "-f", cid)
    assert not failures, f"{len(failures)} failures: {failures[:12]}"
    print(f"PASS {count} archive replacement cases, untouched symlink targets, gzip, and noOverwriteDirNonDir")


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
