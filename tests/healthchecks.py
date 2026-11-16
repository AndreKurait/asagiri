#!/usr/bin/env python3
"""Exercise real health probes, their timing and filters on an explicit engine."""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import time
import urllib.parse
import uuid

LABEL = "dev.asagiri.healthcheck-test"


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
    record = {"schema": "asagiri.healthchecks.v1", "status": "running",
              "owner_token": token, "cases": [], "cleanup": [], "container_names": {}}
    owned = {}

    def save():
        if args.output:
            temporary = args.output.with_suffix(args.output.suffix + ".tmp")
            temporary.write_text(json.dumps(record, indent=2) + "\n")
            temporary.replace(args.output)

    def request(method, path, body=None):
        connection = Connection(args.socket)
        try:
            data = None if body is None else json.dumps(body).encode()
            connection.request(method, "/v1.47" + path, data,
                               {} if data is None else {"Content-Type": "application/json"})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def api(method, path, expected=200, body=None):
        status, raw = request(method, path, body)
        assert status == expected, (method, path, status, raw)
        return json.loads(raw) if raw else None

    version = api("GET", "/version")
    if args.reference_docker:
        assert not version.get("AsagiriVersion") and "Podman" not in json.dumps(version), version
    else:
        assert version.get("AsagiriVersion"), version
        if not args.native_asagiri:
            assert (Path("/.dockerenv").exists() and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
                    and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
                    and os.environ.get("ASAGIRI_SOCKET") == args.socket), "Use a disposable test engine"
    image = api("GET", "/images/" + urllib.parse.quote(args.image, safe="") + "/json")
    record["version"] = version.get("AsagiriVersion", version["Version"])
    record["image_id"] = image["Id"]
    # No pulls or image updates: every container uses the already inspected ID.
    definitions = {
        "healthy": {"Test": ["CMD", "true"], "Interval": 200_000_000, "Retries": 3},
        "unhealthy": {"Test": ["CMD-SHELL", "exit 1"], "Interval": 200_000_000, "Retries": 3},
        "timeout": {"Test": ["CMD", "sleep", "5"], "Interval": 200_000_000,
                    "Timeout": 1_000_000_000, "Retries": 1},
        "start-interval": {"Test": ["CMD", "true"], "Interval": 30_000_000_000,
                           "StartPeriod": 20_000_000_000, "StartInterval": 250_000_000},
    }
    expected = {"healthy": "healthy", "unhealthy": "unhealthy",
                "timeout": "unhealthy", "start-interval": "healthy"}
    started = {}
    observed = {}
    save()
    try:
        for case, healthcheck in definitions.items():
            name = "asagiri-health-" + token + "-" + case
            assert request("GET", "/containers/" + name + "/json")[0] == 404, name
            record["container_names"][case] = name
            save()
            container = api("POST", "/containers/create?name=" + name, 201, {
                "Image": image["Id"], "Cmd": ["sleep", "3600"], "User": "65534:65534",
                "Labels": {LABEL: token}, "Healthcheck": healthcheck,
                "HostConfig": {"NetworkMode": "none", "CapDrop": ["ALL"],
                               "SecurityOpt": ["no-new-privileges"],
                               "Memory": 64 << 20, "PidsLimit": 16},
            })
            cid = container["Id"]
            owned[case] = cid
            started[case] = time.monotonic()
            api("POST", "/containers/" + cid + "/start", 204)
        deadline = time.monotonic() + 20
        pending = set(definitions)
        while pending:
            for case in sorted(pending):
                inspected = api("GET", "/containers/" + owned[case] + "/json")
                assert inspected["State"]["Running"], (case, inspected["State"])
                health = inspected["State"]["Health"]
                if health["Status"] == expected[case]:
                    observed[case] = {
                        "case": case, "status": "passed", "health": health,
                        "elapsed_seconds": time.monotonic() - started[case],
                        "container_id": owned[case],
                    }
                    pending.remove(case)
            assert time.monotonic() < deadline, ("health status deadline", sorted(pending))
            if pending:
                time.sleep(0.05)
        assert observed["unhealthy"]["health"]["FailingStreak"] >= 3, observed["unhealthy"]
        timeout_log = observed["timeout"]["health"]["Log"]
        assert any(p["ExitCode"] == -1 and "Health check exceeded timeout (1s)" in p["Output"]
                   for p in timeout_log), timeout_log
        # An ignored StartInterval waits 30 seconds. This bounded check allows
        # runner scheduling noise without accepting the normal interval.
        assert observed["start-interval"]["elapsed_seconds"] < 8, observed["start-interval"]
        for case in ("healthy", "start-interval"):
            assert observed[case]["health"]["FailingStreak"] == 0, observed[case]
            assert any(p["ExitCode"] == 0 for p in observed[case]["health"]["Log"]), observed[case]
        record["cases"].extend(observed[case] for case in definitions)
        for health, cases in (("healthy", ["healthy", "start-interval"]),
                              ("unhealthy", ["unhealthy", "timeout"]), ("none", [])):
            query = urllib.parse.urlencode({
                "all": "1", "filters": json.dumps({"label": [LABEL + "=" + token], "health": [health]}),
            })
            listed = api("GET", "/containers/json?" + query)
            assert {c["Id"] for c in listed} == {owned[c] for c in cases}, (health, listed)
            record["cases"].append({"case": "filter-" + health, "status": "passed",
                                    "container_ids": sorted(c["Id"] for c in listed)})
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = repr(error)
        record["observed"] = observed
        raise
    finally:
        cleanup_errors = []
        for case, name in record["container_names"].items():
            try:
                status, raw = request("GET", "/containers/" + name + "/json")
                cid = owned.get(case)
                if status != 404:
                    assert status == 200, (name, status, raw)
                    inspected = json.loads(raw)
                    assert (cid is None or inspected["Id"] == cid) and (inspected["Config"]["Labels"] or {}).get(LABEL) == token, inspected
                    cid = inspected["Id"]
                    api("DELETE", "/containers/" + cid + "?force=true", 204)
                status, raw = request("GET", "/containers/" + name + "/json")
                assert status == 404, (name, status, raw)
                record["cleanup"].append({"case": case, "container_id": cid, "status": 404})
            except BaseException as error:
                cleanup_errors.append({"case": case, "container_name": name, "error": repr(error)})
        if cleanup_errors:
            record.update(status="failed", cleanup_errors=cleanup_errors)
        save()
        print(json.dumps(record, sort_keys=True), flush=True)
        assert not cleanup_errors, cleanup_errors


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("image")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--reference-docker", action="store_true")
    mode.add_argument("--native-asagiri", action="store_true")
    parser.add_argument("--output", type=Path)
    options = parser.parse_args()
    if options.output:
        with options.output.open("x"):
            pass
    check(options)
