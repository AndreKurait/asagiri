#!/usr/bin/env python3
"""Check Docker's directory-versus-contents archive naming, including path stat."""
import base64
import http.client
import io
import json
import socket
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse


def check(path, image):
    cli = ["docker", "-H", "unix://" + path]

    def run(*args):
        return subprocess.run(cli + list(args), check=True, capture_output=True, timeout=30).stdout

    cid = run("run", "-d", "--network", "none", image, "sleep", "120").decode().strip()
    try:
        run("exec", cid, "sh", "-c",
            "mkdir -p /tmp/archive-directory-contents && printf content > /tmp/archive-directory-contents/file")
        for state in ("running", "stopped"):
            if state == "stopped":
                run("stop", "--time", "0", cid)
                run("wait", cid)
            for suffix in ("", "/", "/.", "/./"):
                connection = http.client.HTTPConnection("localhost", timeout=5)
                connection.sock = socket.socket(socket.AF_UNIX)
                connection.sock.settimeout(5)
                connection.sock.connect(path)
                try:
                    source = "/tmp/archive-directory-contents" + suffix
                    connection.request("GET", "/v1.47/containers/" + cid + "/archive?path="
                                       + urllib.parse.quote(source, safe=""))
                    response = connection.getresponse()
                    data = response.read()
                    assert response.status == 200, (response.status, data)
                    stat = json.loads(base64.b64decode(response.getheader("X-Docker-Container-Path-Stat")))
                    name = "." if suffix in ("/.", "/./") else "archive-directory-contents"
                    assert stat["name"] == name, (state, source, stat)
                    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                        assert archive.getnames() == [name, name + "/file"], (state, source, archive.getnames())
                        assert archive.extractfile(name + "/file").read() == b"content"
                    print("PASS", state, source, flush=True)
                finally:
                    connection.close()
            # Docker rebases "." to this new destination name. Every child
            # must keep "./" for that rebase to include it in the directory.
            with tempfile.TemporaryDirectory(prefix="kiri-archive-contents-") as destination:
                from pathlib import Path
                copied = Path(destination) / "new-directory"
                run("cp", cid + ":/tmp/archive-directory-contents/.", str(copied))
                assert (copied / "file").read_bytes() == b"content"
                assert not (Path(destination) / "file").exists()
                print("PASS", state, "docker cp into new directory", flush=True)
    finally:
        result = subprocess.run(cli + ["rm", "-f", cid], capture_output=True, timeout=30)
        if result.returncode:
            if sys.exc_info()[0] is None:
                result.check_returncode()
            else:
                print("Cleanup failed:", result.stderr.decode(errors="replace"), file=sys.stderr)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
