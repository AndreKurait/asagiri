#!/usr/bin/env python3
"""Check DNS and subnet reuse after successful and rejected container starts."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("socket")
parser.add_argument("image")
parser.add_argument("--docker-reference", action="store_true")
args = parser.parse_args()
if not args.docker_reference:
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
    assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
prefix = "asagiri-dns-reuse-" + uuid.uuid4().hex[:12]
scratch = tempfile.TemporaryDirectory(prefix=prefix + "-")
networks, containers, findings = [], [], []
record = {"prefix": prefix, "checks": [], "findings": findings,
          "engine": "Docker" if args.docker_reference else "Asagiri"}


def cli(*items, ok=True):
    value = subprocess.run(["docker", "-H", "unix://" + args.socket, *items],
                           capture_output=True, text=True, timeout=90)
    if ok:
        assert value.returncode == 0, (items, value.returncode, value.stdout, value.stderr)
    return value


def check(message, **data):
    record["checks"].append({"name": message, **data})
    print("PASS " + message, flush=True)


def inspect_net(name):
    value = json.loads(cli("network", "inspect", name).stdout)[0]
    ipam = next(row for row in value["IPAM"]["Config"] if ":" not in row["Subnet"])
    return ipam["Subnet"], ipam["Gateway"]


def host_dns_sockets(gateway):
    address = socket.inet_aton(gateway)[::-1].hex().upper() + ":0035"
    return [row.split()[9] for row in Path("/proc/net/udp").read_text().splitlines()[1:]
            if row.split()[1] == address]


def wait_for_gateway_release(gateway):
    deadline = time.monotonic() + 10
    while True:
        remaining = host_dns_sockets(gateway)
        if not remaining:
            return
        assert time.monotonic() < deadline, ("gateway DNS socket survives network deletion", gateway, remaining)
        time.sleep(.05)


def agent_sockets():
    rows = []
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            if (path / "comm").read_text().strip() != "asagiri-init":
                continue
            stat = (path / "stat").read_text().rsplit(")", 1)[1].split()
            if stat[0] == "Z":
                continue
            fds = []
            for fd in (path / "fd").iterdir():
                try:
                    fds.append(os.readlink(fd))
                except FileNotFoundError:
                    pass  # A descriptor may close while this live process is sampled.
            rows.append({"pid": int(path.name), "start_time": stat[19],
                         "sockets": sorted(fd for fd in fds if fd.startswith("socket:"))})
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
    return rows


def require_agent(expected=None):
    # Copy, removal and container-start workers briefly inherit the agent's
    # process name after fork. The data-directory lock records the agent PID;
    # counting every process named asagiri-init mistakes workers for restarts.
    lock = Path(os.environ["ASAGIRI_DATA_DIR"]) / "agent.lock"
    text = lock.read_text().strip()
    assert text.isdecimal() and int(text) > 0, ("invalid recorded agent PID", text)
    pid = int(text)
    rows = [row for row in agent_sockets() if row["pid"] == pid]
    assert lock.read_text().strip() == text, "agent PID changed while sampling descriptors"
    assert len(rows) == 1, ("expected one readable live Asagiri agent", rows)
    if expected is not None:
        assert (rows[0]["pid"], rows[0]["start_time"]) == (expected["pid"], expected["start_time"]), \
            ("agent changed during cleanup checks", expected, rows)
    return rows


def agent_mounts(pid, cid):
    return [row.split()[4] for row in Path(f"/proc/{pid}/mountinfo").read_text().splitlines()
            if row.split()[4].endswith("/" + cid + "/rootfs")]


def create_network(name, subnet=None):
    options = ["--subnet", subnet] if subnet else []
    cli("network", "create", *options, name)
    networks.append(name)
    return inspect_net(name)


try:
    subnet = None
    for index in range(3):
        name = prefix + "-normal-" + str(index)
        actual, gateway = create_network(name, subnet)
        if subnet is None:
            subnet = actual
        assert actual == subnet
        cid = cli("create", "--name", prefix + "-normal", "--network", name, args.image, "true").stdout.strip()
        containers.append(cid)
        cli("start", "-a", cid)
        cli("rm", cid)
        containers.remove(cid)
        cli("network", "rm", name)
        networks.remove(name)
        wait_for_gateway_release(gateway)
    check("three network lifecycles reuse the same subnet and release gateway DNS", subnet=subnet)

    network = prefix + "-failure"
    _, gateway = create_network(network, subnet)
    active_alias = "active." + prefix + ".invalid"
    address = str(ipaddress.ip_network(subnet).network_address + 20)
    live = cli("run", "-d", "--name", prefix + "-live", "--network", network,
               "--ip", address, "--network-alias", active_alias, args.image, "sleep", "3600").stdout.strip()
    containers.append(live)
    before = [] if args.docker_reference else require_agent()
    alias = "removed." + prefix + ".invalid"
    bad = cli("create", "--name", prefix + "-bad", "--network", network,
              "--network-alias", alias, "--sysctl", "net.ipv4.ip_local_port_range=invalid",
              args.image, "true").stdout.strip()
    containers.append(bad)
    proof = Path(scratch.name) / "proof.txt"
    proof.write_text(prefix + " writable data\n")
    cli("cp", str(proof), bad + ":/failure-proof")
    failures = []
    for _ in range(3):
        started = cli("start", bad, ok=False)
        assert started.returncode != 0, started
        if "sysctl" not in started.stderr.lower():
            findings.append("retry of the rejected start fails at a different setup boundary")
        state = json.loads(cli("inspect", bad).stdout)[0]["State"]
        assert not state["Running"], state
        time.sleep(0.1)
        if not args.docker_reference:
            require_agent(before[0])
        mounts = agent_mounts(before[0]["pid"], bad) if not args.docker_reference else []
        failures.append({"exit": started.returncode, "stderr": started.stderr, "state": state,
                         "agent_rootfs_mounts": mounts})
    if any(item["agent_rootfs_mounts"] for item in failures):
        findings.append("rejected starts leave their rootfs mounted in the agent")
    recovered = Path(scratch.name) / "recovered.txt"
    cli("cp", bad + ":/failure-proof", str(recovered))
    assert recovered.read_bytes() == proof.read_bytes()
    check("writable data survives repeated rejected starts")
    cli("rm", bad)
    containers.remove(bad)
    after = []
    if not args.docker_reference:
        deadline = time.monotonic() + 10
        while True:
            after = require_agent(before[0])
            mounts = agent_mounts(before[0]["pid"], bad)
            cgroups = [str(path) for path in Path("/sys/fs/cgroup").rglob(bad)]
            if not mounts and not cgroups or time.monotonic() >= deadline:
                break
            time.sleep(.05)
    record["failed_starts"] = failures
    record["agent_sockets_before"] = before
    record["agent_sockets_after"] = after
    if not args.docker_reference:
        record["agent_rootfs_mounts_after_removal"] = mounts
        record["cgroups_after_removal"] = cgroups
        if record["agent_rootfs_mounts_after_removal"]:
            findings.append("removing the rejected container leaves rootfs mounts")
        if record["cgroups_after_removal"]:
            findings.append("removing the rejected container leaves its cgroup")
    if not args.docker_reference:
        extra = sorted(set(after[0]["sockets"]) - set(before[0]["sockets"]))
        record["additional_agent_sockets"] = extra
        if extra:
            findings.append("rejected starts retain agent sockets after container removal")
        else:
            check("rejected starts release their agent sockets while the peer remains running", attempts=3)
    check("repeated invalid sysctl starts are rejected and the container is removable")
    answer = cli("exec", live, "nslookup", "-type=A", active_alias)
    assert address in answer.stdout, answer.stdout
    check("the existing peer and its DNS registration survive another container's rejected starts")
    old = cli("exec", live, "nslookup", "-type=A", alias, ok=False)
    record["removed_alias_lookup"] = {"exit": old.returncode, "stdout": old.stdout, "stderr": old.stderr}
    if old.returncode == 0:
        findings.append("a removed container's alias still resolves after its rejected start")
    else:
        check("the rejected and removed container's alias no longer resolves")
    cli("rm", "-f", live)
    containers.remove(live)
    cli("network", "rm", network)
    networks.remove(network)
    wait_for_gateway_release(gateway)
    name = prefix + "-recreated"
    actual, _ = create_network(name, subnet)
    assert actual == subnet
    cli("run", "--rm", "--network", name, args.image, "true")
    cli("network", "rm", name)
    networks.remove(name)
    check("the failed-start network releases DNS and its subnet is usable again")
finally:
    errors = []
    for cid in reversed(containers):
        value = cli("rm", "-f", cid, ok=False)
        if value.returncode and "no such container" not in value.stderr.lower():
            errors.append(value.stderr)
    for name in reversed(networks):
        value = cli("network", "rm", name, ok=False)
        if value.returncode and "not found" not in value.stderr.lower():
            errors.append(value.stderr)
    record["cleanup_errors"] = errors
    scratch.cleanup()
    print(json.dumps(record), flush=True)
    assert not errors, errors
assert not findings, findings
print(json.dumps({"probe": "network-dns-reuse", "checks_passed": len(record["checks"]),
                  "failed_starts": len(record.get("failed_starts", [])), "findings": findings}), flush=True)
