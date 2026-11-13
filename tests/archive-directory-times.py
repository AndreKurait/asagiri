#!/usr/bin/env python3
"""Directory timestamps survive child extraction, like Docker's archive API."""
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
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as tar:
        for name, kind, mtime in entries:
            header = tarfile.TarInfo(name)
            header.type = tarfile.DIRTYPE if kind == "dir" else tarfile.REGTYPE
            header.mode = 0o750 if kind == "dir" else 0o640
            header.mtime = mtime
            header.size = 0 if kind == "dir" else 4
            tar.addfile(header, None if kind == "dir" else io.BytesIO(b"data"))
    return output.getvalue()


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

    cid = cli("run", "-d", "--network", "none", image, "sleep", "300")
    count = 0
    try:
        for state in ("running", "stopped"):
            if state == "stopped":
                cli("stop", "-t", "0", cid)
                cli("wait", cid)
            for compressed in (False, True):
                # "." describes the extraction root, which Docker leaves alone.
                # A deferred metadata pass must not accidentally change it.
                root_metadata = []
                for upload in (False, True):
                    if upload:
                        body = archive([(".", "dir", 7)])
                        if compressed:
                            body = gzip.compress(body)
                        status, data = request("PUT", f"/containers/{cid}/archive?path=/tmp", body)
                        assert status == 200, (state, "root-header", status, data)
                    status, data = request("GET", f"/containers/{cid}/archive?path=/tmp")
                    assert status == 200, (status, data)
                    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
                        member = next(m for m in tar if m.name.rstrip("/") == "tmp")
                        root_metadata.append((member.mode, member.uid, member.gid, member.mtime))
                assert root_metadata[0] == root_metadata[1], (
                    state, compressed, "root-header", root_metadata)
                count += 1
                for scenario in ("nested", "child-first", "merge", "duplicate", "epoch"):
                    count += 1
                    root = f"times-{count}"
                    dirs = [(root, "dir", 1_000_000_000),
                            (root + "/inner", "dir", 1_000_000_100)]
                    files = [(root + "/inner/file", "file", 1_000_000_200)]
                    if scenario == "merge":
                        status, data = request("PUT", f"/containers/{cid}/archive?path=/tmp",
                                               archive(dirs + [(root + "/kept", "file", 100)]))
                        assert status == 200, (status, data)
                    entries = files + dirs if scenario == "child-first" else dirs + files
                    if scenario == "duplicate":
                        entries += [(root, "dir", 1_000_000_300)]
                    if scenario == "epoch":
                        entries += [(root, "dir", 0)]
                    body = archive(entries)
                    if compressed:
                        body = gzip.compress(body)
                    status, data = request("PUT", f"/containers/{cid}/archive?path=/tmp", body)
                    assert status == 200, (state, scenario, status, data)
                    status, data = request("GET", f"/containers/{cid}/archive?" +
                                           urlencode({"path": "/tmp/" + root}))
                    assert status == 200, (status, data)
                    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
                        members = {m.name.rstrip("/"): m for m in tar}
                        expected = 0 if scenario == "epoch" else (
                            1_000_000_300 if scenario == "duplicate" else 1_000_000_000)
                        for name, mtime in ((root, expected), (root + "/inner", 1_000_000_100),
                                            (root + "/inner/file", 1_000_000_200)):
                            member = members[name]
                            assert member.mtime == mtime, (
                                state, compressed, scenario, name, member.mtime, mtime)
                            assert member.mode == (0o750 if member.isdir() else 0o640)
                        assert tar.extractfile(root + "/inner/file").read() == b"data"
                        if scenario == "merge":
                            assert tar.extractfile(root + "/kept").read() == b"data"
        print(f"PASS {count} directory timestamp scenarios", flush=True)
    finally:
        cli("rm", "-f", cid)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
