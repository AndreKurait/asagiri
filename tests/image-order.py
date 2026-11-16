#!/usr/bin/env python3
"""Check creation-time image ordering with owned zero-layer metadata fixtures."""
import argparse
import datetime
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import socket
import tarfile
import urllib.parse
import uuid

LABEL = "dev.asagiri.image-order-test"


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
    repository = "asagiri-image-order-" + token
    record = {"schema": "asagiri.image-order.v1", "status": "running",
              "cases": [], "images": [], "cleanup": []}
    if args.output:
        assert not args.output.exists(), "Preserve the existing fixture receipt"

    def save():
        if args.output:
            temporary = args.output.with_suffix(args.output.suffix + ".tmp")
            temporary.write_text(json.dumps(record, indent=2) + "\n")
            temporary.replace(args.output)

    def request(method, path, body=None):
        connection = Connection(args.socket)
        try:
            connection.request(method, "/v1.47" + path, body,
                               {} if body is None else {"Content-Type": "application/x-tar"})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def read(path):
        status, raw = request("GET", path)
        assert status == 200, (status, raw)
        return json.loads(raw)

    version = read("/version")
    if args.reference_docker:
        assert not version.get("AsagiriVersion") and "Podman" not in json.dumps(version), version
    else:
        assert version.get("AsagiriVersion"), version
        if not args.native_asagiri:
            assert (Path("/.dockerenv").exists() and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
                    and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
                    and os.environ.get("ASAGIRI_SOCKET") == args.socket), "Use a disposable test engine"
    record["version"] = version.get("AsagiriVersion", version["Version"])

    def inventory():
        return sorted((row["Id"], sorted(row.get("RepoTags") or []), sorted(row.get("RepoDigests") or []))
                      for row in read("/images/json?all=1"))

    before = inventory()
    before_ids = {row[0] for row in before}
    # Alphabetical tag order, load order and lexical date order all differ
    # from creation order. Docker sorts its integer Created response field.
    for tag, created in [
        ("b-newest", "2025-01-01T10:00:00.987654321Z"),
        ("a-oldest", "2025-01-01T08:00:00Z"),
        ("c-middle", "2025-01-01T12:00:00+03:00"),
    ]:
        reference = repository + ":" + tag
        config = json.dumps({
            "architecture": version.get("Arch", "arm64"), "os": "linux",
            "created": created, "config": {"Labels": {LABEL: token}},
            "rootfs": {"type": "layers", "diff_ids": []},
        }, separators=(",", ":")).encode()
        image_id = "sha256:" + hashlib.sha256(config).hexdigest()
        assert image_id not in before_ids
        assert request("GET", "/images/" + reference + "/json")[0] == 404
        record["images"].append({
            "reference": reference, "id": image_id, "created": created,
            "created_unix": int(datetime.datetime.fromisoformat(created.replace("Z", "+00:00")).timestamp()),
            "loaded": False,
        })
    save()

    try:
        for image in record["images"]:
            config = json.dumps({
                "architecture": version.get("Arch", "arm64"), "os": "linux",
                "created": image["created"], "config": {"Labels": {LABEL: token}},
                "rootfs": {"type": "layers", "diff_ids": []},
            }, separators=(",", ":")).encode()
            filename = image["id"][7:] + ".json"
            entries = {
                filename: config,
                "manifest.json": json.dumps([{
                    "Config": filename, "RepoTags": [image["reference"]], "Layers": [],
                }]).encode(),
            }
            archive = io.BytesIO()
            with tarfile.open(fileobj=archive, mode="w") as tar:
                for name, data in entries.items():
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    tar.addfile(member, io.BytesIO(data))
            status, raw = request("POST", "/images/load?quiet=1", archive.getvalue())
            assert status == 200, (status, raw)
            assert all(not json.loads(line).get("error") for line in raw.splitlines() if line), raw
            actual = read("/images/" + image["reference"] + "/json")
            assert actual["Id"] == image["id"] and actual["Config"]["Labels"][LABEL] == token, actual
            image["loaded"] = True
            save()

        expected = sorted(record["images"], key=lambda image: image["created_unix"], reverse=True)
        wanted = {image["id"] for image in expected}
        queries = [
            ("unfiltered", None, expected),
            ("repository", {"reference": [repository]}, expected),
            ("label", {"label": [LABEL + "=" + token]}, expected),
            ("wildcard", {"reference": [repository + ":*"]}, expected),
            ("two tags", {"reference": [expected[0]["reference"], expected[2]["reference"]]},
             [expected[0], expected[2]]),
        ]
        for name, filters, selected in queries:
            query = "" if filters is None else "?" + urllib.parse.urlencode({"filters": json.dumps(filters)})
            rows = read("/images/json" + query)
            actual = [row for row in rows if row["Id"] in wanted]
            expected_pairs = [(image["id"], image["created_unix"]) for image in selected]
            actual_pairs = [(row["Id"], row["Created"]) for row in actual]
            record["cases"].append({
                "name": name, "passed": actual_pairs == expected_pairs,
                "actual": actual_pairs, "expected": expected_pairs,
            })
        assert all(case["passed"] for case in record["cases"]), "Image lists are not newest-created first"
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        errors = []
        for image in record["images"]:
            try:
                target = "/images/" + image["id"]
                status, raw = request("GET", target + "/json")
                if status != 404:
                    assert status == 200, (status, raw)
                    actual = json.loads(raw)
                    assert actual["Id"] == image["id"] and actual["Config"]["Labels"][LABEL] == token
                    assert set(actual.get("RepoTags") or []) <= {image["reference"]}, actual
                    status, raw = request("DELETE", "/images/" + image["reference"])
                    assert status == 200, (status, raw)
                assert request("GET", target + "/json")[0] == 404
                assert request("GET", "/images/" + image["reference"] + "/json")[0] == 404
                record["cleanup"].append({"id": image["id"], "verified_absent": True})
            except BaseException as error:
                errors.append(repr(error))
                record["cleanup"].append({"id": image["id"], "error": repr(error)})
        try:
            assert inventory() == before, "The image inventory changed beyond the owned fixtures"
            record["original_inventory_preserved"] = True
        except BaseException as error:
            errors.append(repr(error))
        if errors:
            record.update(status="failed", cleanup_errors=errors)
        save()
        print(json.dumps(record, indent=2), flush=True)
        assert not errors, errors


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--reference-docker", action="store_true")
    mode.add_argument("--native-asagiri", action="store_true")
    parser.add_argument("--output", type=Path)
    check(parser.parse_args())
