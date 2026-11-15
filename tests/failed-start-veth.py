"""A rejected start must retire its veth before another start reuses the IP."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import tempfile
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("socket")
parser.add_argument("image")
parser.add_argument("--expect-stale", action="store_true")
args = parser.parse_args()
assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
assert Path("/.dockerenv").is_file()
assert os.readlink("/proc/1/exe") == "/opt/asagiri/asagiri"
docker = ["docker", "-H", "unix://" + args.socket]
network = "failed-veth-" + uuid.uuid4().hex[:12]
containers, held, tracer, starting = [], None, None, None
scratch = tempfile.TemporaryDirectory(prefix=network + "-")
trace = Path(scratch.name) / "sysctl.trace"
record = {"probe": "failed-start-veth", "expected_stale": args.expect_stale}


def cli(*items, timeout=30):
    return subprocess.check_output(docker + list(items), stderr=subprocess.STDOUT, timeout=timeout)


try:
    cli("network", "create", network)
    info = json.loads(cli("network", "inspect", network))[0]
    subnet = ipaddress.ip_network(next(row["Subnet"] for row in info["IPAM"]["Config"] if ":" not in row["Subnet"]))
    address, peer_address = str(subnet.network_address + 10), str(subnet.network_address + 20)
    peer = cli("run", "-d", "--network", network, "--ip", peer_address,
               "--network-alias", "live-peer", args.image, "sleep", "120").decode().strip()
    containers.append(peer)
    bad = cli("create", "--network", network, "--ip", address,
              "--sysctl", "net.ipv4.ip_local_port_range=invalid", args.image, "true").decode().strip()
    containers.append(bad)
    agents = []
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            stat = (path / "stat").read_text().rsplit(")", 1)[1].split()
            if (path / "comm").read_text().strip() == "asagiri-init" and stat[0] != "Z" and stat[1] == "1":
                agents.append(int(path.name))
        except (FileNotFoundError, ProcessLookupError):
            pass
    assert len(agents) == 1, agents
    agent = agents[0]
    sysctl = "/proc/sys/net/ipv4/ip_local_port_range"
    calls = {"x86_64": ("open", "openat"), "aarch64": ("openat",)}[platform.machine()]
    command = ["strace", "-f", "-P", sysctl, "-e", "trace=" + ",".join(calls)]
    for call in calls:
        command += ["-e", "inject=" + call + ":delay_enter=5s"]
    with trace.open("xb") as log:
        tracer = subprocess.Popen(command + ["-p", str(agent)], stdout=log, stderr=log)
    deadline = time.monotonic() + 10
    while f"TracerPid:\t{tracer.pid}\n" not in Path(f"/proc/{agent}/status").read_text():
        assert tracer.poll() is None and time.monotonic() < deadline, trace.read_text()
        time.sleep(.01)
    started_at = time.monotonic()
    starting = subprocess.Popen(docker + ["start", bad], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    # Keep the failed child's namespace alive after SIGKILL, modeling delayed
    # namespace release without relying on kernel garbage-collection timing.
    deadline = time.monotonic() + 10
    while held is None:
        for cgroup in Path("/sys/fs/cgroup").rglob(bad):
            try:
                for pid in (cgroup / "cgroup.procs").read_text().split():
                    path = f"/proc/{int(pid)}/ns/net"
                    if os.readlink(path) != os.readlink(f"/proc/{agent}/ns/net"):
                        held = os.open(path, os.O_RDONLY | os.O_CLOEXEC)
                        break
            except (FileNotFoundError, ProcessLookupError):
                pass
            if held is not None:
                break
        assert starting.poll() is None or held is not None, "start ended before its namespace could be held"
        assert time.monotonic() < deadline, "no failed-start network namespace"
        time.sleep(.01)
    links = []
    while not links:
        links = [path.name for path in Path("/sys/class/net").glob("kv" + bad[:9] + "*")]
        assert time.monotonic() < deadline, "no host-side veth for the delayed start"
        time.sleep(.01)
    assert len(links) == 1, links
    namespace_inode = os.fstat(held).st_ino
    out, err = starting.communicate(timeout=30)
    assert starting.returncode != 0 and b"sysctl" in err.lower(), (out, err)
    assert time.monotonic() - started_at >= 4.5
    starting = None
    tracer.send_signal(signal.SIGINT)
    tracer.wait(timeout=10)
    tracer = None
    trace_text = trace.read_text()
    assert sysctl in trace_text and "DELAYED" in trace_text, trace_text
    assert all(Path("/sys/class/net", link).exists() for link in links)
    cli("rm", bad)
    containers.remove(bad)
    replacement = cli("run", "-d", "--network", network, "--ip", address,
                      args.image, "sleep", "120").decode().strip()
    containers.append(replacement)
    inspected = json.loads(cli("inspect", replacement))[0]
    assert inspected["NetworkSettings"]["Networks"][network]["IPAddress"] == address
    remaining = [link for link in links if Path("/sys/class/net", link).exists()]
    assert os.fstat(held).st_ino == namespace_inode
    record.update(old_links=links, old_links_remaining=remaining,
                  namespace_held_open=True, namespace_inode=namespace_inode,
                  same_ip_reused=True, address=address, trace=trace_text)
    if args.expect_stale:
        assert remaining == links, record
        record["outcome"] = "stale-link-reproduced"
    else:
        assert not remaining, record
        cli("exec", replacement, "ping", "-c", "1", "-W", "3", peer_address, timeout=5)
        lookup = cli("exec", replacement, "nslookup", "-type=A", "live-peer", "127.0.0.11", timeout=5)
        assert peer_address.encode() in lookup, lookup
        record.update(outcome="passed", peer_ping=True, peer_dns=True)
finally:
    if tracer is not None and tracer.poll() is None:
        tracer.send_signal(signal.SIGINT)
        tracer.wait(timeout=15)
    if starting is not None:
        starting.communicate(timeout=30)
    if held is not None:
        os.close(held)
    for cid in reversed(containers):
        subprocess.run(docker + ["rm", "-f", cid], capture_output=True, timeout=30)
    subprocess.run(docker + ["network", "rm", network], capture_output=True, timeout=30)
    scratch.cleanup()
    print(json.dumps(record), flush=True)
