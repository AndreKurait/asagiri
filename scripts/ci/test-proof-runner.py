#!/usr/bin/env python3
"""Regressions for trustworthy verifier execution and failure evidence."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("ipam_verifier", Path(__file__).with_name("verify-ipam.py"))
TOOLS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TOOLS)


class ProofRunnerTests(unittest.TestCase):
    def result(self, diagnostics):
        return {
            "success": False, "errors": 1, "encountered-vir-error": False,
            "error_diagnostics": diagnostics,
        }

    def test_only_expected_proof_failures_count(self):
        expected = ("postcondition not satisfied", "possible arithmetic underflow/overflow")
        for diagnostic in expected:
            self.assertTrue(TOOLS.rejected_by_proof(1, self.result([diagnostic]), expected))
        for diagnostics in (
            [], ["resource limit exceeded"], ["solver timed out"],
            ["postcondition not satisfied", "resource limit exceeded"],
            ["mismatched types"], ["invariant not satisfied before loop"],
        ):
            with self.subTest(diagnostics=diagnostics):
                self.assertFalse(TOOLS.rejected_by_proof(1, self.result(diagnostics), expected))
        valid = self.result(["postcondition not satisfied"])
        self.assertFalse(TOOLS.rejected_by_proof(0, valid, expected))
        for key, value in (("success", True), ("errors", 0), ("encountered-vir-error", True)):
            self.assertFalse(TOOLS.rejected_by_proof(1, dict(valid, **{key: value}), expected))

    def test_library_and_solver_overrides_cannot_escape_archive_pin(self):
        overrides = {name: "/another/installation" for name in (
            "VERUS_ROOT", "VARGO_TARGET_DIR", "VERUS_Z3_PATH", "VERUS_CVC5_PATH",
        )}
        pin = {"rust": "pinned-rust", "version": "pinned-verus", "commit": "pinned-commit"}
        reply = {
            "verus": {"version": pin["version"], "commit": pin["commit"]},
            "verification-results": self.result([]),
        }
        completed = subprocess.CompletedProcess(
            [], 1, json.dumps(reply),
            "error: postcondition not satisfied\nerror: aborting due to 1 previous error\n",
        )
        with tempfile.TemporaryDirectory() as folder, mock.patch.dict(os.environ, overrides):
            with mock.patch.object(TOOLS.subprocess, "run", return_value=completed) as run:
                _, result = TOOLS.verify(Path("/pinned/verus"), Path("source.rs"), Path(folder) / "proof", pin)
            env = run.call_args.kwargs["env"]
            self.assertEqual(env["RUSTUP_TOOLCHAIN"], pin["rust"])
            for name in overrides:
                self.assertNotIn(name, env)
            self.assertEqual(result["error_diagnostics"], ["postcondition not satisfied"])

    def test_proof_failure_survives_cleanup_failure(self):
        with tempfile.TemporaryDirectory() as folder:
            record = {"status": "failed", "error": "ValueError: original proof failure"}
            with mock.patch.object(TOOLS.shutil, "rmtree", side_effect=OSError("cleanup failed")):
                TOOLS.finish_record(record, Path(folder), Path("/owned/cache"))
            saved = json.loads((Path(folder) / "verification.json").read_text())
            self.assertEqual(saved["status"], "failed")
            self.assertEqual(saved["error"], "ValueError: original proof failure")
            self.assertEqual(saved["cleanup_error"], "OSError: cleanup failed")

    def test_cleanup_failure_after_success_is_reported_and_fails(self):
        with tempfile.TemporaryDirectory() as folder:
            record = {"status": "passed"}
            with mock.patch.object(TOOLS.shutil, "rmtree", side_effect=OSError("cleanup failed")):
                with self.assertRaisesRegex(OSError, "cleanup failed"):
                    TOOLS.finish_record(record, Path(folder), Path("/owned/cache"))
            saved = json.loads((Path(folder) / "verification.json").read_text())
            self.assertEqual(saved["status"], "failed")
            self.assertIn("cleanup_error", saved)


if __name__ == "__main__":
    unittest.main()
