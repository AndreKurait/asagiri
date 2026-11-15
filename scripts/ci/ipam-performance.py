#!/usr/bin/env python3
"""Compare the actual IPv4/IPv6 allocation methods from two revisions."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[2]
NETWORK = "crates/asagiri/src/daemon/network.rs"
GAP = "crates/asagiri/src/daemon/address_gap.rs"


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def digest(data):
    return hashlib.sha256(data.encode() if isinstance(data, str) else data).hexdigest()


def function(text, signature):
    if text.count(signature) != 1:
        raise ValueError("Expected exactly one production function: " + signature)
    start = text.index(signature)
    brace = text.index("{", start)
    depth = 0
    # Skip ordinary Rust comments and string literals so braces in a
    # diagnostic or comment cannot truncate the method being measured.
    tokens = re.finditer(r'//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|[{}]', text[brace:], re.DOTALL)
    for token in tokens:
        if token.group() == "{":
            depth += 1
        elif token.group() == "}":
            depth -= 1
            if depth == 0:
                return text[start:brace + token.end()]
    raise ValueError("Unterminated production function: " + signature)


def source_at(revision, path, working=False):
    if working:
        file = ROOT / path
        return file.read_text() if file.exists() else None
    found = subprocess.run(["git", "show", revision + ":" + path], cwd=ROOT, capture_output=True, text=True)
    if found.returncode:
        if path != GAP:
            raise ValueError("Cannot read " + path + " at " + revision)
        return None
    return found.stdout


def module(name, revision, fixture, working=False):
    network = source_at(revision, NETWORK, working)
    gap = source_at(revision, GAP, working)
    v6 = network[network.index("impl V6 {"):network.index("impl Network {")]
    v4 = network[network.index("impl Network {"):network.index("pub struct Networks {")]
    methods = function(network, "fn mask6(") + "\n"
    if "fn merged_hosts<" in network:
        methods += function(network, "fn merged_hosts<") + "\n"
    selected = {"mask6": digest(function(network, "fn mask6("))}
    for kind, text, names in (("Network", v4, ("reserved", "allocate_avoiding", "release")),
                               ("V6", v6, ("reserved", "host", "last", "allocate_avoiding", "release"))):
        methods += "impl " + kind + " {\n"
        for method in names:
            exact = function(text, "fn " + method + "(")
            selected[kind + "::" + method] = digest(exact)
            methods += exact + "\n"
        methods += "}\n"
    if fixture.count("// PRODUCTION_METHODS") != 1:
        raise ValueError("Benchmark fixture insertion point changed")
    implementation = fixture.replace("// PRODUCTION_METHODS", methods)
    # Keep each revision's helper in its original parent-module relationship.
    gap_module = (gap or "").replace("//!", "//")
    result = "mod " + name + " {\nmod address_gap {\n" + gap_module + "\n}\npub(super) mod implementation {\n" + implementation + "\n}\n}\n"
    return result, {"network_sha256": digest(network), "gap_sha256": digest(gap) if gap else None, "method_sha256": selected}


def summarize(samples):
    out = {}
    for name in sorted({row["case"] for row in samples}):
        rows = [row for row in samples if row["case"] == name]
        a = [row["baseline_ns"] / row["loops"] for row in rows]
        b = [row["candidate_ns"] / row["loops"] for row in rows]
        median_a, median_b = statistics.median(a), statistics.median(b)
        out[name] = {
            "pairs": len(rows), "baseline_median_ns": median_a, "candidate_median_ns": median_b,
            "change_pct": 100 * (median_b / median_a - 1),
            "paired_median_delta_ns": statistics.median(y - x for x, y in zip(a, b, strict=True)),
            "candidate_faster_pairs": sum(y < x for x, y in zip(a, b, strict=True)),
            "baseline_p95_ns": sorted(a)[(95 * len(a) + 99) // 100 - 1],
            "candidate_p95_ns": sorted(b)[(95 * len(b) + 99) // 100 - 1],
        }
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", default="HEAD^1")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--working-tree", action="store_true")
    parser.add_argument("--pairs", type=int, default=100)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 20 <= args.pairs <= 1000 or args.pairs % 2:
        raise ValueError("Use an even pair count between 20 and 1000")
    baseline = git("rev-parse", "--verify", args.baseline + "^{commit}")
    head = git("rev-parse", "--verify", args.head + "^{commit}")
    if not all(re.fullmatch(r"[0-9a-f]{40}", value) for value in (baseline, head)):
        raise ValueError("Invalid comparison commit")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    harness = (ROOT / "tests/ipam-benchmark.rs").read_text()
    fixture = (ROOT / "tests/ipam-benchmark-module.rs").read_text()
    before, before_hashes = module("baseline", baseline, fixture)
    after, after_hashes = module("candidate", head, fixture, args.working_tree)
    if harness.count("// MODULES") != 1:
        raise ValueError("Benchmark module insertion point changed")
    program = harness.replace("// MODULES", before + "\n" + after)
    path = args.output / "ipam-benchmark.rs"
    path.write_text(program)
    binary = args.output / "ipam-benchmark"
    rust = json.loads((ROOT / "scripts/ci/verus.json").read_text())["rust"]
    compiler = ["rustup", "run", rust, "rustc"]
    command = [*compiler, "-O", "--edition=2024", "--crate-name=ipam_benchmark", str(path), "-o", str(binary)]
    subprocess.run(command, check=True)
    cpu = platform.processor()
    if Path("/proc/cpuinfo").exists():
        cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), cpu)
    elif platform.system() == "Darwin":
        cpu = subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip()
    record = {
        "schema": "asagiri.ipam-performance.v1",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "baseline": baseline, "candidate": head, "working_tree": args.working_tree,
        "harness_commit": git("rev-parse", "HEAD"),
        "harness_sha256": digest(harness), "fixture_sha256": digest(fixture),
        "baseline_sources": before_hashes, "candidate_sources": after_hashes,
        "program_sha256": digest(program), "binary_sha256": digest(binary.read_bytes()),
        "rustc": subprocess.check_output([*compiler, "-Vv"], text=True),
        "host": {"system": platform.system(), "kernel": platform.release(), "architecture": platform.machine(), "cpu": cpu, "cpus": os.cpu_count()},
        "github_run_id": os.environ.get("GITHUB_RUN_ID"), "github_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "measurement": "one allocation and release; fixture construction excluded; five paired warmup batches; alternating order",
        "pairs_per_case": args.pairs, "status": "running",
    }
    raw = args.output / "samples.jsonl"
    try:
        with raw.open("w") as output:
            subprocess.run([str(binary), str(args.pairs)], stdout=output, check=True, timeout=300)
        rows = [json.loads(line) for line in raw.read_text().splitlines()]
        if len(rows) != 26 * args.pairs:
            raise ValueError("Missing benchmark samples")
        for name in {row["case"] for row in rows}:
            group = [row for row in rows if row["case"] == name]
            if [row["pair"] for row in group] != list(range(args.pairs)) or sum(row["baseline_first"] for row in group) != args.pairs // 2:
                raise ValueError("Unbalanced or incomplete benchmark pairs")
            if len({row["address"] for row in group}) != 1:
                raise ValueError("Allocation result changed during the benchmark")
        record["samples_sha256"] = digest(raw.read_bytes())
        record["summary"] = summarize(rows)
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = type(error).__name__ + ": " + str(error)
        raise
    finally:
        record["completed_at"] = datetime.now(timezone.utc).isoformat()
        (args.output / "report.json").write_text(json.dumps(record, indent=2) + "\n")
    table = ["| Case | Baseline ns | Candidate ns | Change | Faster pairs |", "|---|---:|---:|---:|---:|"]
    for name, row in record["summary"].items():
        table.append(f"| {name} | {row['baseline_median_ns']:.1f} | {row['candidate_median_ns']:.1f} | {row['change_pct']:+.1f}% | {row['candidate_faster_pairs']}/{args.pairs} |")
    text = "\n".join(table) + "\n"
    (args.output / "summary.md").write_text(text)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
            summary.write("IPv4/IPv6 allocation microbenchmarks, including release. Informational timings; raw paired samples are in the workflow artifact.\n\n" + text)
    print(text)


if __name__ == "__main__":
    main()
