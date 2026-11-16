#!/usr/bin/env python3
"""Compare a public ancestor and this commit on one hosted Linux runner."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import statistics
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
ORDER = ("baseline", "candidate", "candidate", "baseline")
ENGINES = ("Asagiri", "Docker", "Podman")
WORKLOADS = ("ping", "list", "run", "exec", "copy")
INPUTS = {
    "harness_sha256": "scripts/reference-bench.py",
    "runner_sha256": "scripts/ci/reference-run.py",
    "images_sha256": "scripts/ci/reference-images.json",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def summary(values):
    require(values and all(type(x) in (int, float) and math.isfinite(x) and x >= 0 for x in values),
            "Timing samples must be finite nonnegative numbers")
    return {"median": statistics.median(values),
            "p95": sorted(values)[math.ceil(.95 * len(values)) - 1], "samples": len(values)}


def check_leg(result, *, source, binaries, inputs, pins, context):
    require((result["schema"], result["workload"], result["rounds"], result["warmup_runs"],
             result["network"]) == (2, "docker-api-reference-v2", 400, 10, "none"),
            "Unexpected workload, counts, warmup, or network")
    require(datetime.fromisoformat(result["measured_at"]).utcoffset() is not None,
            "Measurement time must include its timezone")
    require(set(result["workloads"]) == set(WORKLOADS), "Workload set differs")
    provenance = result["provenance"]
    require(provenance["commit"] == source and re.fullmatch("[0-9a-f]{40}", source),
            "Measurement belongs to a different source")
    for key, value in context.items():
        require(provenance[key] == value, "Workflow provenance differs: " + key)
    require(provenance["asagiri_build_kind"] == "musl-portable", "Unexpected build kind")
    require(set(binaries) == {"asagiri", "asagiri-init"}, "Unexpected binary set")
    require(all(re.fullmatch("[0-9a-f]{64}", value) for value in binaries.values()),
            "Invalid binary digest")
    require(provenance["binaries"] == binaries, "Measurement used different binaries")
    for field in INPUTS:
        require(provenance[field] == inputs[field], "Measurement source differs: " + field)
    require(result["host"]["system"] == "Linux" and result["host"]["architecture"] == "x86_64",
            "This pair requires native x86 Linux")
    require(type(result["host"]["cpus"]) is int and result["host"]["cpus"] > 0,
            "Invalid host CPU count")
    require(all(result["host"].values()), "Incomplete host metadata")
    require(result["image"] ==
            "alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab",
            "Workload image differs")
    require(result["copy"] == {
        "payload_bytes": 65536, "sha256": hashlib.sha256(bytes(range(256)) * 256).hexdigest(),
        "volume_driver": "local", "includes_client_archive_and_hash_validation": True,
    }, "Copy workload differs")
    for key in ("samples_ms", "metrics_ms", "cleanup", "engines"):
        require(set(result[key]) == set(ENGINES), "Engine set differs: " + key)
    identities = []
    for engine, pin in zip(ENGINES, ("asagiri_host", "docker", "podman")):
        host = provenance["hosts"][engine]
        require(host["image"] == pins[pin] and "@sha256:" in host["image"], "Host image differs")
        require(host["privileged"] is True and host["published_ports"] is False,
                "Expected separate private engines without published ports")
        require(re.fullmatch("sha256:[0-9a-f]{64}", host["image_id"]), "Invalid host image identity")
        image = result["engines"][engine]["image"]
        version = result["engines"][engine]["version"]
        require(isinstance(version["Version"], str) and version["Version"], "Missing engine version")
        require((version["Os"], version["Arch"]) == ("linux", "amd64"), "Wrong engine architecture")
        require(bool(version.get("AsagiriVersion")) == (engine == "Asagiri"),
                "Engine identity does not match its endpoint")
        identity = (image["Architecture"], image["Os"], tuple(image["RootFS"]["Layers"]))
        require(identity[:2] == ("amd64", "linux") and identity[2], "Workload image identity is invalid")
        require(all(re.fullmatch("sha256:[0-9a-f]{64}", layer) for layer in identity[2]),
                "Invalid image layer digest")
        identities.append(identity)
        cleanup = result["cleanup"][engine]
        require(cleanup["volume_verified_absent"] is True, "Benchmark volume was not removed")
        require(len(cleanup["containers_verified_absent"]) == 1
                and re.fullmatch("[0-9a-f]{64}", cleanup["containers_verified_absent"][0]),
                "Benchmark container cleanup is missing")
        require(set(result["samples_ms"][engine]) == set(result["metrics_ms"][engine]) == set(WORKLOADS),
                "Workload measurements are missing")
        for workload in WORKLOADS:
            values = result["samples_ms"][engine][workload]
            require(len(values) == 400, "Wrong number of timing samples")
            require(summary(values) == result["metrics_ms"][engine][workload],
                    "Reported statistics differ from raw samples")
    require(len(set(identities)) == 1, "Engines used different workload image layers")
    return identities[0]


def compare(legs, sources, binaries, inputs, pins, context):
    require(len(legs) == 4 and tuple(leg["variant"] for leg in legs) == ORDER,
            "Expected baseline, candidate, candidate, baseline order")
    require(sources["baseline"] != sources["candidate"], "Baseline and candidate must differ")
    digests = [leg["comparison_sha256"] for leg in legs]
    require(len(set(digests)) == 4 and all(re.fullmatch("[0-9a-f]{64}", value) for value in digests),
            "Each leg must have a distinct retained measurement")
    require(len({leg["result"]["measured_at"] for leg in legs}) == 4, "A measurement was reused")
    identities, hosts, engines = [], [], []
    for leg in legs:
        variant, result = leg["variant"], leg["result"]
        identities.append(check_leg(result, source=sources[variant], binaries=binaries[variant],
                                    inputs=inputs, pins=pins, context=context))
        hosts.append(result["host"])
        engines.append(result["provenance"]["hosts"])
    require(all(host == hosts[0] for host in hosts), "Measurements came from different hosts")
    require(all(identity == identities[0] for identity in identities), "Workload images changed")
    require(all(engine == engines[0] for engine in engines), "Private engine hosts changed")
    for engine in ENGINES:
        versions = [leg["result"]["engines"][engine]["version"] for leg in legs]
        container_ids = [leg["result"]["cleanup"][engine]["containers_verified_absent"][0] for leg in legs]
        require(len(set(container_ids)) == 4, "Repeated legs reused the same benchmark fixture")
        require(versions[0] == versions[3] and versions[1] == versions[2], "Engine version changed between repeated runs")
        if engine != "Asagiri":
            require(all(version == versions[0] for version in versions), "Reference engine version changed")
    rows = []
    for engine in ENGINES:
        for workload in WORKLOADS:
            measurements = {}
            for variant in ("baseline", "candidate"):
                values = [value for leg in legs if leg["variant"] == variant
                          for value in leg["result"]["samples_ms"][engine][workload]]
                measurements[variant] = summary(values)
                measurements[variant]["over_240_ms"] = sum(value > 240 for value in values)
                measurements[variant]["leg_medians"] = [
                    leg["result"]["metrics_ms"][engine][workload]["median"]
                    for leg in legs if leg["variant"] == variant]
            before, after = measurements["baseline"]["median"], measurements["candidate"]["median"]
            rows.append({"engine": engine, "workload": workload, **measurements,
                         "median_ratio": after / before if before else None})
    return {
        "schema": "asagiri.engine-comparison-pair.v1", "status": "passed",
        "sources": sources, "order": list(ORDER), "host": hosts[0], "context": context,
        "binaries": binaries, "inputs": inputs, "samples": 24000, "metrics": rows,
        "limits": "Four sequential runs on one hosted x86 Linux machine, using the unchanged "
                  "five-workload harness. Order reduces simple time-order bias but does not establish "
                  "performance on all machines or workloads. Passing verifies measurement integrity; "
                  "it does not assert a speedup.",
    }


def report(value):
    lines = [
        "# Same-machine engine comparison", "",
        f"Baseline `{value['sources']['baseline']}`; candidate `{value['sources']['candidate']}`.",
        "Order: baseline, candidate, candidate, baseline. Each row combines 800 samples per version.",
        "",
        "| Engine | Workload | Baseline median | Candidate median | Baseline p95 | Candidate p95 | Candidate / baseline |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for row in value["metrics"]:
        baseline, candidate = row["baseline"], row["candidate"]
        ratio = "n/a" if row["median_ratio"] is None else f"{row['median_ratio']:.3f}"
        lines.append(
            f"| {row['engine']} | {row['workload']} | {baseline['median']:.3f} ms | "
            f"{candidate['median']:.3f} ms | {baseline['p95']:.3f} ms | {candidate['p95']:.3f} ms | {ratio} |")
    lines += ["", "Lifecycle samples over 240 ms (baseline / candidate):"]
    lines += [f"- {row['engine']}: {row['baseline']['over_240_ms']} / {row['candidate']['over_240_ms']}"
              for row in value["metrics"] if row["workload"] == "run"]
    lines += ["", value["limits"], ""]
    return "\n".join(lines)


def git(*args, cwd=ROOT):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--registry", type=Path, required=True)
    args = parser.parse_args()
    require(platform.system() == "Linux" and platform.machine() == "x86_64", "Hosted x86 Linux is required")
    require(os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
            and os.environ.get("GITHUB_REPOSITORY") == "AndreKurait/asagiri",
            "Run this only in the public repository's hosted Actions workflow")
    require(re.fullmatch("[0-9a-f]{40}", args.baseline), "Supply a full lowercase public commit SHA")
    head = git("rev-parse", "HEAD")
    require(args.baseline != head, "Baseline must precede the measured commit")
    git("merge-base", "--is-ancestor", args.baseline, head)
    require(git("status", "--porcelain") == "", "The measured checkout must be clean")
    temporary = Path(os.environ["RUNNER_TEMP"]).resolve()
    output, target, registry = (path.resolve() for path in (args.out, args.target, args.registry))
    require(all(path.is_relative_to(temporary) and path != temporary for path in (output, target, registry)),
            "Output and caches must be inside this hosted job's temporary directory")
    require(not output.exists(), "Inspect the existing measurement before repeating it")
    require(shutil.disk_usage(temporary).free >= 3 * 1024**3, "Fewer than 3 GiB free")
    output.mkdir(mode=0o700)
    sources = {"baseline": args.baseline, "candidate": head}
    context = {key: os.environ[environment] for key, environment in [
        ("repository", "GITHUB_REPOSITORY"), ("run_id", "GITHUB_RUN_ID"),
        ("run_attempt", "GITHUB_RUN_ATTEMPT"), ("event", "GITHUB_EVENT_NAME"),
    ]}
    require(context["event"] == "workflow_dispatch", "The paired comparison is an explicit workflow run")
    write(output / "started.json", {"at": datetime.now(timezone.utc).isoformat(), "sources": sources,
                                   "context": context, "order": ORDER, "runner_sha256": sha(Path(__file__))})

    def execute(label, command, cwd):
        log = output / (label + ".log")
        require(not log.exists(), "Command was already attempted: " + label)
        write(output / (label + ".started.json"), {"command": list(map(str, command)), "cwd": str(cwd)})
        with log.open("x") as stream:
            result = subprocess.run(command, cwd=cwd, stdout=stream, stderr=subprocess.STDOUT)
        write(output / (label + ".json"), {"exit_code": result.returncode, "log_sha256": sha(log)})
        require(result.returncode == 0, "Inspect " + str(log))

    with tempfile.TemporaryDirectory(prefix="asagiri-reference-pair-", dir=temporary) as scratch:
        baseline = Path(scratch) / "baseline"
        git("worktree", "add", "--detach", str(baseline), args.baseline)
        try:
            checkouts = {"baseline": baseline, "candidate": ROOT}
            inputs = {field: sha(ROOT / name) for field, name in INPUTS.items()}
            for field, name in INPUTS.items():
                require(sha(baseline / name) == inputs[field],
                        "Baseline must use the same comparison workload, runner and pinned images: " + name)
            for name in ("scripts/ci/build-linux.sh", "scripts/ci/images.json", ".cargo/config.toml"):
                require(sha(baseline / name) == sha(ROOT / name),
                        "Baseline must use the same build recipe and compiler images: " + name)
            pins = json.loads((ROOT / INPUTS["images_sha256"]).read_text())
            bins, binaries = {}, {}
            for variant in ("baseline", "candidate"):
                checkout = checkouts[variant]
                bins[variant] = output / (variant + "-binaries")
                execute("build-" + variant, [
                    "sh", "scripts/ci/build-linux.sh", bins[variant], target, registry, "build",
                ], checkout)
                require(git("rev-parse", "HEAD", cwd=checkout) == sources[variant], "Source moved during build")
                require((bins[variant] / "SOURCE_COMMIT").read_text().strip() == sources[variant],
                        "Compiled binaries belong to another source")
                binaries[variant] = {name: sha(bins[variant] / name) for name in ("asagiri", "asagiri-init")}
            legs = []
            for index, variant in enumerate(ORDER, 1):
                checkout = checkouts[variant]
                require(git("status", "--porcelain", cwd=checkout) == "", "Source changed during comparison")
                require(all(sha(checkout / name) == inputs[field] for field, name in INPUTS.items()),
                        "Comparison inputs changed")
                require(all(sha(bins[variant] / name) == expected for name, expected in binaries[variant].items()),
                        "A measured binary changed")
                directory = output / f"leg-{index}-{variant}"
                try:
                    execute(f"measure-{index}-{variant}", [
                        "sudo", "--preserve-env=GITHUB_REPOSITORY,GITHUB_RUN_ID,GITHUB_RUN_ATTEMPT,"
                        "GITHUB_EVENT_NAME,ImageOS,ImageVersion",
                        "python3", checkout / "scripts/ci/reference-run.py",
                        "--binaries", bins[variant], "--out", directory, "--runs", "400",
                    ], checkout)
                finally:
                    if directory.exists():
                        subprocess.run(["sudo", "chown", "-R", f"{os.getuid()}:{os.getgid()}", directory],
                                       check=True)
                result = json.loads((directory / "comparison.json").read_text())
                check_leg(result, source=sources[variant], binaries=binaries[variant],
                          inputs=inputs, pins=pins, context=context)
                legs.append({"variant": variant, "result": result,
                             "comparison_sha256": sha(directory / "comparison.json")})
            value = compare(legs, sources, binaries, inputs, pins, context)
            value["leg_files"] = [
                {"variant": leg["variant"], "comparison_sha256": leg["comparison_sha256"],
                 "path": f"leg-{index}-{leg['variant']}/comparison.json"}
                for index, leg in enumerate(legs, 1)]
            value["build_inputs"] = {
                name: sha(ROOT / name)
                for name in ("scripts/ci/build-linux.sh", "scripts/ci/images.json", ".cargo/config.toml")}
            value["compiler_image"] = os.environ.get("ASAGIRI_CI_RUST_IMAGE", "rust:1.99.1-alpine")
            write(output / "comparison-pair.json", value)
            (output / "report.md").write_text(report(value))
            print(report(value), flush=True)
        finally:
            git("worktree", "remove", "--force", str(baseline))


if __name__ == "__main__":
    main()
