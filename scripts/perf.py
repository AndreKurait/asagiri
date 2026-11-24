#!/usr/bin/env python3
"""Asagiri's speed check: main against a pull request, on the same machine.

Two Linux engines run side by side, one built from main and one from the
pull request, each in its own privileged container (see .github/workflows/
perf.yml). Requests alternate between them, main then the change, so
whatever else the machine is doing slows both alike, and the medians are
compared. Binary sizes are exact; memory and system-call counts can vary
with scheduling and asynchronous cleanup and have their own tolerances.

  perf.py bench --base SOCK:PID --head SOCK:PID [--runs N] --out results.json
  perf.py sizes --base DIR --head DIR --out sizes.json
  perf.py report results.json sizes.json    (markdown to stdout; exit 1 past a limit)

Only the standard library, so it runs on any machine with Python 3.
"""

import argparse
import hashlib
import http.client
import io
import json
import math
import os
import platform
import re
import signal
import socket
import statistics
import subprocess
import sys
import tarfile
import time

IMAGE = "alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab"
WORKLOAD = "linux-engine-v3"
TIMINGS = ("run", "ping", "list", "create", "copy")
METADATA_TAG = "asagiri-perf-metadata:v1"


def tar_bytes(entries):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in entries.items():
            member = tarfile.TarInfo(name)
            member.mode = 0o644
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    return output.getvalue()


def fixtures(architecture):
    """Deterministic OCI metadata and a 4 KiB file; no registry or clock input."""
    architecture = {"x86_64": "amd64", "aarch64": "arm64"}.get(architecture, architecture)
    config = json.dumps({
        "architecture": architecture, "os": "linux",
        "config": {"Cmd": ["true"], "StopSignal": "SIGTERM"},
        "rootfs": {"type": "layers", "diff_ids": []},
        "history": [{"created_by": "RUN " + "x" * 512, "empty_layer": True} for _ in range(128)],
    }, sort_keys=True, separators=(",", ":")).encode()
    checksum = hashlib.sha256(config).hexdigest()
    name = checksum + ".json"
    image = tar_bytes({name: config, "manifest.json": json.dumps([{
        "Config": name, "RepoTags": [METADATA_TAG], "Layers": [],
    }], sort_keys=True, separators=(",", ":")).encode()})
    upload = tar_bytes({"asagiri-perf-copy": b"x" * 4096})
    identities = {
        "config_sha256": checksum, "config_bytes": len(config), "history_entries": 128,
        "copy_sha256": hashlib.sha256(upload).hexdigest(), "copy_payload_bytes": 4096,
        "copy_tar_bytes": len(upload),
    }
    return image, upload, identities

# What fails a pull request: worse than main by more than both the share
# and the amount. The amount keeps tiny numbers' noise from failing a
# change; the share keeps big numbers honest.
LIMITS = {
    "run": (0.10, 0.5),            # docker run --rm alpine true: +10% and +0.5 ms
    "ping": (0.15, 0.05),          # GET /_ping: +15% and +0.05 ms
    "list": (0.15, 0.10),          # GET /containers/json: +15% and +0.1 ms
    "rss_idle": (0.10, 2048),      # daemon memory at rest: +10% and +2 MiB (KiB)
    "rss_after": (0.10, 4096),     # daemon memory after the runs: +10% and +4 MiB (KiB)
    "syscalls": (0.10, 50),        # system calls per container: +10% and +50
}


class UnixHTTP(http.client.HTTPConnection):
    """HTTP over the engine's unix socket, one connection kept open, as the
    docker CLI does."""

    def __init__(self, path):
        super().__init__("localhost", timeout=60)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(60)
        self.sock.connect(self.path)


