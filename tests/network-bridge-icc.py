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

    def network(name, icc=None, statuses=(201,)):
        body = {"Name": name}
        if icc is not None:
            body["Options"] = {ICC: icc}
        return request("POST", "/networks/create", body, statuses)

    def run(name, mode="bridge"):
        body = {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": mode}}
        cid = request("POST", "/containers/create?name=" + name, body, (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def connect(net, cid):
        request("POST", "/networks/" + net + "/connect", {"Container": cid}, (200, 204))

    def endpoint(cid, net):
        return request("GET", "/containers/" + cid + "/json")["NetworkSettings"]["Networks"][net]

    def ping(cid, target):
        """Whether `target` answers, and what ping printed."""
        output = "/tmp/icc-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "ping -c 1 -W 2 " + target + " > " + output + " 2>&1"]}, (201,))["Id"]
        # Upstream's separate empty-response fix changes this 204 to Docker's 200.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 20
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "ping exec did not exit"
            time.sleep(.01)
        data = request("GET", "/containers/" + cid + "/archive?path=" + output)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            lines = archive.extractfile(archive.getmembers()[0]).read().decode().splitlines()
        return state["ExitCode"] == 0, lines

    def reaches(cid, target):
        return ping(cid, target)[0]

    try:
        apart, shared = "icc-apart-" + tag, "icc-shared-" + tag
        network(apart, "false")
        assert request("GET", "/networks/" + apart)["Options"][ICC] == "false"
        one, two = run("icc-one-" + tag, apart), run("icc-two-" + tag, apart)
        first, second = endpoint(one, apart), endpoint(two, apart)
        assert not reaches(one, second["IPAddress"]) and not reaches(two, first["IPAddress"])
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
        assert not reaches(one, second["IPAddress"])
        print("PASS a second network they share connects them, and only over itself", flush=True)

        late = run("icc-late-" + tag, shared)
        connect(apart, late)
        joined = endpoint(late, apart)["IPAddress"]
        assert not reaches(late, first["IPAddress"]) and not reaches(one, joined)
        assert reaches(late, endpoint(one, shared)["IPAddress"])
        print("PASS a container connected later is kept apart too", flush=True)

        # Go's ParseBool spellings, as Docker reads the option.
        for value, expected in (("0", False), ("F", False), ("true", True), ("1", True)):
            name = "icc-" + value.lower() + "-" + tag
            network(name, value)
            left, right = run(name + "-left", name), run(name + "-right", name)
            assert reaches(left, endpoint(right, name)["IPAddress"]) == expected, (value, expected)
        print("PASS the option takes Go's boolean spellings, and on leaves containers connected", flush=True)

        bad = "icc-bad-" + tag
        for value in ("maybe", "", "no"):
            error = network(bad, value, (400,))["message"]
            assert ICC in error, error
        request("GET", "/networks/" + bad, statuses=(404,))
        print("PASS a value that is not a boolean is refused and leaves no network behind", flush=True)
        print("6 bridge ICC scenarios passed")
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
