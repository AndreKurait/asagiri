#!/usr/bin/env python3
"""Verify production runtime transitions with pinned Verus and negative controls."""
import argparse
from datetime import datetime, timezone
import importlib.util
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONTRACTS = {
    "lifecycle-advance": {
        "source": "crates/asagiri/src/daemon/lifecycle_advance.rs",
        "signature": "pub(super) fn advance(current: u64) -> u64 {",
        "negative_diagnostics": ("postcondition not satisfied", "possible arithmetic underflow/overflow"),
        "minimum_verified": 2,
        "mutants": {
            "retain-token": ("current + 1", "current"),
            "skip-wrap": ("current == u64::MAX", "false"),
        },
    },
    "lifecycle-generation": {
        "source": "crates/asagiri/src/daemon/lifecycle_generation.rs",
        "signature": "pub(super) fn is_current(current: u64, captured: u64) -> bool {",
        "negative_diagnostics": ("postcondition not satisfied",),
        "minimum_verified": 2,
        "mutants": {
            "accept-stale": ("current == captured", "current >= captured"),
            "reject-current": ("current == captured", "current != captured"),
        },
    },
    "layer-admission": {
        "source": "crates/asagiri/src/daemon/layer_admission.rs",
        "signature": "pub(super) fn transition(active: usize, limit: usize, acquiring: bool, busy: bool, cancelled: bool) -> Option<usize> {",
        "negative_diagnostics": ("postcondition not satisfied", "possible arithmetic underflow/overflow"),
        "minimum_verified": 2,
        "mutants": {
            "ignore-cancellation": ("cancelled || busy", "false || busy"),
            "ignore-digest-owner": ("cancelled || busy", "cancelled || false"),
            "exceed-capacity": ("active == limit", "false"),
            "uncounted-permit": ("Some(active + 1)", "Some(active)"),
            "release-empty": ("active == 0", "false"),
            "accept-invalid-count": ("active > limit", "false"),
        },
    },
    "health-transition": {
        "source": "crates/asagiri/src/daemon/health_transition.rs",
        "signature": "pub(super) fn after_probe(success: bool, starting: bool, in_start_period: bool, failures: u32, retries: u32) -> (u32, Option<bool>) {",
        "negative_diagnostics": ("postcondition not satisfied", "possible arithmetic underflow/overflow"),
        "minimum_verified": 2,
        "mutants": {
            "overflow-streak": ("failures == u32::MAX", "false"),
            "skip-failure": ("failures + 1", "failures"),
            "delay-unhealthy": ("counted >= retries", "counted > retries"),
            "success-keeps-streak": ("return (0, Some(true));", "return (failures, Some(true));"),
            "extend-grace": ("starting && in_start_period", "starting || in_start_period"),
            "failure-reports-healthy": ("Some(false)", "Some(true)"),
        },
    },
    "http-number": {
        "source": "crates/asagiri/src/daemon/http_number.rs",
        "signature": "pub(super) fn parse_number(bytes: &[u8], hex: bool) -> Option<u64> {",
        "minimum_verified": 5,
        "negative_diagnostics": (
            "postcondition not satisfied", "invariant not satisfied before loop",
            "invariant not satisfied at end of loop body", "assertion failed",
        ),
        "annotations": {
            "    while index < bytes.len() {": "proofs/http-number-loop.txt",
            "                // Reject the first invalid digit without scanning the suffix.": "proofs/http-number-invalid.txt",
            "            // A later digit cannot make an overflowing prefix representable.": "proofs/http-number-overflow.txt",
        },
        "mutants": {
            "accept-empty": ("if bytes.is_empty() {\n        return None;", "if bytes.is_empty() {\n        return Some(0);"),
            "accept-invalid-digit": ("b'0'..=b'9'", "b'0'..=b':'"),
            "ignore-overflow": ("next > u64::MAX as u128", "false"),
            "hexadecimal-base-ten": ("if hex { 16u128 } else { 10u128 }", "if hex { 10u128 } else { 10u128 }"),
            "uppercase-off-by-one": ("byte - b'A' + 10", "byte - b'A' + 11"),
            "skip-digit": ("value = next as u64;", "value = 0;"),
        },
    },
}


