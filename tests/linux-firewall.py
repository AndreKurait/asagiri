#!/usr/bin/env python3
"""Exercise bridge egress and scoped firewall rules on disposable Linux hosts."""
import argparse
from datetime import datetime, timezone
import hashlib
import ipaddress
import json
from pathlib import Path
import re
import shutil
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
SOCKET = "unix:///run/asagiri/docker.sock"
IMAGE = "alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab"
SERVER = """
import selectors, socket
selector = selectors.DefaultSelector()
for kind, port in ((socket.SOCK_STREAM, 18473), (socket.SOCK_DGRAM, 18473), (socket.SOCK_DGRAM, 53)):
    connection = socket.socket(socket.AF_INET, kind)
    connection.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    connection.bind(("0.0.0.0", port))
    if kind == socket.SOCK_STREAM:
        connection.listen(16)
    selector.register(connection, selectors.EVENT_READ, (kind, port))
print("ready", flush=True)
while True:
    for key, _ in selector.select():
        if key.data[0] == socket.SOCK_DGRAM:
            data, peer = key.fileobj.recvfrom(1024)
            if key.data[1] == 53:
                question = b"\\x07asagiri\\x04test\\x00\\x00\\x01\\x00\\x01"
                if len(data) != 12 + len(question) or data[12:] != question:
                    continue
                reply = (data[:2] + b"\\x81\\x80\\x00\\x01\\x00\\x01\\x00\\x00\\x00\\x00"
                         + question + b"\\xc0\\x0c\\x00\\x01\\x00\\x01\\x00\\x00\\x00\\x3c\\x00\\x04"
                         + socket.inet_aton("192.0.2.123"))
            else:
                reply = peer[0].encode() + b" " + data
            key.fileobj.sendto(reply, peer)
        else:
            connection, peer = key.fileobj.accept()
            with connection:
                connection.settimeout(2)
                data = connection.recv(1024)
                connection.sendall(peer[0].encode() + b" " + data)
"""
CLIENT = """
import socket, sys
dns = sys.argv[2] == "dns"
connection = socket.socket(socket.AF_INET, socket.SOCK_DGRAM if sys.argv[2] in ("udp", "dns") else socket.SOCK_STREAM)
connection.settimeout(1.5)
try:
    connection.connect((sys.argv[1], int(sys.argv[3])))
    question = b"\\x07asagiri\\x04test\\x00\\x00\\x01\\x00\\x01"
    query = b"\\x61\\x73\\x01\\x00\\x00\\x01\\x00\\x00\\x00\\x00\\x00\\x00" + question
    connection.send(query if dns else b"asagiri-firewall-proof")
    reply = connection.recv(1024)
    if dns:
        assert reply[:12] == b"\\x61\\x73\\x81\\x80\\x00\\x01\\x00\\x01\\x00\\x00\\x00\\x00"
        assert reply[12:len(query)] == question and len(reply) == len(query) + 16
        assert reply[-16:-4] == b"\\xc0\\x0c\\x00\\x01\\x00\\x01\\x00\\x00\\x00\\x3c\\x00\\x04"
        print(socket.inet_ntoa(reply[-4:]), end="")
    else:
        print(reply.decode(), end="")
except (TimeoutError, ConnectionRefusedError, OSError):
    sys.exit(7)
finally:
    connection.close()
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bin", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--user-network", action="store_true",
                      help="Use a custom bridge after reserving another network's subnet")
    mode.add_argument("--docker-first", action="store_true",
                      help="Check bridge selection, reachability and restart beside an existing Docker bridge")
    args = parser.parse_args()
    binary_dir, output = args.bin.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    assert shutil.disk_usage(output).free >= 3 * 1024**3
    source = (binary_dir / "SOURCE_COMMIT").read_text().strip()
    probe_source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    assert all(re.fullmatch(r"[0-9a-f]{40}", value) for value in (source, probe_source))
    # A local diagnostic may use the exact same runtime with new test/docs
    # files. Keep both identities; CI builds from this checkout itself.
    subprocess.run(["git", "diff", "--exit-code", source, probe_source, "--",
                    "crates", "Cargo.lock", "Cargo.toml"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    expected = dict(line.split(maxsplit=1)[::-1] for line in (binary_dir / "SHA256SUMS").read_text().splitlines())
    binaries = {name: hashlib.sha256((binary_dir / name).read_bytes()).hexdigest()
                for name in ("asagiri", "asagiri-init")}
    assert set(expected) == set(binaries)
    assert all(expected.get(name) == value for name, value in binaries.items())
    docker_arch = subprocess.check_output(["docker", "info", "--format", "{{.Architecture}}"], text=True).strip()
    architecture = {"aarch64": "arm64", "arm64": "arm64", "x86_64": "amd64", "amd64": "amd64"}[docker_arch]
    host_image = json.loads((ROOT / "scripts/ci/reference-images.json").read_text())["docker"]
    token = uuid.uuid4().hex[:12]
    network, host, peer = (f"asagiri-firewall-{token}-{part}" for part in ("net", "engine", "peer"))
    hosts = []
    network_created = False
    record = {
        "schema": "asagiri.linux-firewall.v1", "status": "running",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "source_commit": source, "probe_commit": probe_source,
        "probe_worktree_clean": not subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip(),
        "probe_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "binaries": binaries, "host_image": host_image, "image": IMAGE,
        "architecture": architecture,
        "network_mode": "docker-first" if args.docker_first else "custom" if args.user_network else "default",
        "outer_ports_published": False, "user_docker_socket_mounted": False,
        "hosts": [host, peer], "network": network, "checks": [],
    }
    log = (output / "test.log").open("w")

    def save():
        (output / "verification.json").write_text(json.dumps(record, indent=2) + "\n")

    def run(*command, check=True, timeout=120):
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
        log.write("$ " + repr(command) + "\n" + result.stdout + result.stderr + "\n")
        log.flush()
        if check and result.returncode:
            raise RuntimeError(f"{command[:5]}: exit {result.returncode}\n{result.stdout[-2000:]}\n{result.stderr[-2000:]}")
        return result

    def engine(*command, **kwargs):
        return run("docker", "exec", host, *command, **kwargs)

    def dock(*command, **kwargs):
        return engine("docker", "--host", SOCKET, *command, **kwargs)

    def reference(*command, **kwargs):
        return engine("docker", "--host", "unix:///run/firewall-reference.sock", *command, **kwargs)

    def iptables(*command):
        return engine("iptables", "-w", "2", *command)

    def result(name, **fields):
        record["checks"].append({"name": name, "status": "passed", **fields})
        print("PASS " + name, flush=True)
        save()

    def check_diagnostic(name, expected_chain):
        # Inspect this fixture's real namespace and binary. Read-only output
        # must describe its actual network and cannot change either ruleset.
        def stable_rules(value):
            if isinstance(value, list):
                return [stable_rules(item) for item in value]
            if isinstance(value, dict):
                # Background packets may advance counter values while the
                # inspection runs. Their expression and all rules stay.
                return {key: ({field: stable_rules(data) for field, data in item.items()
                               if field not in ("packets", "bytes")}
                              if key == "counter" and isinstance(item, dict)
                              else stable_rules(item)) for key, item in value.items()}
            return value
        nft_before = stable_rules(json.loads(engine("nft", "-j", "list", "ruleset").stdout))
        filter_before = iptables("-S").stdout
        tested = engine("/opt/asagiri/asagiri", "diag", "firewall", timeout=15)
        text = tested.stdout
        assert f"Bridge {bridge_interface}:" in text and f"gateway {gateway})" in text
        assert "masquerade expression: present" in text
        assert "iptables forwarding policy: -P FORWARD DROP" in text
        assert f"-I {expected_chain} 1 -i {bridge_interface} -o eth0 -j ACCEPT" in text
        assert "does not simulate rule traversal or prove a packet is blocked" in text
        assert stable_rules(json.loads(engine("nft", "-j", "list", "ruleset").stdout)) == nft_before
        assert iptables("-S").stdout == filter_before
        path = output / ("diagnostic-" + name + ".txt")
        path.write_text(text)
        record.setdefault("diagnostics", []).append({
            "name": name, "status": "passed", "exit": tested.returncode,
            "bridge": bridge_interface, "gateway": gateway, "chain": expected_chain,
            "firewall_rules_unchanged": True,
            "output_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        })
        save()

    def exchange(name, address, via_container, allowed, expected_ip):
        for protocol in ("tcp", "udp", "dns"):
            port = "53" if protocol == "dns" else "18473"
            command = ("exec", "firewall-client", "python3", "-c", CLIENT, address, protocol, port)
            tested = dock(*command, check=False) if via_container else engine(
                "python3", "-c", CLIENT, address, protocol, port, check=False)
            if allowed:
                assert tested.returncode == 0, (name, protocol, tested.stderr)
                expected_reply = "192.0.2.123" if protocol == "dns" else expected_ip + " asagiri-firewall-proof"
                assert tested.stdout == expected_reply, (name, protocol, tested.stdout)
            else:
                assert tested.returncode == 7 and not tested.stdout, (name, protocol, tested)
            result(name + " " + protocol, allowed=allowed, exit=tested.returncode, reply=tested.stdout)

    def await_api(cli):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if cli("version", check=False, timeout=10).returncode == 0:
                return
            time.sleep(0.5)
        raise TimeoutError("The private engine did not start")

    def check_docker_first():
        engine("sh", "-ec",
               "dockerd --host=unix:///run/firewall-reference.sock "
               "--data-root=/test-docker --exec-root=/run/firewall-reference "
               "--pidfile=/run/firewall-reference.pid --bip=172.17.0.1/16 "
               "> /tmp/firewall-docker.log 2>&1 &")
        await_api(reference)
        record["reference_docker"] = json.loads(reference("version", "--format", "{{json .Server}}").stdout)
        docker_subnet = ipaddress.ip_network("172.17.0.0/16")

        def docker_bridge():
            return {
                "routes": json.loads(engine("ip", "-j", "-4", "route", "show", "dev", "docker0").stdout),
                "addresses": json.loads(engine("ip", "-j", "-4", "address", "show", "dev", "docker0").stdout)[0]["addr_info"],
            }

        before = docker_bridge()
        assert any(route.get("dst") == str(docker_subnet) for route in before["routes"]), before
        record["docker_bridge_before"] = before
        engine("sh", "-ec",
               "mkdir -p /tmp/gateway; printf 'asagiri-gateway' > /tmp/gateway/index.html; "
               "python3 -m http.server 18473 --bind 0.0.0.0 --directory /tmp/gateway > /tmp/gateway.log 2>&1 &")

        def gateway_request(cli, container, gateway):
            reply = cli("exec", container, "wget", "-qO-", "-T", "5", f"http://{gateway}:18473/")
            assert reply.stdout == "asagiri-gateway", reply.stdout

        reference("pull", IMAGE)
        reference("run", "-d", "--name", "firewall-reference", IMAGE, "sleep", "3600")
        gateway_request(reference, "firewall-reference", "172.17.0.1")
        result("Docker can reach its gateway before Asagiri starts")
        for attempt in range(1, 3):
            engine("sh", "-ec",
                   "rm -f /tmp/asagiri.pid /tmp/asagiri.status; "
                   "( /opt/asagiri/asagiri daemon & pid=$!; echo \"$pid\" >/tmp/asagiri.pid; "
                   "wait \"$pid\"; echo \"$?\" >/tmp/asagiri.status ) >>/tmp/asagiri.log 2>&1 &")
            await_api(dock)
            if attempt == 1:
                dock("pull", IMAGE)
            bridge = json.loads(dock("network", "inspect", "bridge").stdout)[0]["IPAM"]["Config"][0]
            subnet = ipaddress.ip_network(bridge["Subnet"])
            assert not subnet.overlaps(docker_subnet), bridge
            dock("run", "-d", "--name", "firewall-client", "--pull=never", IMAGE, "sleep", "3600")
            gateway_request(dock, "firewall-client", bridge["Gateway"])
            gateway_request(reference, "firewall-reference", "172.17.0.1")
            result("Both containers can reach their own gateway", start=attempt, asagiri_bridge=bridge)
            dock("rm", "-f", "firewall-client")
            engine("sh", "-ec",
                   "kill -TERM \"$(cat /tmp/asagiri.pid)\"; "
                   "for i in $(seq 60); do if test -s /tmp/asagiri.status; then "
                   "test \"$(cat /tmp/asagiri.status)\" = 0; exit; fi; sleep 0.5; done; exit 1")
            after = docker_bridge()
            assert after == before, (before, after)
            gateway_request(reference, "firewall-reference", "172.17.0.1")
            result("Docker retains its bridge and gateway after Asagiri shuts down", start=attempt, docker_bridge=after)
        reference("rm", "-f", "firewall-reference")

    try:
        network_created = True
        # Docker selects an unused subnet. These two hosts alone join this
        # task-owned bridge; no user socket or published port is involved.
        run("docker", "network", "create", "--label", "asagiri.test=" + token, network)
        host_command = "apk add -q --no-cache python3 iproute2 nftables iptables && "
        host_command += "exec sleep infinity" if args.docker_first else "exec /opt/asagiri/asagiri daemon"
        fixtures = (
            (host, ["--privileged", "--cgroupns=private", "--tmpfs", "/run", "--tmpfs", "/tmp",
                    "-e", "ASAGIRI_SOCKET=/run/asagiri/docker.sock", "-e", "ASAGIRI_DATA_DIR=/test-store",
                    "-e", "ASAGIRI_TEST_ENGINE=1", "-v", str(binary_dir) + ":/opt/asagiri:ro",
                    "-v", "/var/lib/asagiri", "-v", "/test-store", "-v", "/test-docker"],
             ["sh", "-ec", host_command]),
            (peer, ["--cap-add", "NET_ADMIN"],
             ["sh", "-ec", "apk add -q --no-cache python3 iproute2 && exec python3 -u -c " + "'" + SERVER.replace("'", "'\"'\"'") + "'"]),
        )
        for name, options, command in fixtures[:1] if args.docker_first else fixtures:
            hosts.append(name)
            run("docker", "run", "-d", "--name", name, "--network", network,
                "--label", "asagiri.test=" + token, *options, host_image, *command)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if args.docker_first:
                if engine("python3", "--version", check=False, timeout=10).returncode == 0:
                    break
            elif dock("version", check=False, timeout=10).returncode == 0 and "ready" in run("docker", "logs", peer).stdout:
                break
            time.sleep(0.5)
        else:
            raise TimeoutError("Private firewall fixtures did not start")
        info = json.loads(run("docker", "inspect", *hosts).stdout)
        for item in info:
            assert not item["HostConfig"]["PortBindings"]
            assert set(item["NetworkSettings"]["Networks"]) == {network}
            assert all(mount["Source"] == str(binary_dir) and not mount["RW"]
                       for mount in item["Mounts"] if mount["Type"] == "bind")
        assert info[0]["HostConfig"]["CgroupnsMode"] == "private"
        (output / "hosts.json").write_text(json.dumps(info, indent=2) + "\n")
        record["kernel"] = engine("uname", "-sr").stdout.strip()
        if args.docker_first:
            check_docker_first()
            record["status"] = "passed"
            print("Docker-first default bridge checks passed", flush=True)
            return
        host_ip, peer_ip = (item["NetworkSettings"]["Networks"][network]["IPAddress"] for item in info)
        record["peer_ip"], record["engine_ip"] = peer_ip, host_ip
        dock("pull", IMAGE)
        tested_network = "bridge"
        if args.user_network:
            dock("network", "create", "firewall-placeholder")
            tested_network = "firewall-custom"
            dock("network", "create", tested_network)
        dock("run", "-d", "--name", "firewall-client", "--network", tested_network, IMAGE, "sleep", "3600")
        dock("exec", "firewall-client", "apk", "add", "-q", "--no-cache", "python3")
        bridge = json.loads(dock("network", "inspect", tested_network).stdout)[0]
        record["bridge"] = bridge
        gateway = next(item["Gateway"] for item in bridge["IPAM"]["Config"] if ":" not in item["Subnet"])
        interfaces = json.loads(engine("ip", "-j", "-4", "address", "show").stdout)
        matching = [item["ifname"] for item in interfaces
                    if any(address.get("local") == gateway for address in item["addr_info"])]
        assert len(matching) == 1 and re.fullmatch(r"asagiri[0-9]+", matching[0]), matching
        bridge_interface = matching[0]
        assert (bridge_interface != "asagiri0") == args.user_network
        record["bridge_interface"] = bridge_interface
        record["gateway"] = gateway
        container_ip = json.loads(dock("inspect", "firewall-client").stdout)[0]["NetworkSettings"]["Networks"][tested_network]["IPAddress"]
        assert iptables("-S", "FORWARD").stdout.strip() == "-P FORWARD ACCEPT"
        version = engine("iptables", "--version").stdout.strip()
        assert "nf_tables" in version, version
        record["iptables_version"] = version
        own_rules = engine("nft", "-j", "list", "table", "ip", "asagiri").stdout
        assert '"masquerade"' in own_rules
        (output / "asagiri-rules.json").write_text(own_rules)
        result("Asagiri owns its NAT table and iptables uses nf_tables")
        exchange("default forwarding", peer_ip, True, True, host_ip)
        iptables("-P", "FORWARD", "DROP")
        check_diagnostic("forward-drop", "FORWARD")
        exchange("DROP blocks bridge egress", peer_ip, True, False, host_ip)
        exchange("host network remains reachable under DROP", peer_ip, False, True, host_ip)

        # The same narrow rules documented for an iptables-managed host.
        # Keep DROP, other bridges and unsolicited ingress outside them.
        outbound = ["-i", bridge_interface, "-o", "eth0", "-j", "ACCEPT"]
        replies = ["-i", "eth0", "-o", bridge_interface, "-m", "conntrack",
                   "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"]
        for rule in (outbound, replies):
            iptables("-I", "FORWARD", "1", *rule)
        exchange("scoped forwarding restores bridge egress", peer_ip, True, True, host_ip)
        assert iptables("-S", "FORWARD").stdout.startswith("-P FORWARD DROP\n")
        result("global FORWARD policy remains DROP")

        # Direct unsolicited traffic must not reach an unpublished listener.
        dock("exec", "-d", "firewall-client", "python3", "-u", "-c", SERVER)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            listening = dock("exec", "firewall-client", "python3", "-c", CLIENT,
                             "127.0.0.1", "tcp", "18473", check=False)
            if listening.returncode == 0:
                assert listening.stdout == "127.0.0.1 asagiri-firewall-proof"
                break
            time.sleep(0.1)
        else:
            raise TimeoutError("Unpublished listener did not become ready")
        run("docker", "exec", peer, "ip", "route", "add", container_ip + "/32", "via", host_ip)
        rejected = run("docker", "exec", peer, "python3", "-c", CLIENT,
                       container_ip, "tcp", "18473", check=False)
        assert rejected.returncode == 7 and not rejected.stdout
        result("unsolicited ingress to an unpublished container port remains blocked")

        for rule in (outbound, replies):
            iptables("-D", "FORWARD", *rule)
        exchange("removing scoped rules restores DROP", peer_ip, True, False, host_ip)
        assert iptables("-S", "FORWARD").stdout.strip() == "-P FORWARD DROP"
        after_rules = engine("nft", "-j", "list", "table", "ip", "asagiri").stdout
        assert json.loads(after_rules) == json.loads(own_rules), "Host rules must not edit Asagiri's own table"
        result("host rules leave Asagiri's NAT and isolation table unchanged")

        # Start a real second engine in this same disposable host namespace.
        # Its socket and storage belong to this fixture, not the outer host.
        engine("sh", "-ec",
               "dockerd --host=unix:///run/firewall-reference.sock "
               "--data-root=/test-docker --exec-root=/run/firewall-reference "
               "--pidfile=/run/firewall-reference.pid "
               "> /tmp/firewall-docker.log 2>&1 &")
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if reference("version", check=False, timeout=10).returncode == 0:
                break
            time.sleep(0.5)
        else:
            raise TimeoutError("The private reference Docker daemon did not start")
        record["reference_docker"] = json.loads(reference("version", "--format", "{{json .Server}}").stdout)
        docker_user_before = iptables("-S", "DOCKER-USER").stdout
        assert docker_user_before.startswith("-N DOCKER-USER\n")
        assert iptables("-S", "FORWARD").stdout.startswith("-P FORWARD DROP\n")
        check_diagnostic("docker-user", "DOCKER-USER")
        reference("pull", IMAGE)
        reference("run", "-d", "--name", "firewall-reference", IMAGE, "sleep", "3600")
        reference("exec", "firewall-reference", "apk", "add", "-q", "--no-cache", "python3")
        exchange("Docker coexistence still requires bridge forwarding", peer_ip, True, False, host_ip)
        for rule in (outbound, replies):
            iptables("-I", "DOCKER-USER", "1", *rule)
        exchange("DOCKER-USER scoped rules restore Asagiri egress", peer_ip, True, True, host_ip)
        for protocol in ("tcp", "udp", "dns"):
            tested = reference("exec", "firewall-reference", "python3", "-c", CLIENT,
                               peer_ip, protocol, "53" if protocol == "dns" else "18473")
            expected_reply = "192.0.2.123" if protocol == "dns" else host_ip + " asagiri-firewall-proof"
            assert tested.stdout == expected_reply, tested.stdout
            result("Docker's own bridge still reaches the peer " + protocol)
        listening = dock("exec", "firewall-client", "python3", "-c", CLIENT,
                         "127.0.0.1", "tcp", "18473")
        assert listening.stdout == "127.0.0.1 asagiri-firewall-proof"
        rejected = run("docker", "exec", peer, "python3", "-c", CLIENT,
                       container_ip, "tcp", "18473", check=False)
        assert rejected.returncode == 7 and not rejected.stdout
        result("Docker coexistence preserves unsolicited ingress blocking")
        for rule in (outbound, replies):
            iptables("-D", "DOCKER-USER", *rule)
        exchange("removing DOCKER-USER rules restores Asagiri blocking", peer_ip, True, False, host_ip)
        assert iptables("-S", "DOCKER-USER").stdout == docker_user_before
        assert iptables("-S", "FORWARD").stdout.startswith("-P FORWARD DROP\n")
        assert json.loads(engine("nft", "-j", "list", "table", "ip", "asagiri").stdout) == json.loads(own_rules)
        result("Docker and Asagiri firewall ownership remains intact")
        reference("rm", "-f", "firewall-reference")
        dock("rm", "-f", "firewall-client")
        if args.user_network:
            dock("network", "rm", tested_network, "firewall-placeholder")
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = repr(error)
        raise
    finally:
        removed = []
        if host in hosts:
            diagnostic = engine("cat", "/tmp/firewall-docker.log", check=False, timeout=10)
            (output / "reference-docker.log").write_text(diagnostic.stdout + diagnostic.stderr)
            if args.docker_first:
                diagnostic = engine(
                    "cat", "/tmp/asagiri.log", "/tmp/gateway.log",
                    "/root/.asagiri/logs/guest-console.log", check=False, timeout=10,
                )
                (output / "docker-first-runtime.log").write_text(diagnostic.stdout + diagnostic.stderr)
        for name in reversed(hosts):
            logs = run("docker", "logs", name, check=False, timeout=20)
            (output / (("engine" if name == host else "peer") + ".log")).write_text(logs.stdout + logs.stderr)
            run("docker", "rm", "-fv", name, check=False, timeout=45)
            if run("docker", "inspect", name, check=False, timeout=10).returncode:
                removed.append(name)
        if network_created:
            run("docker", "network", "rm", network, check=False, timeout=20)
        record["private_hosts_removed"] = set(removed) == set(hosts)
        record["private_network_removed"] = run("docker", "network", "inspect", network, check=False, timeout=10).returncode != 0
        record["completed_at"] = datetime.now(timezone.utc).isoformat()
        log.close()
        record["log_sha256"] = hashlib.sha256((output / "test.log").read_bytes()).hexdigest()
        save()
        assert record["private_hosts_removed"] and record["private_network_removed"]
    print("Private Linux firewall checks passed", flush=True)


if __name__ == "__main__":
    main()
