#!/usr/bin/env python3
"""Exercise dense IPAM, reservations, range exhaustion and retry on a Docker API."""
import http.client
import ipaddress
import json
import socket
import sys
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
    prefix = "ipam-" + uuid.uuid4().hex[:10]
    containers, networks = [], []
    results = []

    def request(method, target, body=None, statuses=(200,)):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            value = json.loads(data) if data else None
            if response.status == 201 and target == "/containers/create":
                containers.append(value["Id"])
            if response.status == 201 and target == "/networks/create":
                networks.append(value["Id"])
            assert response.status in statuses, (method, target, response.status, value)
            return value
        finally:
            connection.close()

    def network(suffix, config, ipv6=False):
        name = prefix + suffix
        request("POST", "/networks/create", {
            "Name": name, "Driver": "bridge", "EnableIPv6": ipv6,
            "Labels": {"asagiri.ipam-test": prefix}, "IPAM": {"Config": config},
        }, (201,))
        return name

    def create(net, address=None):
        body = {"Image": image, "Cmd": ["sleep", "300"], "Labels": {"asagiri.ipam-test": prefix},
                "HostConfig": {"NetworkMode": net}}
        if address:
            body["NetworkingConfig"] = {"EndpointsConfig": {net: {"IPAMConfig": {"IPv4Address": address}}}}
        return request("POST", "/containers/create", body, (201,))["Id"]

    def start(cid, net, v4, v6=None):
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        state = request("GET", "/containers/" + cid + "/json")
        assert state["State"]["Running"], state["State"]
        endpoint = state["NetworkSettings"]["Networks"][net]
        assert ipaddress.ip_address(endpoint["IPAddress"]) == ipaddress.ip_address(v4), endpoint
        if v6:
            assert ipaddress.ip_address(endpoint["GlobalIPv6Address"]) == ipaddress.ip_address(v6), endpoint

    def run(net, v4, v6=None, static=None):
        cid = create(net, static)
        start(cid, net, v4, v6)
        return cid

    def remove(cid):
        request("DELETE", "/containers/" + cid + "?force=1", statuses=(204, 404))

    def exhausted(cid):
        error = request("POST", "/containers/" + cid + "/start", statuses=(500,))
        assert error["message"], error
        state = request("GET", "/containers/" + cid + "/json")
        assert not state["State"]["Running"], state["State"]

    try:
        dual = network("-dense", [
            {"Subnet": "10.73.0.0/24", "Gateway": "10.73.0.1",
             "AuxiliaryAddresses": {"reserved-a": "10.73.0.33", "reserved-b": "10.73.0.34"}},
            {"Subnet": "fd54:77::/112", "Gateway": "fd54:77::1",
             "AuxiliaryAddresses": {"reserved-a": "fd54:77::21", "reserved-b": "fd54:77::22"}},
        ], ipv6=True)
        dense = [run(dual, "10.73.0." + str(host), "fd54:77::" + format(host, "x")) for host in range(2, 33)]
        run(dual, "10.73.0.35", "fd54:77::23")
        run(dual, "10.73.0.40", "fd54:77::24", static="10.73.0.40")
        for host, six in ((36, 37), (37, 38), (38, 39), (39, 40), (41, 41)):
            run(dual, "10.73.0." + str(host), "fd54:77::" + format(six, "x"))
        remove(dense[7])
        run(dual, "10.73.0.9", "fd54:77::9")
        results.append("dense dual-stack reservations, static IPv4 and lowest-address reuse")
        # Finish this scenario before creating the next pool.
        for cid in list(containers):
            remove(cid)

        ranged = network("-range", [{
            "Subnet": "10.73.1.0/24", "Gateway": "10.73.1.1", "IPRange": "10.73.1.128/27",
            "AuxiliaryAddresses": {"reserved-a": "10.73.1.130", "reserved-b": "10.73.1.131"},
        }])
        run(ranged, "10.73.1.10", static="10.73.1.10")
        leases = [run(ranged, "10.73.1." + str(host)) for host in [128, 129, *range(132, 160)]]
        retry = create(ranged)
        exhausted(retry)
        other = run(dual, "10.73.0.2", "fd54:77::2")
        request("POST", "/networks/" + ranged + "/connect", {"Container": other}, (500,))
        inspected = request("GET", "/containers/" + other + "/json")
        # Docker retains an empty requested-network entry on this failure.
        # Neither engine may leave an allocated address or live endpoint.
        failed_endpoint = inspected["NetworkSettings"]["Networks"].get(ranged, {})
        assert not any(failed_endpoint.get(key) for key in ("IPAddress", "GlobalIPv6Address", "EndpointID")), failed_endpoint
        remove(leases[0])
        request("POST", "/networks/" + ranged + "/connect", {"Container": other}, (200,))
        inspected = request("GET", "/containers/" + other + "/json")
        assert inspected["NetworkSettings"]["Networks"][ranged]["IPAddress"] == "10.73.1.128"
        request("POST", "/networks/" + ranged + "/disconnect", {"Container": other, "Force": True}, (200,))
        start(retry, ranged, "10.73.1.128")
        results.append("IP range includes its endpoints, excludes reservations, exhausts and recovers on start/connect")
        for cid in list(containers):
            remove(cid)

        tiny = network("-tiny", [
            {"Subnet": "10.73.2.0/29", "Gateway": "10.73.2.1"},
            {"Subnet": "fd54:78::/126", "Gateway": "fd54:78::1"},
        ], ipv6=True)
        first = run(tiny, "10.73.2.2", "fd54:78::2")
        run(tiny, "10.73.2.3", "fd54:78::3")
        retry = create(tiny)
        exhausted(retry)
        remove(first)
        start(retry, tiny, "10.73.2.2", "fd54:78::2")
        results.append("IPv6 last host is usable; IPv6 exhaustion and retry release both families")
        print(json.dumps({"cases": results, "status": "passed"}))
    finally:
        failures = []
        for cid in reversed(containers):
            try:
                remove(cid)
            except Exception as error:
                failures.append(str(error))
        for net in reversed(networks):
            try:
                request("DELETE", "/networks/" + net, statuses=(204, 404))
            except Exception as error:
                failures.append(str(error))
        assert not failures, "IPAM cleanup failed: " + "; ".join(failures)
    print("PASS dense IPv4/IPv6 allocation, static and auxiliary reservations, range exhaustion, rollback and reuse")


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        raise SystemExit("usage: network-allocation.py SOCKET [IMAGE]")
    check(sys.argv[1], sys.argv[2] if len(sys.argv) == 3 else "alpine:3.21")
