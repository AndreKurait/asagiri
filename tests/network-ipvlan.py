#!/usr/bin/env python3
"""Check ipvlan networks without a parent: containers reach each other on one, and nothing else through it."""
import http.client
import io
import json
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
    tag = "kiv-" + uuid.uuid4().hex[:8] + "-"

    def request(method, target, body=None, statuses=(200,), raw=False):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            # Track a create even if the assertion below rejects its status.
            if response.status == 201 and target.startswith("/containers/create"):
                containers.append(json.loads(data)["Id"])
            if response.status == 201 and target == "/networks/create":
                networks.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            if raw:
                return data
            return json.loads(data) if data else None
        finally:
            connection.close()

    def network(name, subnets, **settings):
        request("POST", "/networks/create", {"Name": tag + name, "Driver": "ipvlan",
                                             "IPAM": {"Config": subnets}, **settings}, (201,))
        return tag + name

    def run(net, name, aliases=(), publish=False):
        body = {"Image": image, "Cmd": ["sleep", "600"], "HostConfig": {"NetworkMode": net}}
        if aliases:
            body["NetworkingConfig"] = {"EndpointsConfig": {net: {"Aliases": list(aliases)}}}
        if publish:
            body["ExposedPorts"] = {"80/tcp": {}}
            body["HostConfig"]["PortBindings"] = {"80/tcp": [{"HostIp": "127.0.0.1", "HostPort": ""}]}
        cid = request("POST", "/containers/create?name=" + tag + name, body, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def stop(cid):
        request("POST", "/containers/" + cid + "/stop?t=0", statuses=(204,))
        # The exit may be recorded a moment after the stop returns.
        request("POST", "/containers/" + cid + "/wait?condition=not-running")

    # Docker answers 200 to both; Asagiri answers 204 today.
    def join(net, cid):
        request("POST", "/networks/" + net + "/connect", {"Container": cid}, (200, 204))

    def leave(net, cid):
        request("POST", "/networks/" + net + "/disconnect", {"Container": cid}, (200, 204))

    def output(cid, command):
        target = "/tmp/ipvlan-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        # Docker answers 200; Asagiri answers 204 today.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 30
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "ipvlan probe exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + target, raw=True)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            text = archive.extractfile(archive.getmembers()[0]).read().decode()
        assert state["ExitCode"] == 0, (command, state["ExitCode"], text)
        return text

    # What inspect shows of the container on that network.
    def endpoint(cid, net):
        return request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"][net]

    # The IPv4 addresses on the container's interfaces, loopback aside.
    def addresses(cid):
        found = set()
        for line in output(cid, "ip -4 -o addr show").splitlines():
            address = line.split()[3].split("/")[0]
            if not address.startswith("127."):
                found.add(address)
        return found

    # The flags of the interface that holds the address, as `ip link`
    # shows them.
    def flags(cid, address):
        for line in output(cid, "ip -4 -o addr show").splitlines():
            if line.split()[3].split("/")[0] == address:
                shown = output(cid, "ip -o link show " + line.split()[1])
                return shown.split("<")[1].split(">")[0].split(",")
        raise AssertionError((address, "is on no interface"))

    # The container's default routes, IPv4 or IPv6.
    def ways_out(cid, family="-4"):
        return [line for line in output(cid, "ip " + family + " route").splitlines() if line.startswith("default")]

    # Whether a ping from the container gets an answer, by address or by
    # name. A new address may take a moment to answer, IPv6 above all.
    def reaches(cid, target, family="-4", tries=5):
        attempt = "ping " + family + " -c 1 -W 1 " + target + " >/dev/null 2>&1"
        loop = "for i in $(seq " + str(tries) + "); do " + attempt + " && echo yes && exit 0; sleep 1; done; echo no"
        return output(cid, loop).strip() == "yes"

    passed = 0
    try:
        # A network of its own subnet and no gateway: the first address is
        # a container's.
        plain = network("plain", [{"Subnet": "172.31.244.0/24"}])
        shown = request("GET", "/networks/" + plain)
        assert shown["Driver"] == "ipvlan" and shown["Internal"] is False, shown
        config = shown["IPAM"]["Config"]
        assert [c["Subnet"] for c in config] == ["172.31.244.0/24"] and not config[0].get("Gateway"), config
        one = run(plain, "one", aliases=["first"])
        two = run(plain, "two")
        assert endpoint(one, plain)["IPAddress"] == "172.31.244.1", endpoint(one, plain)
        assert endpoint(two, plain)["IPAddress"] == "172.31.244.2", endpoint(two, plain)
        assert addresses(one) == {"172.31.244.1"} and addresses(two) == {"172.31.244.2"}, (addresses(one), addresses(two))
        passed += 1
        print("PASS an ipvlan network gives its containers its addresses, from the first one", flush=True)

        # Containers on it reach each other, by address and by name.
        assert reaches(one, "172.31.244.2") and reaches(two, "172.31.244.1")
        assert "nameserver 127.0.0.11" in output(one, "cat /etc/resolv.conf")
        assert reaches(two, tag + "one") and reaches(two, "first") and reaches(one, tag + "two")
        passed += 1
        print("PASS its containers reach each other by address, name and alias", flush=True)

        # It is no way out: no gateway and no default route.
        assert endpoint(one, plain)["Gateway"] == "", endpoint(one, plain)
        assert ways_out(one) == [], ways_out(one)
        passed += 1
        print("PASS it has no gateway and gives no default route", flush=True)

        # A gateway that is asked for is shown and kept from containers,
        # and still no way out.
        gated = network("gated", [{"Subnet": "172.31.245.0/24", "Gateway": "172.31.245.1"}])
        config = request("GET", "/networks/" + gated)["IPAM"]["Config"]
        assert config[0].get("Gateway") == "172.31.245.1", config
        behind = run(gated, "behind")
        assert endpoint(behind, gated)["IPAddress"] == "172.31.245.2", endpoint(behind, gated)
        assert endpoint(behind, gated)["Gateway"] == "", endpoint(behind, gated)
        assert ways_out(behind) == [], ways_out(behind)
        passed += 1
        print("PASS a gateway that is asked for is kept from containers and is no way out", flush=True)

        # Ports are not published from it: the container runs with none.
        ported = run(plain, "ported", publish=True)
        state = request("GET", "/containers/" + ported + "/json")
        assert state["State"]["Running"] is True, state["State"]
        assert not any(state["NetworkSettings"]["Ports"].values()), state["NetworkSettings"]["Ports"]
        passed += 1
        print("PASS a published port is left out", flush=True)

        # A running container joins and leaves: it keeps going out the way
        # it did, and the address goes with the network.
        late = run("bridge", "late")
        before = ways_out(late)
        assert len(before) == 1, before
        join(plain, late)
        address = endpoint(late, plain)["IPAddress"]
        assert address.startswith("172.31.244.") and address in addresses(late), (address, addresses(late))
        assert ways_out(late) == before, (ways_out(late), before)
        assert reaches(one, address) and reaches(late, "172.31.244.1") and reaches(one, tag + "late")
        leave(plain, late)
        assert address not in addresses(late), addresses(late)
        assert not reaches(one, address, tries=2)
        join(plain, late)
        address = endpoint(late, plain)["IPAddress"]
        assert address in addresses(late) and reaches(one, address), (address, addresses(late))
        passed += 1
        print("PASS a running container joins, leaves and joins again", flush=True)

        # A stopped container is off the network, and back when it starts.
        address = endpoint(one, plain)["IPAddress"]
        assert reaches(two, address)
        stop(one)
        assert not reaches(two, address, tries=2)
        request("POST", "/containers/" + one + "/start", statuses=(204,))
        address = endpoint(one, plain)["IPAddress"]
        assert addresses(one) == {address}, (address, addresses(one))
        assert reaches(two, address) and reaches(one, "172.31.244.2") and reaches(two, tag + "one")
        passed += 1
        print("PASS a stopped container is off the network, and back on it when it starts", flush=True)

        # With the private flag containers do not reach each other.
        private = network("private", [{"Subnet": "172.31.246.0/24"}], Options={"ipvlan_flag": "private"})
        assert request("GET", "/networks/" + private)["Options"].get("ipvlan_flag") == "private"
        alone = run(private, "alone")
        apart = run(private, "apart")
        assert addresses(alone) == {"172.31.246.1"} and addresses(apart) == {"172.31.246.2"}
        assert not reaches(alone, "172.31.246.2", tries=2) and not reaches(apart, "172.31.246.1", tries=2)
        passed += 1
        print("PASS the private flag keeps containers apart", flush=True)

        # IPv6 the same way: addresses, no gateway, no default route.
        six = network("six", [{"Subnet": "172.31.247.0/24"}, {"Subnet": "fd42:7d::/64"}], EnableIPv6=True)
        config = request("GET", "/networks/" + six)["IPAM"]["Config"]
        assert sorted(c["Subnet"] for c in config) == ["172.31.247.0/24", "fd42:7d::/64"], config
        assert not any(c.get("Gateway") for c in config), config
        left = run(six, "left")
        right = run(six, "right")
        there = endpoint(right, six)
        assert there["GlobalIPv6Address"].startswith("fd42:7d::") and there["IPv6Gateway"] == "", there
        assert ways_out(left, "-6") == [] and ways_out(left) == [], (ways_out(left, "-6"), ways_out(left))
        assert reaches(left, there["GlobalIPv6Address"], "-6", tries=8)
        passed += 1
        print("PASS IPv6 on an ipvlan network", flush=True)

        # In l3 mode containers reach each other too, by address and by
        # name, and there is still no way out. The kernel answers no ARP
        # on an l3 link, which is how the mode shows in the container.
        routed = network("routed", [{"Subnet": "172.31.248.0/24"}], Options={"ipvlan_mode": "l3"})
        assert request("GET", "/networks/" + routed)["Options"].get("ipvlan_mode") == "l3"
        near = run(routed, "near")
        far = run(routed, "far")
        assert addresses(near) == {"172.31.248.1"} and addresses(far) == {"172.31.248.2"}, (addresses(near), addresses(far))
        assert reaches(near, "172.31.248.2") and reaches(far, "172.31.248.1") and reaches(near, tag + "far")
        assert ways_out(near) == [], ways_out(near)
        assert "NOARP" in flags(near, "172.31.248.1"), flags(near, "172.31.248.1")
        assert "NOARP" not in flags(two, "172.31.244.2"), flags(two, "172.31.244.2")
        passed += 1
        print("PASS l3 mode", flush=True)

        # An ipvlan link has its parent's MAC and takes no other: a
        # container that asks for one does not start. A running container
        # that joins with one is on the network without it.
        body = {"Image": image, "Cmd": ["sleep", "600"], "HostConfig": {"NetworkMode": plain},
                "NetworkingConfig": {"EndpointsConfig": {plain: {"MacAddress": "02:42:ac:1f:f4:63"}}}}
        named = request("POST", "/containers/create?name=" + tag + "named", body, (201,))["Id"]
        refused = request("POST", "/containers/" + named + "/start", statuses=(400, 500))
        assert "ipvlan interfaces do not support custom mac address assignment" in refused["message"], refused
        assert request("GET", "/containers/" + named + "/json")["State"]["Running"] is False
        lone = run("bridge", "lone")
        request("POST", "/networks/" + plain + "/connect",
                {"Container": lone, "EndpointConfig": {"MacAddress": "02:42:ac:1f:f4:64"}}, (200, 204))
        address = endpoint(lone, plain)["IPAddress"]
        assert address in addresses(lone) and reaches(two, address), (address, addresses(lone))
        assert "02:42:ac:1f:f4:64" not in output(lone, "ip -o link show"), output(lone, "ip -o link show")
        passed += 1
        print("PASS a MAC address that is asked for is refused at start and not set on a join", flush=True)

        # A mode or a flag the driver does not know is refused, and nothing
        # is made.
        for option, words in (("ipvlan_mode", "requested ipvlan mode 'bogus' is not valid"),
                              ("ipvlan_flag", "requested ipvlan flag 'bogus' is not valid")):
            refused = request("POST", "/networks/create", {"Name": tag + "bogus", "Driver": "ipvlan",
                                                           "Options": {option: "bogus"}}, (400, 500))
            assert words in refused["message"], refused
            request("GET", "/networks/" + tag + "bogus", statuses=(404,))
        passed += 1
        print("PASS an unknown mode and an unknown flag are refused", flush=True)
        print(passed, "ipvlan scenarios passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(containers):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        for net in reversed(networks):
            try:
                request("DELETE", "/networks/" + net, statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove everything the ipvlan test made") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
