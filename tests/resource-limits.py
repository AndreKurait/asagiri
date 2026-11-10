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
        assert inspect(cid)["OomKillDisable"] is False
        start(cid)
        # Docker clears this nullable field at start when the host cannot
        # disable the OOM killer (cgroup v2). Both forms mean it stays enabled.
        assert inspect(cid)["OomKillDisable"] in (None, False)
        assert capture(cid, command) == ["67108864", "33554432", "16777216", cpu, mem, "32", "20000 100000"]
        print("PASS create enforces all resource fields", flush=True)

        update(cid, {"MemorySwap": 134217728, "MemoryReservation": 8388608, "PidsLimit": -1, "CpuQuota": -1})
        expected = ["67108864", "67108864", "8388608", cpu, mem, "max", "max 100000"]
        assert capture(cid, command) == expected
        print("PASS partial update and removal of CPU/PID limits", flush=True)

        before = inspect(cid)
        # Negative CpuShares are left to the creates below: Docker refuses them
        # there, but an update of Docker 29 takes them, and the container then
        # cannot be started again.
        for invalid in ({"MemorySwap": 1}, {"MemoryReservation": 1}, {"Memory": -1},
                        {"CpuPeriod": 1}, {"CpusetCpus": "3-1"},
                        {"NanoCpus": 1000000000, "CpuQuota": 10000}):
            # Docker reports some merged-limit failures from runc as 500;
            # Kiri can reject the same combination before contacting the guest.
            error = update(cid, invalid, (400, 500))
            assert error["message"], error
            assert inspect(cid) == before, invalid
        assert capture(cid, command) == expected
        print("PASS invalid updates preserve saved and kernel limits", flush=True)

        # Reject after another controller could have changed. Check the kernel,
        # rather than trusting inspect to show a successful rollback. The CPU
        # is well formed but absent, so only the kernel can refuse it.
        error = update(cid, {"Memory": 50331648, "CpusetCpus": "8191"}, (400, 500))
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

        # cgroup v2 cannot switch the OOM killer off for one container: the
        # request is accepted and dropped with a warning, not refused.
        warning = "Your kernel does not support OomKillDisable. OomKillDisable discarded."
        reply = request("POST", "/containers/create",
                        {"Image": image, "Cmd": ["sleep", "120"],
                         "HostConfig": {"NetworkMode": "none", "OomKillDisable": True}}, (201,))
        assert warning in reply["Warnings"], reply
        oom = reply["Id"]
        assert inspect(oom)["OomKillDisable"] in (None, False)
        assert update(oom, {"OomKillDisable": True})["Warnings"] == [warning]
        assert update(oom, {"PidsLimit": 64}) == {"Warnings": None}
        start(oom)
        assert capture(oom, "cat /sys/fs/cgroup/pids.max") == ["64"]
        print("PASS unsupported OOM-killer control is discarded with a warning", flush=True)

        # --cpus may ask for every CPU a container can run on, and no more.
        count = int(capture(probe, "nproc")[0])
        cpu_range = "range of CPUs is from 0.01 to %d.00, as there are only %d CPUs available" % (count, count)
        assert inspect(create({"NanoCpus": count * 1000000000}))["NanoCpus"] == count * 1000000000
        for values in ({"NanoCpus": count * 1000000000 + 1}, {"NanoCpus": 9223372036854775807}, {"NanoCpus": -1}):
            error = request("POST", "/containers/create",
                            {"Image": image, "Cmd": ["true"], "HostConfig": values}, (400,))
            assert error["message"] == cpu_range, (values, error)
            assert update(oom, values, (400,))["message"] == cpu_range, values
        print("PASS CPU limit is bounded by the CPUs available", flush=True)

        # Inspect reports the limits as they were given, so its HostConfig
        # creates the same container again. The weight of 512 shares depends
        # on the runc an engine follows: 20 before runc 1.3, 59 since.
        for host, given, kernel in (
                ({"NanoCpus": 1500000000, "CpuShares": 512, "Memory": 67108864, "PidsLimit": 32},
                 (1500000000, 0, 0, 512), ["150000 100000", ("20", "59"), "67108864", "32"]),
                ({"CpuQuota": 20000}, (0, 20000, 0, 0), ["20000 100000", ("100",), "max", None])):
            config = inspect(create(host))
            assert tuple(config[key] for key in ("NanoCpus", "CpuQuota", "CpuPeriod", "CpuShares")) == given, config
            copy = request("POST", "/containers/create",
                           {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": config}, (201,))["Id"]
            assert inspect(copy) == config
            start(copy)
            values = capture(copy, "cd /sys/fs/cgroup && cat cpu.max cpu.weight memory.max pids.max")
            assert values[0] == kernel[0] and values[1] in kernel[1] and values[2] == kernel[2], values
            assert kernel[3] in (None, values[3]), values
        print("PASS inspected limits recreate the container", flush=True)

        # --cpus and a quota or period replace each other in the kernel, so
        # an update may not add one to a container that has the other.
        cpus = create({"NanoCpus": 1500000000})
        start(cpus)
        quota = create({"CpuQuota": 20000})
        for target, values, reason in (
                (cpus, {"CpuQuota": 20000}, "CPU Quota cannot be updated as NanoCPUs has already been set"),
                (cpus, {"CpuPeriod": 50000}, "CPU Period cannot be updated as NanoCPUs has already been set"),
                (quota, {"NanoCpus": 1000000000}, "Nano CPUs cannot be updated as CPU Quota has already been set"),
                (cid, {"NanoCpus": 1000000000}, "Nano CPUs cannot be updated as CPU Period has already been set")):
            error = update(target, values, (409,))
            assert error["message"] == "Cannot update container %s: Conflicting options: %s" % (target, reason), error
        assert capture(cpus, "cat /sys/fs/cgroup/cpu.max") == ["150000 100000"]
        update(cpus, {"NanoCpus": 500000000})
        assert inspect(cpus)["NanoCpus"] == 500000000
        assert capture(cpus, "cat /sys/fs/cgroup/cpu.max") == ["50000 100000"]
        update(cpus, {"CpuQuota": -1})
        assert capture(cpus, "cat /sys/fs/cgroup/cpu.max") == ["max 100000"]
        print("PASS CPU limit forms cannot be mixed by update", flush=True)

        # Values Docker accepts and reads as "no limit" are not refused.
        for accepted in ({"MemorySwap": -2}, {"CpuQuota": -2}, {"NanoCpus": 1}):
            create(accepted)
        lenient = create({"Memory": 67108864, "MemoryReservation": -1, "NanoCpus": 1000000000, "CpuQuota": -1})
        start(lenient)
        assert capture(lenient, "cd /sys/fs/cgroup && cat memory.low cpu.max") == ["0", "max 100000"]
        # A reservation is a hint: an update may raise it above the limit.
        update(lenient, {"MemoryReservation": 134217728})
        assert capture(lenient, "cd /sys/fs/cgroup && cat memory.max memory.low") == ["67108864", "134217728"]
        print("PASS limits Docker leaves to the kernel are accepted", flush=True)

        # Clients match on these messages, so they are Docker's own.
        for invalid, message in (
                ({"Memory": 1048576}, "Minimum memory limit allowed is 6MB"),
                ({"MemorySwap": 1}, "You should always set the Memory limit when using Memoryswap limit, see usage"),
                ({"Memory": 67108864, "MemorySwap": 6291456},
                 "Minimum memoryswap limit should be larger than memory limit, see usage"),
                ({"MemoryReservation": 1}, "Minimum memory reservation allowed is 6MB"),
                ({"Memory": 67108864, "MemoryReservation": 134217728},
                 "Minimum memory limit can not be less than memory reservation limit, see usage"),
                ({"NanoCpus": 1000000000, "CpuQuota": 10000},
                 "Conflicting options: Nano CPUs and CPU Quota cannot both be set"),
                ({"NanoCpus": 1000000000, "CpuPeriod": 10000},
                 "Conflicting options: Nano CPUs and CPU Period cannot both be set"),
                ({"CpuPeriod": 1}, "CPU cfs period can not be less than 1ms (i.e. 1000) or larger than 1s (i.e. 1000000)"),
                ({"CpuQuota": 999}, "CPU cfs quota can not be less than 1ms (i.e. 1000)"),
                ({"CpuShares": -1}, "invalid CPU shares (-1): value must be a positive integer"),
                ({"CpusetCpus": "3-1"}, "Invalid value 3-1 for cpuset cpus: invalid format: 3-1"),
                ({"CpusetCpus": "999999"}, "Invalid value 999999 for cpuset cpus: value of out range, maximum is 8192"),
                ({"CpusetMems": "x"}, "Invalid value x for cpuset mems: invalid format: x")):
            error = request("POST", "/containers/create",
                            {"Image": image, "Cmd": ["true"], "HostConfig": invalid}, (400,))
            assert error["message"] == message, (invalid, error)
        print("PASS invalid creates", flush=True)
        print("13 resource control scenarios passed")
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

