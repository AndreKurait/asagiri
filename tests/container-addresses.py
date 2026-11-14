#!/usr/bin/env python3
"""Check address-pool persistence and allocation on a disposable engine."""
import concurrent.futures
import http.client
import ipaddress
import json
import os
import socket
import sys
import uuid


def check(path):
    if os.environ.get("ASAGIRI_TEST_ENGINE") != "1":
        raise RuntimeError("Run only inside a disposable engine harness")

    def request(method, target, body=None, status=200):
        connection = http.client.HTTPConnection("localhost", timeout=20)
        connection.sock = socket.socket(socket.AF_UNIX)
        connection.sock.settimeout(20)
        connection.sock.connect(path)
        try:
            connection.request(method, target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status == status, (method, target, response.status, data)
            return json.loads(data) if data else None
        finally:
            connection.close()

    original = request("GET", "/asagiri/addresses")
    original_proxy = request("GET", "/asagiri/proxy")["settings"]
    host = [ipaddress.ip_network(item["net"]) for item in original["host"]]
    existing = request("GET", "/networks")
    taken = [ipaddress.ip_network(config["Subnet"]) for network in existing
             for config in network.get("IPAM", {}).get("Config", []) if ":" not in config.get("Subnet", "")]
    pool = next(ipaddress.ip_network(f"10.{n}.0.0/16") for n in range(200, 256)
                if not any(ipaddress.ip_network(f"10.{n}.0.0/16").overlaps(net) for net in host + taken))
    networks = []
    try:
        saved = request("PUT", "/asagiri/addresses", {"pool": str(pool)})
        assert saved["pool"] == str(pool) and saved["bridge"] == original["bridge"], saved
        first = request("POST", "/networks/create", {"Name": "pool-" + uuid.uuid4().hex}, 201)["Id"]
        networks.append(first)
        before = request("GET", "/networks/" + first)
        allocated = ipaddress.ip_network(before["IPAM"]["Config"][0]["Subnet"])
        assert allocated.subnet_of(pool) and allocated.prefixlen == 24, before
        request("PUT", "/asagiri/addresses", {"pool": original["default"]})
        assert request("GET", "/networks/" + first)["IPAM"] == before["IPAM"], "existing network changed with the pool"
        print("PASS new networks use the pool; existing networks and the live bridge keep their addresses", flush=True)

        for bad in ("8.8.0.0/16", "10.0.0.0/7", "10.200.0.0/25", "invalid"):
            request("PUT", "/asagiri/addresses", {"pool": bad}, 400)
        request("POST", "/asagiri/proxy", {"mode": "none", "pool": "8.8.0.0/16"}, 400)
        assert request("GET", "/asagiri/proxy")["settings"] == original_proxy
        assert request("GET", "/asagiri/addresses")["pool"] == original["default"]

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
            results = list(executor.map(lambda _: request("PUT", "/asagiri/addresses", {"pool": str(pool)}), range(32)))
        assert all(result["pool"] == str(pool) for result in results), results
        assert request("GET", "/asagiri/addresses")["pool"] == str(pool)
        print("PASS invalid pools change nothing; concurrent saves keep valid complete settings", flush=True)
    finally:
        try:
            for network in networks:
                request("DELETE", "/networks/" + network, status=204)
        finally:
            request("PUT", "/asagiri/addresses", {"pool": original["pool"]})


if __name__ == "__main__":
    check(sys.argv[1])
