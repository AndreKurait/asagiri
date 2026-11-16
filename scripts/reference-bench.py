#!/usr/bin/env python3
"""Compare Docker-compatible Unix endpoints using the same API workload."""
import argparse
from datetime import datetime, timezone
import hashlib
import http.client
import io
import json
import math
import os
from pathlib import Path
import platform
import re
import socket
import statistics
import struct
import tarfile
import time
import urllib.parse
import uuid

IMAGE = "alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab"
LABEL = "dev.asagiri.reference-benchmark"
MAX_RESPONSE = 4 << 20
WORKLOAD = "docker-api-reference-v2"
COPY_PAYLOAD = bytes(range(256)) * 256
COPY_SHA256 = hashlib.sha256(COPY_PAYLOAD).hexdigest()
EXEC_OUTPUT = b"asagiri-reference-exec"
SERVICE_ROOT = "/asagiri-reference"
WORKLOADS = {
    "ping": "GET /_ping",
    "list": "List this run's containers when none exist",
    "run": "Create, start, wait for and remove an Alpine true container",
    "exec": "Create, start, read and inspect a successful exec in a running container",
    "copy": "Upload and download a 64 KiB file in a named volume; verify its SHA-256",
}


class APIError(RuntimeError):
    def __init__(self, engine, method, path, status, data):
        self.status = status
        super().__init__(f"{engine}: {method} {path}: {status} {data[:512]!r}")


def copy_archive(payload=COPY_PAYLOAD):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        entry = tarfile.TarInfo("payload")
        entry.size, entry.mode, entry.mtime = len(payload), 0o644, 0
        archive.addfile(entry, io.BytesIO(payload))
    return output.getvalue()


def verify_copy(data):
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:") as archive:
        entries = archive.getmembers()
        if len(entries) != 1 or entries[0].name.removeprefix("./") != "payload" or not entries[0].isfile():
            raise ValueError("Copy returned an unexpected archive member")
        if entries[0].size != len(COPY_PAYLOAD):
            raise ValueError("Copy returned the wrong file length")
        payload = archive.extractfile(entries[0]).read(len(COPY_PAYLOAD) + 1)
    if hashlib.sha256(payload).hexdigest() != COPY_SHA256:
        raise ValueError("Copy round trip changed the file bytes")


