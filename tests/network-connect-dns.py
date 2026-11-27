#!/usr/bin/env python3
"""Check DNS and reachability for containers connected before and after startup."""
import ipaddress
import http.client
import io
import json
import re
import socket
import sys
import tarfile
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    containers = []
    networks = []
    tag = "kcd-" + uuid.uuid4().hex[:8]

    def request(method, target, body=None, statuses=(200,)):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            if response.status == 201 and target.startswith("/containers/create"):
                containers.append(json.loads(data)["Id"])
            if response.status == 201 and target == "/networks/create":
                networks.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            if "/archive?" in target:
                return data
            return json.loads(data) if data else None
        finally:
            connection.close()

    def run(name, host=None, endpoints=None):
        body = {"Image": image, "Cmd": ["sleep", "300"], "HostConfig": host or {}}
        if endpoints:
            body["NetworkingConfig"] = {"EndpointsConfig": endpoints}
        cid = request("POST", "/containers/create?name=" + name, body, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    # A command's exit status and output. The output goes to a file in the
    # container and is read through the archive API: that asks nothing of
    # how an engine attaches to an exec.
    def sh(cid, command):
        target = "/tmp/out-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 30
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "the exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + target)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            return state["ExitCode"], archive.extractfile(archive.getmembers()[0]).read().decode()

    def resolv(cid):
        code, text = sh(cid, "cat /etc/resolv.conf")
        assert code == 0, text
        return [line for line in text.splitlines() if line and not line.startswith("#")]

    def nameservers(cid):
        return [line.split()[1] for line in resolv(cid) if line.startswith("nameserver")]

    # Only the A record is asked for: a name the engine knows has no AAAA
    # record, and that question would go on to servers that may not answer.
    def resolves(cid, name, address):
        code, text = sh(cid, "nslookup -type=a " + name)
        return code == 0 and ("Address: " + address + "\n") in text + "\n"

    # UDP datagrams that reached the container and found no one listening.
    def unanswered(cid):
        code, text = sh(cid, "awk '/^Udp: [0-9]/ {print $3}' /proc/net/snmp")
        assert code == 0 and text.strip().isdigit(), (code, text)
        return int(text)

    def join(network, cid, aliases=()):
        request("POST", "/networks/" + network + "/connect",
                {"Container": cid, "EndpointConfig": {"Aliases": list(aliases)}}, (200, 204))

    def address(cid, network):
        return request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"][network]["IPAddress"]

    cases = 0
    try:
        name = tag + "-net"
        network = request("POST", "/networks/create", {
            "Name": name, "IPAM": {"Config": [{"Subnet": "10.249.73.0/24"}]},
        }, (201,))["Id"]
        primary_alias = tag + "-primary-alias"
        peer = run(tag + "-peer", {"NetworkMode": name}, {name: {"Aliases": [primary_alias]}})
        peer_ip = address(peer, name)

        # The default bridge has no names, so a container there asks other
        # resolvers. With its first user network it asks 127.0.0.11, which
        # knows that network's names.
        late = run(tag + "-late")
        assert not resolves(late, tag + "-peer", peer_ip), "the default bridge resolved a name of another network"
        join(network, late)
        assert nameservers(late) == ["127.0.0.11"], resolv(late)
        assert resolves(late, tag + "-peer", peer_ip), sh(late, "nslookup -type=a " + tag + "-peer")
        assert resolves(peer, tag + "-late", address(late, name)), sh(peer, "nslookup -type=a " + tag + "-late")
        print("PASS a container on the default bridge resolves the network it joins", flush=True)
        cases += 1

        # A router can set its own IPv6 addresses on an IPv4-only bridge.
        router = run(tag + "-manual-v6", {"CapAdd": ["NET_ADMIN"], "Sysctls": {
            "net.ipv6.conf.all.disable_ipv6": "0",
            "net.ipv6.conf.default.disable_ipv6": "0",
        }})
        join(network, router)
        endpoint = request("GET", "/containers/" + router + "/json")["NetworkSettings"]["Networks"][name]
        assert not endpoint["GlobalIPv6Address"], endpoint
        mac = endpoint["MacAddress"]
        assert re.fullmatch(r"(?:[0-9a-f]{2}:){5}[0-9a-f]{2}", mac), endpoint
        code, text = sh(router, f"""
            set -e
            dev=$(ip -o link show | awk -F ': ' '$0 ~ /{mac}/ {{sub(/@.*/, "", $2); print $2}}')
            test -n "$dev"
            ip -6 addr add fe80:ec2::1/64 dev "$dev"
            ip -6 route add 2001:db8:ec2::/64 dev "$dev"
            ip -6 addr show dev "$dev"
        """)
        assert code == 0 and "fe80:ec2::1/64" in text, (code, text)
        print("PASS manually configured IPv6 on a hot-attached IPv4-only bridge", flush=True)
        cases += 1

        # Leaving the network takes its names away again; the resolver stays.
        request("POST", "/networks/" + network + "/disconnect", {"Container": late}, (200, 204))
        assert nameservers(late) == ["127.0.0.11"], resolv(late)
        assert not resolves(late, tag + "-peer", peer_ip), "a name of a network the container left still resolves"
        print("PASS the names go with the network", flush=True)
        cases += 1

        # A container that is connected when it starts resolves them too.
        join(network, late)
        request("POST", "/containers/" + late + "/stop?t=0", statuses=(204,))
        request("POST", "/containers/" + late + "/wait?condition=not-running")
        request("POST", "/containers/" + late + "/start", statuses=(204,))
        assert nameservers(late) == ["127.0.0.11"], resolv(late)
        assert resolves(late, tag + "-peer", peer_ip), sh(late, "nslookup -type=a " + tag + "-peer")
        print("PASS after a restart with both networks", flush=True)
        cases += 1

        # `--dns`, `--dns-search` and `--dns-option`: the servers leave the
        # file (127.0.0.11 forwards to them), the rest of it stays. The
        # server here is a container that answers nothing; its kernel counts
        # the questions that reach it.
        server = run(tag + "-server")
        server_ip = address(server, "bridge")
        own = run(tag + "-own", {"Dns": [server_ip], "DnsSearch": ["corp.example"], "DnsOptions": ["ndots:2"]})
        assert nameservers(own) == [server_ip], resolv(own)
        join(network, own)
        lines = resolv(own)
        assert nameservers(own) == ["127.0.0.11"], lines
        assert "search corp.example" in lines, lines
        assert any(line.startswith("options") and "ndots:2" in line.split() for line in lines), lines
        # The name is asked for as it is: with the search domain it would
        # first go to the servers given, and nothing answers there.
        assert resolves(own, tag + "-peer.", peer_ip), sh(own, "nslookup -type=a " + tag + "-peer.")
        # A name no network has goes on to the container's own server.
        before = unanswered(server)
        sh(own, "timeout 5 nslookup -type=a outside-" + tag + ".example.")
        assert unanswered(server) > before, "127.0.0.11 did not ask the container's own DNS server"
        print("PASS a container with its own DNS servers", flush=True)
        cases += 1

        # A file the container has changed is the container's.
        edited = run(tag + "-edited")
        code, text = sh(edited, "echo 'nameserver 192.0.2.54' > /etc/resolv.conf")
        assert code == 0, text
        join(network, edited)
        assert resolv(edited) == ["nameserver 192.0.2.54"], resolv(edited)
        print("PASS a changed resolv.conf is left alone", flush=True)
        cases += 1

        # A container that starts on a user network already asks 127.0.0.11,
        # and resolves a second network's names once it joins.
        other = tag + "-other"
        second = request("POST", "/networks/create", {
            "Name": other, "IPAM": {"Config": [{"Subnet": "10.249.74.0/24"}]},
        }, (201,))["Id"]
        far = run(tag + "-far", {"NetworkMode": other})
        far_ip = address(far, other)
        assert not resolves(peer, tag + "-far", far_ip)
        second_alias = tag + "-second-alias"
        join(second, peer, [second_alias])
        assert nameservers(peer) == ["127.0.0.11"], resolv(peer)
        assert resolves(peer, tag + "-far", far_ip), sh(peer, "nslookup -type=a " + tag + "-far")
        print("PASS a container on a user network joins a second one", flush=True)
        cases += 1

        # The application checks the resolver, resolves both peers without
        # retries, and sends traffic through both extra interfaces.
        for i in range(3):
            client = request("POST", "/containers/create?name=" + tag + f"-prejoined-{i}", {
                "Image": image, "HostConfig": {"NetworkMode": "bridge"},
                "Cmd": ["sh", "-exc",
                        'cat /etc/resolv.conf; grep -Fx "nameserver 127.0.0.11" /etc/resolv.conf; '
                        'nslookup -type=a "$1" | grep -Fx "Address: $2"; '
                        'nslookup -type=a "$3" | grep -Fx "Address: $4"; '
                        'ip -4 addr show eth1 | grep "inet "; ip -4 addr show eth2 | grep "inet "; '
                        'ping -c 1 -W 2 "$2"; ping -c 1 -W 2 "$4"',
                        "sh", tag + "-peer", peer_ip, tag + "-far", far_ip],
            }, (201,))["Id"]
            join(network, client)
            join(second, client)
            request("POST", "/containers/" + client + "/start", statuses=(204,))
            outcome = request("POST", "/containers/" + client + "/wait")
            if outcome["StatusCode"] != 0:
                connection = Connection(path)
                try:
                    connection.request("GET", "/v1.47/containers/" + client + "/logs?stdout=1&stderr=1")
                    logs = connection.getresponse().read().decode(errors="replace")
                finally:
                    connection.close()
                raise AssertionError((outcome, logs))
        print("PASS a preconnected bridge container resolves its peer from its first lookup", flush=True)
        cases += 1

        # The second reservation fails. A different container can claim
        # the first reservation's address before the failed one is removed.
        free_ip = str(ipaddress.IPv4Address(peer_ip) + 40)
        for suffix in ("blocked", "retry"):
            cid = request("POST", "/containers/create?name=" + tag + "-" + suffix, {
                "Image": image, "HostConfig": {"NetworkMode": "bridge"}, "Cmd": ["sleep", "120"],
            }, (201,))["Id"]
            request("POST", "/networks/" + network + "/connect", {
                "Container": cid, "EndpointConfig": {"IPAMConfig": {"IPv4Address": free_ip}},
            }, (200, 204))
            if suffix == "blocked":
                request("POST", "/networks/" + second + "/connect", {
                    "Container": cid, "EndpointConfig": {"IPAMConfig": {"IPv4Address": far_ip}},
                }, (200, 204))
                error = request("POST", "/containers/" + cid + "/start", statuses=(409,))
                assert "Address already in use" in error["message"], error
                assert not request("GET", "/containers/" + cid + "/json")["State"]["Running"]
            else:
                request("POST", "/containers/" + cid + "/start", statuses=(204,))
                assert address(cid, name) == free_ip
        print("PASS failed preconnection releases the address reserved on the first network", flush=True)
        cases += 1

        old = tag + "-peer"
        new = tag + "-renamed"
        second_ip = address(peer, other)
        observers = ((late, peer_ip, primary_alias), (far, second_ip, second_alias))
        for observer, ip, alias in observers:
            assert resolves(observer, old, ip), ("old name missing before rename", old, ip)
            assert resolves(observer, alias, ip), ("alias missing before rename", alias, ip)
        request("POST", "/containers/" + peer + "/rename?name=" + new, statuses=(204,))
        for observer, ip, alias in observers:
            assert resolves(observer, new, ip), ("new name missing after rename", new, ip)
            assert resolves(observer, alias, ip), ("alias lost after rename", alias, ip)
            assert not resolves(observer, old, ip), ("old name still resolves", old, ip)
        assert not resolves(late, second_alias, second_ip), "second network alias leaked to first"
        assert not resolves(far, primary_alias, peer_ip), "first network alias leaked to second"
        print("PASS rename replaces the DNS name and preserves per-network aliases", flush=True)
        cases += 1
        print(cases, "network connect DNS scenarios passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(containers):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        for network in reversed(networks):
            try:
                request("DELETE", "/networks/" + network, statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove everything the network connect DNS test made") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