def support():
    spec = importlib.util.spec_from_file_location("asagiri_ipam_verifier", ROOT / "scripts/ci/verify-ipam.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def executable(source, signature):
    if source.count(signature) != 1:
        raise ValueError("Production signature changed; review the runtime proof contract")
    prefix, rest = source.split(signature)
    if any(line.strip() and not line.startswith("//!") for line in prefix.splitlines()):
        raise ValueError("Expected only module documentation before the verified function")
    if not rest.endswith("}\n"):
        raise ValueError("The verified function must occupy its source file")
    return rest[:-2].strip("\n")


def render(source, signature, template, annotations):
    if template.count("/* EXECUTABLE_BODY */") != 1:
        raise ValueError("The proof must contain exactly one production body insertion")
    body = executable(source, signature)
    annotated = body
    for anchor, insertion in annotations.items():
        if annotated.count(anchor) != 1:
            raise ValueError("Proof annotation must identify one source location")
        annotated = annotated.replace(anchor, insertion)
    recovered = annotated
    for anchor, insertion in reversed(list(annotations.items())):
        recovered = recovered.replace(insertion, anchor)
    if recovered != body:
        raise ValueError("Proof instrumentation changed executable code")
    return template.replace("/* EXECUTABLE_BODY */", annotated)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.cache = args.cache.resolve()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    tools = support()
    paths = {"scripts/ci/verify-runtime.py", "scripts/ci/verify-ipam.py", "scripts/ci/verus.json"}
    for name, contract in CONTRACTS.items():
        paths.update((contract["source"], "proofs/" + name + ".rs"))
        paths.update(contract.get("annotations", {}).values())
    checkout_before = tools.source_state(paths)
    pin = json.loads((ROOT / "scripts/ci/verus.json").read_text())
    record = {
        "schema": "asagiri.runtime-proof.v1",
        "started_at": datetime.now(timezone.utc).isoformat(),
        **checkout_before,
        "verus": pin["version"], "verus_commit": pin["commit"],
        "no_cheating": True, "contracts": {}, "status": "running",
    }
    extracted = None
    try:
        inputs = {
            name: {
                "source": (ROOT / contract["source"]).read_text(),
                "template": (ROOT / ("proofs/" + name + ".rs")).read_text(),
                "annotations": {
                    path: (ROOT / path).read_text()
                    for path in contract.get("annotations", {}).values()
                },
            }
            for name, contract in CONTRACTS.items()
        }
        record["source_state_after_read"] = tools.source_state(paths)
        if record["source_state_after_read"] != checkout_before:
            raise ValueError("Checkout changed while reading proof inputs")
        extracted, binary, asset = tools.install_verus(args.cache, pin)
        record["verus_archive"] = asset
        for name, contract in CONTRACTS.items():
            folder = args.output / name
            folder.mkdir()
            source = inputs[name]["source"]
            template = inputs[name]["template"]
            annotations = {
                anchor: inputs[name]["annotations"][path].rstrip("\n")
                for anchor, path in contract.get("annotations", {}).items()
            }
            body = executable(source, contract["signature"])
            generated = folder / "production.rs"
            generated.write_text(render(source, contract["signature"], template, annotations))
            result = {
                "source": contract["source"], "source_sha256": tools.sha256(source.encode()),
                "executable_body_sha256": tools.sha256(body.encode()),
                "template_sha256": tools.sha256(template.encode()),
                "annotation_sha256": {
                    path: tools.sha256(text.encode())
                    for path, text in inputs[name]["annotations"].items()
                },
                "generated_sha256": tools.sha256(generated.read_bytes()),
                "negative_controls": {},
            }
            record["contracts"][name] = result
            rc, verified = tools.verify(binary, generated, folder / "production", pin)
            result["production"] = verified
            if rc or not verified["success"] or verified["errors"] or not verified["is-verifying-entire-crate"]:
                raise ValueError("Production runtime proof failed: " + name)
            if verified["verified"] < contract["minimum_verified"]:
                raise ValueError("Expected the complete contract and its non-vacuity witnesses to be verified")
            for mutation, (before, after) in contract["mutants"].items():
                if source.count(before) != 1:
                    raise ValueError("Mutation must identify one production expression: " + mutation)
                mutant = folder / (mutation + ".rs")
                mutant.write_text(render(source.replace(before, after), contract["signature"], template, annotations))
                rc, verified = tools.verify(binary, mutant, folder / mutation, pin)
                result["negative_controls"][mutation] = verified
                if not tools.rejected_by_proof(rc, verified, contract["negative_diagnostics"]):
                    raise ValueError("Broken production variant was not rejected by the prover: " + mutation)
        record["source_state_after_proof"] = tools.source_state(paths)
        if record["source_state_after_proof"] != checkout_before:
            raise ValueError("Checkout changed during proof verification")
        record["status"] = "passed"
    except BaseException as error:
        record["status"] = "failed"
        record["error"] = type(error).__name__ + ": " + str(error)
        raise
    finally:
        tools.finish_record(record, args.output, extracted)
    print(json.dumps(record, indent=2))


if __name__ == "__main__":
    main()
