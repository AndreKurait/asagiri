#!/usr/bin/env python3
"""Exercise kind's automatic dual-stack network and gateway DNS selection."""
import ipaddress
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid


def check(path, image):
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1" and Path("/.dockerenv").is_file()
    docker = ["docker", "-H", "unix://" + path]
    token = "kind-network-" + uuid.uuid4().hex[:8]
    networks, containers = [], []

    def call(*args, timeout=30):
        return subprocess.check_output(docker + list(args), stderr=subprocess.STDOUT, timeout=timeout).decode().strip()

    def inspect(kind, name):
        return json.loads(call(kind, "inspect", name))[0]

    try:
        # The normal IPv4 command and kind v0.27's exact network options.
        for name, extra in ((token + "-plain", []), (token, [
            "-d=bridge", "-o", "com.docker.network.bridge.enable_ip_masquerade=true",
            "--ipv6", "--subnet", "fc00:f853:ccd:e793::/64",
        ])):
            call("network", "create", *extra, name)
            networks.append(name)
        plain = inspect("network", networks[0])
        kind = inspect("network", token)
        plain4 = next(ipaddress.ip_network(c["Subnet"]) for c in plain["IPAM"]["Config"]
                      if ipaddress.ip_network(c["Subnet"]).version == 4)
        kind4 = next(ipaddress.ip_network(c["Subnet"]) for c in kind["IPAM"]["Config"]
                     if ipaddress.ip_network(c["Subnet"]).version == 4)
        assert not plain4.overlaps(kind4)
        assert any(c["Subnet"] == "fc00:f853:ccd:e793::/64" for c in kind["IPAM"]["Config"]), kind
        peer = call("run", "-d", "--network", token, "--network-alias", "kind-registry", image, "sleep", "120")
        containers.append(peer)
        node = call("run", "-d", "--privileged", "--network", token, image, "sleep", "120")
        containers.append(node)
        peer_network = inspect("container", peer)["NetworkSettings"]["Networks"][token]
        node_network = inspect("container", node)["NetworkSettings"]["Networks"][token]
        gateway = str(ipaddress.ip_address(node_network["Gateway"]))
        assert "nameserver 127.0.0.11" in call("exec", node, "cat", "/etc/resolv.conf")

        def lookup(record, hostname, resolver=None):
            args = ["exec", node, "nslookup", "-type=" + record, hostname]
            if resolver is not None:
                args.append(resolver)
            return call(*args, timeout=20)

        # Both the engine's embedded loopback address and the bridge gateway
        # must answer network-local A/AAAA records.
        for resolver in ("127.0.0.11", gateway):
            assert peer_network["IPAddress"] in lookup("a", "kind-registry", resolver)
            assert peer_network["GlobalIPv6Address"] in lookup("aaaa", "kind-registry", resolver)
            lookup("a", "registry-1.docker.io", resolver)
        # kind replaces the loopback resolver with the Docker host address,
        # falling back to the default gateway. Test each actual destination.
        selected = call("exec", node, "sh", "-c",
                        "getent hosts host.docker.internal 2>/dev/null | head -n1 | awk '{print $1}'")
        try:
            selected_ip = ipaddress.ip_address(selected)
        except ValueError:
            selected_ip = ipaddress.ip_address(gateway)
        if selected_ip.version != 4 or selected_ip.is_loopback:
            selected_ip = ipaddress.ip_address(gateway)
        selected = str(selected_ip)
        for resolver in dict.fromkeys((gateway, selected)):
            call("exec", node, "sh", "-c", 'printf "nameserver %s\\n" "$1" > /etc/resolv.conf',
                 "kind-dns-test", resolver)
            assert peer_network["IPAddress"] in lookup("a", "kind-registry")
            assert peer_network["GlobalIPv6Address"] in lookup("aaaa", "kind-registry")
            lookup("a", "registry-1.docker.io")
        print(json.dumps({"probe": "kind-network", "automaticIPv4": str(plain4),
                          "automaticDualStackIPv4": str(kind4), "gateway": gateway,
                          "kindResolver": selected, "localAandAAAA": "passed",
                          "externalRegistryDNS": "passed"}), flush=True)
    finally:
        for cid in reversed(containers):
            subprocess.run(docker + ["rm", "-f", cid], capture_output=True, timeout=30)
        for name in reversed(networks):
            subprocess.run(docker + ["network", "rm", name], capture_output=True, timeout=30)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2])
