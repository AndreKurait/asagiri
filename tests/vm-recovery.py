#!/usr/bin/env python3
"""Crash only the agent inside live-tests.sh's disposable Linux engine.

After each crash, read persisted state without any API requests. An API call
could boot the replacement and hide a broken automatic recovery worker.
"""
import http.client
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid


def process(pid):
    root = Path("/proc") / str(pid)
    fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
    return os.readlink(root / "exe"), int(fields[1]), int(fields[19]), (root / "comm").read_text().strip()


def check(path, image):
    assert sys.platform == "linux" and os.geteuid() == 0
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1" and Path("/.dockerenv").is_file()
    assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    assert path == "/run/asagiri/docker.sock"
    daemon = process(1)
    assert daemon[0] == "/opt/asagiri/asagiri", daemon
    assert Path("/proc/1/cmdline").read_bytes().split(b"\0")[:2] == [b"/opt/asagiri/asagiri", b"daemon"]
    assert hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"), "Linux pidfds are required"
    records = Path("/test-store/containers.json")
    containers = []
    eligible = []
    excluded = {}
    removal_proofs = {}
    prefix = "asagiri-recovery-" + uuid.uuid4().hex[:10]

    def cli(*args, ok=True):
        result = subprocess.run(["docker", "--host", "unix://" + path, *args],
                                capture_output=True, text=True, timeout=60)
        if ok:
            assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result

    def agents():
        found = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                identity = process(int(entry.name))
            except (FileNotFoundError, ProcessLookupError):
                continue
            # Cleanup workers share the executable and briefly become PID 1's
            # children while their parent-death SIGKILL completes. Their name
            # is asagiri-clean; they are not a replacement engine. The separate
            # cleanup-responsive crash test requires those workers to exit.
            if identity[0] == "/opt/asagiri/asagiri-init" and identity[1] == 1 and identity[3] == "asagiri-init":
                found.append((int(entry.name), identity))
        return found

    def inspect_status(cid):
        connection = http.client.HTTPConnection("localhost", timeout=10)
        connection.sock = socket.socket(socket.AF_UNIX)
        connection.sock.settimeout(10)
        connection.sock.connect(path)
        try:
            connection.request("GET", "/containers/" + cid + "/json", headers={"Connection": "close"})
            response = connection.getresponse()
            response.read()
            return response.status
        finally:
            connection.close()

    def crash_agent():
        assert process(1) == daemon, "the daemon must not restart"
        found = agents()
        assert len(found) == 1, found
        pid, identity = found[0]
        descriptor = os.pidfd_open(pid)
        try:
            assert process(pid) == identity, "the agent changed before its pidfd was opened"
            signal.pidfd_send_signal(descriptor, signal.SIGKILL)
            stopped = select.poll()
            stopped.register(descriptor, select.POLLIN)
            assert stopped.poll(10000), "the killed agent did not exit"
        finally:
            os.close(descriptor)
        return pid

    def state():
        return {item["id"]: item for item in json.loads(records.read_text())}

    try:
        cases = [
            ("always", "always", True), ("unless", "unless-stopped", True),
            ("no", "no", True), ("failure", "on-failure", True),
            ("stopped-always", "always", True), ("stopped-unless", "unless-stopped", True),
            ("created", "always", False),
        ]
        for name, policy, start in cases:
            cid = cli("create", "--name", prefix + "-" + name, "--restart", policy,
                      "--network", "none", image, "sleep", "3600").stdout.strip()
            containers.append(cid)
            if start:
                cli("start", cid)
            if name.startswith("stopped-"):
                cli("stop", "-t", "0", cid)
            if name in ("always", "unless"):
                eligible.append(cid)
                cli("exec", cid, "sh", "-c", "echo persisted > /recovery-proof")
            else:
                excluded[cid] = "exited" if start else "created"
            if name in ("no", "created"):
                content = (prefix + "-" + name).encode()
                with tempfile.NamedTemporaryFile(prefix=prefix + "-", dir="/tmp") as proof:
                    proof.write(content)
                    proof.flush()
                    cli("cp", proof.name, cid + ":/removal-proof")
                upper = Path("/var/lib/asagiri/containers") / cid / "upper"
                assert (upper / "removal-proof").read_bytes() == content
                removal_proofs[cid] = (upper, content)

        for count in (1, 2):
            old_pid = crash_agent()
            deadline = time.monotonic() + 45
            while True:
                saved = state()
                if all(saved[cid]["status"] == "running" and saved[cid]["restart_count"] == count
                       and not saved[cid]["vm_stopped"] for cid in eligible):
                    break
                assert time.monotonic() < deadline, ("automatic recovery did not finish", saved)
                time.sleep(0.05)
            for cid, status in excluded.items():
                assert saved[cid]["status"] == status and saved[cid]["restart_count"] == 0, saved[cid]
            replacement = agents()
            assert len(replacement) == 1 and replacement[0][0] != old_pid, replacement
            assert process(1) == daemon
            # Only after the persisted proof may an API request be made.
            for cid in eligible:
                assert cli("exec", cid, "cat", "/recovery-proof").stdout.strip() == "persisted"
            print(f"PASS agent crash {count}: automatic recovery, RestartCount={count}, writable layers and excluded policies", flush=True)

        current_agent = agents()
        for cid, (upper, content) in removal_proofs.items():
            assert (upper / "removal-proof").read_bytes() == content
            assert saved[cid]["status"] == excluded[cid]
            if excluded[cid] == "exited":
                assert saved[cid]["vm_stopped"] is True
            cli("rm", cid)
            assert inspect_status(cid) == 404
            deadline = time.monotonic() + 10
            while upper.parent.exists():
                assert time.monotonic() < deadline, ("removed container left its writable layer", cid)
                time.sleep(0.05)
            assert agents() == current_agent and process(1) == daemon, "removal restarted the engine"
            for survivor in eligible:
                assert cli("exec", survivor, "cat", "/recovery-proof").stdout.strip() == "persisted"
            containers.remove(cid)
            excluded.pop(cid)
        print("PASS created and crash-stopped rm: writable layers reclaimed by the awake agent; unrelated containers preserved", flush=True)

        for cid in eligible:
            cli("stop", "-t", "0", cid)
        crash_agent()
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            saved = state()
            assert all(saved[cid]["status"] == "exited" and saved[cid]["restart_count"] == 2 for cid in eligible)
            # Allow the killed child to be reaped; it must never be replaced.
            found = agents()
            assert not found, ("an engine without eligible containers must remain asleep", found)
            assert process(1) == daemon
            time.sleep(0.05)
        print("PASS no eligible restart policies: the agent stays down and the daemon stays alive", flush=True)
        print("3 actual agent crashes passed")
    finally:
        failures = []
        for cid in reversed(containers):
            result = cli("rm", "-f", cid, ok=False)
            if result.returncode or inspect_status(cid) != 404:
                failures.append((cid, result.stderr))
        if failures and sys.exc_info()[0] is None:
            raise AssertionError(("could not clean up recovery containers", failures))


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
