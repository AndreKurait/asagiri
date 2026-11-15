#!/usr/bin/env python3
"""Reject application startup when the cgroup device filter cannot be installed.

The block node uses reserved major zero and is opened without reading or
writing it. No real disk/device data is accessed. --expect-fallback records the
old behavior; --reference checks the normal device policy against Docker.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import signal
import stat
import subprocess
import tarfile
import time
import uuid


def check(path, base, output, expect_fallback=False, reference=False):
    assert platform.system() == "Linux" and Path("/.dockerenv").is_file()
    assert not (expect_fallback and reference)
    if not reference:
        assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
        assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
        assert os.readlink("/proc/1/exe") == "/opt/asagiri/asagiri"
    # A reserved, unassigned block major, never one of the host's disks.
    block_devices = Path("/proc/devices").read_text().split("Block devices:\n", 1)[1]
    assert all(line.split()[0] != "0" for line in block_devices.splitlines() if line.split())
    output.mkdir(parents=True, exist_ok=False)
    prefix = "asagiri-device-filter-" + uuid.uuid4().hex[:12]
    docker = ["docker", "-H", "unix://" + path]
    containers, image, tracer = [], None, None
    report = {"probe": "device-filter-failure", "reference": reference,
              "expected_old_fallback": expect_fallback, "cases": []}

    def cli(*args, data=None, ok=True):
        result = subprocess.run([*docker, *args], input=data, capture_output=True, timeout=40)
        if ok:
            assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result

    def inspect(cid):
        return json.loads(cli("inspect", cid).stdout)[0]

    program = """
export LC_ALL=C
printf 'device-filter-probe-started\\n'
stat -c 'node %f %t %T' /device-filter-node
awk '$1 == "CapEff:" { print "caps " $2 }' /proc/self/status
cat /device-filter-marker
if error=$( (exec 3</device-filter-node) 2>&1); then
    printf 'unexpected-device-open\\n'
else
    printf 'device-open-error %s\\n' "$error"