class Engine:
    def __init__(self, spec):
        sock, pid = spec.rsplit(":", 1)
        self.sock, self.pid = sock, int(pid)
        self.conn = UnixHTTP(sock)

    def call(self, method, path, body=None, want=(200, 201, 204, 304)):
        data = body if isinstance(body, bytes) else json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/x-tar" if isinstance(body, bytes) else "application/json"} if data is not None else {}
        for attempt in range(2):
            try:
                self.conn.request(method, path, body=data, headers=headers)
                r = self.conn.getresponse()
                out = r.read()
                break
            except (http.client.HTTPException, OSError):
                # The engine closed a kept connection: open a new one, once.
                self.conn.close()
                self.conn = UnixHTTP(self.sock)
                if attempt:
                    raise
        if r.status not in want:
            raise RuntimeError(f"{method} {path}: {r.status} {out[:200]!r}")
        return out

    def run_once(self):
        """docker run --rm alpine true, as its API calls: create, start,
        wait for the exit, remove."""
        marks = [time.perf_counter_ns()]
        cid = json.loads(self.call("POST", "/containers/create", {"Image": IMAGE, "Cmd": ["true"]}))["Id"]
        marks.append(time.perf_counter_ns())
        self.call("POST", f"/containers/{cid}/start")
        marks.append(time.perf_counter_ns())
        status = json.loads(self.call("POST", f"/containers/{cid}/wait"))["StatusCode"]
        marks.append(time.perf_counter_ns())
        if status != 0:
            raise RuntimeError(f"Benchmark container exited with {status}")
        self.call("DELETE", f"/containers/{cid}?force=1")
        marks.append(time.perf_counter_ns())
        return [(b - a) / 1e6 for a, b in zip(marks, marks[1:])]


    def create_once(self):
        """Create/remove with a large OCI history, without starting a process."""
        cid = json.loads(self.call("POST", "/containers/create", {
            "Image": METADATA_TAG, "Cmd": ["true"], "HostConfig": {"NetworkMode": "none"},
        }))["Id"]
        self.call("DELETE", f"/containers/{cid}?force=1&v=1")

    def copy_once(self):
        """A framed upload followed by a request on the same pooled client."""
        self.call("PUT", f"/containers/{self.copy_container}/archive?path=/tmp", self.copy_archive)
        self.call("GET", "/_ping")

    def rss_kib(self):
        with open(f"/proc/{self.pid}/status") as f:
            m = re.search(r"VmRSS:\s+(\d+)", f.read())
        if not m:
            raise RuntimeError(f"Cannot measure resident memory for daemon {self.pid}")
        return int(m.group(1))


def wait_ready(e, secs=60):
    end = time.time() + secs
    while time.time() < end:
        try:
            e.call("GET", "/_ping")
            return
        except Exception:
            e.conn.close()
            e.conn = UnixHTTP(e.sock)
            time.sleep(0.5)
    raise SystemExit(f"{e.sock}: the engine did not answer")


def timed(f):
    t = time.perf_counter()
    f()
    return (time.perf_counter() - t) * 1000


def summary(xs):
    xs = sorted(xs)
    if not xs or any(not math.isfinite(x) or x < 0 for x in xs):
        raise ValueError("Measurements must be finite, non-negative, and non-empty")
    return {"median": statistics.median(xs), "p95": xs[math.ceil(len(xs) * 0.95) - 1], "n": len(xs)}


def syscalls_per_run(e, runs):
    """System calls the daemon (every thread, and what it starts) makes for
    one container, from strace's count over `runs` of them."""
    out = f"/tmp/asagiri-strace-{e.pid}.txt"
    st = subprocess.Popen(["strace", "-f", "-c", "-o", out, "-p", str(e.pid)], stderr=subprocess.DEVNULL)
    try:
        time.sleep(1.0)
        if st.poll() is not None:
            raise RuntimeError("strace could not attach to the measured daemon")
        for _ in range(runs):
            e.run_once()
        time.sleep(0.5)
    finally:
        st.send_signal(signal.SIGINT)
        try:
            st.wait(timeout=30)
        except subprocess.TimeoutExpired:
            st.kill()
            st.wait(timeout=5)
    with open(out) as f:
        text = f.read()
    m = re.search(r"^\s*100\.00\s+\S+\s+\S+\s+(\d+)\s+(?:\d+\s+)?total\s*$", text, re.M)
    if not m:
        raise SystemExit(f"could not read strace's total:\n{text[-400:]}")
    return int(m.group(1)) / runs