def stream_output(data):
    output = bytearray()
    while data:
        if len(data) < 8 or data[0] not in (1, 2) or data[1:4] != b"\0\0\0":
            raise ValueError("Exec returned an invalid Docker stream header")
        size = struct.unpack(">I", data[4:8])[0]
        if size > len(data) - 8:
            raise ValueError("Exec returned a truncated Docker stream")
        output.extend(data[8:8 + size])
        data = data[8 + size:]
    return bytes(output)


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
        self.service, self.volume = None, None
        self.archive = copy_archive()
        filters = json.dumps({"label": [LABEL + "=" + nonce]}, separators=(",", ":"))
        self.list_path = "/containers/json?all=1&" + urllib.parse.urlencode({"filters": filters})

    def call(self, method, path, body=None, statuses=(200, 201, 204, 304), *, raw=None):
        if body is not None and raw is not None:
            raise ValueError("A request cannot contain both JSON and an archive")
        encoded = raw if raw is not None else json.dumps(body).encode() if body is not None else None
        headers = ({"Content-Type": "application/x-tar"} if raw is not None
                   else {"Content-Type": "application/json"} if body is not None else {})
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
                    raise APIError(self.name, method, path, response.status, data)
                return data
            except (http.client.HTTPException, OSError):
                self.conn.close()
                self.conn = UnixHTTP(self.path)
                if attempt or method not in ("GET", "HEAD"):
                    raise

    def prepare(self, *, pull=True):
        version = json.loads(self.call("GET", "/version"))
        info = json.loads(self.call("GET", "/info"))
        if pull:
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

    def prepare_service(self):
        if self.service is not None or self.volume is not None:
            raise RuntimeError("The workload service is already initialized")
        self.volume = f"asagiri-reference-{self.nonce}-{self.name.lower()}-data"
        self.call("POST", "/volumes/create",
                  {"Name": self.volume, "Driver": "local", "Labels": {LABEL: self.nonce}})
        name = f"asagiri-reference-{self.nonce}-{self.name.lower()}-service"
        body = {
            "Image": IMAGE, "Entrypoint": [], "Cmd": ["sleep", "2147483647"],
            "Labels": {LABEL: self.nonce},
            "HostConfig": {"NetworkMode": "none", "AutoRemove": False,
                           "Mounts": [{"Type": "volume", "Source": self.volume, "Target": SERVICE_ROOT,
                                       "VolumeOptions": {"NoCopy": True}}]},
        }
        created = json.loads(self.call("POST", "/containers/create?name=" + name, body))
        if not re.fullmatch(r"[a-fA-F0-9]{12,64}", created["Id"]):
            raise ValueError("Invalid workload container ID")
        self.service = created["Id"]
        self.call("POST", f"/containers/{self.service}/start")

    def exec_once(self):
        if self.service is None:
            raise RuntimeError("The workload service has not been initialized")
        created = json.loads(self.call("POST", f"/containers/{self.service}/exec", {
            "AttachStdout": True, "AttachStderr": True, "Tty": False,
            "Cmd": ["sh", "-c", "printf asagiri-reference-exec"],
        }))
        eid = created["Id"]
        if not re.fullmatch(r"[a-fA-F0-9]{12,64}", eid):
            raise ValueError("Invalid exec ID")
        output = self.call("POST", f"/exec/{eid}/start", {"Detach": False, "Tty": False})
        if stream_output(output) != EXEC_OUTPUT:
            raise ValueError("The measured exec did not return its expected output")
        deadline = time.monotonic() + 10
        while True:
            state = json.loads(self.call("GET", f"/exec/{eid}/json"))
            if state.get("Running") is False:
                if state.get("ExitCode") != 0:
                    raise RuntimeError("The measured exec failed: " + repr(state))
                return
            if time.monotonic() >= deadline:
                raise TimeoutError("The measured exec did not finish")
            time.sleep(0.001)

    def copy_once(self):
        if self.service is None:
            raise RuntimeError("The workload service has not been initialized")
        root = urllib.parse.urlencode({"path": SERVICE_ROOT})
        self.call("PUT", f"/containers/{self.service}/archive?{root}", raw=self.archive)
        path = urllib.parse.urlencode({"path": SERVICE_ROOT + "/payload"})
        verify_copy(self.call("GET", f"/containers/{self.service}/archive?{path}"))

    def ping_once(self):
        if self.call("GET", "/_ping").strip() != b"OK":
            raise ValueError("The measured ping did not return OK")

    def list_once(self):
        if json.loads(self.call("GET", self.list_path)) != []:
            raise ValueError("The measured filtered container list was not empty")

    def operation(self, metric):
        return {"ping": self.ping_once, "list": self.list_once,
                "run": self.run_once, "exec": self.exec_once, "copy": self.copy_once}[metric]

    def cleanup(self):
        # The filter is only an optimization: verify ownership before removing
        # each item, even if an endpoint ignores its filters.
        errors, absent = [], []
        volume_absent = self.volume is None
        try:
            try:
                items = json.loads(self.call("GET", self.list_path))
                for item in items:
                    if (item.get("Labels") or {}).get(LABEL) != self.nonce:
                        continue
                    cid = item["Id"]
                    if re.fullmatch(r"[a-fA-F0-9]{12,64}", cid):
                        try:
                            self.call("DELETE", f"/containers/{cid}?force=1", statuses=(204, 404))
                            self.call("GET", f"/containers/{cid}/json", statuses=(404,))
                            absent.append(cid)
                        except Exception as error:
                            errors.append(str(error))
            except Exception as error:
                errors.append(str(error))
            if self.volume is not None:
                path = "/volumes/" + urllib.parse.quote(self.volume, safe="")
                try:
                    try:
                        volume = json.loads(self.call("GET", path))
                    except APIError as error:
                        if error.status != 404:
                            raise
                        volume = None
                    if volume is not None:
                        if volume.get("Name") != self.volume or (volume.get("Labels") or {}).get(LABEL) != self.nonce:
                            raise RuntimeError("Workload volume ownership differs; it was left intact")
                        self.call("DELETE", path, statuses=(204, 404))
                        self.call("GET", path, statuses=(404,))
                    volume_absent = True
                except Exception as error:
                    errors.append(str(error))
        finally:
            self.conn.close()
        if errors:
            raise RuntimeError("; ".join(errors))
        return {"containers_verified_absent": absent, "volume_verified_absent": volume_absent}


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


