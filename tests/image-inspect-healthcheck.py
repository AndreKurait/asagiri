#!/usr/bin/env python3
"""Check image-inspect Healthcheck omission against an explicit engine socket.

Loads only zero-layer metadata fixtures; no container is started. This also
runs unchanged against Docker to check the reference response.
"""
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import uuid


def check(path):
    env = dict(os.environ)
    env.pop("DOCKER_CONTEXT", None)

    def cli(*args, data=None):
        result = subprocess.run(
            ["docker", "--host", "unix://" + path, *args], env=env,
            input=data, capture_output=True, timeout=30)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result.stdout

    cases = [
        ("absent", {}, None),
        ("null", {"Healthcheck": None}, None),
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
            print("PASS image inspect Healthcheck " + case, flush=True)
        finally:
            cli("image", "rm", tag)


if __name__ == "__main__":
    check(sys.argv[1])
