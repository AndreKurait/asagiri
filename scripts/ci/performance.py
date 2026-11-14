#!/usr/bin/env python3
"""Record paired engine benchmarks and archive verified main-run data.

The measuring workflow has a read-only token. A separate trusted workflow
validates its artifact as data and writes only the performance-history branch.
No downloaded script, report, or repository checkout is executed.
"""
import argparse
import base64
import copy
from datetime import datetime, timezone
import hashlib
import io
import json
import math
import os
from pathlib import Path
import platform
import re
import stat
import statistics
import subprocess
import time
import zipfile

REPO = "AndreKurait/asagiri"
BRANCH = "performance-history"
INDEX = "benchmarks/v1/index.json"
MAX_BYTES = 4 * 1024 * 1024
MAX_HISTORY = 500
METRICS = ("run_ms", "run_p95_ms", "ping_ms", "ping_p95_ms", "list_ms",
           "list_p95_ms", "rss_idle_kib", "rss_after_kib", "syscalls",
           "engine_bytes", "init_bytes")
LEGACY_LIMITS = {"run": [0.10, 0.5], "ping": [0.15, 0.05], "list": [0.15, 0.10],
                 "rss_idle": [0.10, 2048], "rss_after": [0.10, 4096], "syscalls": [0.10, 50]}
LIMIT_METRICS = {"run": "run_ms", "ping": "ping_ms", "list": "list_ms",
                 "rss_idle": "rss_idle_kib", "rss_after": "rss_after_kib", "syscalls": "syscalls"}


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def document(data):
    require(len(data) <= MAX_BYTES, "Benchmark data exceeds its size limit")

    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "Duplicate JSON member")
            result[key] = value
        return result

    def invalid(value):
        raise ValueError("Non-finite JSON number: " + value)

    return json.loads(data, object_pairs_hook=unique, parse_constant=invalid)


def number(value, maximum=1e15):
    require(type(value) in (int, float) and math.isfinite(value) and 0 <= value <= maximum,
            "Measurement must be finite, non-negative and bounded")
    return value


def text(value, maximum=256):
    require(isinstance(value, str) and 0 < len(value) <= maximum
            and all(ord(c) >= 32 for c in value), "Invalid metadata text")
    return value


def sha(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value), "Invalid source commit")
    return value


def command(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, **kwargs).stdout


def api(path, **kwargs):
    return document(command("gh", "api", f"repos/{REPO}/{path}", **kwargs))


def measurements(results, sizes):
    values = {}
    for key in ("run", "ping", "list"):
        for suffix, field in (("_ms", "median"), ("_p95_ms", "p95")):
            values[key + suffix] = [results[side][key][field] for side in ("base", "head")]
    for key in ("rss_idle", "rss_after", "syscalls"):
        values[key + ("_kib" if key.startswith("rss") else "")] = [
            results[side][key] for side in ("base", "head")]
    for key in ("engine", "init"):
        values[key + "_bytes"] = [sizes[side][key] for side in ("base", "head")]
    return values


def regressions(values, limits):
    failed = []
    for key, (share, amount) in limits.items():
        base, head = values[LIMIT_METRICS[key]]
        delta = head - base
        if delta > amount and (base == 0 or delta / base > share):
            failed.append(key)
    return sorted(failed)


def series(record):
    env = record["environment"]
    # Runner image and hardware changes start separate series. Kernel/image
    # details remain in every raw record even within a hardware series.
    keys = ("architecture", "cpu_model", "cpus", "memory_kib", "runner_image")
    return digest(encoded({"workload": record["workload"], "host": {k: env[k] for k in keys}}))[:16]


