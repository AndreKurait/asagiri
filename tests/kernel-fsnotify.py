#!/usr/bin/env python3
"""Boot the actual guest kernels and test host notification injection.

The FUSE filesystem and its fixed contents exist only in guest RAM. No host
filesystem, block device, network interface, or Docker socket enters QEMU.
Compile on Linux, or reuse verified initramfs artifacts with --initramfs.
"""
import argparse
from datetime import datetime, timezone
import importlib.util
import json
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "tests/kernel-loop.py"
SPEC = importlib.util.spec_from_file_location("kernel_loop", HELPER)
LOOP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LOOP)
digest, cpio_file = LOOP.digest, LOOP.cpio_file
SCHEMA = "asagiri.guest-fsnotify.v1"
PASS = "ASAGIRI_FSNOTIFY_TEST_PASS: events scopes namespace contents"
CONTROLS = [
    "C directory=", "M directory=", "A directory=", "D directory=",
    "unprivileged open denied",
    "container namespace denied with inherited descriptor",
    "rejected outside-share path", "rejected parent traversal",
    "rejected symlink traversal", "rejected nested mount",
    "replacement looks up new inode and preserves open file",
    "replacement expires cache without watchers",
    "deletion expires cache without watchers",
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernels", type=Path, default=ROOT / "guest/out")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--initramfs", type=Path,
                        help="reuse generated initramfs files and their verification.json")
    parser.add_argument("--expect-missing-interface", action="store_true",
                        help="negative control against kernels without host notification injection")
    args = parser.parse_args()
    if args.initramfs is None and sys.platform != "linux":
        parser.error("compile on Linux, or supply the verified --initramfs artifact")
    args.output.mkdir(parents=True, exist_ok=False)
    record = {
        "schema": SCHEMA, "status": "running",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "expected_missing_interface": args.expect_missing_interface, "checks": [],
        "init_source_sha256": digest(ROOT / "tests/kernel-fsnotify-init.c"),
        "harness_sha256": digest(Path(__file__)), "helper_sha256": digest(HELPER),
        "host_filesystems_shared": False, "host_devices_shared": False, "network_enabled": False,
    }
    try:
        prepared = {}
        if args.initramfs is not None:
            report = args.initramfs / "verification.json"
            previous = json.loads(report.read_text())
            if (previous.get("schema") != SCHEMA or previous.get("status") != "passed" or
                    previous.get("expected_missing_interface") is not False):
                raise ValueError("initramfs artifacts must have passed the positive guest test")
            for key in ("init_source_sha256", "harness_sha256", "helper_sha256"):
                if previous.get(key) != record[key]:
                    raise ValueError(f"prepared initramfs uses different test source: {key}")
            prepared = {check["architecture"]: check for check in previous["checks"]}
            if len(previous["checks"]) != 2 or set(prepared) != {"arm64", "x86_64"}:
                raise ValueError("prepared initramfs must cover both guest architectures")
            record["prepared_report_sha256"] = digest(report)
        with tempfile.TemporaryDirectory(prefix="asagiri-guest-fsnotify-") as temporary:
            work = Path(temporary)
            for arch, cc, qemu, kernel, machine in [
                ("arm64", "aarch64-linux-gnu-gcc", "qemu-system-aarch64", "Image",
                 ["-machine", "virt", "-cpu", "cortex-a57"]),
                ("x86_64", "gcc", "qemu-system-x86_64", "x86_64/bzImage", []),
            ]:
                image = (args.kernels / kernel).resolve(strict=True)
                archive = args.output / (arch + ".cpio")
                if prepared:
                    original = args.initramfs / (arch + ".cpio")
                    if digest(original) != prepared[arch]["initramfs_sha256"]:
                        raise ValueError(f"{arch} initramfs digest mismatch")
                    shutil.copyfile(original, archive)
                    init_digest = prepared[arch]["init_sha256"]
                else:
                    init = work / ("init-" + arch)
                    subprocess.run([cc, "-static", "-Os", "-Wall", "-Wextra", "-Werror",
                                    str(ROOT / "tests/kernel-fsnotify-init.c"), "-o", str(init)],
                                   check=True, timeout=60)
                    init_digest = digest(init)
                    with archive.open("xb") as output:
                        cpio_file(output, 1, "dev", stat.S_IFDIR | 0o755)
                        cpio_file(output, 2, "proc", stat.S_IFDIR | 0o755)
                        cpio_file(output, 3, "dev/console", stat.S_IFCHR | 0o600, rdev=(5, 1))
                        cpio_file(output, 4, "init", stat.S_IFREG | 0o755, init.read_bytes())
                        cpio_file(output, 5, "TRAILER!!!", 0)
                # Use the same virtio console as the production kernels.
                command = [qemu, *machine, "-accel", "tcg", "-m", "256M",
                           "-display", "none", "-monitor", "none", "-serial", "none",
                           "-device", "virtio-serial-pci",
                           "-chardev", "stdio,id=console,signal=off",
                           "-device", "virtconsole,chardev=console",
                           "-no-reboot", "-nic", "none", "-kernel", str(image),
                           "-initrd", str(archive.resolve()), "-append",
                           "console=hvc0 rdinit=/init panic=-1 asagiri.fsnotify-selftest=1"]
                log = args.output / (arch + ".log")
                with log.open("wb") as output:
                    result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT,
                                            timeout=90)
                text = log.read_text(errors="replace")
                observed = {control: "ASAGIRI_FSNOTIFY_CONTROL_PASS: " + control in text
                            for control in CONTROLS}
                if args.expect_missing_interface:
                    passed = ("ASAGIRI_FSNOTIFY_TEST_FAIL: open guest notification interface: "
                              "No such file or directory" in text and
                              "ASAGIRI_FSNOTIFY_TEST_PASS:" not in text and not any(observed.values()))
                else:
                    passed = PASS in text and "ASAGIRI_FSNOTIFY_TEST_FAIL:" not in text and all(observed.values())
                record["checks"].append({
                    "architecture": arch, "kernel_sha256": digest(image),
                    "init_sha256": init_digest, "initramfs_sha256": digest(archive),
                    "qemu": shutil.which(qemu), "command": command,
                    "exit": result.returncode, "log_sha256": digest(log),
                    "controls": observed, "passed": passed and result.returncode == 0,
                })
            failed = [row["architecture"] for row in record["checks"] if not row["passed"]]
            if failed:
                raise RuntimeError(f"{', '.join(failed)} guest notification tests failed; see {args.output}")
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
