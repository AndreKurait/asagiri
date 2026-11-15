#!/usr/bin/env python3
"""Check image-inspect Healthcheck omission against an explicit engine socket.

Loads only zero-layer metadata fixtures; no container is started. This also
runs unchanged against Docker to check the reference response.
"""
import hashlib
import http.client
import io
import json
import os
import socket
import subprocess
import sys
import tarfile
import uuid
from urllib.parse import urlencode


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path):
    env = dict(os.environ)
    env.pop("DOCKER_CONTEXT", None)

    def cli(*args, data=None):
        result = subprocess.run(
            ["docker", "--host", "unix://" + path, *args], env=env,
            input=data, capture_output=True, timeout=30)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result.stdout

    def listed(tag, labels=()):
        filters = {"reference": [tag]}
        if labels:
            filters["label"] = list(labels)
        connection = Connection(path)
        try:
            connection.request("GET", "/v1.47/images/json?" + urlencode({"filters": json.dumps(filters)}))
            response = connection.getresponse()
            body = response.read()
            assert response.status == 200, (response.status, body)
            return json.loads(body)
        finally:
            connection.close()

    cases = [
        ("absent", {}, None),
        ("null", {"Healthcheck": None}, None),
        ("metadata", {"Labels": {"org.opencontainers.image.title": "Asagiri fixture", "vendor.feature": "enabled"},
                      "ExposedPorts": {"8080/tcp": {}}, "StopSignal": "SIGQUIT"}, None),
        ("disabled", {"Healthcheck": {"Test": ["NONE"]}}, {"Test": ["NONE"]}),
        ("configured", {"Healthcheck": {
            "Test": ["CMD", "true"], "Interval": 1000000000,
            "Timeout": 2000000000, "Retries": 3,
            "StartPeriod": 4000000000, "StartInterval": 5000000000,
        }}, None),
    ]
    for case, config, expected in cases:
        if case == "configured":
            expected = config["Healthcheck"]
        tag = "asagiri-healthcheck-" + uuid.uuid4().hex[:12] + ":test"
        blob = json.dumps({
            "architecture": "arm64", "os": "linux", "config": config,
            "rootfs": {"type": "layers", "diff_ids": []},
        }).encode()
        name = hashlib.sha256(blob).hexdigest() + ".json"
        entries = {
            name: blob,
            "manifest.json": json.dumps([{
                "Config": name, "RepoTags": [tag], "Layers": [],
            }]).encode(),
        }
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            for filename, data in entries.items():
                item = tarfile.TarInfo(filename)
                item.size = len(data)
                tar.addfile(item, io.BytesIO(data))
        try:
            cli("image", "load", data=archive.getvalue())
            actual = json.loads(cli("image", "inspect", tag))[0]["Config"]
            if expected is None:
                assert "Healthcheck" not in actual, (case, actual)
            else:
                assert actual.get("Healthcheck") == expected, (case, actual)
            if case == "metadata":
                for key in ("Labels", "ExposedPorts", "StopSignal"):
                    assert actual.get(key) == config[key], (key, actual)
                for labels in [("vendor.feature",), ("vendor.feature=enabled",),
                               ("vendor.feature=enabled", "org.opencontainers.image.title=Asagiri fixture")]:
                    assert listed(tag, labels), (tag, labels)
                for labels in [("vendor.feature=disabled",), ("missing.label",),
                               ("vendor.feature=enabled", "missing.label")]:
                    assert not listed(tag, labels), (tag, labels)
            rows = listed(tag)
            assert len(rows) == 1 and rows[0]["Labels"] == config.get("Labels"), (case, rows)
            print("PASS image inspect/list labels and Healthcheck " + case, flush=True)
        finally:
            cli("image", "rm", tag)


if __name__ == "__main__":
    check(sys.argv[1])
