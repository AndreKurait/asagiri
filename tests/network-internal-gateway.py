#!/usr/bin/env python3
"""Check that endpoints of internal networks show no gateway, as the container has none."""
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
    tag = uuid.uuid4().hex[:8]

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

    def network(name, **settings):
        request("POST", "/networks/create", {"Name": tag + name, **settings}, (201,))
        return tag + name

    def run(net):
        cid = request("POST", "/containers/create", {"Image": image, "Cmd": ["sleep", "120"],
                                                     "HostConfig": {"NetworkMode": net}}, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def stop(cid):
        request("POST", "/containers/" + cid + "/stop?t=0", statuses=(204,))
        # The exit may be recorded a moment after the stop returns.
        request("POST", "/containers/" + cid + "/wait?condition=not-running")

    def endpoints(cid):
        return request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"]

    def row(cid):
        listed = request("GET", "/containers/json?all=1")
        return [c for c in listed if c["Id"] == cid][0]["NetworkSettings"]["Networks"]

    def output(cid, command):
        target = "/tmp/gateway-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        # Docker answers 200; Kiri answers 204 today.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 20
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "route probe exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + target, raw=True)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            text = archive.extractfile(archive.getmembers()[0]).read().decode()
        assert state["ExitCode"] == 0, (command, state["ExitCode"], text)
        return text

    def default_route(cid, family):
        routes = output(cid, "ip " + family + " route").splitlines()
        via = [line.split()[2] for line in routes if line.startswith("default via ")]
        assert len(via) <= 1, (family, routes)
        return via[0] if via else ""

    def gateways(endpoint):
        return endpoint["Gateway"], endpoint["IPv6Gateway"]

    passed = 0
    try:
        dual = {"EnableIPv6": True}
        closed = network("closed", Internal=True)
        closed6 = network("closed6", Internal=True, **dual,
                          IPAM={"Config": [{"Subnet": "172.31.240.0/24"}, {"Subnet": "fd42:77::/64"}]})
        chosen = network("chosen", Internal=True,
                         IPAM={"Config": [{"Subnet": "172.31.241.0/24", "Gateway": "172.31.241.9"}]})
        routed = network("routed", **dual,
                         IPAM={"Config": [{"Subnet": "172.31.242.0/24"}, {"Subnet": "fd42:78::/64"}]})

        # An internal network: the container has an address and no way out,
        # and its endpoint says so in inspect and in the list.
        for net, six in ((closed, False), (closed6, True), (chosen, False)):
            cid = run(net)
            for endpoint in (endpoints(cid)[net], row(cid)[net]):
                assert gateways(endpoint) == ("", ""), (net, endpoint)
                assert endpoint["IPAddress"], (net, endpoint)
                assert bool(endpoint["GlobalIPv6Address"]) == six, (net, endpoint)
            # Docker fills the old top-level fields for the default bridge only.
            top = request("GET", "/containers/" + cid + "/json")["NetworkSettings"]
            assert not top.get("Gateway") and not top.get("IPv6Gateway"), (net, top)
            inspected = endpoints(cid)[net]
            assert inspected["IPPrefixLen"] > 0 and (inspected["GlobalIPv6PrefixLen"] == 64) == six, (net, inspected)
            assert default_route(cid, "-4") == "" and default_route(cid, "-6") == "", net
            assert inspected["IPAddress"] in output(cid, "ip -4 addr show eth0"), net
            stop(cid)
            assert gateways(endpoints(cid)[net]) == ("", ""), net
            passed += 1
            print("PASS internal network", net[len(tag):])

        # A network with a way out keeps both gateways: the ones the
        # container routes through.
        cid = run(routed)
        for endpoint in (endpoints(cid)[routed], row(cid)[routed]):
            assert gateways(endpoint) == (default_route(cid, "-4"), default_route(cid, "-6")), endpoint
            assert endpoint["Gateway"].startswith("172.31.242.") and endpoint["IPv6Gateway"].startswith("fd42:78::"), endpoint
        passed += 1
        print("PASS routed network")

        # Joined to an internal network as well: that endpoint shows none,
        # the first keeps its own, and the way out stays where it was.
        before = gateways(endpoints(cid)[routed])
        # Docker answers 200; Kiri answers 204 today.
        request("POST", "/networks/" + closed6 + "/connect", {"Container": cid}, (200, 204))
        both = endpoints(cid)
        assert gateways(both[closed6]) == ("", "") and both[closed6]["IPAddress"] and both[closed6]["GlobalIPv6Address"], both
        assert gateways(both[routed]) == before, both
        assert (default_route(cid, "-4"), default_route(cid, "-6")) == before
        passed += 1
        print("PASS routed and internal network together")

        stop(cid)
        after = endpoints(cid)
        assert gateways(after[routed]) == ("", "") and gateways(after[closed6]) == ("", ""), after
        passed += 1
        print("PASS stopped")
    finally:
        # Every removal is tried: the subnets are fixed, and a network left
        # behind would be in the way of the next run.
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for target in ["/containers/" + cid + "?force=true" for cid in containers] + ["/networks/" + net for net in networks]:
            try:
                request("DELETE", target, statuses=(204, 404))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove everything the internal network gateway test made") from errors[0]
    print(passed, "internal network gateway scenarios passed")


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