def bench(a):
    base, head = Engine(a.base), Engine(a.head)
    image_archive, copy_archive, identities = fixtures(platform.machine())
    for e in (base, head):
        wait_ready(e)
        pulled = e.call("POST", f"/images/create?fromImage={IMAGE}")
        for line in pulled.splitlines():
            if json.loads(line).get("error"):
                raise RuntimeError(f"Benchmark image pull failed: {line!r}")
        loaded = e.call("POST", "/images/load", image_archive)
        for line in loaded.splitlines():
            if json.loads(line).get("error"):
                raise RuntimeError(f"Benchmark fixture load failed: {line!r}")
        e.copy_archive = copy_archive
        e.copy_container = json.loads(e.call("POST", "/containers/create", {
            "Image": IMAGE, "Cmd": ["sleep", "3600"], "HostConfig": {"NetworkMode": "none"},
        }))["Id"]
        e.call("POST", f"/containers/{e.copy_container}/start")
        for _ in range(10):  # warm: first runs fill caches
            e.run_once()
            e.create_once()
            e.copy_once()
        copied = e.call("GET", f"/containers/{e.copy_container}/archive?path=/tmp/asagiri-perf-copy")
        with tarfile.open(fileobj=io.BytesIO(copied)) as archive:
            members = archive.getmembers()
            if len(members) != 1 or archive.extractfile(members[0]).read() != b"x" * 4096:
                raise RuntimeError("Benchmark upload did not preserve its payload")
    time.sleep(3)
    res = {"base": {}, "head": {}, "workload": {
        "id": WORKLOAD, "image": IMAGE, "runs": a.runs, "warmup_runs": 10,
        "syscall_runs": 20 if a.strace else 0, "fixtures": identities,
    }, "limits": LIMITS}
    for name, e in (("base", base), ("head", head)):
        res[name]["rss_idle"] = e.rss_kib()
    samples = {k: {"base": [], "head": []} for k in TIMINGS}
    phases = {"base": [], "head": []}
    for i in range(a.runs):
        # Alternate which goes first, so neither always gets the warmer turn.
        order = ((("base", base), ("head", head)) if i % 2 == 0 else (("head", head), ("base", base)))
        for name, e in order:
            samples["ping"][name].append(timed(lambda: e.call("GET", "/_ping")))
            samples["list"][name].append(timed(lambda: e.call("GET", "/containers/json?all=1")))
            samples["run"][name].append(timed(lambda: phases[name].append(e.run_once())))
            samples["create"][name].append(timed(e.create_once))
            samples["copy"][name].append(timed(e.copy_once))
    for k, v in samples.items():
        for name in ("base", "head"):
            res[name][k] = summary(v[name])
    for name, e in (("base", base), ("head", head)):
        res[name]["rss_after"] = e.rss_kib()
    if a.strace:
        for name, e in (("base", base), ("head", head)):
            res[name]["syscalls"] = syscalls_per_run(e, 20)
    res["run_phase_ms_create_start_wait_remove"] = phases
    for side, rows in phases.items():
        print(side, {phase: summary(values) for phase, values in zip(("create", "start", "wait", "remove"), zip(*rows))})
    res["samples_ms"] = samples
    with open(a.out, "w") as f:
        json.dump(res, f, indent=2)
    for e in (base, head):
        e.call("DELETE", f"/containers/{e.copy_container}?force=1&v=1")
        e.conn.close()


def sizes(a):
    def one(d):
        companions = [b for b in os.listdir(d) if b.endswith("-init")]
        if len(companions) != 1:
            raise ValueError(f"expected one *-init binary in {d}")
        init = companions[0]
        return {label: os.path.getsize(os.path.join(d, binary))
                for label, binary in (("engine", init[:-5]), ("init", init))}
    with open(a.out, "w") as f:
        json.dump({"base": one(a.base), "head": one(a.head)}, f, indent=2)