def measure(specs, runs, provenance=None, *, pull=True):
    if not 20 <= runs <= 4000 or not 2 <= len(specs) <= 8:
        raise ValueError("Use 2–8 engines and 20–4000 rounds")
    nonce = uuid.uuid4().hex[:16]
    engines = [Engine(spec, nonce) for spec in specs]
    if len({engine.name for engine in engines}) != len(engines):
        raise ValueError("Engine names must be unique")
    if len({str(Path(engine.path).resolve()) for engine in engines}) != len(engines):
        raise ValueError("Each engine needs a different socket")
    metadata = {}
    samples = {e.name: {key: [] for key in WORKLOADS} for e in engines}
    cleanup = {}
    try:
        for engine in engines:
            metadata[engine.name] = engine.prepare(pull=pull)
        # Containerd-backed Docker uses a manifest ID; classic stores use
        # the config ID. Compare platform and unpacked layer content instead.
        identities = {(m["image"]["Architecture"], m["image"]["Os"],
                       tuple((m["image"].get("RootFS") or {}).get("Layers", [])))
                      for m in metadata.values()}
        if len(identities) != 1 or not next(iter(identities))[2]:
            raise ValueError("Engines did not load the same image for the same architecture")
        # Keep the original empty-list/lifecycle phase free of service fixtures.
        # Each phase compares the same work across every engine in rotating order.
        for metrics in (("ping", "list", "run"), ("exec", "copy")):
            if metrics[0] == "exec":
                for engine in engines:
                    engine.prepare_service()
            for index in range(10):
                for engine in engines[index % len(engines):] + engines[:index % len(engines)]:
                    for metric in metrics:
                        engine.operation(metric)()
            time.sleep(3)
            for index in range(runs):
                order = engines[index % len(engines):] + engines[:index % len(engines)]
                if index // len(engines) % 2:
                    order.reverse()
                for engine in order:
                    for metric in metrics:
                        samples[engine.name][metric].append(timed(engine.operation(metric)))
        result = {"schema": 2, "workload": WORKLOAD, "workloads": WORKLOADS,
                 "measured_at": datetime.now(timezone.utc).isoformat(),
                "image": IMAGE, "network": "none", "rounds": runs, "warmup_runs": 10,
                 "copy": {"payload_bytes": len(COPY_PAYLOAD), "sha256": COPY_SHA256,
                          "volume_driver": "local", "includes_client_archive_and_hash_validation": True},
                 "connection": "Unix HTTP, reusing connections where the protocol permits",
                 "host": host(), "engines": metadata,
                "provenance": provenance or {}, "samples_ms": samples,
                "metrics_ms": {name: {key: summary(values) for key, values in row.items()}
                               for name, row in samples.items()}}
    finally:
        errors = []
        for engine in engines:
            try:
                cleanup[engine.name] = engine.cleanup()
            except Exception as error:
                errors.append(f"{engine.name}: {error}")
        if errors:
            raise RuntimeError("Benchmark cleanup failed: " + "; ".join(errors))
    result["cleanup"] = cleanup
    return result


def report(result):
    lines = ["# Container engine API comparison", "",
             "Measured " + result["measured_at"] + ". " + str(result["rounds"]) + " rounds per engine.",
             "", "All engines load the same pinned Alpine image. Every fixture has networking disabled.",
             "The first phase measures ping, an empty filtered list, and create/start/wait/remove.",
             "The second uses one running container per engine: exec verifies output and exit status;",
             "copy uploads and downloads a 64 KiB file through a named volume and verifies its SHA-256.",
             "Service and volume setup are outside timing. Each phase excludes ten warmup rounds and rotates engine order.",
             "", "| Workload | Engine | Median | p95 |",
             "|---|---|---:|---:|"]
    for key in WORKLOADS:
        for name, metrics in result["metrics_ms"].items():
            metric = metrics[key]
            lines.append(f"| {key} | {name} | {metric['median']:.3f} ms | {metric['p95']:.3f} ms |")
    lines += ["", "The JSON includes every raw timing, image identity, engine version, storage driver,",
              "reported CPU/memory, host details, workflow/source provenance and verified fixture cleanup.",
              "Use separate runs to measure VM startup, idle VM memory, builds, networking or disk throughput.", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", action="append", required=True, help="NAME=unix:///absolute/socket")
    parser.add_argument("--runs", type=int, default=400)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--provenance", type=Path)
    parser.add_argument("--no-pull", action="store_true", help="require the pinned image already present in every engine")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    provenance = json.loads(args.provenance.read_text()) if args.provenance else {}
    provenance["harness_sha256"] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    result = measure(args.engine, args.runs, provenance, pull=not args.no_pull)
    (args.out / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")
    (args.out / "report.md").write_text(report(result))
    print(report(result))


if __name__ == "__main__":
    main()
