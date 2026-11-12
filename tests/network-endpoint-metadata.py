#!/usr/bin/env python3
"""Check that inspect describes each network endpoint as the container sees it."""
import http.client
import io
import json
import re
import socket
import sys
import tarfile
import time
import urllib.parse
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

    def request(method, target, body=None, statuses=(200,)):
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
            if "/archive?" in target:
                return data
            return json.loads(data) if data else None
        finally:
            connection.close()

    def network(name):
        return request("POST", "/networks/create", {"Name": name}, (201,))["Id"]

    def create(name, mode="bridge", endpoint=None, **config):
        body = {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": mode}, **config}
        if endpoint is not None:
            body["NetworkingConfig"] = {"EndpointsConfig": {mode: endpoint}}
        return request("POST", "/containers/create?name=" + name, body, (201,))["Id"]

    def run(name, mode="bridge", endpoint=None, **config):
        cid = create(name, mode, endpoint, **config)
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def stop(cid):
        request("POST", "/containers/" + cid + "/stop?t=0", statuses=(204,))
        # The exit may be recorded a moment after the stop returns.
        request("POST", "/containers/" + cid + "/wait?condition=not-running")

    def endpoints(cid):
        return request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"]

    def members(net):
        return request("GET", "/networks/" + net)["Containers"]

    def capture(cid, command):
        target = "/tmp/endpoint-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        # Docker answers 200; Asagiri answers 204 today.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 20
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "endpoint probe exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + target)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            lines = archive.extractfile(archive.getmembers()[0]).read().decode().splitlines()
        assert state["ExitCode"] == 0, (command, state["ExitCode"], lines)
        return lines

    def interfaces(cid):
        """The MAC address of each interface in the container, by its IPv4 address."""
        lines = capture(cid, "for i in /sys/class/net/eth*; do "
                             "echo \"$(ip -o -4 addr show dev ${i##*/} | awk '{print $4}') $(cat $i/address)\"; done")
        return {address.split("/")[0]: mac for address, mac in (line.split() for line in lines)}

    def described(cid, name, endpoint, net):
        """An endpoint's id and MAC address are real, and the network agrees."""
        mac = interfaces(cid)[endpoint["IPAddress"]]
        assert endpoint["MacAddress"] == mac, (name, endpoint["MacAddress"], mac)
        assert re.fullmatch("[0-9a-f]{64}", endpoint["EndpointID"]), (name, endpoint["EndpointID"])
        assert endpoint["EndpointID"] != cid, name
        member = members(net)[cid]
        assert member["EndpointID"] == endpoint["EndpointID"], (name, member)
        assert member["MacAddress"] == mac, (name, member)

    try:
        cid = run("meta-default-" + tag)
        inspected = request("GET", "/containers/" + cid + "/json")["NetworkSettings"]
        endpoint = inspected["Networks"]["bridge"]
        described(cid, "bridge", endpoint, "bridge")
        # The fields from before per-network endpoints describe the default bridge.
        assert inspected["MacAddress"] == endpoint["MacAddress"], inspected["MacAddress"]
        assert inspected["EndpointID"] == endpoint["EndpointID"], inspected["EndpointID"]
        assert not endpoint["DNSNames"], endpoint["DNSNames"]
        print("PASS default bridge endpoint has its MAC address and an endpoint id", flush=True)

        front, back = "meta-front-" + tag, "meta-back-" + tag
        front_id, back_id = network(front), network(back)
        alias, other, host = "meta-alias-" + tag, "meta-second-" + tag, "meta-host-" + tag
        web = run("meta-web-" + tag, front, {"Aliases": [alias]}, Hostname=host)
        request("POST", "/networks/" + back_id + "/connect", {"Container": web, "EndpointConfig": {"Aliases": [other]}}, (200, 204))
        found = endpoints(web)
        assert sorted(found) == sorted([front, back]), sorted(found)
        described(web, "front", found[front], front_id)
        described(web, "back", found[back], back_id)
        assert found[front]["EndpointID"] != found[back]["EndpointID"]
        assert found[front]["MacAddress"] != found[back]["MacAddress"]
        assert found[front]["NetworkID"] == front_id and found[back]["NetworkID"] == back_id
        print("PASS each network of a container has its own endpoint id and MAC address", flush=True)

        query = urllib.parse.quote(json.dumps({"id": [web]}))
        row = request("GET", "/containers/json?filters=" + query)[0]["NetworkSettings"]["Networks"]
        for name, endpoint in found.items():
            assert row[name]["EndpointID"] == endpoint["EndpointID"], (name, row[name])
            assert row[name]["MacAddress"] == endpoint["MacAddress"], (name, row[name])
        print("PASS container list rows carry the same endpoint ids and MAC addresses", flush=True)

        names = ["meta-web-" + tag, web[:12], host]
        assert sorted(found[front]["DNSNames"]) == sorted(names + [alias]), found[front]["DNSNames"]
        assert sorted(found[back]["DNSNames"]) == sorted(names + [other]), found[back]["DNSNames"]
        # Every listed name resolves from another container on that network.
        for net in (front, back):
            peer = run("meta-peer-" + net, net)
            # Its hostname is its short id, listed once.
            assert sorted(endpoints(peer)[net]["DNSNames"]) == sorted(["meta-peer-" + net, peer[:12]])
            for name in found[net]["DNSNames"]:
                capture(peer, "ping -c 2 -W 3 " + name)
        print("PASS DNS names list the name, aliases, short id and hostname, and all resolve", flush=True)

        wanted = "02:42:0A:E7:29:63"
        fixed = run("meta-mac-" + tag, front, {"MacAddress": wanted})
        endpoint = endpoints(fixed)[front]
        assert endpoint["MacAddress"] == wanted.lower(), endpoint["MacAddress"]
        described(fixed, "requested", endpoint, front_id)
        print("PASS a requested MAC address is the one reported and used", flush=True)

        stop(fixed)
        endpoint = endpoints(fixed)[front]
        assert endpoint["EndpointID"] == "" and endpoint["IPAddress"] == "", endpoint
        # Docker leaves it out of the network's containers; listed, it has none there either.
        assert members(front).get(fixed, {"EndpointID": ""})["EndpointID"] == "", members(front)[fixed]
        stop(cid)
        endpoint = endpoints(cid)["bridge"]
        assert endpoint["EndpointID"] == "" and endpoint["MacAddress"] == "", endpoint
        print("PASS a stopped container has no endpoint id", flush=True)

        fresh = create("meta-new-" + tag, front)
        endpoint = endpoints(fresh)[front]
        assert endpoint["DNSNames"] is None and endpoint["EndpointID"] == "", endpoint
        assert members(front).get(fresh, {"EndpointID": ""})["EndpointID"] == "", members(front)[fresh]
        names = sorted(["meta-new-" + tag, fresh[:12]])
        # A network connected before the start lists them at once.
        request("POST", "/networks/" + back_id + "/connect", {"Container": fresh}, (200, 204))
        found = endpoints(fresh)
        assert found[front]["DNSNames"] is None and sorted(found[back]["DNSNames"]) == names, found
        request("POST", "/containers/" + fresh + "/start", statuses=(204,))
        assert sorted(endpoints(fresh)[front]["DNSNames"]) == names, endpoints(fresh)[front]
        stop(fresh)
        assert sorted(endpoints(fresh)[front]["DNSNames"]) == names, endpoints(fresh)[front]
        print("PASS DNS names of the network a container was created on are listed from its first start on", flush=True)
        print("7 endpoint metadata scenarios passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for kind, items in (("containers", containers), ("networks", networks)):
            for item in reversed(items):
                try:
                    request("DELETE", "/" + kind + "/" + item + ("?force=true" if kind == "containers" else ""),
                            statuses=(204,))
                except Exception as error:
                    errors.append(error)
                    print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove all endpoint-test containers and networks") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
