#!/usr/bin/env python3
"""Restart an actual Linux daemon in a persistent, private host mount namespace."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import time
import uuid


def inner():
    assert platform.system() == "Linux" and os.getpid() == 1
    assert Path("/.dockerenv").is_file() and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
    assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    binary = "/opt/asagiri/asagiri"
    docker = ["docker", "-H", "unix:///run/asagiri/docker.sock"]
    log_path = Path("/tmp/asagiri-service-recovery.log")
    daemon = None
    ids = []

    def call(*args, timeout=30):
        return subprocess.check_output(docker + list(args), stderr=subprocess.STDOUT, timeout=timeout).decode().strip()

    def ready():
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            assert daemon.poll() is None, log_path.read_text()[-12000:]
            if subprocess.run(docker + ["version"], capture_output=True, timeout=5).returncode == 0:
                return
            time.sleep(0.1)
        raise AssertionError("daemon did not become ready")

    def launch():
        nonlocal daemon
        with log_path.open("ab") as log:
            daemon = subprocess.Popen([binary, "daemon"], stdout=log, stderr=log)
        ready()

    def reap():
        while True:
            try:
                pid, _ = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                break
            if pid == 0:
                break

    def mounts():
        return {line.split()[0]: line.split()[4] for line in Path("/proc/self/mountinfo").read_text().splitlines()
                if line.split()[4].startswith("/var/lib/asagiri/containers/")}

    def state(cid):
        return json.loads(call("inspect", cid))[0]

    def restored():
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if state(ids[1])["State"]["Running"]:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("restart-always container did not return")
        if not state(ids[0])["State"]["Running"]:
            call("start", ids[0])
        for cid in ids:
            assert call("exec", cid, "cat", "/service-proof") == "preserved"
        current = mounts()
        expected = {f"/var/lib/asagiri/containers/{cid}/rootfs" for cid in ids}
        assert expected == set(current.values()) and len(current) == len(expected), ("old mounts were stacked instead of reclaimed", current)

    try:
        launch()
        image = json.loads(Path("/src/scripts/ci/images.json").read_text())["perf"]["image"]
        call("pull", "-q", image, timeout=120)
        for policy in ("no", "always"):
            cid = call("run", "-d", "--network", "none", "--restart", policy, image, "sleep", "3600")
            ids.append(cid)
            call("exec", cid, "sh", "-c", "printf preserved > /service-proof")
        identity = call("info", "--format", "{{.ID}}")
        for cycle in range(2):
            previous_pid = daemon.pid
            daemon.terminate()
            daemon.wait(timeout=30)
            reap()
            assert not mounts(), ("mounts survived orderly shutdown", mounts())
            launch()
            assert daemon.pid != previous_pid and call("info", "--format", "{{.ID}}") == identity
            restored()
            print(json.dumps({"probe": "linux-service-recovery", "orderlyCycle": cycle + 1,
                              "persistentMountNamespace": os.readlink("/proc/self/ns/mnt"),
                              "restartAlwaysAndWritableData": "passed"}), flush=True)

        # Reproduce systemd removing a dead service's cgroups before restart.
        # SIGSTOP prevents this old daemon from observing agent loss and
        # automatically repairing the fixture before it can be inspected.
        daemon.send_signal(signal.SIGSTOP)
        agents = []
        for item in Path("/proc").iterdir():
            if not item.name.isdigit():
                continue
            try:
                fields = (item / "stat").read_text().rsplit(")", 1)[1].split()
                if int(fields[1]) == daemon.pid and os.readlink(item / "exe") == "/opt/asagiri/asagiri-init":
                    agents.append(int(item.name))
            except (OSError, ValueError):
                pass
        assert len(agents) == 1, agents
        agent_fd = os.pidfd_open(agents[0])
        try:
            signal.pidfd_send_signal(agent_fd, signal.SIGKILL)
        finally:
            os.close(agent_fd)
        daemon.kill()
        daemon.wait(timeout=10)
        cgroup = Path("/var/lib/asagiri/agent.cgroup").read_text().strip()
        assert cgroup.startswith("/sys/fs/cgroup/"), cgroup
        for cid in ids:
            owned = Path(cgroup) / cid
            if not owned.exists():
                continue
            kill = owned / "cgroup.kill"
            if kill.exists():
                kill.write_text("1")
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                reap()
                if not (owned / "cgroup.procs").read_text().strip():
                    break
                time.sleep(0.05)
            assert not (owned / "cgroup.procs").read_text().strip(), owned
            for child in sorted((p for p in owned.rglob("*") if p.is_dir()), key=lambda p: len(p.parts), reverse=True):
                child.rmdir()
            owned.rmdir()
            assert not owned.exists()
        reap()
        orphaned = {f"/var/lib/asagiri/containers/{cid}/rootfs" for cid in ids}
        assert orphaned <= set(mounts().values()), ("fixture did not leave orphaned overlays", mounts())
        # A permissive overlay kernel can also leave a stack after an earlier
        # recovery. An owned bind mount represents that additional mount layer.
        for rootfs in sorted(orphaned):
            subprocess.run(["mount", "--bind", rootfs, rootfs], check=True, timeout=10)
        old_mount_ids = set(mounts())
        assert len(old_mount_ids) == 2 * len(orphaned), mounts()
        # Deliberately do not unmount the fixture: the new agent must recover it.
        launch()
        assert call("info", "--format", "{{.ID}}") == identity
        restored()
        assert not old_mount_ids.intersection(mounts()), "a previous mount survived recovery"
        print(json.dumps({"probe": "linux-service-recovery", "orphanedOverlays": len(orphaned),
                          "cgroupsRemovedBeforeRestart": True,
                          "restartAlwaysAndWritableData": "passed"}), flush=True)
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.send_signal(signal.SIGCONT)
            for cid in ids:
                subprocess.run(docker + ["rm", "-f", cid], capture_output=True, timeout=30)
            daemon.terminate()
            try:
                daemon.wait(timeout=30)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=10)
        reap()
        if log_path.exists():
            print(log_path.read_text()[-12000:], flush=True)


def outer(binaries):
    source = Path(__file__).resolve().parents[1]
    binaries = Path(binaries).resolve()
    for line in (binaries / "SHA256SUMS").read_text().splitlines():
        expected, filename = line.split()
        assert filename in ("asagiri", "asagiri-init")
        assert hashlib.sha256((binaries / filename).read_bytes()).hexdigest() == expected
    image = json.loads((source / "scripts/ci/images.json").read_text())["live"]["image"]
    name = "asagiri-service-test-" + uuid.uuid4().hex[:12]
    label = "org.asagiri.service-test=" + name
    try:
        subprocess.run([
            "docker", "run", "--name", name, "--label", label, "--privileged",
            "-e", "ASAGIRI_TEST_ENGINE=1", "-e", "ASAGIRI_DATA_DIR=/test-store",
            "-e", "ASAGIRI_SOCKET=/run/asagiri/docker.sock",
            "-v", str(source) + ":/src:ro", "-v", str(binaries) + ":/opt/asagiri:ro",
            "-v", "/var/lib/asagiri", "-v", "/test-store",
            image, "sh", "-ec", "apk add -q --no-cache python3 && exec python3 /src/tests/linux-service-recovery.py --inner",
        ], check=True, timeout=600)
    finally:
        result = subprocess.run(["docker", "inspect", name], capture_output=True, text=True)
        if result.returncode == 0:
            info = json.loads(result.stdout)[0]
            assert info["Config"]["Labels"]["org.asagiri.service-test"] == name
            assert not info["HostConfig"]["PortBindings"]
            subprocess.run(["docker", "rm", "-fv", name], check=True, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binaries", nargs="?")
    parser.add_argument("--inner", action="store_true")
    args = parser.parse_args()
    if args.inner:
        inner()
    else:
        assert args.binaries
        outer(args.binaries)