def report(a):
    with open(a.results) as f:
        res = json.load(f)
    sz = {}
    if a.sizes and os.path.exists(a.sizes):
        with open(a.sizes) as f:
            sz = json.load(f)
    rows, failed = [], []

    def row(label, key, base, head, unit, fmt):
        d = head - base
        pct = d / base * 100 if base else 0.0
        mark = ""
        if key in LIMITS:
            share, amount = LIMITS[key]
            if d > amount and (base == 0 or d / base > share):
                mark = " ❌"
                failed.append(label)
            elif d < -amount and base and -d / base > share:
                mark = " ✅"
        rows.append(f"| {label} | {fmt(base)} {unit} | {fmt(head)} {unit} | {'+' if d >= 0 else ''}{fmt(d)} {unit} ({pct:+.1f}%){mark} |")

    ms = lambda v: f"{v:.3f}"
    for key, label in (("run", "docker run --rm alpine true"), ("ping", "GET /_ping"), ("list", "GET /containers/json"),
                       ("create", "Create/remove with 128-entry OCI history"), ("copy", "4 KiB upload and pooled ping")):
        if key not in res["base"]:
            continue
        b, h = res["base"][key], res["head"][key]
        row(f"{label}, median", key, b["median"], h["median"], "ms", ms)
        row(f"{label}, p95", key + "_p95", b["p95"], h["p95"], "ms", ms)
    mib = lambda v: f"{v / 1024:.1f}"
    row("Daemon memory at rest", "rss_idle", res["base"]["rss_idle"], res["head"]["rss_idle"], "MiB", mib)
    row("Daemon memory after the runs", "rss_after", res["base"]["rss_after"], res["head"]["rss_after"], "MiB", mib)
    if "syscalls" in res["base"]:
        row("Daemon system calls per container", "syscalls", res["base"]["syscalls"], res["head"]["syscalls"], "", lambda v: f"{v:.0f}")
    for b in ("engine", "init"):
        if b in sz.get("base", {}):
            row(f"`{b}` binary", "size", sz["base"][b] / 1024, sz["head"][b] / 1024, "KiB", lambda v: f"{v:.0f}")
    n = res["base"]["run"]["n"]
    print("## Speed: this change against main\n")
    print(f"Both built and run on the same machine, requests alternating between them, {n} of each. ❌ is past the limit, ✅ is faster.\n")
    print("| | main | this change | difference |")
    print("|---|---|---|---|")
    print("\n".join(rows))
    print()
    if res.get("samples_ms"):
        print("Paired diagnostics use each adjacent base/head measurement. They do not change the gates above.\n")
        print("| Timing | Median paired head − base | Head faster pairs |")
        print("|---|---|---|")
        for key, sides in res["samples_ms"].items():
            differences = [head - base for base, head in zip(sides["base"], sides["head"], strict=True)]
            print(f"| {key} | {statistics.median(differences):+.3f} ms | {sum(d < 0 for d in differences)}/{len(differences)} |")
        print()
    if failed:
        print(f"**Slower than main:** {', '.join(failed)}. Limits are in `scripts/perf.py` (`LIMITS`).")
    else:
        print("**Within the limits.**")
    return 1 if failed else 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("bench")
    b.add_argument("--base", required=True, help="main's engine, SOCKET:PID")
    b.add_argument("--head", required=True, help="the change's engine, SOCKET:PID")
    b.add_argument("--runs", type=int, default=400)
    b.add_argument("--no-strace", dest="strace", action="store_false")
    b.add_argument("--out", required=True)
    s = sub.add_parser("sizes")
    s.add_argument("--base", required=True)
    s.add_argument("--head", required=True)
    s.add_argument("--out", required=True)
    r = sub.add_parser("report")
    r.add_argument("results")
    r.add_argument("sizes", nargs="?")
    a = p.parse_args()
    if a.cmd == "bench":
        if not 20 <= a.runs <= 4000:
            p.error("--runs must be between 20 and 4000")
        bench(a)
    elif a.cmd == "sizes":
        sizes(a)
    else:
        sys.exit(report(a))


if __name__ == "__main__":
    main()
