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
            "mkdir -p /tmp/asagiri-archive-modes/dir; "
            "printf contents > /tmp/asagiri-archive-modes/file; "
            "chmod 6751 /tmp/asagiri-archive-modes/file; "
            "chmod 1750 /tmp/asagiri-archive-modes/dir; "
            "ln -s file /tmp/asagiri-archive-modes/link")
        for state in ("running", "stopped"):
            if state == "stopped":
                run("stop", "-t", "0", cid)
                run("wait", cid)
            for operation in ("copy", "export"):
                data = run("cp", cid + ":/tmp/asagiri-archive-modes", "-") if operation == "copy" else run("export", cid)
                with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                    entries = {(member.name[2:] if member.name.startswith("./") else member.name).rstrip("/"): member for member in archive}
                prefix = "asagiri-archive-modes/" if operation == "copy" else "tmp/asagiri-archive-modes/"
                for name, mode, kind in (("file", 0o6751, tarfile.REGTYPE),
                                         ("dir", 0o1750, tarfile.DIRTYPE),
                                         ("link", 0o777, tarfile.SYMTYPE)):
                    member = entries[prefix + name]
                    assert (member.mode, member.type) == (mode, kind), (
                        state, operation, name, oct(member.mode), member.type)
                assert entries[prefix + "link"].linkname == "file"
                print("PASS", state, operation, flush=True)
            # A file copied on its own is archived by itself, not as part of a tree.
            with tarfile.open(fileobj=io.BytesIO(run("cp", cid + ":/tmp/asagiri-archive-modes/file", "-"))) as archive:
                got = [(member.name, member.mode, member.type) for member in archive]
            assert got == [("file", 0o6751, tarfile.REGTYPE)], (state, "copy-file", got)
            print("PASS", state, "copy-file", flush=True)
    finally:
        removing = subprocess.run(docker + ["rm", "-f", cid], capture_output=True, timeout=30)
        if removing.returncode:
            if sys.exc_info()[0] is None:
                removing.check_returncode()
            else:
                print("Cleanup failed:", removing.stderr.decode(errors="replace"), file=sys.stderr)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
