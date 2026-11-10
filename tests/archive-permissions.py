#!/usr/bin/env python3
"""Check copy-out and export tar modes against a running engine.

Usage: python3 tests/archive-permissions.py /path/to/docker.sock [image]
The image must already be pulled. Only this test's container is removed.
"""
import io
import subprocess
import sys
import tarfile


def check(socket, image):
    docker = ["docker", "--host", "unix://" + socket]

    def run(*args):
        return subprocess.run(docker + list(args), check=True, capture_output=True, timeout=30).stdout

    cid = run("run", "-d", "--network", "none", image, "sleep", "120").decode().strip()
    try:
        run("exec", cid, "sh", "-c",
            "mkdir -p /tmp/kiri-archive-modes/dir; "
            "printf contents > /tmp/kiri-archive-modes/file; "
            "chmod 6751 /tmp/kiri-archive-modes/file; "
            "chmod 750 /tmp/kiri-archive-modes/dir; "
            "ln -s file /tmp/kiri-archive-modes/link")
        for state in ("running", "stopped"):
            if state == "stopped":
                run("stop", "--time", "0", cid)
                run("wait", cid)
            for operation in ("copy", "export"):
                data = run("cp", cid + ":/tmp/kiri-archive-modes", "-") if operation == "copy" else run("export", cid)
                with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                    entries = {member.name.removeprefix("./").rstrip("/"): member for member in archive}
                prefix = "kiri-archive-modes/" if operation == "copy" else "tmp/kiri-archive-modes/"
                for name, mode, kind in (("file", 0o6751, tarfile.REGTYPE),
                                         ("dir", 0o750, tarfile.DIRTYPE),
                                         ("link", 0o777, tarfile.SYMTYPE)):
                    member = entries[prefix + name]
                    assert (member.mode, member.type) == (mode, kind), (
                        state, operation, name, oct(member.mode), member.type)
                assert entries[prefix + "link"].linkname == "file"
                print("PASS", state, operation, flush=True)
    finally:
        removing = subprocess.run(docker + ["rm", "-f", cid], capture_output=True, timeout=30)
        if removing.returncode:
            if sys.exc_info()[0] is None:
                removing.check_returncode()
            else:
                print("Cleanup failed:", removing.stderr.decode(errors="replace"), file=sys.stderr)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
