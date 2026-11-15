#!/usr/bin/env python3
"""Preserve renamed directories and chmod-only files when committing a layer.

Kernel controls enable features only on private mounts, never through shared
module parameters. All engine objects and paths belong to this invocation.
"""
import ctypes
import errno
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import uuid


def kernel_control(root):
    libc = ctypes.CDLL(None, use_errno=True)
    libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p,
                           ctypes.c_ulong, ctypes.c_char_p]
    assert libc.unshare(0x00020000) == 0, os.strerror(ctypes.get_errno())
    assert libc.mount(None, b"/", None, (1 << 14) | (1 << 18), None) == 0
    payload = b"data must survive a metadata-only change\n"
    results = {}
    for mode in ("on", "off"):
        base = root / mode
        for name in ("lower", "upper", "work", "merged", "overlap"):
            (base / name).mkdir(parents=True)
        (base / "lower/plain").write_bytes(payload)
        (base / "lower/directory").mkdir()
        (base / "lower/directory/child").write_bytes(payload)
        options = (f"lowerdir={base}/lower,upperdir={base}/upper,workdir={base}/work,"
                   f"index=on,metacopy={mode},redirect_dir={mode}").encode()
        assert libc.mount(b"overlay", os.fsencode(base / "merged"), b"overlay", 0, options) == 0, os.strerror(ctypes.get_errno())
        try:
            # The cleanup barrier must work with exclusive-upper protection.
            mounted = libc.mount(b"overlay", os.fsencode(base / "overlap"), b"overlay", 0, options)
            assert mounted == -1 and ctypes.get_errno() == errno.EBUSY, (mounted, ctypes.get_errno())
            os.chmod(base / "merged/plain", 0o600)
            shutil.move(str(base / "merged/directory"), str(base / "merged/renamed"))
            assert (base / "merged/plain").read_bytes() == payload
            assert (base / "merged/renamed/child").read_bytes() == payload
        finally:
            assert libc.umount(os.fsencode(base / "merged")) == 0
        complete = ((base / "upper/plain").read_bytes() == payload
                    and (base / "upper/renamed/child").is_file()
                    and (base / "upper/renamed/child").read_bytes() == payload)
        assert complete == (mode == "off"), (mode, complete)
        results[mode] = {"complete_upper": complete, "overlapping_mount_refused": True}
    print("PASS private kernel copy-up controls " + json.dumps(results), flush=True)


def check(path, image):
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "Use a disposable engine only"
    parameters = [Path("/sys/module/overlay/parameters") / key for key in
                  ("index", "metacopy", "redirect_dir", "redirect_always_follow")]
    before = {str(p): p.read_bytes() for p in parameters}
    with tempfile.TemporaryDirectory(prefix="overlay-control-", dir="/var/lib/asagiri") as directory:
        subprocess.run([sys.executable, __file__, "--kernel-control", directory], check=True, timeout=30)
    assert {str(p): p.read_bytes() for p in parameters} == before, "shared kernel defaults changed"
    docker = ["docker", "-H", "unix://" + path]
    suffix = uuid.uuid4().hex[:12]
    tag = "asagiri-overlay-" + suffix + ":test"
    bad_tag = "asagiri-overlay-" + suffix + ":rejected"
    cid = None

    def call(*args):
        return subprocess.check_output(docker + list(args), timeout=60)

    try:
        cid = call("run", "-d", "--network", "none", image, "sleep", "120").decode().strip()
        expected = call("exec", cid, "sh", "-ec", "cat /etc/alpine-release; find /etc/apk -type f | sort | xargs sha256sum")
        call("exec", cid, "sh", "-ec", "mv /etc/apk /etc/apk2; chmod 600 /etc/alpine-release")
        call("stop", "-t", "0", cid)
        call("commit", cid, tag)
        actual = call("run", "--rm", "--network", "none", tag, "sh", "-ec",
                      "cat /etc/alpine-release; find /etc/apk2 -type f | sort | xargs sha256sum")
        assert actual.replace(b"/etc/apk2/", b"/etc/apk/") == expected, (expected, actual)
        print("PASS committed renamed directory and chmod-only file keep every byte", flush=True)
        upper = Path("/var/lib/asagiri/containers") / cid / "upper"
        for relative, attribute, value, message in [
                ("etc/alpine-release", "trusted.overlay.metacopy", b"y", b"metadata-only file"),
                ("etc/apk2", "trusted.overlay.redirect", b"/etc/apk", b"directory redirect")]:
            target = upper / relative
            os.setxattr(target, attribute, value, follow_symlinks=False)
            try:
                for command in (["commit", cid, bad_tag], ["diff", cid]):
                    result = subprocess.run(docker + command, capture_output=True, timeout=60)
                    assert result.returncode != 0 and message in result.stderr, (command, result.stdout, result.stderr)
                print("PASS older " + message.decode() + " is refused before a misleading archive", flush=True)
            finally:
                os.removexattr(target, attribute, follow_symlinks=False)
    finally:
        if cid:
            call("rm", "-fv", cid)
        for name in (tag, bad_tag):
            result = subprocess.run(docker + ["image", "rm", "-f", name], capture_output=True, timeout=60)
            if result.returncode and b"No such image" not in result.stderr:
                raise RuntimeError(result.stderr.decode())


if __name__ == "__main__":
    if sys.argv[1] == "--kernel-control":
        kernel_control(Path(sys.argv[2]))
    else:
        check(sys.argv[1], sys.argv[2])
