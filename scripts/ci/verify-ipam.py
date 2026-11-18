#!/usr/bin/env python3
"""Verify the production address-gap search with pinned Verus and Z3."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import tempfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = "crates/asagiri/src/daemon/address_gap.rs"
SIGNATURE = "pub(super) fn first_gap<I: Iterator<Item = u128>>(occupied: I, first: u128, last: u128) -> Option<u128> {"
LOOP = "    for address in occupied {"
MUTANTS = {
    "skip-free-address": ("candidate += 1;", "candidate += 2;"),
    "return-occupied-address": ("return Some(candidate);", "return Some(address);"),
    "ignore-upper-bound": ("if candidate == last {", "if candidate == u128::MAX {"),
    "ignore-lower-bound": ("let mut candidate = first;", "let mut candidate = 0u128;"),
    "reject-single-address-range": ("if first > last {", "if first >= last {"),
    "return-used-address": ("if address == candidate {", "if address < candidate {"),
}


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def body(source):
    if source.count(SIGNATURE) != 1:
        raise ValueError("Production signature changed; update and review the proof contract")
    prefix, rest = source.split(SIGNATURE)
    if any(line.strip() and not line.startswith("//!") for line in prefix.splitlines()):
        raise ValueError("Expected only module documentation before the verified function")
    if not rest.endswith("}\n"):
        raise ValueError("The verified function must occupy the remainder of its source file")
    return rest[:-2].strip("\n")


def render(source, template, invariant):
    executable = body(source)
    if executable.count(LOOP) != 1 or template.count("/* EXECUTABLE_BODY */") != 1:
        raise ValueError("Proof insertion point changed")
    annotated = executable.replace(LOOP, invariant.rstrip())
    # Reversing the sole body insertion must recover the executable bytes.
    if annotated.replace(invariant.rstrip(), LOOP) != executable:
        raise ValueError("Proof instrumentation changed executable code")
    return template.replace("/* EXECUTABLE_BODY */", annotated)


def checked_archive(path, asset):
    if path.stat().st_size != asset["bytes"] or sha256(path.read_bytes()) != asset["sha256"]:
        raise ValueError("Verus archive does not match its pinned digest and size")


def install_verus(cache, pin):
    host = platform.system() + "-" + platform.machine()
    asset = pin["assets"].get(host)
    if asset is None:
        raise ValueError("No pinned Verus binary for " + host)
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / ("verus-" + pin["version"] + "-" + asset["platform"] + ".zip")
    if not archive.exists():
        url = "https://github.com/verus-lang/verus/releases/download/release/" + pin["version"] + "/" + archive.name
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=cache, delete=False) as out:
                temporary = Path(out.name)
                with urllib.request.urlopen(url, timeout=120) as response:
                    shutil.copyfileobj(response, out)
            checked_archive(temporary, asset)
            temporary.replace(archive)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
    checked_archive(archive, asset)
    # Extract afresh from the verified archive: an existing cache executable
    # must not bypass the digest check. The directory belongs to this run.
    extracted = Path(tempfile.mkdtemp(prefix="verified-verus-", dir=cache))
    try:
        prefix = "verus-" + asset["platform"]
        with zipfile.ZipFile(archive) as bundle:
            if sum(info.file_size for info in bundle.infolist()) > 512 * 1024**2:
                raise ValueError("Verus archive exceeds the extraction limit")
            for info in bundle.infolist():
                path = Path(info.filename)
                mode = info.external_attr >> 16
                if path.is_absolute() or ".." in path.parts or path.parts[0] != prefix or stat.S_ISLNK(mode):
                    raise ValueError("Unsafe Verus archive entry: " + info.filename)
            bundle.extractall(extracted)
            for info in bundle.infolist():
                if (info.external_attr >> 16) & 0o111:
                    (extracted / info.filename).chmod(0o755)
        return extracted, extracted / prefix / "verus", asset
    except BaseException:
        shutil.rmtree(extracted)
        raise


def verify(binary, file, out, pin):
    command = [str(binary), str(file), "--no-cheating", "--output-json", "--num-threads", "2"]
    env = dict(os.environ, RUSTUP_TOOLCHAIN=pin["rust"])
    for name in ("VERUS_Z3_PATH", "VERUS_CVC5_PATH", "VERUS_ROOT", "VARGO_TARGET_DIR"):
        env.pop(name, None)
    result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=120)
    out.with_suffix(".stdout.json").write_text(result.stdout)
    out.with_suffix(".stderr.txt").write_text(result.stderr)
    parsed = json.loads(result.stdout)
    if parsed["verus"]["version"] != pin["version"] or parsed["verus"]["commit"] != pin["commit"]:
        raise ValueError("Unexpected Verus version or source commit")
    verification = parsed["verification-results"]
    verification["error_diagnostics"] = [
        message for message in re.findall(r"^error: (.+)$", result.stderr, re.MULTILINE)
        if not message.startswith("aborting due to ")
    ]
    return result.returncode, verification


def rejected_by_proof(returncode, result, expected):
    """A compile error or exhausted solver budget is not a negative control."""
    diagnostics = result.get("error_diagnostics", [])
    return (
        returncode != 0 and not result["success"] and result["errors"] > 0
        and not result["encountered-vir-error"] and bool(diagnostics)
        and all(message in expected for message in diagnostics)
    )


def source_state(paths):
    status = subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)
    return {
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "worktree_clean": not status.strip(),
        "worktree_status_sha256": sha256(status),
        "proof_input_sha256": {
            path: sha256((ROOT / path).read_bytes()) for path in sorted(paths)
        },
    }


def finish_record(record, output, extracted):
    """Cleanup must never mask a proof failure or prevent its manifest."""
    cleanup_error = None
    try:
        if extracted is not None:
            shutil.rmtree(extracted)
    except OSError as error:
        cleanup_error = error
        record["cleanup_error"] = type(error).__name__ + ": " + str(error)
    already_failed = record["status"] == "failed"
    if cleanup_error is not None:
        record["status"] = "failed"
    record["completed_at"] = datetime.now(timezone.utc).isoformat()
    (output / "verification.json").write_text(json.dumps(record, indent=2) + "\n")
    if cleanup_error is not None and not already_failed:
        raise cleanup_error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.cache = args.cache.resolve()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    source = (ROOT / SOURCE).read_text()
    template = (ROOT / "proofs/address-gap.rs").read_text()
    invariant = (ROOT / "proofs/address-gap-loop.txt").read_text()
    pin = json.loads((ROOT / "scripts/ci/verus.json").read_text())
    generated = args.output / "address-gap.rs"
    generated.write_text(render(source, template, invariant))
    record = {
        "schema": "asagiri.ipam-proof.v1",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "worktree_clean": not subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip(),
        "source": SOURCE, "source_sha256": sha256(source.encode()),
        "executable_body_sha256": sha256(body(source).encode()),
        "generated_sha256": sha256(generated.read_bytes()),
        "template_sha256": sha256(template.encode()),
        "invariant_sha256": sha256(invariant.encode()),
        "verus": pin["version"], "verus_commit": pin["commit"],
        "no_cheating": True, "negative_controls": {}, "status": "running",
    }
    extracted = None
    try:
        extracted, binary, asset = install_verus(args.cache, pin)
        record["verus_archive"] = asset
        rc, result = verify(binary, generated, args.output / "production", pin)
        record["production"] = result
        if rc or not result["success"] or result["errors"] or not result["is-verifying-entire-crate"]:
            raise ValueError("Production address-gap proof failed")
        if result["verified"] < 3:
            raise ValueError("Expected the search, its loop, and non-vacuity witnesses to be verified")
        for name, (old, new) in MUTANTS.items():
            if source.count(old) != 1:
                raise ValueError("Negative-control mutation no longer identifies one source expression: " + name)
            mutant = args.output / (name + ".rs")
            mutant.write_text(render(source.replace(old, new), template, invariant))
            rc, result = verify(binary, mutant, args.output / name, pin)
            record["negative_controls"][name] = result
            if not rejected_by_proof(rc, result, (
                "postcondition not satisfied",
                "possible arithmetic underflow/overflow",
                "invariant not satisfied before loop",
                "invariant not satisfied at end of loop body",
                "assertion failed",
            )):
                raise ValueError("Negative control was not rejected by the prover: " + name)
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = type(error).__name__ + ": " + str(error)
        raise
    finally:
        finish_record(record, args.output, extracted)
    print(json.dumps(record, indent=2))


if __name__ == "__main__":
    main()
