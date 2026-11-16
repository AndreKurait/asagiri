#!/usr/bin/env python3
"""Exercise concurrent shared-layer loads, creation, and failed-load recovery."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import io
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tarfile
import uuid

helpers = runpy.run_path(str(Path(__file__).resolve().parents[1] / "scripts/bench-layer-concurrency.py"))
Connection, layer, image_archive = (helpers[name] for name in ("Connection", "layer", "image_archive"))


def check(path):
    assert sys.platform == "linux" and os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "private Linux test engine only"
    prefix = "asagiri-layer-test-" + uuid.uuid4().hex[:10]
    containers, images = [], []
    seed = prefix + "-data"
    good = prefix + ":good"
    bad = prefix + ":bad"
    layer_root = Path("/var/lib/asagiri/layers")

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

    def load(tag, layers):
        status, response = api("POST", "/images/load", image_archive(tag, layers), binary=True)
        assert status == 200, (status, response)
        rows = [json.loads(line) for line in response.splitlines() if line]
        errors = [row["error"] for row in rows if row.get("error")]
        complete = [row for row in rows if row.get("stream", "").startswith("Loaded image")]
        if tag == bad:
            assert errors and not complete, rows
        else:
            assert not errors and complete, rows
            for _, blob in layers:
                name = hashlib.sha256(blob).hexdigest()
                assert (layer_root / name).is_dir() and (layer_root / (name + ".done")).is_file()

    try:
        valid = layer(seed, 8)
        corrupt = b"\x1f\x8bcorrupt" + seed.encode()
        invalid = ("sha256:" + hashlib.sha256(corrupt).hexdigest(), corrupt)
        assert not (layer_root / hashlib.sha256(valid[1]).hexdigest()).exists(), "shared layer must be cold"
        good_tags = [good] + [prefix + ":alias-" + str(index) for index in range(6)]
        images.extend([*good_tags, bad])
        with ThreadPoolExecutor(max_workers=8) as pool:
            loaded = [pool.submit(load, tag, [valid]) for tag in good_tags]
            failed_load = pool.submit(load, bad, [invalid])
            failed_load.result()
            for future in loaded:
                future.result()
        with ThreadPoolExecutor(max_workers=8) as pool:
            futures = [pool.submit(create, tag) for tag in good_tags]
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
        print("PASS concurrent shared loads finish unpacking, complete contents, and recovery after a corrupt transfer", flush=True)
    finally:
        for cid in containers:
            api("DELETE", "/containers/" + cid + "?force=true")
        for tag in images:
            api("DELETE", "/images/" + tag)


if __name__ == "__main__":
    check(sys.argv[1])
