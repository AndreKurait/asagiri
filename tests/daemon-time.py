#!/usr/bin/env python3
"""Bind /info's clock to event replay, using the conversion in Compose 5.7."""
import argparse
import datetime
import http.client
import json
import os
from pathlib import Path
import re
import socket
import urllib.parse
import uuid

LABEL = "dev.asagiri.daemon-time-test"


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def nanoseconds(value):
    assert isinstance(value, str), ("missing SystemTime", value)
    match = re.fullmatch(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d{1,9}))?(Z|[+-]\d\d:\d\d)", value)
    assert match, ("SystemTime is not RFC3339Nano", value)
    seconds = int(datetime.datetime.fromisoformat(match[1] + match[3].replace("Z", "+00:00")).timestamp())
    return seconds * 1_000_000_000 + int((match[2] or "").ljust(9, "0"))


def decimal_seconds(value):
    seconds, nanos = divmod(value, 1_000_000_000)
    return f"{seconds}.{nanos:09d}"


def check(args):
    token = uuid.uuid4().hex
    name = "asagiri-daemon-time-" + token
    record = {"schema": "asagiri.daemon-time.v1", "status": "running", "cases": [],
              "volume": name, "cleanup": {"verified_absent": False}}

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

    def info_time():
        status, raw = request("GET", "/info")
        assert status == 200, (status, raw)
        info = json.loads(raw)
        return info["SystemTime"], nanoseconds(info.get("SystemTime"))

    status, raw = request("GET", "/version")
    assert status == 200, (status, raw)
    version = json.loads(raw)
    if args.reference_docker:
        assert not version.get("AsagiriVersion") and "Podman" not in json.dumps(version), version
    else:
        assert version.get("AsagiriVersion"), version
        if not args.native_asagiri:
            assert (Path("/.dockerenv").exists() and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
                    and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
                    and os.environ.get("ASAGIRI_SOCKET") == args.socket), "Use a disposable test engine"
    record["version"] = version.get("AsagiriVersion", version["Version"])
    # The clock must be usable before creating any resource. Compose fails at
    # this same point if a daemon returns an empty SystemTime.
    first_text, first = info_time()
    record["times"] = {"before": first_text}
    assert request("GET", "/volumes/" + name)[0] == 404, "Fixture name already exists"
    save()

    def remove_owned():
        status, raw = request("GET", "/volumes/" + name)
        if status == 404:
            return
        assert status == 200, (status, raw)
        volume = json.loads(raw)
        assert volume["Name"] == name and (volume.get("Labels") or {}).get(LABEL) == token, volume
        status, raw = request("DELETE", "/volumes/" + name)
        assert status == 204, (status, raw)

    try:
        status, raw = request("POST", "/volumes/create", {"Name": name, "Driver": "local",
                                                         "Labels": {LABEL: token}})
        assert status == 201, (status, raw)
        middle_text, middle = info_time()
        record["times"]["between"] = middle_text
        remove_owned()
        last_text, last = info_time()
        record["times"]["after"] = last_text
        assert first < middle < last, record["times"]

        def events(since):
            # Compose parses RFC3339Nano and sends Unix seconds with nine
            # fractional digits, avoiding the client's own wall clock.
            query = urllib.parse.urlencode({
                "since": decimal_seconds(since), "until": decimal_seconds(last),
                "filters": json.dumps({"type": ["volume"], "volume": [name]}),
            })
            status, raw = request("GET", "/events?" + query)
            assert status == 200, (status, raw)
            rows = [json.loads(line) for line in raw.splitlines() if line]
            assert all(row["Actor"]["ID"] == name and row["Type"] == "volume" for row in rows), rows
            return rows

        whole = events(first)
        assert [row["Action"] for row in whole] == ["create", "destroy"], whole
        assert first <= whole[0]["timeNano"] < middle <= whole[1]["timeNano"] <= last, whole
        later = events(middle)
        assert [row["Action"] for row in later] == ["destroy"], later
        assert events(last) == [], "The final cutoff replayed an older event"
        record["cases"] = [
            "RFC3339Nano SystemTime advances on each information request",
            "SystemTime and volume events use the same daemon clock",
            "Compose-style since replays events emitted before subscription",
            "A later cutoff excludes earlier events",
            "The final cutoff excludes all completed events",
        ]
        record["events"] = whole
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = repr(error)
        raise
    finally:
        try:
            remove_owned()
            status, raw = request("GET", "/volumes/" + name)
            assert status == 404, (status, raw)
            record["cleanup"]["verified_absent"] = True
        except BaseException as error:
            record["status"] = "failed"
            record["cleanup"]["error"] = repr(error)
            raise
        finally:
            save()
            print(json.dumps(record, indent=2), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--reference-docker", action="store_true")
    mode.add_argument("--native-asagiri", action="store_true")
    parser.add_argument("--output", type=Path)
    check(parser.parse_args())