def validate(record):
    expected = {"schema", "repository", "commit", "baseline", "harness", "run_id", "run_attempt",
                "event", "measured_at", "status", "regressions", "workload", "environment",
                "binaries", "metrics", "samples_ms", "limits", "files", "series"}
    require(set(record) == expected and record["schema"] == 1 and record["repository"] == REPO,
            "Invalid performance record schema or repository")
    for field in ("commit", "baseline", "harness"):
        sha(record[field])
    require(record["harness"] in (record["baseline"], record["commit"]), "Unrelated benchmark harness")
    for key in ("run_id", "run_attempt"):
        require(type(record[key]) is int and 0 < record[key] < 10**15, "Invalid workflow run identity")
    require(record["event"] in ("push", "pull_request", "workflow_dispatch"), "Invalid workflow event")
    timestamp = datetime.fromisoformat(text(record["measured_at"]))
    require(timestamp.tzinfo is not None, "Measurement timestamp needs a time zone")
    require(set(record["metrics"]) == set(METRICS), "Missing or unknown metrics")
    for pair in record["metrics"].values():
        require(type(pair) is list and len(pair) == 2, "Each metric must compare two builds")
        for value in pair:
            number(value)
    limits = record["limits"]
    require(set(limits) == set(LEGACY_LIMITS), "Missing regression limits")
    for pair in limits.values():
        require(type(pair) is list and len(pair) == 2, "Invalid regression limit")
        number(pair[0], 10)
        number(pair[1], 10**9)
    failed = regressions(record["metrics"], limits)
    require(record["regressions"] == failed, "Regression list disagrees with measurements")
    require(record["status"] == ("regression" if failed else "within-limits"), "Invalid result status")
    workload = record["workload"]
    require(set(workload) == {"id", "image", "runs", "warmup_runs", "syscall_runs"},
            "Invalid workload description")
    require(workload["id"] in ("linux-engine-v1", "linux-engine-v2"), "Unknown benchmark methodology")
    runs = workload["runs"]
    require(type(runs) is int and 20 <= runs <= 4000
            and workload["warmup_runs"] == 10 and workload["syscall_runs"] == 20, "Invalid sample count")
    if workload["id"] == "linux-engine-v1":
        require(workload["image"] == "alpine:3.21" and record["samples_ms"] is None,
                "Legacy measurements cannot claim raw samples or a pinned image")
    else:
        require(re.fullmatch(r"alpine:3\.21@sha256:[a-f0-9]{64}", workload["image"]),
                "Benchmark image must be pinned by digest")
        samples = record["samples_ms"]
        require(set(samples) == {"run", "ping", "list"}, "Missing raw timing samples")
        for metric, sides in samples.items():
            require(set(sides) == {"base", "head"}, "Missing paired samples")
            for index, side in enumerate(("base", "head")):
                values = sides[side]
                require(type(values) is list and len(values) == runs, "Wrong sample count")
                for value in values:
                    number(value, 1e7)
                require(statistics.median(values) == record["metrics"][metric + "_ms"][index]
                        and sorted(values)[math.ceil(runs * 0.95) - 1]
                        == record["metrics"][metric + "_p95_ms"][index], "Summary differs from raw samples")
    env = record["environment"]
    require(set(env) == {"system", "architecture", "cpu_model", "cpus", "memory_kib", "kernel",
                        "runner_image", "runner_version", "python", "docker", "rust"}, "Invalid runner metadata")
    require(env["system"] == "Linux", "These measurements require a Linux host")
    for key, value in env.items():
        if key in ("cpus", "memory_kib"):
            require(type(value) is int and 0 < value < 10**12, "Invalid runner capacity")
        else:
            text(value)
    require(set(record["binaries"]) == {"base", "head"}, "Missing binary identities")
    for index, side in enumerate(("base", "head")):
        require(set(record["binaries"][side]) == {"engine", "init"}, "Missing executable identities")
        for key, item in record["binaries"][side].items():
            require(set(item) == {"sha256", "bytes"} and re.fullmatch(r"[a-f0-9]{64}", item["sha256"])
                    and item["bytes"] == record["metrics"][key + "_bytes"][index], "Invalid binary identity")
    require(set(record["files"]) == {"results.json", "sizes.json", "report.md"}
            and all(re.fullmatch(r"[a-f0-9]{64}", value) for value in record["files"].values()),
            "Invalid evidence digests")
    require(record["series"] == series(record), "Hardware/workload series identity differs")
    return record


