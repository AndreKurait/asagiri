#!/usr/bin/env python3
"""Reject unsupported run options before side effects and report ignored controls."""
import argparse
from datetime import datetime, timezone
import http.client
import json
from pathlib import Path
import socket
import sys
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(args):
    owner = "asagiri-run-options-" + uuid.uuid4().hex
    containers, volumes = [], []
    record = {"probe": "run-options", "owner": owner, "started_at": datetime.now(timezone.utc).isoformat(),
              "status": "running", "checks": [], "cleanup": []}

    def call(method, path, value=None, statuses=(200,), raw=False):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + path, None if value is None else json.dumps(value),
                               {"Content-Type": "application/json"})
            reply = connection.getresponse()
            data = reply.read()
            result = data if raw else json.loads(data) if data else None
            if method == "POST" and path == "/containers/create" and reply.status == 201:
                containers.append(result["Id"])
            assert reply.status in statuses, (method, path, reply.status, result)
            return result
        finally:
            connection.close()

    def create(host, command=None, statuses=(201,)):
        return call("POST", "/containers/create",
                    {"Image": args.image, "Cmd": command or ["true"], "Labels": {"dev.asagiri.test.owner": owner},
                     "HostConfig": {"NetworkMode": "none", **host}}, statuses)

    def passed(name):
        record["checks"].append(name)
        print("PASS " + name, flush=True)

    try:
        version = call("GET", "/version")
        assert version.get("AsagiriVersion"), "requires an Asagiri engine"
        record["version"] = version["AsagiriVersion"]
        call("GET", "/images/" + args.image + "/json")
        invalid = [
            ({"PidMode": "container:db"}, "--pid=container:db"),
            ({"PidMode": "private"}, "--pid=private"), ({"IpcMode": "container:db"}, "--ipc=container:db"),
            ({"UTSMode": "host"}, "--uts=host"), ({"CgroupnsMode": "host"}, "--cgroupns=host"),
            ({"Runtime": "nvidia"}, "--runtime=nvidia"), ({"Isolation": "hyperv"}, "--isolation=hyperv"),
            ({"UsernsMode": "private"}, "UsernsMode"), ({"CgroupParent": "/tenant"}, "CgroupParent"),
            ({"VolumesFrom": ["db:ro"]}, "--volumes-from"), ({"StorageOpt": {"size": "1G"}}, "--storage-opt"),
            ({"PidMode": False}, "PidMode must be a string"), ({"IpcMode": []}, "IpcMode must be a string"),
            ({"VolumesFrom": ""}, "VolumesFrom must be an array"), ({"StorageOpt": []}, "StorageOpt must be an object"),
            ({"MemorySwappiness": False}, "MemorySwappiness must be an integer"),
            ({"OomScoreAdj": 1.5}, "OomScoreAdj must be an integer"),
            ({"BlkioDeviceReadBps": {}}, "BlkioDeviceReadBps must be an array"),
        ]
        for number, (host, reason) in enumerate(invalid):
            volume = owner + "-" + str(number)
            call("GET", "/volumes/" + volume, statuses=(404,))
            volumes.append(volume)
            error = create({**host, "Mounts": [{"Type": "volume", "Source": volume, "Target": "/data",
                "VolumeOptions": {"Labels": {"dev.asagiri.test.owner": owner},
                                  "DriverConfig": {"Name": "local", "Options": {}}}}]}, statuses=(400,))
            assert reason in error["message"], (host, error)
            call("GET", "/volumes/" + volume, statuses=(404,))
        passed("invalid options are rejected before implicit volume creation")

        for value in [False, [], "invalid"]:
            error = call("POST", "/containers/create", {"Image": args.image,
                "Labels": {"dev.asagiri.test.owner": owner}, "HostConfig": value}, (400,))
            assert "HostConfig must be an object" in error["message"], error
        passed("malformed HostConfig is rejected")

        default = {"PidMode": "", "IpcMode": "private", "UTSMode": "", "CgroupnsMode": "private",
                   "Runtime": "asagiri", "Isolation": "default", "VolumesFrom": [], "StorageOpt": {},
                   "MemorySwappiness": None, "OomScoreAdj": 0, "CpuRealtimePeriod": 0,
                   "CpuRealtimeRuntime": 0, "BlkioWeight": 0, "BlkioWeightDevice": []}
        limits = {"Memory": 67108864, "MemorySwap": 134217728, "MemoryReservation": 16777216,
                  "CpusetCpus": "0", "CpusetMems": "0", "CgroupParent": "/docker/buildx"}
        reply = create({**default, **limits})
        assert reply["Warnings"] == [], reply
        control = reply["Id"]
        inspected = call("GET", "/containers/" + control + "/json")["HostConfig"]
        assert all(inspected[key] == value for key, value in limits.items()), inspected
        assert create({"IpcMode": "shareable", "Runtime": "runc", "MemorySwappiness": -1})["Warnings"] == []
        passed("client defaults and implemented resource controls remain accepted without warnings")

        ignored = [
            ("MemorySwappiness", 0, "--memory-swappiness"), ("OomScoreAdj", -1, "--oom-score-adj"),
            ("CpuRealtimePeriod", 100000, "--cpu-rt-period"), ("CpuRealtimeRuntime", 1000, "--cpu-rt-runtime"),
            ("BlkioWeight", 500, "--blkio-weight"),
            ("BlkioWeightDevice", [{"Path": "/dev/sda", "Weight": 100}], "--blkio-weight-device"),
            ("BlkioDeviceReadBps", [{"Path": "/dev/sda", "Rate": 1024}], "--device-read-bps"),
            ("BlkioDeviceWriteBps", [{"Path": "/dev/sda", "Rate": 1024}], "--device-write-bps"),
            ("BlkioDeviceReadIOps", [{"Path": "/dev/sda", "Rate": 100}], "--device-read-iops"),
            ("BlkioDeviceWriteIOps", [{"Path": "/dev/sda", "Rate": 100}], "--device-write-iops"),
        ]
        for key, value, flag in ignored:
            warning = f"Asagiri does not support {flag}; it was ignored"
            assert create({key: value})["Warnings"] == [warning], key
            assert call("POST", "/containers/" + control + "/update", {key: value})["Warnings"] == [warning], key
            assert call("GET", "/containers/" + control + "/json")["HostConfig"] == inspected, key
        passed("each ignored resource control warns on create and update without changing stored limits")

        call("POST", "/containers/" + control + "/update", {"OomScoreAdj": "invalid"}, (400,))
        assert call("GET", "/containers/" + control + "/json")["HostConfig"] == inspected
        passed("invalid resource updates preserve the existing configuration")

        reply = create({"LogConfig": {"Type": "syslog"}}, ["sh", "-ec", "printf 'run-options-local-log\\n'"])
        assert reply["Warnings"] == [
            "Asagiri does not support the syslog log driver; logs are kept locally and are not sent anywhere"], reply
        logged = reply["Id"]
        call("POST", "/containers/" + logged + "/start", statuses=(204,))
        assert call("POST", "/containers/" + logged + "/wait")["StatusCode"] == 0
        logs = call("GET", "/containers/" + logged + "/logs?stdout=1&stderr=1", raw=True)
        assert b"run-options-local-log\n" in logs, logs
        passed("an unsupported remote log driver warns while preserving local logs")
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(containers):
            try:
                info = call("GET", "/containers/" + cid + "/json")
                assert info["Config"]["Labels"].get("dev.asagiri.test.owner") == owner
                call("DELETE", "/containers/" + cid + "?force=true", statuses=(204,))
                call("GET", "/containers/" + cid + "/json", statuses=(404,))
                record["cleanup"].append({"container": cid, "status": 404})
            except BaseException as error:
                errors.append(repr(error))
        for volume in volumes:
            try:
                # On a failing old engine the invalid create may have created
                # this exact previously absent, labelled volume.
                connection = Connection(args.socket)
                try:
                    connection.request("GET", "/v1.47/volumes/" + volume)
                    response = connection.getresponse()
                    data = response.read()
                    if response.status == 200:
                        info = json.loads(data)
                        assert (info.get("Labels") or {}).get("dev.asagiri.test.owner") == owner, info
                        call("DELETE", "/volumes/" + volume, statuses=(204,))
                    else:
                        assert response.status == 404, (response.status, data)
                finally:
                    connection.close()
                call("GET", "/volumes/" + volume, statuses=(404,))
                record["cleanup"].append({"volume": volume, "status": 404})
            except BaseException as error:
                errors.append(repr(error))
        record["cleanup_errors"] = errors
        record["finished_at"] = datetime.now(timezone.utc).isoformat()
        if errors:
            record["status"] = "failed"
        if args.report:
            Path(args.report).write_text(json.dumps(record, indent=2) + "\n")
        print(json.dumps(record), flush=True)
        if errors and not primary_failure:
            raise AssertionError(errors)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--report")
    check(parser.parse_args())
