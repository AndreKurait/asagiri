#!/usr/bin/env python3
"""Run the compiled cgroup regression as PID 1 in one disposable container."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
import uuid


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_report(report, source, architecture):
    assert report["schema"] == "asagiri.cgroup-startup.v1"
    assert report["source_commit"] == source
    assert report["compiled_architecture"] == architecture
    assert report["status"] == "passed"
    assert "Resource busy (os error 16)" in report["occupied_parent_error"]
    assert report["occupied_parent_missing_files"] == ["cpu.max", "memory.max", "pids.max"]
    assert report["occupied_parent_controllers_after_failure"] == []
    assert report["occupied_parent_type_after_failure"] == "domain"
    assert report["namespace_root_drained"] is True
    assert report["agent_restart_reused_parent"] is True
    assert report["required_controllers_delegated"] == ["cpu", "memory", "pids"]
    for field in ["buildx_aggregate_pids", "buildx_aggregate_memory_bytes"]:
        assert type(report[field]) is int and report[field] > 0
    assert report["live_pid_limit_and_freezer"] == "passed"
    assert report["owned_child_reaped"] is True
    assert report["owned_worker_cgroups_removed"] is True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("fixtures", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    fixtures, out = args.fixtures.resolve(), args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    root = Path(__file__).resolve().parents[2]
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    assert re.fullmatch(r"[0-9a-f]{40}", source)
    assert (fixtures / "SOURCE_COMMIT").read_text().strip() == source
    binary_sha = sha(fixtures / "cgroup-startup")
    assert (fixtures / "SHA256SUMS").read_text() == binary_sha + "  cgroup-startup\n"
    sources = {}
    for line in (fixtures / "SOURCES.sha256").read_text().splitlines():
        digest, name = line.split("  ", 1)
        assert re.fullmatch(r"[0-9a-f]{64}", digest)
        assert not Path(name).is_absolute() and ".." not in Path(name).parts
        assert name not in sources and sha(root / name) == digest
        committed = subprocess.check_output(["git", "show", source + ":" + name], cwd=root)
        assert hashlib.sha256(committed).hexdigest() == digest
        sources[name] = digest
    assert {
        "Cargo.lock", "crates/asagiri-init/src/linux/mod.rs",
        "crates/asagiri-init/src/linux/container.rs", "crates/asagiri-init/tests/cgroup-startup.rs",
    } <= sources.keys()
    assert (fixtures / "RUSTC_VERSION").read_text().startswith("rustc 1.99.1 ")
    image = os.environ["ASAGIRI_CI_PERF_IMAGE"]
    token = uuid.uuid4().hex
    name, label = "asagiri-cgroup-startup-" + token, "dev.asagiri.cgroup-startup"
    record = {
        "schema": "asagiri.cgroup-startup-execution.v1", "status": "running",
        "started_at": datetime.now(timezone.utc).isoformat(), "source_commit": source,
        "binary_sha256": binary_sha, "sources": sources, "image": image,
        "container_name": name, "container_label": label, "owner": token, "commands": [],
    }

    def save():
        temporary = out / "verification.tmp"
        temporary.write_text(json.dumps(record, indent=2) + "\n")
        temporary.replace(out / "verification.json")

    def command(argv, tag, *, check=True, timeout=30):
        started = time.monotonic()
        result = subprocess.run(argv, capture_output=True, timeout=timeout)
        (out / (tag + ".stdout")).write_bytes(result.stdout)
        (out / (tag + ".stderr")).write_bytes(result.stderr)
        record["commands"].append({
            "argv": argv, "tag": tag, "returncode": result.returncode,
            "seconds": time.monotonic() - started,
            "stdout_sha256": hashlib.sha256(result.stdout).hexdigest(),
            "stderr_sha256": hashlib.sha256(result.stderr).hexdigest(),
        })
        save()
        if check and result.returncode:
            raise RuntimeError(f"{tag} exited {result.returncode}; see {out}")
        return result

    save()
    command(["docker", "image", "inspect", image], "image")
    cid = None
    try:
        created = command([
            "docker", "create", "--pull", "never", "--name", name, "--label", label + "=" + token,
            "--cgroupns", "private", "--network", "none", "--user", "0:0",
            "--cap-drop", "ALL", "--cap-add", "SYS_ADMIN",
            "--security-opt", "no-new-privileges", "--security-opt", "seccomp=unconfined",
            "--security-opt", "apparmor=unconfined", "--pids-limit", "64", "--memory", "128m",
            "--read-only", "--tmpfs", "/tmp:rw,nosuid,nodev,size=8m",
            "--mount", f"type=bind,src={fixtures},dst=/fixtures,readonly",
            "--env", "ASAGIRI_REQUIRE_CGROUP_STARTUP_TEST=1",
            "--env", "ASAGIRI_CGROUP_STARTUP_REPORT=/tmp/cgroup-startup-result.json",
            "--entrypoint", "/bin/sh", image, "-ec",
            "uname -m; grep -E '^(CapEff|CapPrm|NoNewPrivs):' /proc/self/status; "
            "sha256sum /fixtures/cgroup-startup; exec /fixtures/cgroup-startup",
        ], "create")
        cid = created.stdout.decode().strip()
        assert re.fullmatch(r"[0-9a-f]{64}", cid)
        record["container_id"] = cid
        save()
        command(["docker", "start", cid], "start")
        waited = command(["docker", "wait", cid], "wait")
        exit_code = int(waited.stdout.decode().strip())
        logs = command(["docker", "logs", cid], "logs")
        text = logs.stdout.decode()
        lines = text.splitlines()
        architecture = {"x86_64": "x86_64", "aarch64": "aarch64"}[lines[0]]
        states = dict(line.split(":", 1) for line in lines if line.startswith(("CapEff:", "CapPrm:", "NoNewPrivs:")))
        assert int(states["CapEff"], 16) == int(states["CapPrm"], 16) == 1 << 21
        assert int(states["NoNewPrivs"]) == 1
        assert binary_sha + "  /fixtures/cgroup-startup" in lines
        report, = [json.loads(line) for line in lines if line.startswith('{"')]
        (out / "kernel-report.json").write_text(json.dumps(report, indent=2) + "\n")
        assert exit_code == 0, f"the regression exited {exit_code}"
        validate_report(report, source, architecture)
        record.update(
            status="passed", exit_code=exit_code, architecture=architecture,
            capability_mask=1 << 21, no_new_privileges=True,
            kernel_report_sha256=sha(out / "kernel-report.json"),
        )
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        try:
            inspected = command(["docker", "inspect", cid or name], "cleanup-inspect", check=False)
            if inspected.returncode == 0:
                info, = json.loads(inspected.stdout)
                assert info["Name"].lstrip("/") == name
                assert info["Config"]["Labels"][label] == token
                assert cid is None or info["Id"] == cid
                cid = info["Id"]
                command(["docker", "rm", "-f", cid], "cleanup-remove")
                absent = command(["docker", "inspect", cid], "cleanup-absent", check=False)
                assert absent.returncode == 1 and b"No such" in absent.stderr
                record["owned_container_removed"] = True
            else:
                assert cid is None and b"No such" in inspected.stderr
                record["owned_container_removed"] = True
        except BaseException as error:
            record.update(status="failed", cleanup_error=repr(error))
            raise
        finally:
            record["finished_at"] = datetime.now(timezone.utc).isoformat()
            save()
    print(json.dumps(record, indent=2))


if __name__ == "__main__":
    main()