def binary_identity(folder, commit):
    require((folder / "SOURCE_COMMIT").read_text().strip() == commit, "Binary source differs from workflow")
    entries = {}
    for line in (folder / "SHA256SUMS").read_text().splitlines():
        checksum, name = line.split()
        require(name in ("asagiri", "asagiri-init") and name not in entries, "Unexpected benchmark executable")
        path = folder / name
        require(not path.is_symlink() and path.is_file(), "Executable must be a regular file")
        with path.open("rb") as stream:
            actual = hashlib.file_digest(stream, "sha256").hexdigest()
        require(checksum == actual, "Binary checksum differs from the fresh build")
        entries["init" if name.endswith("-init") else "engine"] = {"sha256": actual, "bytes": path.stat().st_size}
    require(set(entries) == {"engine", "init"}, "Missing benchmark binary")
    return entries


def record(args):
    results = document((args.directory / "results.json").read_bytes())
    sizes = document((args.directory / "sizes.json").read_bytes())
    values = measurements(results, sizes)
    runs = results["base"]["run"]["n"]
    require(all(results[side][key]["n"] == runs for side in ("base", "head")
                for key in ("run", "ping", "list")), "Unequal paired sample counts")
    limits = results.get("limits", LEGACY_LIMITS)
    failed = regressions(values, limits)
    require(os.environ["COMPARISON_OUTCOME"] == ("failure" if failed else "success"),
            "Benchmark did not complete with the reported regression result")
    cpu = Path("/proc/cpuinfo").read_text()
    model = re.search(r"^(?:model name|Hardware|Processor)\s*:\s*(.+)$", cpu, re.M)
    memory = re.search(r"^MemTotal:\s+(\d+)", Path("/proc/meminfo").read_text(), re.M)
    item = {
        "schema": 1, "repository": os.environ["GITHUB_REPOSITORY"],
        "commit": os.environ["GITHUB_SHA"], "baseline": os.environ["BASE_COMMIT"],
        "harness": os.environ["HARNESS_COMMIT"], "run_id": int(os.environ["GITHUB_RUN_ID"]),
        "run_attempt": int(os.environ["GITHUB_RUN_ATTEMPT"]), "event": os.environ["GITHUB_EVENT_NAME"],
        "measured_at": datetime.now(timezone.utc).isoformat(),
        "status": "regression" if failed else "within-limits", "regressions": failed,
        "workload": results.get("workload", {"id": "linux-engine-v1", "image": "alpine:3.21",
                    "runs": runs, "warmup_runs": 10, "syscall_runs": 20}),
        "environment": {"system": platform.system(), "architecture": platform.machine(),
                        "cpu_model": model[1].strip() if model else platform.machine(),
                        "cpus": os.cpu_count(), "memory_kib": int(memory[1]),
                        "kernel": platform.release(),
                        "runner_image": os.environ.get("ImageOS", os.environ.get("PERF_RUNNER_IMAGE", "local")),
                        "runner_version": os.environ.get("ImageVersion", "local"),
                        "python": platform.python_version(),
                        "docker": command("docker", "version", "--format", "{{.Server.Version}}").decode().strip(),
                        "rust": os.environ.get("RUSTUP_TOOLCHAIN", "unspecified")},
        "binaries": {"base": binary_identity(args.base_binaries, os.environ["BASE_COMMIT"]),
                     "head": binary_identity(args.head_binaries, os.environ["GITHUB_SHA"])},
        "metrics": values, "samples_ms": results.get("samples_ms"), "limits": limits,
        "files": {name: digest((args.directory / name).read_bytes())
                  for name in ("results.json", "sizes.json", "report.md")},
    }
    item["series"] = series(item)
    validate(item)
    (args.directory / "record.json").write_bytes(encoded(item))
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as stream:
            stream.write(f"\nMeasured `{item['commit']}` against `{item['baseline']}`; "
                         f"harness `{item['harness']}`, series `{item['series']}`.\n\n"
                         "[Performance history](https://andrekurait.github.io/asagiri/performance/) "
                         "archives verified main runs, including regressions.\n")
    print(f"Recorded {len(values)} metrics, {runs} paired samples, and verified binary identities.")


