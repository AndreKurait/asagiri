#!/usr/bin/env python3
"""Measure cold/warm concurrent creates and exec latency during layer unpack.

Run beside a disposable Linux engine; no container ports or host mounts are
used. The caller must pull alpine:3.21 first. Synthetic images are created and
removed by this script. Timings are evidence, not portable performance limits.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import gzip
import hashlib
import http.client
import io
import json
import platform
import socket
import statistics
import subprocess
import tarfile
import threading
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=120)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def image_archive(tag, layers):
    config = {
        "architecture": "arm64" if platform.machine() == "aarch64" else "amd64",
        "os": "linux", "config": {"Cmd": ["true"]},
        "rootfs": {"type": "layers", "diff_ids": [digest for digest, _ in layers]},
        "history": [{"created_by": "synthetic layer benchmark"} for _ in layers],
    }
    config = json.dumps(config).encode()
    config_name = hashlib.sha256(config).hexdigest() + ".json"
    names = [f"{index}/layer.tar" for index in range(len(layers))]
    manifest = [{"Config": config_name, "RepoTags": [tag], "Layers": names}]
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        entries = [(config_name, config), ("manifest.json", json.dumps(manifest).encode())]
        entries += [(name, data) for name, (_, data) in zip(names, layers)]
        for name, data in entries:
            header = tarfile.TarInfo(name)
            header.size = len(data)
            archive.addfile(header, io.BytesIO(data))
    return stream.getvalue()


def layer(seed, files):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        for index in range(files):
            content = hashlib.sha256(f"{seed}/{index}".encode()).digest() * 256
            header = tarfile.TarInfo(f"bench/{seed}/{index}")
            header.size, header.mode = len(content), 0o644
            archive.addfile(header, io.BytesIO(content))
    plain = stream.getvalue()
    return "sha256:" + hashlib.sha256(plain).hexdigest(), gzip.compress(plain, mtime=0)


def summary(samples):
    ordered = sorted(samples)
    return {
        "count": len(samples), "median_ms": statistics.median(samples),
        "p95_ms": ordered[min(len(ordered) - 1, int(len(ordered) * 0.95))],
        "max_ms": max(samples),
    } if samples else {"count": 0}


def bench(args):
    prefix = "asagiri-layer-bench-" + uuid.uuid4().hex[:10]
    docker = ["docker", "--host", "unix://" + args.socket]
    containers, images = [], []

    def cli(*command):
        return subprocess.run(docker + list(command), capture_output=True, check=True,
                              text=True, timeout=120).stdout.strip()

    def api(method, path, body=None, binary=False):
        conn = Connection(args.socket)
        try:
            data = body if binary else json.dumps(body).encode() if body is not None else None
            headers = {"Content-Type": "application/x-tar" if binary else "application/json"} if data is not None else {}
            conn.request(method, path, data, headers)
            response = conn.getresponse()
            payload = response.read()
            assert response.status in (200, 201, 204), (method, path, response.status, payload)
            return payload
        finally:
            conn.close()

    def create(tag):
        start = time.perf_counter()
        data = json.loads(api("POST", "/containers/create", {
            "Image": tag, "HostConfig": {"NetworkMode": "none"},
        }))
        containers.append(data["Id"])
        return data["Id"], (time.perf_counter() - start) * 1000

    results = {"cold_create_ms": [], "warm_create_ms": [], "cold_batch_ms": [],
               "warm_batch_ms": [], "exec_during_cold_ms": [], "exec_idle_ms": []}
    stop = threading.Event()
    failures = []
    parent = cli("run", "-d", "--network", "none", "alpine:3.21", "sleep", "300")
    containers.append(parent)

    def probe():
        while not stop.is_set():
            start = time.perf_counter()
            try:
                cli("exec", parent, "true")
            except Exception as error:
                failures.append(str(error))
                return
            results["exec_during_cold_ms"].append((time.perf_counter() - start) * 1000)
            stop.wait(0.02)

    try:
        for _ in range(10):
            start = time.perf_counter()
            cli("exec", parent, "true")
            results["exec_idle_ms"].append((time.perf_counter() - start) * 1000)
        for round_index in range(args.rounds):
            tags = []
            for index in range(args.images):
                seed = f"{prefix}-{round_index}-{index}"
                layers = [layer(f"{seed}-{n}", args.files) for n in range(args.layers)]
                tag = seed + ":test"
                response = api("POST", "/images/load", image_archive(tag, layers), binary=True)
                assert all(not json.loads(line).get("error") for line in response.splitlines() if line), response
                images.append(tag)
                tags.append(tag)
            for phase in ("cold", "warm"):
                stop.clear()
                thread = threading.Thread(target=probe) if phase == "cold" else None
                if thread:
                    thread.start()
                started = time.perf_counter()
                try:
                    with ThreadPoolExecutor(max_workers=args.images) as pool:
                        for _, elapsed in pool.map(create, tags):
                            results[phase + "_create_ms"].append(elapsed)
                    results[phase + "_batch_ms"].append((time.perf_counter() - started) * 1000)
                finally:
                    stop.set()
                    if thread:
                        thread.join()
                for cid in containers[1:]:
                    cli("rm", "-f", cid)
                del containers[1:]
        assert not failures, failures
        print(json.dumps({
            "workload": {"rounds": args.rounds, "parallel_images": args.images,
                         "layers_per_image": args.layers, "files_per_layer": args.files,
                         "bytes_per_file": 8192},
            "measurements": {key: summary(values) for key, values in results.items()},
        }, indent=2))
    finally:
        for cid in containers:
            cli("rm", "-f", cid)
        for tag in images:
            cli("image", "rm", tag)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--images", type=int, default=4)
    parser.add_argument("--layers", type=int, default=3)
    parser.add_argument("--files", type=int, default=128)
    args = parser.parse_args()
    assert min(args.rounds, args.images, args.layers, args.files) > 0
    bench(args)
