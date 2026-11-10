#!/usr/bin/env python3
"""Check the cpu.weight a container gets for its CPU shares, at create and after an update."""
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
    owned = []

    def request(method, target, body=None, statuses=(200,)):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            if target == "/containers/create" and response.status == 201:
                owned.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            if "/archive?" in target:
                return data
            return json.loads(data) if data else None
        finally:
            connection.close()

    def run(host):
        cid = request("POST", "/containers/create",
                      {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": "none", **host}},
                      (201,))["Id"]
        request("POST", "/containers/" + cid + "/start", statuses=(204,))
        return cid

    def shares(cid):
        return request("GET", "/containers/" + cid + "/json")["HostConfig"]["CpuShares"]

    # The weight is read from a file the container writes, through the archive
    # API: that asks nothing of how an engine attaches to an exec.
    def weight(cid):
        target = "/tmp/weight-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "cat /sys/fs/cgroup/cpu.weight > " + target]}, (201,))["Id"]
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 10
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "the exec did not exit"
            time.sleep(.01)
        assert state["ExitCode"] == 0, state
        data = request("GET", "/containers/" + cid + "/archive?path=" + target)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            return int(archive.extractfile(archive.getmembers()[0]).read())

    cases = 0
    try:
        # A container that asks for nothing has the kernel's default weight,
        # and so does one that asks for Docker's default of 1024 shares.
        assert weight(run({})) == 100
        print("PASS no shares: weight 100", flush=True)
        cases += 1
        # Docker 29 (runc 1.3 and later). Docker 28 drew a straight line
        # between the ends, on which 1024 shares were a weight of 39.
        for given, expected in ((2, 1), (512, 59), (1024, 100), (2048, 174), (262144, 10000)):
            cid = run({"CpuShares": given})
            assert weight(cid) == expected, (given, weight(cid))
            # An engine that keeps the weight alone reads it back as shares
            # that give it: the ends and the default are the shares given.
            assert given == 512 or given == 2048 or shares(cid) == given, (given, shares(cid))
            print("PASS %d shares: weight %d" % (given, expected), flush=True)
            cases += 1
        for given, expected in ((4096, 303), (1024, 100), (2, 1)):
            request("POST", "/containers/" + cid + "/update", {"CpuShares": given})
            assert weight(cid) == expected, (given, weight(cid))
            assert given == 4096 or shares(cid) == given, (given, shares(cid))
            print("PASS update to %d shares: weight %d" % (given, expected), flush=True)
            cases += 1
        print(cases, "CPU shares cases passed")
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(owned):
            try:
                request("DELETE", "/containers/" + cid + "?force=true", statuses=(204,))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove all CPU shares test containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