def unpack(data):
    require(len(data) <= MAX_BYTES, "Artifact is too large")
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        files = archive.infolist()
        require(len(files) == 4 and {item.filename for item in files}
                == {"record.json", "results.json", "sizes.json", "report.md"}, "Unexpected artifact paths")
        require(sum(item.file_size for item in files) <= MAX_BYTES, "Expanded artifact is too large")
        require(all(not item.is_dir() and not stat.S_ISLNK(item.external_attr >> 16)
                    and not item.flag_bits & 1 for item in files), "Invalid artifact entry")
        content = {item.filename: archive.read(item) for item in files}
    item = validate(document(content["record.json"]))
    require(all(digest(content[name]) == checksum for name, checksum in item["files"].items()),
            "Benchmark evidence was changed")
    results = document(content["results.json"])
    require(measurements(results, document(content["sizes.json"])) == item["metrics"]
            and results.get("samples_ms") == item["samples_ms"]
            and results.get("limits", LEGACY_LIMITS) == item["limits"],
            "Archived metrics or samples differ from measured results")
    return item


def trusted_run(run):
    require(run["repository"]["full_name"] == REPO and run["head_repository"]["full_name"] == REPO
            and run["head_branch"] == "main" and run["event"] in ("push", "workflow_dispatch")
            and run["path"] == ".github/workflows/perf.yml" and run["name"] == "Speed"
            and run["status"] == "completed" and run["conclusion"] in ("success", "failure"),
            "History accepts only completed official main Speed measurements")
    return run


def step_outcomes(jobs, event):
    # Retain old manual archives whose job used the original name.
    names = {"speed"} if event == "push" else {"speed", "speed experiment"}
    selected = [job for job in jobs if job["name"] in names]
    require(len(selected) <= 1, "Duplicate benchmark jobs")
    steps = selected[0]["steps"] if selected else []
    outcomes = {step["name"]: step["conclusion"] for step in steps}
    require(len(outcomes) == len(steps), "Duplicate benchmark steps")
    return outcomes


def checked_run(run_id, attempt=None):
    require(re.fullmatch(r"[1-9]\d{0,14}", str(run_id)), "Invalid requested run")
    path = f"actions/runs/{run_id}"
    if attempt:
        require(re.fullmatch(r"[1-9]\d{0,5}", str(attempt)), "Invalid requested attempt")
        path += f"/attempts/{attempt}"
    run = trusted_run(api(path))
    require(api(f"compare/{sha(run['head_sha'])}...main")["merge_base_commit"]["sha"] == run["head_sha"],
            "Measured commit is not part of current main history")
    jobs = api(f"actions/runs/{run_id}/attempts/{run['run_attempt']}/jobs?per_page=100")["jobs"]
    outcomes = step_outcomes(jobs, run["event"])
    if ("Record measurements and source identities" not in outcomes
            or outcomes.get("Compare") in (None, "skipped", "cancelled")):
        print("This run predates archived measurements or did not reach the comparison; no data is synthesized.")
        return None
    artifacts = api(f"actions/runs/{run_id}/artifacts?per_page=100")["artifacts"]
    matches = [item for item in artifacts
               if item["name"] == f"performance-{run_id}-{run['run_attempt']}" and not item["expired"]]
    require(len(matches) == 1, "Missing or duplicate performance artifact")
    artifact = matches[0]
    require(0 < artifact["size_in_bytes"] <= MAX_BYTES
            and re.fullmatch(r"sha256:[a-f0-9]{64}", artifact.get("digest", "")), "Unverified artifact metadata")
    data = command("gh", "api", f"repos/{REPO}/actions/artifacts/{artifact['id']}/zip")
    require("sha256:" + digest(data) == artifact["digest"], "Artifact checksum verification failed")
    item = unpack(data)
    require(item["commit"] == run["head_sha"] and item["run_id"] == run["id"]
            and item["run_attempt"] == run["run_attempt"] and item["event"] == run["event"],
            "Artifact identifies a different workflow run")
    require(item["workload"]["runs"] == 400, "Official history requires 400 paired samples")
    require(api(f"compare/{item['baseline']}...{item['commit']}")["merge_base_commit"]["sha"] == item["baseline"],
            "Baseline is not an ancestor of the measured commit")
    require(outcomes.get("Compare") == ("failure" if item["regressions"] else "success")
            and outcomes.get("Record measurements and source identities") == "success"
            and outcomes.get("Keep benchmark data and report") == "success",
            "Official workflow steps did not produce the claimed result")
    return item


