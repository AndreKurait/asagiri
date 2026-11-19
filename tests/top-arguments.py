#!/usr/bin/env python3
"""Check default top columns and visible refusal of unsupported ps arguments."""
import argparse
import datetime
import http.client
import json
import os
from pathlib import Path
import socket
import time
import urllib.parse
import uuid

LABEL = "dev.asagiri.top-arguments-test"
TITLES = ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"]


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(args):
    token = uuid.uuid4().hex
    name = "asagiri-top-" + token
    record = {
        "schema": "asagiri.top-arguments.v1", "status": "running",
        "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "reference_docker": args.reference_docker, "native_asagiri": args.native_asagiri,
        "cases": [], "owned_name": name, "cleanup": {"verified_absent": False},
    }
    attempted_create = False

    def save():
        if args.output:
            args.output.write_text(json.dumps(record, indent=2) + "\n")

    def request(method, path, body=None):
        connection = Connection(args.socket)
        try:
            raw = None if body is None else json.dumps(body).encode()
            connection.request(method, "/v1.47" + path, raw,
                               {} if body is None else {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            return response.status, None if not data else json.loads(data)
        finally:
            connection.close()

    def result(case, passed, **details):
        record["cases"].append({"name": case, "passed": bool(passed), **details})
        save()

    try:
        status, version = request("GET", "/version")
        assert status == 200, (status, version)
        if args.reference_docker:
            assert not version.get("AsagiriVersion") and "Podman" not in json.dumps(version)
        else:
            assert version.get("AsagiriVersion"), version
            if not args.native_asagiri:
                assert (Path("/.dockerenv").exists() and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
                        and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
                        and os.environ.get("ASAGIRI_SOCKET") == args.socket), "Use a disposable test engine"
        record["version"] = version.get("AsagiriVersion", version["Version"])
        assert request("GET", "/containers/" + name + "/json")[0] == 404
        assert request("GET", "/images/" + urllib.parse.quote(args.image, safe="") + "/json")[0] == 200, "Cache the fixture image first"
        attempted_create = True
        save()
        status, created = request("POST", "/containers/create?name=" + name, {
            "Image": args.image, "Cmd": ["sleep", "600"], "User": "65534:65534",
            "Labels": {LABEL: token},
            "HostConfig": {"NetworkMode": "none", "ReadonlyRootfs": True,
                           "CapDrop": ["ALL"], "SecurityOpt": ["no-new-privileges:true"],
                           "Memory": 32 * 1024 * 1024, "PidsLimit": 16},
        })
        assert status == 201 and created.get("Id"), (status, created)
        record["container_id"] = created["Id"]
        assert request("POST", "/containers/" + name + "/start")[0] == 204
        # Start acknowledges completed root setup, just before exec. Wait for
        # the requested program, rather than treating the transient /init
        # command name as a top-format regression.
        deadline = time.monotonic() + 10
        while True:
            status, body = request("GET", "/containers/" + name + "/top")
            assert status == 200 and isinstance(body, dict), (status, body)
            if any(row and "sleep" in row[-1] for row in body.get("Processes", [])):
                break
            assert time.monotonic() < deadline, ("fixture program did not become visible", body)
            time.sleep(0.025)
        for label, query in [("default", ""), ("empty", "?ps_args="), ("explicit-default", "?ps_args=-ef")]:
            status, body = request("GET", "/containers/" + name + "/top" + query)
            processes = body.get("Processes", []) if isinstance(body, dict) else []
            passed = (status == 200 and body.get("Titles") == TITLES and processes
                      and all(len(row) == len(TITLES) and row[1].isdigit() for row in processes)
                      and any("sleep" in row[-1] for row in processes))
            result(label, passed, http_status=status, response=body)
        for value, expected_columns in [
            ("aux", ["USER", "PID", "%CPU", "%MEM", "VSZ", "RSS", "TTY", "STAT", "START", "TIME", "COMMAND"]),
            ("-eo pid,rss,args", ["PID", "RSS", "COMMAND"]),
            ("-ef -e", TITLES),
        ]:
            query = urllib.parse.urlencode({"ps_args": value})
            status, body = request("GET", "/containers/" + name + "/top?" + query)
            processes = body.get("Processes", []) if isinstance(body, dict) else []
            passed = (status == 200 and body.get("Titles") == expected_columns and processes
                      and all(len(row) == len(expected_columns) for row in processes))
            if value == "-eo pid,rss,args":
                passed = passed and all(row[0].isdigit() and row[1].isdigit() for row in processes)
            result("custom-" + value, passed, http_status=status, response=body)
        status, body = request(
            "GET", "/containers/" + name + "/top?" +
            urllib.parse.urlencode({"ps_args": "--asagiri-deliberately-invalid-option"}))
        result("invalid-option", status in ({400, 500} if args.reference_docker else {400}),
               http_status=status, response=body)
        status, body = request("GET", "/containers/" + name + "-missing/top")
        result("missing-container", status == 404, http_status=status, response=body)
        status, body = request("POST", "/containers/" + name + "/stop?t=1")
        assert status in (204, 304), (status, body)
        status, body = request("GET", "/containers/" + name + "/top")
        result("stopped-container", status == 409, http_status=status, response=body)
        assert len(record["cases"]) == 9 and all(case["passed"] for case in record["cases"]), "top argument behavior differs from the declared contract"
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = repr(error)
        raise
    finally:
        try:
            if attempted_create:
                status, container = request("GET", "/containers/" + name + "/json")
                if status == 200:
                    assert (container["Config"].get("Labels") or {}).get(LABEL) == token, "Refuse unowned cleanup"
                    if record.get("container_id"):
                        assert container["Id"] == record["container_id"], "Fixture identity changed"
                    deleted, body = request("DELETE", "/containers/" + name + "?force=1")
                    assert deleted in (204, 404), (deleted, body)
                else:
                    assert status == 404, (status, container)
                status, body = request("GET", "/containers/" + name + "/json")
                assert status == 404, (status, body)
                if record.get("container_id"):
                    assert request("GET", "/containers/" + record["container_id"] + "/json")[0] == 404
                record["cleanup"]["verified_absent"] = True
        except BaseException as error:
            record["status"] = "failed"
            record["cleanup"]["error"] = repr(error)
            raise
        finally:
            record["finished_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            save()
            print(json.dumps(record), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    engine = parser.add_mutually_exclusive_group()
    engine.add_argument("--reference-docker", action="store_true")
    engine.add_argument("--native-asagiri", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.output:
        assert args.output.parent.is_dir() and not args.output.exists()
    check(args)


if __name__ == "__main__":
    main()
