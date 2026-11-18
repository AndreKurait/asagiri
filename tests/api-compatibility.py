#!/usr/bin/env python3
"""Compare command resets, endpoint-only creates, filters, IPAM and host overrides."""
import argparse
import hashlib
import http.client
import ipaddress
import json
import os
from pathlib import Path
import socket
import subprocess
import urllib.parse
import uuid

LABEL = "dev.asagiri.api-compatibility"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(args):
    if not args.reference_docker:
        assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "Use a disposable test engine"
        assert os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
    nonce = uuid.uuid4().hex
    # Sort after host/none to exercise their selection as the primary endpoint.
    prefix = "z-api-compat-" + nonce
    owned = {"containers": [], "networks": [], "volumes": []}
    record = {
        "probe": "api-compatibility", "status": "running",
        "reference_docker": args.reference_docker,
        "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "owner": nonce, "checks": [], "cleanup": [],
    }

    def call(method, target, body=None, statuses=(200,), with_status=False):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + target,
                               None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read(4 * 1024**2 + 1)
            assert len(data) <= 4 * 1024**2, "API response exceeded the fixture limit"
            assert response.status in statuses, (method, target, response.status, data[:1000])
            result = json.loads(data) if data else None
            return (response.status, result) if with_status else result
        finally:
            connection.close()

    def container(name, body, statuses=(201,), with_status=False):
        name = prefix + "-" + name
        call("GET", "/containers/" + name + "/json", statuses=(404,))
        # Record ownership before the request: a failing implementation can
        # create a container even when this fixture expects a rejection.
        owned["containers"].append(name)
        return call("POST", "/containers/create?name=" + name, {
            "Image": args.image, "Labels": {LABEL: nonce}, **body,
        }, statuses, with_status)

    def network(name, **options):
        name = prefix + "-" + name
        call("GET", "/networks/" + name, statuses=(404,))
        owned["networks"].append(name)
        result = call("POST", "/networks/create", {
            "Name": name, "Driver": "bridge", "Labels": {LABEL: nonce}, **options,
        }, (201,))
        return name, result["Id"]

    def inspect(cid):
        return call("GET", "/containers/" + cid + "/json")

    def start(cid):
        call("POST", "/containers/" + cid + "/start", statuses=(204,))

    def execute(cid, *command):
        environment = {key: value for key, value in os.environ.items()
                       if key not in {"DOCKER_CONTEXT", "DOCKER_HOST", "DOCKER_API_VERSION",
                                      "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"}}
        result = subprocess.run(["docker", "--host", "unix://" + args.socket, "exec", cid, *command],
                                env=environment, capture_output=True, text=True, timeout=30)
        assert result.returncode == 0, (command, result.returncode, result.stdout, result.stderr)
        return result.stdout.strip()

    def passed(name):
        record["checks"].append(name)
        print("PASS " + name, flush=True)

    def cleanup():
        errors = []
        for kind in ("containers", "networks", "volumes"):
            for name in reversed(owned[kind]):
                try:
                    target = "/" + kind + "/" + name
                    item = call("GET", target + ("/json" if kind == "containers" else ""),
                                statuses=(200, 404))
                    if "message" in item:
                        continue
                    if kind != "volumes":
                        labels = item["Config"]["Labels"] if kind == "containers" else item["Labels"]
                        assert labels.get(LABEL) == nonce, ("Lost resource ownership", kind, name)
                    # Volumes have a unique name checked absent before the
                    # rejected create; an erroneous auto-created volume has
                    # no label to recover by.
                    call("DELETE", target + ("?force=true&v=true" if kind == "containers" else ""),
                         statuses=(204,))
                    record["cleanup"].append({"kind": kind, "name": name})
                except Exception as error:
                    errors.append(f"{kind}/{name}: {error}")
        return errors

    try:
        record["version"] = call("GET", "/version")
        assert bool(record["version"].get("AsagiriVersion")) != args.reference_docker
        record["image"] = call("GET", "/images/" + urllib.parse.quote(args.image, safe="") + "/json")["Id"]

        command = ["/bin/sh", "-c", "exit 23"]
        cid = container("reset", {"Entrypoint": [""], "Cmd": command,
                                 "HostConfig": {"NetworkMode": "none"}})["Id"]
        config = inspect(cid)["Config"]
        assert not config.get("Entrypoint") and config["Cmd"] == command, config
        start(cid)
        assert call("POST", "/containers/" + cid + "/wait")["StatusCode"] == 23
        passed("empty entrypoint sentinel runs the explicit command")
        for index, override in enumerate(({}, {"Cmd": []})):
            volume = prefix + "-rejected-" + str(index)
            call("GET", "/volumes/" + volume, statuses=(404,))
            owned["volumes"].append(volume)
            name = "no-command-" + str(index)
            rejected = container(name, {
                "Entrypoint": [""], "HostConfig": {
                    "NetworkMode": "none", "Binds": [volume + ":/side-effect"],
                }, **override,
            }, (400,))
            assert "no command specified" in rejected["message"].lower(), rejected
            call("GET", "/containers/" + prefix + "-" + name + "/json", statuses=(404,))
            call("GET", "/volumes/" + volume, statuses=(404,))
        passed("missing and empty commands reject before container or volume creation")

        net, net_id = network("endpoints")
        clients = []
        for index, (mode, endpoint) in enumerate(((None, net), ("", net), ("default", net), ("default", net_id))):
            host = {} if mode is None else {"NetworkMode": mode}
            cid = container("endpoint-" + str(index), {
                "Entrypoint": [], "Cmd": ["sleep", "120"],
                "HostConfig": host, "NetworkingConfig": {"EndpointsConfig": {endpoint: {}}},
            })["Id"]
            start(cid)
            settings = inspect(cid)["NetworkSettings"]["Networks"]
            assert set(settings) == {net} and settings[net]["NetworkID"] == net_id, settings
            clients.append(cid)
        passed("endpoint-only creates select the requested name or ID without joining bridge")
        alias = "peer-" + nonce
        peer = container("peer", {
            "Entrypoint": [], "Cmd": ["sleep", "120"], "HostConfig": {},
            "NetworkingConfig": {"EndpointsConfig": {net: {"Aliases": [alias]}}},
        })["Id"]
        start(peer)
        peer_ip = inspect(peer)["NetworkSettings"]["Networks"][net]["IPAddress"]
        # First lookup after start: no retry that could conceal wrong initial
        # network selection or a missing initial DNS entry.
        addresses = {line.split()[0] for line in execute(clients[0], "getent", "hosts", alias).splitlines()}
        assert addresses == {peer_ip}, (addresses, peer_ip)
        passed("endpoint-only peers resolve immediately on their selected network")

        record["incompatible_networks"] = []
        for special in ("host", "none"):
            special_id = call("GET", "/networks/" + special)["Id"]
            for index, key in enumerate((special, special_id)):
                cid = container(special + "-single-" + str(index), {
                    "Entrypoint": [], "Cmd": ["/bin/true"],
                    "HostConfig": {"NetworkMode": special},
                    "NetworkingConfig": {"EndpointsConfig": {key: {}}},
                })["Id"]
                start(cid)
                assert call("POST", "/containers/" + cid + "/wait")["StatusCode"] == 0
                assert set(inspect(cid)["NetworkSettings"]["Networks"]) == {special}
            for index, (mode, endpoints) in enumerate((
                (None, {special: {}, net: {}}),
                ("default", {special_id: {}, net_id: {}}),
                (special, {net: {}}),
                (net_id, {special_id: {}}),
            )):
                name = special + "-mixed-" + str(index)
                volume = prefix + "-" + name + "-volume"
                call("GET", "/volumes/" + volume, statuses=(404,))
                owned["volumes"].append(volume)
                host = {"Binds": [volume + ":/side-effect"]}
                if mode is not None:
                    host["NetworkMode"] = mode
                create_status, result = container(name, {
                    "Entrypoint": [], "Cmd": ["sleep", "120"],
                    "HostConfig": host, "NetworkingConfig": {"EndpointsConfig": endpoints},
                }, (201, 400) if args.reference_docker else (400,), with_status=True)
                outcome = {
                    "mode": mode, "endpoints": list(endpoints),
                    "create_status": create_status, "rejected_at": "create",
                }
                if create_status == 201:
                    # Docker can defer host conflicts until start and accepts
                    # some mixed none requests. Preserve its actual outcome;
                    # Asagiri rejects these unsupported combinations before
                    # create can make containers or volumes.
                    assert args.reference_docker
                    cid = result["Id"]
                    status, result = call("POST", "/containers/" + cid + "/start",
                                          statuses=(204, 400, 500), with_status=True)
                    info = inspect(cid)
                    outcome.update(start_status=status, rejected_at="start",
                                   running=info["State"]["Running"],
                                   network_mode=info["HostConfig"]["NetworkMode"],
                                   networks=info["NetworkSettings"]["Networks"])
                    if status == 204:
                        assert special == "none" and info["State"]["Running"], info
                        assert outcome["networks"] and set(outcome["networks"]) <= {net, "none", "bridge"}, info
                        outcome.update(rejected_at=None,
                                       interfaces=execute(cid, "ip", "-o", "addr", "show"))
                    else:
                        assert not info["State"]["Running"], info
                if outcome["rejected_at"] is not None:
                    assert "network" in result["message"].lower(), result
                    outcome["message"] = result["message"]
                if not args.reference_docker:
                    call("GET", "/containers/" + prefix + "-" + name + "/json", statuses=(404,))
                    call("GET", "/volumes/" + volume, statuses=(404,))
                record["incompatible_networks"].append(outcome)
        passed("single host and none networks work by name or ID")
        if args.reference_docker:
            passed("reference Docker mixed-network outcomes are recorded")
        else:
            passed("mixed host and none endpoints reject before container or volume creation")

        for pattern, expected in ((net_id[:12], [net_id]), ("^/?" + net_id + "$", [net_id]),
                                  (net_id[12:24], [net_id]), ("[", [])):
            filters = {"id": [pattern], "name": ["^" + net + "$"]}
            query = urllib.parse.urlencode({"filters": json.dumps(filters)})
            actual = [row["Id"] for row in call("GET", "/networks?" + query)]
            assert actual == expected, (pattern, actual, expected)
        passed("network ID filters accept prefix, anchored and unanchored regexes")

        existing = [ipaddress.ip_network(config["Subnet"])
                    for item in call("GET", "/networks")
                    for config in item.get("IPAM", {}).get("Config") or [] if config.get("Subnet")]
        subnet = next(ipaddress.ip_network(f"10.240.{octet}.0/24") for octet in range(1, 255)
                      if not any(pool.version == 4 and pool.overlaps(ipaddress.ip_network(f"10.240.{octet}.0/24"))
                                 for pool in existing))
        ipv6 = f"fd{nonce[:2]}:{nonce[2:6]}:{nonce[6:10]}::/64"
        off_net, off_id = network("ipv6-off", EnableIPv6=False, IPAM={
            "Driver": "default", "Config": [{"Subnet": str(subnet)}, {"Subnet": ipv6}],
        })
        info = call("GET", "/networks/" + off_id)
        pools = [ipaddress.ip_network(config["Subnet"]) for config in info["IPAM"]["Config"]]
        # Docker retains the unused IPv6 entry in network inspection. Check
        # the active address families rather than requiring that metadata
        # to be removed.
        assert not info["EnableIPv6"], info
        assert [pool for pool in pools if pool.version == 4] == [subnet], info
        assert {pool for pool in pools if pool.version == 6} <= {ipaddress.ip_network(ipv6)}, info
        cid = container("ipv6-off", {
            "Entrypoint": [], "Cmd": ["sleep", "120"],
            "HostConfig": {"NetworkMode": off_net},
        })["Id"]
        start(cid)
        endpoint = inspect(cid)["NetworkSettings"]["Networks"][off_net]
        assert ipaddress.ip_address(endpoint["IPAddress"]) in subnet, endpoint
        assert endpoint["IPPrefixLen"] == subnet.prefixlen, endpoint
        assert not endpoint.get("GlobalIPv6Address") and not endpoint.get("GlobalIPv6PrefixLen"), endpoint
        global_ipv6 = execute(cid, "ip", "-6", "addr", "show", "eth0", "scope", "global")
        assert not global_ipv6, global_ipv6
        record["disabled_ipv6"] = {"ipam": info["IPAM"]["Config"], "endpoint": endpoint,
                                   "global_ipv6": global_ipv6}
        passed("IPv6-disabled networks accept mixed IPAM and allocate only IPv4")

        cid = container("host-override", {
            "Entrypoint": [], "Cmd": ["sleep", "120"], "HostConfig": {
                "NetworkMode": "none", "ExtraHosts": ["host.docker.internal:203.0.113.19"],
            },
        })["Id"]
        start(cid)
        hosts = execute(cid, "cat", "/etc/hosts")
        matches = [line.split()[0] for line in hosts.splitlines()
                   if "host.docker.internal" in line.split("#", 1)[0].split()[1:]]
        assert matches == ["203.0.113.19"], hosts
        resolved = execute(cid, "getent", "hosts", "host.docker.internal").splitlines()
        assert resolved and {line.split()[0] for line in resolved} == {"203.0.113.19"}, resolved
        passed("explicit host.docker.internal overrides have no built-in fallback")

        cid = container("host-override-ipv6", {
            "Entrypoint": [], "Cmd": ["sleep", "120"], "HostConfig": {
                "NetworkMode": "none", "ExtraHosts": ["HOST.DOCKER.INTERNAL:2001:db8::19"],
            },
        })["Id"]
        start(cid)
        hosts = execute(cid, "cat", "/etc/hosts")
        matches = [line.split()[0] for line in hosts.splitlines()
                   if "host.docker.internal" in
                   [name.lower() for name in line.split("#", 1)[0].split()[1:]]]
        assert matches == ["2001:db8::19"], hosts
        passed("uppercase IPv6 host overrides have no built-in IPv4 fallback")
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=str(error))
        raise
    finally:
        errors = cleanup()
        if errors:
            record.update(status="failed", cleanup_errors=errors)
        print(json.dumps(record, indent=2), flush=True)
        if errors and "error" not in record:
            raise AssertionError(("Fixture cleanup failed", errors))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image", nargs="?", default="alpine:3.21")
    parser.add_argument("--reference-docker", action="store_true")
    check(parser.parse_args())
