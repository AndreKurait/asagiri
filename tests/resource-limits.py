#!/usr/bin/env python3
"""Check cgroup enforcement, rejected updates, and concurrent field preservation."""
import concurrent.futures
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
            # Track a create even if the assertion below rejects its status.
            if target == "/containers/create" and response.status == 201:
                owned.append(json.loads(data)["Id"])
            assert response.status in statuses, (method, target, response.status, data)
            if "/archive?" in target:
                return data
            return json.loads(data) if data else None
        finally:
            connection.close()

    def create(host):
        return request("POST", "/containers/create",
                       {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": "none", **host}},
                       (201,))["Id"]

    def start(cid):
        request("POST", "/containers/" + cid + "/start", statuses=(204,))

    def inspect(cid):
        return request("GET", "/containers/" + cid + "/json")["HostConfig"]

    def update(cid, values, statuses=(200,)):
        return request("POST", "/containers/" + cid + "/update", values, statuses)

    def capture(cid, command):
        target = "/tmp/limits-" + uuid.uuid4().hex
        eid = request("POST", "/containers/" + cid + "/exec",
                      {"Cmd": ["sh", "-c", "(" + command + ") > " + target + " 2>&1"]}, (201,))["Id"]
        # Upstream's separate empty-response fix changes this 204 to Docker's 200.
        request("POST", "/exec/" + eid + "/start", {"Detach": True}, (200, 204))
        deadline = time.monotonic() + 10
        while True:
            state = request("GET", "/exec/" + eid + "/json")
            if not state["Running"]:
                break
            assert time.monotonic() < deadline, "resource probe exec did not exit"
            time.sleep(.01)
        assert state["ExitCode"] == 0, state
        data = request("GET", "/containers/" + cid + "/archive?path=" + target)
        with tarfile.open(fileobj=io.BytesIO(data)) as archive:
            return archive.extractfile(archive.getmembers()[0]).read().decode().splitlines()

    files = ("memory.max", "memory.swap.max", "memory.low", "cpuset.cpus", "cpuset.mems", "pids.max", "cpu.max")
    command = "cat " + " ".join("/sys/fs/cgroup/" + item for item in files)
    try:
        probe = create({})
        start(probe)
        effective = capture(probe, "cat /sys/fs/cgroup/cpuset.cpus.effective /sys/fs/cgroup/cpuset.mems.effective")
        cpu, mem = (value.split(",")[0].split("-")[0] for value in effective)
        cid = create({"Memory": 67108864, "MemorySwap": 100663296, "MemoryReservation": 16777216,
                      "CpusetCpus": cpu, "CpusetMems": mem, "PidsLimit": 32, "CpuQuota": 20000, "CpuPeriod": 100000})
        start(cid)
        assert capture(cid, command) == ["67108864", "33554432", "16777216", cpu, mem, "32", "20000 100000"]
        print("PASS create enforces all resource fields", flush=True)

        update(cid, {"MemorySwap": 134217728, "MemoryReservation": 8388608, "PidsLimit": -1, "CpuQuota": -1})
        expected = ["67108864", "67108864", "8388608", cpu, mem, "max", "max 100000"]
        assert capture(cid, command) == expected
        print("PASS partial update and removal of CPU/PID limits", flush=True)

        before = inspect(cid)
        for invalid in ({"MemorySwap": 1}, {"MemoryReservation": 1}, {"Memory": -1},
                        {"CpuPeriod": 1}, {"CpuShares": -1}, {"CpusetCpus": "3-1"},
                        {"NanoCpus": 1000000000, "CpuQuota": 10000}):
            # Docker 28 reports some merged-limit failures from runc as 500;
            # Kiri can reject the same combination before contacting the guest.
            error = update(cid, invalid, (400, 500))
            assert error["message"], error
            assert inspect(cid) == before, invalid
        assert capture(cid, command) == expected
        print("PASS invalid updates preserve saved and kernel limits", flush=True)

        # Reject after another controller could have changed. Check the kernel,
        # rather than trusting inspect to show a successful rollback.
        error = update(cid, {"Memory": 50331648, "CpusetCpus": "999999"}, (400, 500))
        assert error["message"], error
        assert inspect(cid) == before
        assert capture(cid, command) == expected
        print("PASS rejected CPU-set update restores other controllers", flush=True)

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            for _ in range(5):
                update(cid, {"MemoryReservation": 8388608, "PidsLimit": 16})
                futures = [pool.submit(update, cid, {"MemoryReservation": 16777216}),
                           pool.submit(update, cid, {"PidsLimit": 64})]
                for future in futures:
                    future.result()
                config = inspect(cid)
                assert config["MemoryReservation"] == 16777216 and config["PidsLimit"] == 64, config
        assert capture(cid, command)[2:6] == ["16777216", cpu, mem, "64"]
        print("PASS concurrent updates preserve independent fields", flush=True)

        default_swap = create({"Memory": 67108864})
        assert inspect(default_swap)["MemorySwap"] == 134217728
        start(default_swap)
        assert capture(default_swap, "cat /sys/fs/cgroup/memory.swap.max") == ["67108864"]
        unlimited_swap = create({"Memory": 67108864, "MemorySwap": -1})
        start(unlimited_swap)
        assert capture(unlimited_swap, "cat /sys/fs/cgroup/memory.swap.max") == ["max"]
        print("PASS default and unlimited swap", flush=True)

        stopped = create({"Memory": 67108864, "PidsLimit": 32, "CpuQuota": 20000, "CpuPeriod": 100000})
        update(stopped, {"MemorySwap": 100663296, "PidsLimit": 0, "CpuQuota": -1})
        start(stopped)
        assert capture(stopped, "cat /sys/fs/cgroup/memory.swap.max /sys/fs/cgroup/pids.max /sys/fs/cgroup/cpu.max") == [
            "33554432", "max", "max 100000"]
        print("PASS stopped-container updates apply at start", flush=True)

        for invalid in ({"MemoryReservation": 1}, {"MemorySwap": 1},
                        {"Memory": 67108864, "MemoryReservation": 134217728},
                        {"NanoCpus": 9223372036854775807}):
            error = request("POST", "/containers/create",
                            {"Image": image, "Cmd": ["true"], "HostConfig": invalid}, (400,))
            assert error["message"], error
        print("PASS invalid creates", flush=True)
        print("8 resource control scenarios passed")
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
            raise RuntimeError("Could not remove all resource-test containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")

