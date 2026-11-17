#!/usr/bin/env python3
"""Compare tmpfs inspect metadata with its kernel mount, including restarts.

Uses an already cached Alpine image and uniquely labelled, private-network
containers. The restart phases retain one stopped fixture across an externally
managed daemon restart; this script never restarts the daemon itself.
"""
import argparse
from datetime import datetime, timezone
import http.client
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time
import uuid


TMPFS = {"/scratch": "rw,nosuid,nodev,noexec,size=8m,mode=1770", "/empty": ""}
LABEL = "dev.asagiri.test.owner"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(args):
    owner = "asagiri-tmpfs-" + uuid.uuid4().hex
    owned, retained = [], False
    started = time.monotonic()
    record = {"probe": "tmpfs-inspect", "status": "running", "checks": [], "cleanup": [],
              "started_at": datetime.now(timezone.utc).isoformat(),
              "reference_docker": args.reference_docker}
    docker = ["docker", "--host", "unix://" + args.socket]

    def call(method, target, body=None, statuses=(200,)):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            reply = connection.getresponse()
            data = reply.read()
            result = json.loads(data) if data else None
            if target.startswith("/containers/create?") and reply.status == 201:
                owned.append(result["Id"])
            assert reply.status in statuses, (method, target, reply.status, result)
            return result
        finally:
            connection.close()

    def inspect(cid):
        info = call("GET", "/containers/" + cid + "/json")
        assert info["Id"] == cid and info["Config"]["Labels"].get(LABEL) == owner, info
        assert info["Name"].startswith("/" + owner + "-"), info["Name"]
        return info

    def metadata(cid, expected):
        info = inspect(cid)
        # Docker omits an empty Tmpfs map. A nonempty map must be present,
        # with the original destination keys and option strings.
        assert info["HostConfig"].get("Tmpfs") == expected, info["HostConfig"]
        return info

    def create(host):
        reply = call("POST", "/containers/create?name=" + owner + "-" + str(len(owned)),
                     {"Image": args.image, "Cmd": ["sh", "-ec",
                         "trap 'exit 0' TERM; while :; do sleep 1 & wait $!; done"],
                      "Labels": {LABEL: owner}, "HostConfig": {
                          "NetworkMode": "none", "ReadonlyRootfs": True, "Memory": 67108864,
                          "NanoCpus": 500000000, "PidsLimit": 32, **host}}, (201,))
        assert re.fullmatch(r"[0-9a-f]{64}", reply["Id"]), reply
        return reply["Id"]

    def start(cid):
        call("POST", "/containers/" + cid + "/start", statuses=(204,))
        assert inspect(cid)["State"]["Running"]

    def stop(cid):
        call("POST", "/containers/" + cid + "/stop?t=1", statuses=(204, 304))
        assert not inspect(cid)["State"]["Running"]

    def execute(cid, *command):
        result = subprocess.run(docker + ["exec", cid, *command],
                                capture_output=True, text=True, timeout=30)
        assert result.returncode == 0, (command, result.returncode, result.stdout, result.stderr)
        return result.stdout.strip()

    def kernel(cid, target="/scratch", size=8 << 20, mode="1770"):
        rows = [line.split() for line in execute(cid, "cat", "/proc/self/mountinfo").splitlines()]
        matches = [row for row in rows if row[4] == target]
        assert len(matches) == 1, (target, matches)
        row = matches[0]
        separator = row.index("-")
        assert row[separator + 1] == "tmpfs", row
        flags = set(row[5].split(","))
        assert {"rw", "nosuid", "nodev", "noexec"} <= flags, row
        fs_type, blocks, block_size = execute(cid, "stat", "-f", "-c", "%T %b %S", target).split()
        assert fs_type == "tmpfs" and int(blocks) * int(block_size) == size, (
            target, fs_type, blocks, block_size)
        actual_mode = execute(cid, "stat", "-c", "%a", target)
        assert actual_mode == mode, (target, actual_mode, mode)
        record.setdefault("kernel_mounts", []).append({
            "container": cid, "target": target, "mountinfo": " ".join(row),
            "bytes": int(blocks) * int(block_size), "mode": actual_mode})

    def ephemeral(cid, exists):
        execute(cid, "sh", "-ec", "test " + ("-f" if exists else "! -e") + " /scratch/marker")

    def passed(name):
        record["checks"].append(name)
        print("PASS " + name, flush=True)

    try:
        record["engine"] = call("GET", "/info")["ID"]
        version = call("GET", "/version")
        if args.reference_docker:
            assert not version.get("AsagiriVersion"), version
        else:
            assert version.get("AsagiriVersion"), version
        record["version"] = version
        if args.verify_restart:
            saved = json.loads(Path(args.verify_restart).read_text())
            assert saved["engine"] == record["engine"], "restart changed engine identity"
            assert re.fullmatch(r"asagiri-tmpfs-[0-9a-f]{32}", saved["owner"]), saved
            assert re.fullmatch(r"[0-9a-f]{64}", saved["container"]), saved
            assert saved["tmpfs"] == TMPFS, saved
            owner = saved["owner"]
            cid = saved["container"]
            info = inspect(cid)
            assert info["Image"] == saved["image"] and not info["State"]["Running"], info
            owned.append(cid)
            metadata(cid, TMPFS)
            start(cid)
            metadata(cid, TMPFS)
            kernel(cid)
            ephemeral(cid, False)
            passed("daemon restart preserves the tmpfs map and reapplies an empty kernel mount")
        else:
            call("GET", "/images/" + args.image + "/json")
            cid = create({"Tmpfs": TMPFS})
            metadata(cid, TMPFS)
            start(cid)
            metadata(cid, TMPFS)
            kernel(cid)
            assert execute(cid, "stat", "-f", "-c", "%T", "/empty") == "tmpfs"
            execute(cid, "sh", "-ec", "printf 'ephemeral\\n' > /scratch/marker")
            ephemeral(cid, True)
            passed("create and start preserve option strings and apply the actual tmpfs flags, size and mode")
            stop(cid)
            metadata(cid, TMPFS)
            if args.prepare_restart:
                data = {"engine": record["engine"], "owner": owner, "container": cid,
                        "image": inspect(cid)["Image"], "tmpfs": TMPFS}
                fd = os.open(args.prepare_restart, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                with os.fdopen(fd, "w") as out:
                    json.dump(data, out)
                    out.write("\n")
                retained = True
                record["retained_for_restart"] = data
                passed("stopped tmpfs fixture retained for an external daemon restart")
            else:
                start(cid)
                ephemeral(cid, False)
                execute(cid, "sh", "-ec", "printf 'ephemeral\\n' > /scratch/marker")
                call("POST", "/containers/" + cid + "/restart?t=1", statuses=(204,))
                metadata(cid, TMPFS)
                kernel(cid)
                ephemeral(cid, False)
                passed("container restart preserves the map and discards prior tmpfs contents")

                for host in ({}, {"Tmpfs": None}):
                    metadata(create(host), None)
                mount = create({"Mounts": [{"Type": "tmpfs", "Target": "/mount-style",
                                            "TmpfsOptions": {"SizeBytes": 2 << 20, "Mode": 0o700}}]})
                metadata(mount, None)
                start(mount)
                metadata(mount, None)
                kernel(mount, "/mount-style", 2 << 20, "700")
                passed("omitted and null maps stay empty; Mounts tmpfs does not become a Tmpfs map")
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        if not retained:
            for cid in reversed(owned):
                try:
                    if inspect(cid)["State"]["Running"]:
                        stop(cid)
                    call("DELETE", "/containers/" + cid + "?v=1", statuses=(204,))
                    call("GET", "/containers/" + cid + "/json", statuses=(404,))
                    record["cleanup"].append({"container": cid, "status": 404})
                except BaseException as error:
                    errors.append(repr(error))
        record.update(owner=owner, cleanup_errors=errors,
                      finished_at=datetime.now(timezone.utc).isoformat(),
                      elapsed_seconds=time.monotonic() - started)
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
    parser.add_argument("image", nargs="?", default="alpine:3.21")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--report")
    phases = parser.add_mutually_exclusive_group()
    phases.add_argument("--prepare-restart")
    phases.add_argument("--verify-restart")
    check(parser.parse_args())
