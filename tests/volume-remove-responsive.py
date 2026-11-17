"""Volume reclamation must not block unrelated exec or DNS in the init loop.

Only for a fresh disposable privileged test engine. Delay unlinkat in the
agent and its workers, then verify control traffic while deletion is blocked.
"""
import argparse
import json
import os
from pathlib import Path
import platform
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

    name = "volume-responsive-" + uuid.uuid4().hex[:8]
    survivor = tracer = log = None
    try:
        call("network", "create", name)
        survivor = call("run", "-d", "--network", name, "--network-alias", "survivor",
                        args.image, "sleep", "180").decode().strip()
        call("volume", "create", name)
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
        # The test owns this volume. Populate it through the agent's mount
        # namespace without creating another container cleanup operation.
        root = Path("/proc", str(init[0]), "root/var/lib/asagiri/volumes")
        old = root / name / "_data"
        old.mkdir(parents=True, exist_ok=True)
        (old / "proof").write_bytes(b"old")
        log = open("/tmp/volume-delay-" + name + ".trace", "x+")
        tracer = subprocess.Popen(["strace", "-f", "-yy", "-e", "trace=unlinkat",
                                   "-e", "inject=unlinkat:delay_enter=15s:when=1", "-p", str(init[0])],
                                  stdout=log, stderr=log)
        deadline = time.monotonic() + 10
        while "TracerPid:\t" + str(tracer.pid) + "\n" not in Path("/proc", str(init[0]), "status").read_text():
            assert time.monotonic() < deadline, "strace did not attach"
            time.sleep(.02)
        call("volume", "rm", name)
        blocked, deadline = None, time.monotonic() + 5
        while blocked is None and time.monotonic() < deadline:
            for item in Path("/proc").iterdir():
                if not item.name.isdigit():
                    continue
                try:
                    if "TracerPid:\t" + str(tracer.pid) + "\n" not in (item / "status").read_text():
                        continue
                    syscall = (item / "syscall").read_text().split()
                    if syscall and syscall[0] == {"x86_64": "263", "aarch64": "35"}[platform.machine()]:
                        blocked = int(item.name)
                        break
                except OSError:
                    pass
            time.sleep(.01)
        assert blocked is not None, "No volume unlink fault observed"
        report = {"initPid": init[0], "blockedPid": blocked, "injectedDelaySeconds": 15}
        began = time.monotonic()
        try:
            call("exec", survivor, "true", timeout=3)
            report["unrelatedExec"] = "completed"
        except subprocess.TimeoutExpired:
            report["unrelatedExec"] = "timeout"
        report["unrelatedExecSeconds"] = time.monotonic() - began
        if args.expect_blocked:
            assert blocked == init[0] and report["unrelatedExec"] == "timeout", report
        else:
            assert blocked != init[0] and report["unrelatedExec"] == "completed", report
            began = time.monotonic()
            call("exec", survivor, "nslookup", "survivor", "127.0.0.11", timeout=3)
            report["unrelatedDnsSeconds"] = time.monotonic() - began
            assert report["unrelatedDnsSeconds"] < 3, report
        # Let the injected I/O finish before stopping the tracer or touching
        # the namespace. The old files must actually be reclaimed.
        deadline = time.monotonic() + 30
        while any(root.glob(".removing-*")) or (root / name).exists():
            assert time.monotonic() < deadline, "Volume reclamation did not complete"
            time.sleep(.05)
        tracer.send_signal(signal.SIGINT)
        tracer.wait(timeout=10)
        tracer = None
        log.seek(0)
        trace = log.read()
        assert '"proof"' in trace and "/volumes/" in trace, trace
        report["removedTreeAbsent"] = True
        call("exec", survivor, "true", timeout=3)
        print(json.dumps(report, indent=2), flush=True)
    finally:
        if tracer is not None:
            tracer.send_signal(signal.SIGINT)
            tracer.wait(timeout=20)
        if log is not None:
            log.close()
        if survivor is not None:
            subprocess.run(docker + ["rm", "-f", survivor], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        subprocess.run(docker + ["volume", "rm", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        subprocess.run(docker + ["network", "rm", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)


if __name__ == "__main__":
    main()