def summary(item):
    fields = ("commit", "baseline", "run_id", "run_attempt", "measured_at", "status",
              "regressions", "series", "metrics")
    return {**{key: copy.deepcopy(item[key]) for key in fields}, "workload": item["workload"]["id"],
            "architecture": item["environment"]["architecture"], "cpu_model": item["environment"]["cpu_model"],
            "runner_image": item["environment"]["runner_image"],
            "path": f"benchmarks/v1/runs/{item['run_id']}-{item['run_attempt']}.json"}


def update_index(previous, item):
    require(set(previous) == {"schema", "repository", "entries"}
            and previous["schema"] == 1 and previous["repository"] == REPO
            and len(previous["entries"]) <= MAX_HISTORY, "Invalid history index")
    latest = summary(item)
    for row in previous["entries"]:
        require(set(row) == set(latest), "Unexpected history index fields")
        sha(row["commit"])
        sha(row["baseline"])
        require(type(row["run_id"]) is int and row["run_id"] > 0
                and type(row["run_attempt"]) is int and row["run_attempt"] > 0
                and row["path"] == f"benchmarks/v1/runs/{row['run_id']}-{row['run_attempt']}.json"
                and row["status"] in ("within-limits", "regression")
                and re.fullmatch(r"[0-9a-f]{16}", row["series"]), "Invalid indexed measurement")
        require(datetime.fromisoformat(row["measured_at"]).tzinfo is not None, "Invalid indexed timestamp")
        require(set(row["metrics"]) == set(METRICS), "Invalid indexed metrics")
        for pair in row["metrics"].values():
            require(type(pair) is list and len(pair) == 2, "Invalid indexed comparison")
            for value in pair:
                number(value)
        for key in ("workload", "architecture", "cpu_model", "runner_image"):
            text(row[key])
    entries = {row["path"]: row for row in previous["entries"]}
    require(len(entries) == len(previous["entries"]), "Duplicate history entries")
    existing = entries.get(latest["path"])
    require(existing is None or existing == latest, "Cannot rewrite an existing benchmark")
    entries[latest["path"]] = latest
    ordered = sorted(entries.values(), key=lambda row: (row["measured_at"], row["run_id"], row["run_attempt"]))
    return {"schema": 1, "repository": REPO, "entries": ordered[-MAX_HISTORY:]}


def remote_file(path, revision):
    response = subprocess.run(["gh", "api", "-H", "Accept: application/vnd.github.raw+json",
                               f"repos/{REPO}/contents/{path}?ref={revision}"], capture_output=True)
    if response.returncode:
        if b"(HTTP 404)" in response.stderr:
            return None
        raise RuntimeError(response.stderr.decode())
    require(len(response.stdout) <= MAX_BYTES, "Remote history file is too large")
    return response.stdout


