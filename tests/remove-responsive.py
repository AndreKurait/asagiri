"""Explicit rm must not stall unrelated control requests while retiring its root.

Run only in the disposable privileged test engine with strace installed.
Delays the removed container's umount2 by 15 seconds, not stop's cleanup.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import select
import signal
import subprocess
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--expect-blocked", action="store_true")
    args = parser.parse_args()
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
    assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    assert Path("/.dockerenv").is_file()
    assert os.readlink("/proc/1/exe") == "/opt/asagiri/asagiri"
    docker = ["docker", "-H", "unix://" + args.socket]

    def call(*command, timeout=30):
        return subprocess.check_output(docker + list(command), timeout=timeout, stderr=subprocess.STDOUT)

    network = "remove-" + uuid.uuid4().hex[:8]
    ids, tracer, removal, log, worker_fd = [], None, None, None, None
    try:
        call("network", "create", network)
        for name in ("survivor", "victim"):
            ids.append(call("run", "-d", "--network", network, "--network-alias", name,
                            args.image, "sleep", "180").decode().strip())
        survivor, victim = ids
        call("exec", victim, "sh", "-c", "printf preserved > /remove-proof")
        call("stop", "-t", "0", victim)
        # Complete the stopped copy through EOF before injecting the fault.
        # The delayed syscall below must belong to explicit removal.
        proof = "/tmp/remove-proof-" + victim
        call("cp", victim + ":/remove-proof", proof)
        assert Path(proof).read_bytes() == b"preserved"
        Path(proof).unlink()
        # A diff also waits any old cleanup; neither successful operation
        # requires a retained idle overlay to be unmounted.
        call("diff", victim)
        init = []
        for item in Path("/proc").iterdir():
            if not item.name.isdigit():
                continue
            try:
                fields = (item / "stat").read_text().rsplit(")", 1)[1].split()
                if (item / "comm").read_text().strip() == "asagiri-init" and fields[0] != "Z" and fields[1] == "1":
                    init.append(int(item.name))
            except (OSError, ValueError):
                pass
        assert len(init) == 1, init
        root = Path("/var/lib/asagiri/containers") / victim / "rootfs"
        assert root.is_dir() and root.resolve() == root, root
        through_agent = Path("/proc", str(init[0]), "root") / str(root).lstrip("/")
        # The container directory is the same owned directory in both views.
        # Its rootfs may already be an overlay in the agent's mount namespace.
        assert (root.parent.stat().st_dev, root.parent.stat().st_ino) == (through_agent.parent.stat().st_dev, through_agent.parent.stat().st_ino)
        rootfs = str(root)
        def mounts():
            return [line for line in Path("/proc", str(init[0]), "mountinfo").read_text().splitlines()
                    if line.split()[4] == rootfs]
        existing = mounts()
        if existing:
            assert len(existing) == 1 and existing[0].split(" - ", 1)[1].startswith("overlay "), existing
            removal_mount = "retained overlay"
        else:
            # Preserve the old-engine negative control: give an unmounted
            # victim a real mount, so removal still has mandatory work.
            subprocess.check_call(["nsenter", "-t", str(init[0]), "-m", "--",
                                   "mount", "--bind", rootfs, rootfs], timeout=10)
            removal_mount = "controlled bind mount"
        assert len(mounts()) == 1, mounts()
        mount_id = mounts()[0].split()[0]
        log = open("/tmp/remove-delay-" + victim + ".trace", "xb")
        tracer = subprocess.Popen(["strace", "-f", "-P", rootfs, "-e", "trace=umount2",
                                   "-e", "inject=umount2:delay_enter=15s", "-p", str(init[0])],
                                  stdout=log, stderr=log)
        deadline = time.monotonic() + 10
        while "TracerPid:\t" + str(tracer.pid) + "\n" not in Path("/proc", str(init[0]), "status").read_text():
            assert time.monotonic() < deadline, "strace did not attach"
            time.sleep(.02)
        call("exec", survivor, "true", timeout=3)
        began = time.monotonic()
        removal = subprocess.Popen(docker + ["rm", victim], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        blocked, deadline = None, time.monotonic() + 5
        while blocked is None and time.monotonic() < deadline:
            for item in Path("/proc").iterdir():
                if not item.name.isdigit():
                    continue
                try:
                    if "TracerPid:\t" + str(tracer.pid) + "\n" not in (item / "status").read_text():
                        continue
                    syscall = (item / "syscall").read_text().split()
                    if syscall and syscall[0] == {"x86_64": "166", "aarch64": "39"}[platform.machine()]:
                        blocked = int(item.name)
                        break
                except OSError:
                    pass
            time.sleep(.01)
        assert blocked is not None, "No explicit-removal umount2 fault observed"
        worker_fd = os.pidfd_open(blocked)
        report = {"initPid": init[0], "blockedPid": blocked, "rootfs": rootfs,
                  "injectedDelaySeconds": 15, "earlierRequiredCleanupFinished": True,
                  "copyBytesAndEofBeforeInjection": True, "removalMount": removal_mount,
                  "retiringMountId": mount_id}
        started = time.monotonic()
        try:
            call("exec", survivor, "true", timeout=3)
            report["unrelatedExec"] = "completed"
        except subprocess.TimeoutExpired:
            report["unrelatedExec"] = "timeout"
        report["unrelatedExecSeconds"] = time.monotonic() - started
        if args.expect_blocked:
            assert blocked == init[0] and report["unrelatedExec"] == "timeout", report
        else:
            assert blocked != init[0] and report["unrelatedExec"] == "completed", report
            assert report["unrelatedExecSeconds"] < 3, report
            assert os.readlink(f"/proc/{blocked}/ns/mnt") == os.readlink(f"/proc/{init[0]}/ns/mnt")
            started = time.monotonic()
            call("exec", survivor, "nslookup", "survivor", "127.0.0.11", timeout=3)
            report["unrelatedDnsSeconds"] = time.monotonic() - started
            assert report["unrelatedDnsSeconds"] < 3, report
            assert root.exists(), "Removal fault barrier ended before responsiveness checks"
        out, err = removal.communicate(timeout=30)
        assert removal.returncode == 0, (out, err)
        deadline = time.monotonic() + 30
        while root.parent.exists():
            assert time.monotonic() < deadline, "Explicit removal never completed"
            time.sleep(.05)
        if not args.expect_blocked:
            poll = select.poll()
            poll.register(worker_fd, select.POLLIN)
            assert poll.poll(3000), "Explicit removal's cleanup worker did not exit"
        assert not mounts(), "Removed container still has a mounted root"
        ids.remove(victim)
        report["removedTreeAbsent"] = True
        report["cleanupWorkerExited"] = not args.expect_blocked
        report["removedMountAbsent"] = True
        report["totalSeconds"] = time.monotonic() - began
        tracer.send_signal(signal.SIGINT)
        tracer.wait(timeout=10)
        tracer = None
        call("exec", survivor, "true", timeout=3)
        print(json.dumps(report, indent=2), flush=True)
    finally:
        if tracer is not None:
            tracer.send_signal(signal.SIGINT)
            tracer.wait(timeout=20)
        if log is not None:
            log.close()
        if worker_fd is not None:
            os.close(worker_fd)
        if removal is not None and removal.poll() is None:
            removal.communicate(timeout=30)
        for cid in ids:
            subprocess.run(docker + ["rm", "-f", cid], stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=30)
        subprocess.run(docker + ["network", "rm", network], stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=30)


if __name__ == "__main__":
    main()
