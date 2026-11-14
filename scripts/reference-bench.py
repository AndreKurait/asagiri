#!/usr/bin/env python3
"""Compare Docker-compatible Unix endpoints using the same API workload."""
import argparse
from datetime import datetime, timezone
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import platform
import re
import socket
import statistics
import time
import urllib.parse
import uuid

IMAGE = "alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab"
LABEL = "dev.asagiri.reference-benchmark"
MAX_RESPONSE = 4 << 20
WORKLOAD = "docker-api-reference-v1"


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(60)
        self.sock.connect(self.path)


class Engine:
    def __init__(self, spec, nonce):
        self.name, self.path = spec.split("=", 1)
        if not re.fullmatch(r"[A-Za-z][A-Za-z0-9_-]{0,31}", self.name):
            raise ValueError("Engine names must be short letters, numbers, hyphens or underscores")
        self.path = self.path.removeprefix("unix://")
        if not self.path.startswith("/"):
            raise ValueError("Use an absolute Unix socket path or unix:///path")
        self.conn = UnixHTTP(self.path)
        self.nonce, self.sequence = nonce, 0
        filters = json.dumps({"label": [LABEL + "=" + nonce]}, separators=(",", ":"))
        self.list_path = "/containers/json?all=1&" + urllib.parse.urlencode({"filters": filters})

    def call(self, method, path, body=None, statuses=(200, 201, 204, 304)):
        encoded = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json"} if body is not None else {}
        # Never retry a mutation after losing its response. The final label
        # sweep also finds a create whose response was lost.
        for attempt in range(2 if method in ("GET", "HEAD") else 1):
            try:
                self.conn.request(method, path, body=encoded, headers=headers)
                response = self.conn.getresponse()
                data = response.read(MAX_RESPONSE + 1)
                if len(data) > MAX_RESPONSE:
                    raise ValueError("Engine response exceeds the measurement limit")
                if response.status not in statuses:
                    raise RuntimeError(f"{self.name}: {method} {path}: {response.status} {data[:512]!r}")
                return data
            except (http.client.HTTPException, OSError):
                self.conn.close()
                self.conn = UnixHTTP(self.path)
                if attempt or method not in ("GET", "HEAD"):
                    raise

    def prepare(self):
        version = json.loads(self.call("GET", "/version"))
        info = json.loads(self.call("GET", "/info"))
        pulled = self.call("POST", "/images/create?" + urllib.parse.urlencode({"fromImage": IMAGE}))
        for line in pulled.splitlines():
            if line.strip() and json.loads(line).get("error"):
                raise RuntimeError(self.name + ": image pull failed: " + line.decode()[:512])
        image = json.loads(self.call("GET", "/images/" + urllib.parse.quote(IMAGE, safe="") + "/json"))
        return {
            "version": {key: version.get(key) for key in (
                "Version", "ApiVersion", "MinAPIVersion", "Os", "Arch", "AsagiriVersion", "Components")},
            "engine": {key: info.get(key) for key in (
                "NCPU", "MemTotal", "Driver", "DriverStatus", "KernelVersion", "OperatingSystem", "Architecture")},
            "image": {key: image.get(key) for key in ("Id", "RepoDigests", "Architecture", "Os", "RootFS")},
        }

    def run_once(self):
        self.sequence += 1
        name = f"asagiri-reference-{self.nonce}-{self.name.lower()}-{self.sequence}"
        body = {"Image": IMAGE, "Cmd": ["true"], "Labels": {LABEL: self.nonce},
                "HostConfig": {"NetworkMode": "none", "AutoRemove": False}}
        created = json.loads(self.call("POST", "/containers/create?name=" + name, body))
        cid = created["Id"]
        if not re.fullmatch(r"[a-fA-F0-9]{12,64}", cid):
            raise ValueError("Invalid created container ID")
        self.call("POST", f"/containers/{cid}/start")
        waited = json.loads(self.call("POST", f"/containers/{cid}/wait?condition=not-running"))
        if waited.get("StatusCode") != 0 or waited.get("Error"):
            raise RuntimeError(self.name + ": the measured container failed: " + repr(waited))
        self.call("DELETE", f"/containers/{cid}?force=1")

    def cleanup(self):
        # The filter is only an optimization: verify ownership before removing
        # each item, even if an endpoint ignores its filters.
        for item in json.loads(self.call("GET", self.list_path)):
            if (item.get("Labels") or {}).get(LABEL) != self.nonce:
                continue
            cid = item["Id"]
            if re.fullmatch(r"[a-fA-F0-9]{12,64}", cid):
                self.call("DELETE", f"/containers/{cid}?force=1", statuses=(204, 404))
        self.conn.close()


def summary(values):
    if not values or any(not math.isfinite(v) or v < 0 for v in values):
        raise ValueError("Timing samples must be finite, non-negative and nonempty")
    return {"median": statistics.median(values), "p95": sorted(values)[math.ceil(len(values) * .95) - 1],
            "samples": len(values)}


