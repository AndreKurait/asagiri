#!/usr/bin/env python3
"""Check that a bridge network with enable_icc off keeps its containers apart, and only those."""
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

    ICC = "com.docker.network.bridge.enable_icc"

    def network(name, icc=None, statuses=(201,), **settings):
        body = {"Name": name, **settings}
        if icc is not None:
            body["Options"] = {ICC: icc}
        return request("POST", "/networks/create", body, statuses)

    def run(name, mode="bridge", caps=()):
        body = {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": mode, "CapAdd": list(caps)}}
        cid = request("POST", "/containers/create?name=" + name, body, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def connect(net, cid):
        request("POST", "/networks/" + net + "/connect", {"Container": cid}, (200, 204))

    def endpoint(cid, net):
        return request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"][net]

    def shell(cid, command):
        """Whether `command` succeeded in the container, and what it printed."""
        output = "/tmp/icc-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "{ " + command + "; } > " + output + " 2>&1"]}, (201,))["Id"]
        # Docker answers 200; Kiri answers 204 today.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 20
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "exec did not exit: " + command
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + output)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            lines = archive.extractfile(archive.getmembers()[0]).read().decode().splitlines()
        return state["ExitCode"] == 0, lines

    def ping(cid, target):
        """Whether `target` answers one ping within a second, and what ping printed."""
        return shell(cid, "ping -c 1 -W 1 " + target)

    def reaches(cid, target):
        # A second try, so one packet lost right after a start or a connect is not a failure.
        return shell(cid, "ping -c 1 -W 2 " + target + " || ping -c 1 -W 2 " + target)[0]

    def blocked(cid, target):
        return not ping(cid, target)[0]

    try:
        apart, shared = "icc-apart-" + tag, "icc-shared-" + tag
        network(apart, "false")
        assert request("GET", "/networks/" + apart)["Options"][ICC] == "false"
        one, two = run("icc-one-" + tag, apart), run("icc-two-" + tag, apart)
        first, second = endpoint(one, apart), endpoint(two, apart)
        assert blocked(one, second["IPAddress"]) and blocked(two, first["IPAddress"])
        # The name still resolves: only the traffic is kept apart.
        answered, lines = ping(one, "icc-two-" + tag)
        assert not answered and second["IPAddress"] in lines[0], lines
        print("PASS containers on a network with enable_icc off do not reach each other", flush=True)

        assert reaches(one, first["Gateway"]) and reaches(two, second["Gateway"])
        assert reaches(one, first["IPAddress"])
        print("PASS they still reach the gateway and themselves", flush=True)

        network(shared)
        connect(shared, one)
        connect(shared, two)
        assert reaches(one, endpoint(two, shared)["IPAddress"]) and reaches(two, endpoint(one, shared)["IPAddress"])
        assert blocked(one, second["IPAddress"])
        print("PASS a second network they share connects them, and only over itself", flush=True)

        late = run("icc-late-" + tag, shared)
        connect(apart, late)
        joined = endpoint(late, apart)["IPAddress"]
        assert blocked(late, first["IPAddress"]) and blocked(one, joined)
        assert reaches(late, endpoint(one, shared)["IPAddress"])
        print("PASS a container connected later is kept apart too", flush=True)

        request("POST", "/containers/" + two + "/restart?t=0", statuses=(204,))
        again = endpoint(two, apart)["IPAddress"]
        assert blocked(one, again) and blocked(two, first["IPAddress"])
        assert reaches(two, endpoint(one, shared)["IPAddress"])
        print("PASS a restarted container is kept apart too", flush=True)

        # Sent to the gateway instead of the peer, a packet is routed back
        # into the network rather than switched within it: that is no way
        # round either, even with both ends set up for it.
        east, west = run("icc-east-" + tag, apart, ["NET_ADMIN"]), run("icc-west-" + tag, apart, ["NET_ADMIN"])
        for cid, peer in ((east, west), (west, east)):
            address, gateway = endpoint(peer, apart)["IPAddress"], endpoint(cid, apart)["Gateway"]
            routed, lines = shell(cid, "ip route add " + address + "/32 via " + gateway + " && ip route get " + address)
            assert routed and "via " + gateway in lines[0], lines
        assert blocked(east, endpoint(west, apart)["IPAddress"]) and blocked(west, endpoint(east, apart)["IPAddress"])
        assert reaches(east, endpoint(east, apart)["Gateway"])
        print("PASS a route through the gateway does not connect them", flush=True)

        six = "icc-six-" + tag
        network(six, "false", EnableIPv6=True)
        north, south = run("icc-north-" + tag, six), run("icc-south-" + tag, six)
        assert reaches(north, endpoint(north, six)["IPv6Gateway"]) and reaches(south, endpoint(south, six)["IPv6Gateway"])
        assert blocked(north, endpoint(south, six)["GlobalIPv6Address"]) and blocked(south, endpoint(north, six)["GlobalIPv6Address"])
        print("PASS IPv6 is kept apart as IPv4 is", flush=True)

        # Go's ParseBool spellings, as Docker reads the option.
        for value, expected in (("0", False), ("F", False), ("true", True), ("1", True)):
            name = "icc-" + value.lower() + "-" + tag
            network(name, value)
            left, right = run(name + "-left", name), run(name + "-right", name)
            address = endpoint(right, name)["IPAddress"]
            assert reaches(left, address) if expected else blocked(left, address), (value, expected)
        print("PASS the option takes Go's boolean spellings, and on leaves containers connected", flush=True)

        bad = "icc-bad-" + tag
        for value in ("maybe", "", "no"):
            error = network(bad, value, (400,))["message"]
            assert ICC in error, error
        request("GET", "/networks/" + bad, statuses=(404,))
        print("PASS a value that is not a boolean is refused and leaves no network behind", flush=True)
        print("9 bridge ICC scenarios passed")
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
            raise RuntimeError("Could not remove all ICC-test containers and networks") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
