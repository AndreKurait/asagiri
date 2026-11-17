#!/usr/bin/env python3
"""Check image aliases and retagging in the container ancestor filter."""
import argparse
import hashlib
import http.client
import io
import json
from pathlib import Path
import socket
import tarfile
import time
from urllib.parse import quote, urlencode
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(args):
    record = {"schema": "asagiri.container-ancestor.v1", "status": "running", "cases": [], "cleanup": []}
    prefix = "asagiri-ancestor-" + uuid.uuid4().hex
    containers, images = [], []
    started = time.monotonic()

    def request(method, path, data=None, expected=200):
        connection = Connection(args.socket)
        try:
            raw = data if isinstance(data, bytes) else json.dumps(data).encode() if data is not None else None
            connection.request(method, "/v1.47" + path, raw,
                               {"Content-Type": "application/x-tar" if isinstance(data, bytes) else "application/json"})
            response = connection.getresponse()
            body = response.read()
            assert response.status in expected if isinstance(expected, tuple) else response.status == expected, (method, path, response.status, body[:1000])
            return json.loads(body) if body and response.status != 204 else None
        finally:
            connection.close()

    def image(tag):
        config = json.dumps({"architecture": architecture, "os": "linux",
                             "config": {"Cmd": ["/unused"], "Labels": {"dev.asagiri.ancestor-test": tag}},
                             "rootfs": {"type": "layers", "diff_ids": []}}).encode()
        digest = hashlib.sha256(config).hexdigest()
        manifest = [{"Config": digest + ".json", "RepoTags": [tag], "Layers": []}]
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w") as archive:
            for name, body in [(digest + ".json", config), ("manifest.json", json.dumps(manifest).encode())]:
                node = tarfile.TarInfo(name)
                node.size = len(body)
                archive.addfile(node, io.BytesIO(body))
        images.append("sha256:" + digest)
        request("POST", "/images/load", output.getvalue())
        return images[-1]

    def create(reference):
        value = request("POST", "/containers/create?" + urlencode({"name": prefix + "-" + str(len(containers))}),
                        {"Image": reference, "Labels": {"dev.asagiri.ancestor-test": prefix}}, 201)
        containers.append(value["Id"])
        return value["Id"]

    def filtered(name, selectors, expected):
        filters = {"ancestor": selectors, "label": ["dev.asagiri.ancestor-test=" + prefix]}
        rows = request("GET", "/containers/json?" + urlencode({"all": "1", "filters": json.dumps(filters)}))
        actual = sorted(row["Id"] for row in rows)
        record["cases"].append({"name": name, "expected": sorted(expected), "actual": actual})
        assert actual == sorted(expected), (name, actual, sorted(expected))

    try:
        version = request("GET", "/version")
        assert bool(version.get("AsagiriVersion")) is not args.reference_docker, "Unexpected engine"
        architecture = {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"}[version["Arch"]]
        record["engine"] = version
        first_tag, second_tag, alias = prefix + ":first", prefix + ":second", prefix + ":alias"
        first, second = image(first_tag), image(second_tag)
        by_id, by_tag, other = create(first), create(first_tag), create(second)
        request("POST", "/images/" + quote(first, safe="") + "/tag?" + urlencode({"repo": prefix, "tag": "alias"}), expected=201)
        filtered("tag-finds-ID-created-container", [first_tag], [by_id, by_tag])
        filtered("new-alias", [alias], [by_id, by_tag])
        filtered("full-ID", [first], [by_id, by_tag])
        filtered("short-ID", [first.removeprefix("sha256:")[:12]], [by_id, by_tag])
        filtered("multiple-selectors", [alias, second_tag], [by_id, by_tag, other])
        filtered("unknown-image", [prefix + ":missing"], [])
        request("POST", "/images/" + quote(second, safe="") + "/tag?" + urlencode({"repo": prefix, "tag": "first"}), expected=201)
        filtered("moved-tag-uses-current-image", [first_tag], [other])
        filtered("retained-alias", [alias], [by_id, by_tag])
        record["status"] = "passed"
    except Exception as error:
        record.update(status="failed", error=repr(error))
    finally:
        for kind, identifiers in [("containers", containers), ("images", images)]:
            for identifier in reversed(identifiers):
                try:
                    request("DELETE", "/" + kind + "/" + quote(identifier, safe="") + "?force=1", expected=(200, 204, 404))
                    record["cleanup"].append({"kind": kind, "id": identifier, "passed": True})
                except Exception as error:
                    record.update(status="failed")
                    record["cleanup"].append({"kind": kind, "id": identifier, "error": repr(error)})
        record["seconds"] = time.monotonic() - started
        if args.output:
            args.output.write_text(json.dumps(record, indent=2) + "\n")
        print(json.dumps(record, indent=2))
    assert record["status"] == "passed", "Inspect the ancestor filter evidence"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("--reference-docker", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.output:
        assert not args.output.exists(), "Preserve existing evidence"
    check(args)
