#!/usr/bin/env python3
"""Verify production runtime transitions with pinned Verus and negative controls."""
import argparse
from datetime import datetime, timezone
import importlib.util
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONTRACTS = {
    "health-transition": {
        "source": "crates/asagiri/src/daemon/health_transition.rs",
        "signature": "pub(super) fn after_probe(success: bool, starting: bool, in_start_period: bool, failures: u32, retries: u32) -> (u32, Option<bool>) {",
        "negative_diagnostics": ("postcondition not satisfied", "possible arithmetic underflow/overflow"),
        "mutants": {
            "overflow-streak": ("failures == u32::MAX", "false"),
            "skip-failure": ("failures + 1", "failures"),
            "delay-unhealthy": ("counted >= retries", "counted > retries"),
            "success-keeps-streak": ("return (0, Some(true));", "return (failures, Some(true));"),
            "extend-grace": ("starting && in_start_period", "starting || in_start_period"),
            "failure-reports-healthy": ("Some(false)", "Some(true)"),
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


def render(source, signature, template):
    if template.count("/* EXECUTABLE_BODY */") != 1:
        raise ValueError("The proof must contain exactly one production body insertion")
    body = executable(source, signature)
    return template.replace("/* EXECUTABLE_BODY */", body)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.cache = args.cache.resolve()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    tools = support()
    checkout_before = tools.source_state()
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
            name: ((ROOT / contract["source"]).read_text(), (ROOT / ("proofs/" + name + ".rs")).read_text())
            for name, contract in CONTRACTS.items()
        }
        record["source_state_after_read"] = tools.source_state()
        if record["source_state_after_read"] != checkout_before:
            raise ValueError("Checkout changed while reading proof inputs")
        extracted, binary, asset = tools.install_verus(args.cache, pin)
        record["verus_archive"] = asset
        for name, contract in CONTRACTS.items():
            folder = args.output / name
            folder.mkdir()
            source, template = inputs[name]
            body = executable(source, contract["signature"])
            generated = folder / "production.rs"
            generated.write_text(render(source, contract["signature"], template))
            result = {
                "source": contract["source"], "source_sha256": tools.sha256(source.encode()),
                "executable_body_sha256": tools.sha256(body.encode()),
                "template_sha256": tools.sha256(template.encode()),
                "generated_sha256": tools.sha256(generated.read_bytes()),
                "negative_controls": {},
            }
            record["contracts"][name] = result
            rc, verified = tools.verify(binary, generated, folder / "production", pin)
            result["production"] = verified
            if rc or not verified["success"] or verified["errors"] or not verified["is-verifying-entire-crate"]:
                raise ValueError("Production runtime proof failed: " + name)
            if verified["verified"] < 2:
                raise ValueError("Expected the transition and its non-vacuity witnesses to be verified")
            for mutation, (before, after) in contract["mutants"].items():
                if source.count(before) != 1:
                    raise ValueError("Mutation must identify one production expression: " + mutation)
                mutant = folder / (mutation + ".rs")
                mutant.write_text(render(source.replace(before, after), contract["signature"], template))
                rc, verified = tools.verify(binary, mutant, folder / mutation, pin)
                result["negative_controls"][mutation] = verified
                if not tools.rejected_by_proof(rc, verified, contract["negative_diagnostics"]):
                    raise ValueError("Broken production variant was not rejected by the prover: " + mutation)
        record["source_state_after_proof"] = tools.source_state()
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
