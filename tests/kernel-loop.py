#!/usr/bin/env python3
"""Boot both actual guest kernels and exercise loop-backed ext4 persistence.

Requires Linux, gcc, gcc-aarch64-linux-gnu, qemu-system-{x86,arm}, and e2fsprogs.
Only generated initramfs files enter the diskless guests. No host filesystem,
block device, network interface, or Docker socket is passed to QEMU.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for data in iter(lambda: source.read(1024 * 1024), b""):
            h.update(data)
    return h.hexdigest()


def cpio_file(output, inode, name, mode, data=b"", rdev=(0, 0)):
    name = name.encode() + b"\0"
    fields = [inode, mode, 0, 0, 1, 0, len(data), 0, 0, *rdev, len(name), 0]
    header = b"070701" + "".join(f"{value:08x}" for value in fields).encode()
    output.write(header + name + b"\0" * (-(len(header) + len(name)) % 4))
    output.write(data + b"\0" * (-len(data) % 4))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernels", type=Path, default=ROOT / "guest/out")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expect-missing-loop", action="store_true",
                        help="negative control against kernels without CONFIG_BLK_DEV_LOOP")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("run on Linux; QEMU itself needs no root privileges")
    args.output.mkdir(parents=True, exist_ok=False)
    record = {"schema": "asagiri.guest-loop.v1", "status": "running",
              "started_at": datetime.now(timezone.utc).isoformat(),
              "source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "expected_missing_loop": args.expect_missing_loop, "checks": [],
              "init_source_sha256": digest(ROOT / "tests/kernel-loop-init.c"),
              "harness_sha256": digest(Path(__file__)),
              "host_filesystems_shared": False, "host_devices_shared": False, "network_enabled": False}
    try:
        with tempfile.TemporaryDirectory(prefix="asagiri-guest-loop-") as temporary:
            work = Path(temporary)
            disk = work / "filesystem.img"
            with disk.open("xb") as target:
                target.truncate(16 * 1024 * 1024)
            subprocess.run(["mkfs.ext4", "-q", "-F", "-E", "lazy_itable_init=0,lazy_journal_init=0",
                            str(disk)], check=True, timeout=30)
            for arch, cc, qemu, kernel, machine in [
                ("arm64", "aarch64-linux-gnu-gcc", "qemu-system-aarch64", "Image",
                 ["-machine", "virt", "-cpu", "cortex-a57"]),
                ("x86_64", "gcc", "qemu-system-x86_64", "x86_64/bzImage", []),
            ]:
                image = (args.kernels / kernel).resolve(strict=True)
                init = work / ("init-" + arch)
                subprocess.run([cc, "-static", "-Os", "-Wall", "-Wextra", "-Werror",
                                str(ROOT / "tests/kernel-loop-init.c"), "-o", str(init)],
                               check=True, timeout=60)
                archive = work / (arch + ".cpio")
                with archive.open("xb") as output:
                    cpio_file(output, 1, "dev", stat.S_IFDIR | 0o755)
                    cpio_file(output, 2, "proc", stat.S_IFDIR | 0o755)
                    cpio_file(output, 3, "dev/console", stat.S_IFCHR | 0o600, rdev=(5, 1))
                    cpio_file(output, 4, "init", stat.S_IFREG | 0o755, init.read_bytes())
                    cpio_file(output, 5, "filesystem.img", stat.S_IFREG | 0o600, disk.read_bytes())
                    cpio_file(output, 6, "TRAILER!!!", 0)
                # These minimal kernels use virtio-console, as the installed
                # VM does. Neither PL011 (ttyAMA0) nor 8250 (ttyS0) is built in.
                command = [qemu, *machine, "-accel", "tcg", "-m", "256M",
                           "-display", "none", "-monitor", "none", "-serial", "none",
                           "-device", "virtio-serial-pci",
                           "-chardev", "stdio,id=console,signal=off",
                           "-device", "virtconsole,chardev=console",
                           "-no-reboot", "-nic", "none", "-kernel", str(image),
                           "-initrd", str(archive), "-append",
                           "console=hvc0 rdinit=/init panic=-1 asagiri.loop-selftest=1"]
                log = args.output / (arch + ".log")
                with log.open("wb") as output:
                    result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT,
                                            timeout=90)
                text = log.read_text(errors="replace")
                passed = result.returncode == 0 and (
                    "ASAGIRI_LOOP_TEST_FAIL: open loop-control: No such file or directory" in text
                    and "ASAGIRI_LOOP_TEST_PASS:" not in text if args.expect_missing_loop else
                    "ASAGIRI_LOOP_TEST_PASS: attach ext4 write detach reattach read" in text
                    and "ASAGIRI_LOOP_TEST_FAIL:" not in text)
                record["checks"].append({"architecture": arch, "kernel_sha256": digest(image),
                                         "init_sha256": digest(init), "initramfs_sha256": digest(archive),
                                         "qemu": shutil.which(qemu), "command": command,
                                         "exit": result.returncode, "log_sha256": digest(log),
                                         "passed": passed})
            failed = [row["architecture"] for row in record["checks"] if not row["passed"]]
            if failed:
                raise RuntimeError(f"{', '.join(failed)} loop-device boot test failed; see {args.output}")
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = repr(error)
        raise
    finally:
        (args.output / "verification.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps({"status": record["status"], "architectures": len(record["checks"])}))


if __name__ == "__main__":
    main()
