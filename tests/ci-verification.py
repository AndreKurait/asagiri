#!/usr/bin/env python3
"""Exercise proof-input edits and corrupted audit associations."""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts/ci" / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RuntimeProofRunnerTests(unittest.TestCase):
    def run_fixture(self, changed=None):
        runner = load("verify-runtime")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            paths = {"scripts/ci/verify-ipam.py", "scripts/ci/verify-runtime.py", "scripts/ci/verus.json"}
            for name, contract in runner.CONTRACTS.items():
                paths.update((contract["source"], "proofs/" + name + ".rs"))
                paths.update(contract.get("annotations", {}).values())
            for path in paths:
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / path, target)
            runner.ROOT = root
            tools = runner.support()
            calls = 0

            def verify(_binary, source, _output, _pin):
                nonlocal calls
                calls += 1
                if calls == 1 and changed:
                    target = root / changed
                    target.write_bytes(target.read_bytes() + b"\n")
                production = source.name == "production.rs"
                # These doubles exercise the runner's bookkeeping only.
                # Actual proofs and negative controls still run with Verus in CI.
                return (0 if production else 1), {
                    "success": production, "errors": 0 if production else 1,
                    "verified": 10, "is-verifying-entire-crate": True,
                    "encountered-vir-error": False,
                    "error_diagnostics": [] if production else ["postcondition not satisfied"],
                }

            def git(command, **_kwargs):
                if command == ["git", "rev-parse", "HEAD"]:
                    return "a" * 40 + "\n"
                self.assertEqual(command, ["git", "status", "--porcelain"])
                # A second edit to an already-dirty file leaves this unchanged.
                return b" M crates/asagiri/src/daemon/health_transition.rs\n"

            output = root / "record"
            caught = None
            with (
                patch.object(runner, "support", return_value=tools),
                patch.object(tools, "install_verus", return_value=(None, root / "verus", {})),
                patch.object(tools, "verify", side_effect=verify),
                patch.object(tools.subprocess, "check_output", side_effect=git),
                patch("sys.argv", ["verify-runtime.py", "--cache", str(root / "cache"),
                                   "--output", str(output)]),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                try:
                    runner.main()
                except ValueError as error:
                    caught = error
            return caught, json.loads((output / "verification.json").read_text())

    def test_unchanged_dirty_inputs_are_allowed_and_recorded(self):
        error, record = self.run_fixture()
        self.assertIsNone(error)
        self.assertEqual(record["status"], "passed")
        self.assertFalse(record["worktree_clean"])

    def test_editing_an_already_dirty_production_input_fails_the_run(self):
        error, record = self.run_fixture("crates/asagiri/src/daemon/health_transition.rs")
        self.assertIsNotNone(error, "A second dirty-file edit was silently accepted")
        self.assertIn("Checkout changed during proof verification", str(error))
        self.assertEqual(record["status"], "failed")

    def test_editing_a_proof_or_toolchain_pin_fails_the_run(self):
        for path in ["proofs/health-transition.rs", "proofs/http-number-loop.txt",
                     "scripts/ci/verus.json"]:
            with self.subTest(path=path):
                error, record = self.run_fixture(path)
                self.assertIsNotNone(error, "An edited proof input was silently accepted")
                self.assertEqual(record["status"], "failed")

    def test_cleanup_failure_preserves_the_original_proof_error_and_manifest(self):
        tools = load("verify-ipam")
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            record = {"status": "failed", "error": "ValueError: original proof failure"}
            with patch.object(tools.shutil, "rmtree", side_effect=OSError("cache cleanup failed")):
                with self.assertRaisesRegex(ValueError, "original proof failure"):
                    try:
                        raise ValueError("original proof failure")
                    finally:
                        tools.finish_record(record, output, output / "extracted")
            saved = json.loads((output / "verification.json").read_text())
            self.assertEqual(saved["error"], "ValueError: original proof failure")
            self.assertIn("cache cleanup failed", saved["cleanup_error"])
            self.assertEqual(saved["status"], "failed")


class AuditCoverageTests(unittest.TestCase):
    def setUp(self):
        self.documents = {
            name: json.loads((ROOT / "docs" / name).read_text())
            for name in ["upstream-issue-audit.json", "issue-verification.json",
                         "upstream-audit-tasks.json", "upstream-pr-audit.json"]
        }

    def check(self):
        checker = load("verify-issue-coverage")
        original_read = Path.read_text

        def read(path, *args, **kwargs):
            if path.parent == ROOT / "docs" and path.name in self.documents:
                return json.dumps(self.documents[path.name])
            return original_read(path, *args, **kwargs)

        # --write avoids an unrelated Markdown mismatch concealing a missing
        # association check. Writes are intercepted; the checkout is untouched.
        with (patch.object(Path, "read_text", read), patch.object(Path, "write_text"),
              patch("sys.argv", ["verify-issue-coverage.py", "--write"]),
              contextlib.redirect_stdout(io.StringIO())):
            checker.main()

    def test_the_real_catalog_and_shared_issue_targets_are_valid(self):
        self.check()

    def test_an_issue_cannot_link_to_an_unrelated_existing_task(self):
        rows = self.documents["issue-verification.json"]["issues"]
        row = next(row for row in rows if row["issue"] == 14)
        row["tracking"] = copy.deepcopy(next(row for row in rows if row.get("tracking", {}).get("issue") == 92)["tracking"])
        with self.assertRaises(AssertionError):
            self.check()

    def test_a_pr_cannot_link_to_an_unrelated_existing_task(self):
        rows = self.documents["upstream-pr-audit.json"]["pulls"]
        row = next(row for row in rows if row["number"] == 119)
        row["tracking"] = copy.deepcopy(next(row for row in rows if row.get("tracking", {}).get("issue") == 260)["tracking"])
        with self.assertRaises(AssertionError):
            self.check()

    def test_a_supplemental_finding_cannot_change_its_public_source(self):
        finding = self.documents["upstream-audit-tasks.json"]["additional_source_findings"][0]
        finding["upstream_url"] = "https://github.com/libracoder/kiri/pull/512"
        with self.assertRaises(AssertionError):
            self.check()

    def test_an_unknown_pr_state_cannot_escape_the_counted_buckets(self):
        audit = self.documents["upstream-pr-audit.json"]
        row = next(row for row in audit["pulls"] if row["state"] == "open")
        row["state"] = "open "
        audit["counts"]["open"] -= 1
        with self.assertRaises(AssertionError):
            self.check()


if __name__ == "__main__":
    unittest.main()
