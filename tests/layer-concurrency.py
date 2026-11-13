#!/usr/bin/env python3
"""Exercise shared-layer creation and recovery after an invalid layer."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import io
import json
from pathlib import Path
import runpy
import subprocess
import sys
import tarfile
import uuid

helpers = runpy.run_path(str(Path(__file__).resolve().parents[1] / "scripts/bench-layer-concurrency.py"))
Connection, layer, image_archive = (helpers[name] for name in ("Connection", "layer", "image_archive"))


def check(path):
    prefix = "asagiri-layer-test-" + uuid.uuid4().hex[:10]
    containers, images = [], []
    seed = prefix + "-data"
    good = prefix + ":good"
    bad = prefix + ":bad"

    def api(method, target, body=None, binary=False):
        connection = Connection(path)
        try:
            data = body if binary else json.dumps(body).encode() if body is not None else None
            connection.request(method, target, data, {
                "Content-Type": "application/x-tar" if binary else "application/json",
            })
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def create(tag):
        status, data = api("POST", "/containers/create", {
            "Image": tag, "HostConfig": {"NetworkMode": "none"},
        })
        if status == 201:
            cid = json.loads(data)["Id"]
            containers.append(cid)
            return cid
        assert tag == bad and status >= 400, (tag, status, data)
        return None

    try:
        valid = layer(seed, 8)
        invalid = ("sha256:" + hashlib.sha256(b"corrupt").hexdigest(), b"\x1f\x8bcorrupt")
        for tag, layers in ((good, [valid]), (bad, [invalid])):
            status, response = api("POST", "/images/load", image_archive(tag, layers), binary=True)
            assert status == 200, (status, response)
            assert all(not json.loads(line).get("error") for line in response.splitlines() if line), response
            images.append(tag)
        with ThreadPoolExecutor(max_workers=8) as pool:
            futures = [pool.submit(create, good) for _ in range(7)]
            failed = pool.submit(create, bad)
            assert failed.result() is None
            created = [future.result() for future in futures]
        for cid in created:
            archive = subprocess.run(
                ["docker", "--host", "unix://" + path, "cp", f"{cid}:/bench/{seed}/7", "-"],
                capture_output=True, check=True, timeout=30).stdout
            with tarfile.open(fileobj=io.BytesIO(archive)) as files:
                content = files.extractfile(files.getmembers()[0]).read()
            assert content == hashlib.sha256(f"{seed}/7".encode()).digest() * 256
        assert create(good), "a failed transfer must not poison subsequent creates"
        print("PASS concurrent shared layers, complete contents, and recovery after a corrupt transfer", flush=True)
    finally:
        for cid in containers:
            api("DELETE", "/containers/" + cid + "?force=true")
        for tag in images:
            api("DELETE", "/images/" + tag)


if __name__ == "__main__":
    check(sys.argv[1])