def publish(item):
    require(os.environ.get("GITHUB_ACTIONS") == "true" and os.environ.get("GITHUB_REPOSITORY") == REPO
            and os.environ.get("GITHUB_REF") == "refs/heads/main"
            and os.environ.get("GITHUB_WORKFLOW") == "Performance history",
            "Automatic writes require the trusted performance-history workflow")
    query = """mutation($input: CreateCommitOnBranchInput!) {
      createCommitOnBranch(input: $input) {
        commit { oid url signature { __typename isValid state wasSignedByGitHub } }
      }
    }"""
    reference = subprocess.run(["gh", "api", f"repos/{REPO}/git/ref/heads/{BRANCH}"], capture_output=True)
    if reference.returncode:
        require(b"(HTTP 404)" in reference.stderr, reference.stderr.decode())
        # The initial ref reuses an existing reviewed commit. Subsequent
        # data-only commits are signed by GitHub; main is never a write target.
        start = api("git/ref/heads/main")["object"]["sha"]
        payload = {"ref": "refs/heads/" + BRANCH, "sha": start}
        created = subprocess.run(["gh", "api", "--method", "POST", f"repos/{REPO}/git/refs",
                                  "--input", "-"], input=encoded(payload), capture_output=True)
        if created.returncode:
            require(b"(HTTP 422)" in created.stderr, created.stderr.decode())
            # Another completed run may have created this exact ref first.
            sha(api(f"git/ref/heads/{BRANCH}")["object"]["sha"])
    for retry in range(5):
        head = api(f"git/ref/heads/{BRANCH}")["object"]["sha"]
        path = summary(item)["path"]
        archive = remote_file(path, head)
        if archive is not None:
            require(document(archive) == item, "Refuse to overwrite an archived run")
            print("This verified run is already archived.")
            return
        previous = remote_file(INDEX, head)
        index = update_index(document(previous) if previous else
                             {"schema": 1, "repository": REPO, "entries": []}, item)
        payload = {"query": query, "variables": {"input": {
            "branch": {"repositoryNameWithOwner": REPO, "branchName": BRANCH}, "expectedHeadOid": head,
            "message": {"headline": f"Record performance for {item['commit'][:12]} ({item['run_id']}/{item['run_attempt']})",
                        "body": "Verified official Speed artifact; paired measurements and runner metadata.\n\n"
                                "Signed-off-by: github-actions[bot] "
                                "<41898282+github-actions[bot]@users.noreply.github.com>"},
            "fileChanges": {"additions": [{"path": name, "contents": base64.b64encode(encoded(value)).decode()}
                                         for name, value in ((path, item), (INDEX, index))]},
        }}}
        response = subprocess.run(["gh", "api", "graphql", "--input", "-"],
                                  input=encoded(payload), capture_output=True)
        result = document(response.stdout) if response.stdout else {}
        if response.returncode or result.get("errors"):
            # Independent workflow completions may race. Retry only if the
            # branch actually advanced; permission or schema errors must fail.
            if retry < 4 and api(f"git/ref/heads/{BRANCH}")["object"]["sha"] != head:
                time.sleep(retry + 1)
                continue
            raise RuntimeError(str(result.get("errors")) + response.stderr.decode())
        commit = result["data"]["createCommitOnBranch"]["commit"]
        signature = commit["signature"]
        require(signature and signature["__typename"] == "GpgSignature"
                and signature["isValid"] and signature["wasSignedByGitHub"],
                "Archive commit did not receive a verified GitHub GPG signature")
        print("Archived performance:", commit["url"])
        return
    raise RuntimeError("History changed repeatedly; retry this publication")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    capture = commands.add_parser("record")
    capture.add_argument("--directory", type=Path, required=True)
    capture.add_argument("--base-binaries", type=Path, required=True)
    capture.add_argument("--head-binaries", type=Path, required=True)
    for name in ("check-run", "publish"):
        sub = commands.add_parser(name)
        sub.add_argument("--run", required=True)
        sub.add_argument("--attempt")
    args = parser.parse_args()
    if args.command == "record":
        record(args)
    else:
        item = checked_run(args.run, args.attempt)
        if item is None:
            return
        if args.command == "publish":
            publish(item)
        else:
            print(json.dumps(summary(item), indent=2))


if __name__ == "__main__":
    main()