fi
"""

    def checked_output(result, fallback=False, retained=False):
        assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
        text = result.stdout.decode()
        assert "device-filter-probe-started\n" in text
        node = next(line.split() for line in text.splitlines() if line.startswith("node "))
        assert stat.S_ISBLK(int(node[1], 16)) and node[2:] == ["0", "2a"], node
        caps = int(next(line.split()[1] for line in text.splitlines() if line.startswith("caps ")), 16)
        assert bool(caps & (1 << 27)) is not fallback, hex(caps)
        assert ("retained-after-filter-failure\n" if retained else "initial-marker\n") in text
        expected = "No such device or address" if fallback else "Operation not permitted"
        assert expected in text and "unexpected-device-open" not in text, text
        return {"cap_eff": hex(caps), "open_error": expected, "application_started": True}

    def detach():
        nonlocal tracer
        if tracer is not None:
            if tracer.poll() is None:
                tracer.send_signal(signal.SIGINT)
            tracer.wait(timeout=15)
            tracer = None

    try:
        identity = cli("info", "--format", "{{.OperatingSystem}}").stdout.decode()
        assert ("Asagiri" not in identity) if reference else ("Asagiri" in identity)
        exported = cli("create", "--network", "none", base, "true").stdout.decode().strip()
        containers.append(exported)
        rootfs = io.BytesIO(cli("export", exported).stdout)
        with tarfile.open(fileobj=rootfs, mode="a") as archive:
            node = tarfile.TarInfo("device-filter-node")
            node.type, node.mode, node.devmajor, node.devminor = tarfile.BLKTYPE, 0o600, 0, 42
            archive.addfile(node)
            marker = tarfile.TarInfo("device-filter-marker")
            payload = b"initial-marker\n"
            marker.mode, marker.size = 0o600, len(payload)
            archive.addfile(marker, io.BytesIO(payload))
        image = cli("import", "-", prefix, data=rootfs.getvalue()).stdout.decode().strip()
        assert image.startswith("sha256:"), image
        normal = cli("run", "--rm", "--network", "none", "--entrypoint", "sh", image, "-ec", program)
        report["normal_policy"] = checked_output(normal)
        (output / "normal.log").write_bytes(normal.stdout + normal.stderr)
        assert not cli("ps", "-q").stdout.strip(), "Fault injection requires an otherwise idle private engine"
        if reference:
            report["status"] = "passed"
        else:
            agents = []
            for item in Path("/proc").iterdir():
                if not item.name.isdigit():
                    continue
                try:
                    fields = (item / "stat").read_text().rsplit(")", 1)[1].split()
                    if ((item / "comm").read_text().strip() == "asagiri-init"
                            and fields[0] != "Z" and int(fields[1]) == 1):
                        assert os.readlink(item / "exe") == "/opt/asagiri/asagiri-init"
                        agents.append(int(item.name))
                except (OSError, ValueError):
                    pass
            assert len(agents) == 1, agents
            agent = agents[0]
            assert f"TracerPid:\t0\n" in Path("/proc", str(agent), "status").read_text()
            for occurrence, command in ((1, "BPF_PROG_LOAD"), (2, "BPF_PROG_ATTACH")):
                cid = cli("create", "--network", "none", "--entrypoint", "sh",
                          image, "-ec", program).stdout.decode().strip()
                containers.append(cid)
                saved = io.BytesIO()
                with tarfile.open(fileobj=saved, mode="w") as archive:
                    marker = tarfile.TarInfo("device-filter-marker")
                    payload = b"retained-after-filter-failure\n"
                    marker.mode, marker.size = 0o600, len(payload)
                    archive.addfile(marker, io.BytesIO(payload))
                cli("cp", "-", cid + ":/", data=saved.getvalue())
                trace_path = output / (command + ".trace")
                with trace_path.open("wb") as trace_log:
                    tracer = subprocess.Popen([
                        "strace", "-f", "-e", "trace=bpf",
                        "-e", f"inject=bpf:error=EPERM:when={occurrence}",
                        "-p", str(agent),
                    ], stdout=trace_log, stderr=trace_log)
                    deadline = time.monotonic() + 10
                    while f"TracerPid:\t{tracer.pid}\n" not in Path("/proc", str(agent), "status").read_text():
                        assert tracer.poll() is None, "strace exited before attaching"
                        assert time.monotonic() < deadline, "strace did not attach"
                        time.sleep(0.02)
                    result = cli("start", "-a", cid, ok=False)
                    (output / (command + ".log")).write_bytes(result.stdout + result.stderr)
                    state = inspect(cid)
                    assert state["Id"] == cid and not state["State"]["Running"], state
                    if expect_fallback:
                        observed = checked_output(result, fallback=True, retained=True)
                    else:
                        assert result.returncode != 0 and b"cannot start without a device access filter" in result.stderr, result
                        assert b"device-filter-probe-started" not in result.stdout
                        assert not cli("logs", cid).stdout.strip(), "Rejected start ran the application"
                        observed = {"application_started": False, "start_exit": result.returncode}
                    detach()
                trace = trace_path.read_text()
                injected = [line for line in trace.splitlines() if "(INJECTED)" in line and "EPERM" in line]
                assert len(injected) == 1 and command in injected[0], trace
                assert f"TracerPid:\t0\n" in Path("/proc", str(agent), "status").read_text()
                retry = cli("start", "-a", cid)
                (output / (command + "-retry.log")).write_bytes(retry.stdout + retry.stderr)
                retried = checked_output(retry, retained=True)
                assert inspect(cid)["Id"] == cid
                report["cases"].append({
                    "injected": command, "errno": "EPERM", **observed,
                    "trace_sha256": hashlib.sha256(trace_path.read_bytes()).hexdigest(),
                    "identity_and_writable_data_retained": True, "retry": retried,
                })
                cli("rm", "-f", cid)
                containers.remove(cid)
            report["status"] = "baseline-defect-reproduced" if expect_fallback else "passed"
    finally:
        detach()
        for cid in reversed(containers):
            cli("rm", "-f", cid, ok=False)
        if image is not None:
            cli("image", "rm", "-f", prefix, ok=False)
        (output / "verification.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report), flush=True)
    if expect_fallback:
        print("REPRODUCED old fallback: application starts without device filtering on load/attach failures", flush=True)
    elif reference:
        print("PASS Docker reference denies the pre-existing unassigned block node", flush=True)
    else:
        print("PASS device filter: default denial, injected load/attach errors, retained data and retry", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--expect-fallback", action="store_true")
    parser.add_argument("--reference", action="store_true")
    args = parser.parse_args()
    output = args.output or Path("/tmp") / ("asagiri-device-filter-" + uuid.uuid4().hex[:12])
    check(args.socket, args.image, output, args.expect_fallback, args.reference)