def timed(function):
    start = time.perf_counter_ns()
    function()
    return (time.perf_counter_ns() - start) / 1_000_000


def host():
    cpu = platform.processor()
    if Path("/proc/cpuinfo").is_file():
        text = Path("/proc/cpuinfo").read_text()
        match = re.search(r"^model name\s*:\s*(.+)$", text, re.M)
        if match:
            cpu = match[1]
    return {"system": platform.system(), "architecture": platform.machine(),
            "kernel": platform.release(), "cpu_model": cpu, "cpus": os.cpu_count(),
            "python": platform.python_version(),
            "runner_image": os.environ.get("ImageOS"), "runner_version": os.environ.get("ImageVersion")}


def measure(specs, runs, provenance=None):
    if not 20 <= runs <= 4000 or not 2 <= len(specs) <= 8:
        raise ValueError("Use 2–8 engines and 20–4000 rounds")
    nonce = uuid.uuid4().hex[:16]
    engines = [Engine(spec, nonce) for spec in specs]
    if len({engine.name for engine in engines}) != len(engines):
        raise ValueError("Engine names must be unique")
    if len({str(Path(engine.path).resolve()) for engine in engines}) != len(engines):
        raise ValueError("Each engine needs a different socket")
    metadata = {}
    samples = {e.name: {key: [] for key in ("ping", "list", "run")} for e in engines}
    try:
        for engine in engines:
            metadata[engine.name] = engine.prepare()
        # Containerd-backed Docker uses a manifest ID; classic stores use
        # the config ID. Compare platform and unpacked layer content instead.
        identities = {(m["image"]["Architecture"], m["image"]["Os"],
                       tuple((m["image"].get("RootFS") or {}).get("Layers", [])))
                      for m in metadata.values()}
        if len(identities) != 1 or not next(iter(identities))[2]:
            raise ValueError("Engines did not load the same image for the same architecture")
        for index in range(10):
            for engine in engines[index % len(engines):] + engines[:index % len(engines)]:
                engine.run_once()
        time.sleep(3)
        for index in range(runs):
            # Rotate the first engine and reverse each complete cycle.
            order = engines[index % len(engines):] + engines[:index % len(engines)]
            if index // len(engines) % 2:
                order.reverse()
            for engine in order:
                row = samples[engine.name]
                row["ping"].append(timed(lambda: engine.call("GET", "/_ping")))
                row["list"].append(timed(lambda: engine.call("GET", engine.list_path)))
                row["run"].append(timed(engine.run_once))
        return {"schema": 1, "workload": WORKLOAD, "measured_at": datetime.now(timezone.utc).isoformat(),
                "image": IMAGE, "network": "none", "rounds": runs, "warmup_runs": 10,
                "connection": "persistent Unix HTTP", "host": host(), "engines": metadata,
                "provenance": provenance or {}, "samples_ms": samples,
                "metrics_ms": {name: {key: summary(values) for key, values in row.items()}
                               for name, row in samples.items()}}
    finally:
        errors = []
        for engine in engines:
            try:
                engine.cleanup()
            except Exception as error:
                errors.append(f"{engine.name}: {error}")
        if errors:
            raise RuntimeError("Benchmark cleanup failed: " + "; ".join(errors))


def report(result):
    lines = ["# Container engine API comparison", "",
             "Measured " + result["measured_at"] + ". " + str(result["rounds"]) + " rounds per engine.",
             "", "All engines load the same pinned Alpine image. Containers run `true` with networking disabled.",
             "Each lifecycle measures create, start, wait and remove; list requests select this run's containers.",
             "Warmup runs are excluded. Engine order rotates. Results describe these endpoints and settings.",
             "", "| Engine | Lifecycle median / p95 | Ping median / p95 | List median / p95 |",
             "|---|---:|---:|---:|"]
    for name, metrics in result["metrics_ms"].items():
        pairs = [f"{metrics[key]['median']:.3f} / {metrics[key]['p95']:.3f} ms" for key in ("run", "ping", "list")]
        lines.append("| " + name + " | " + " | ".join(pairs) + " |")
    lines += ["", "The JSON includes every raw timing, image identity, engine version, storage driver,",
              "reported CPU/memory, host details and workflow/source provenance.",
              "Use separate runs to measure VM startup, idle VM memory, builds, networking or disk throughput.", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", action="append", required=True, help="NAME=unix:///absolute/socket")
    parser.add_argument("--runs", type=int, default=400)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--provenance", type=Path)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    provenance = json.loads(args.provenance.read_text()) if args.provenance else {}
    provenance["harness_sha256"] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    result = measure(args.engine, args.runs, provenance)
    (args.out / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")
    (args.out / "report.md").write_text(report(result))
    print(report(result))


if __name__ == "__main__":
    main()
