#!/usr/bin/env python3
"""Check that a bridge network's MTU option reaches its containers, and that a bad one is refused."""
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

    MTU = "com.docker.network.driver.mtu"

    def network(name, mtu=None, statuses=(201,), **settings):
        body = {"Name": name, **settings}
        if mtu is not None:
            body["Options"] = {MTU: mtu}
        return request("POST", "/networks/create", body, statuses)

    def run(name, mode="bridge"):
        body = {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": mode}}
        cid = request("POST", "/containers/create?name=" + name, body, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def capture(cid, command):
        target = "/tmp/mtu-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        # Upstream's separate empty-response fix changes this 204 to Docker's 200.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 20
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "MTU probe exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + target)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            lines = archive.extractfile(archive.getmembers()[0]).read().decode().splitlines()
        assert state["ExitCode"] == 0, (command, state["ExitCode"], lines)
        return lines

    def mtu(cid, interface="eth0"):
        return capture(cid, "cat /sys/class/net/" + interface + "/mtu")[0]

    try:
        small = "mtu-small-" + tag
        network(small, "1400")
        assert request("GET", "/networks/" + small)["Options"][MTU] == "1400"
        one, two = run("mtu-one-" + tag, small), run("mtu-two-" + tag, small)
        assert mtu(one) == "1400" and mtu(two) == "1400", (mtu(one), mtu(two))
        # Larger packets still arrive, in fragments, also through the bridge's own address.
        gateway = request("GET", "/containers/" + one + "/json")["NetworkSettings"]["Networks"][small]["Gateway"]
        capture(one, "ping -c 1 -W 3 -s 3000 mtu-two-" + tag + " && ping -c 1 -W 3 -s 3000 " + gateway)
        print("PASS a network's MTU option is the MTU of its containers' interfaces", flush=True)

        jumbo = "mtu-jumbo-" + tag
        network(jumbo, "9000")
        big, peer = run("mtu-big-" + tag, jumbo), run("mtu-peer-" + tag, jumbo)
        assert mtu(big) == "9000" and mtu(peer) == "9000", (mtu(big), mtu(peer))
        capture(big, "ping -c 1 -W 3 -s 8000 mtu-peer-" + tag)
        print("PASS an MTU above 1500 works between containers of that network", flush=True)

        late = run("mtu-late-" + tag)
        request("POST", "/networks/" + small + "/connect", {"Container": late}, (200, 204))
        assert mtu(late, "eth1") == "1400" and mtu(late, "eth0") == "1500", (mtu(late, "eth1"), mtu(late, "eth0"))
        capture(late, "ping -c 1 -W 3 " + request("GET", "/containers/" + one + "/json")["NetworkSettings"]["Networks"][small]["IPAddress"])
        print("PASS a container connected later gets the MTU on that interface only", flush=True)

        default = "mtu-default-" + tag
        network(default, "0")
        assert mtu(run("mtu-zero-" + tag, default)) == "1500"
        print("PASS an MTU of 0 leaves the default", flush=True)

        six = "mtu-six-" + tag
        network(six, "1280", EnableIPv6=True)
        assert mtu(run("mtu-v6-" + tag, six)) == "1280"
        print("PASS an IPv6 network takes IPv6's smallest MTU", flush=True)

        bad = "mtu-bad-" + tag
        error = network(bad, "abc", (400,))["message"]
        assert MTU in error and "abc" in error, error
        error = network(bad, "-5", (400,))["message"]
        assert "invalid MTU number: -5" in error, error
        # Docker fails these when it sets the bridge up, with a 500.
        for value in ("", "10", "67", "65536"):
            network(bad, value, (400, 500))
        network(bad, "1000", (400, 500), EnableIPv6=True)
        request("GET", "/networks/" + bad, statuses=(404,))
        print("PASS a bad MTU is refused and leaves no network behind", flush=True)
        print("6 bridge MTU scenarios passed")
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
            raise RuntimeError("Could not remove all MTU-test containers and networks") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
