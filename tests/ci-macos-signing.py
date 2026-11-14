#!/usr/bin/env python3
"""Check signing configuration, credential cleanup, and final artifact ordering."""
import base64
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "signing", Path(__file__).resolve().parents[1] / "scripts/ci/macos-signing.py")
signing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(signing)


class SigningTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = patch.dict(os.environ, {
            "GITHUB_ACTIONS": "true", "RUNNER_TEMP": str(self.root),
            "GITHUB_ENV": str(self.root / "github-env"),
        }, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)
        platform = patch.object(signing.sys, "platform", "darwin")
        platform.start()
        self.addCleanup(platform.stop)

    def credentials(self):
        values = (
            base64.b64encode(b"private certificate").decode(), "private-password",
            base64.b64encode(b"private API key").decode(), "ABCDEFGHIJ",
            "12345678-1234-1234-1234-123456789012",
        )
        os.environ.update(zip(signing.SECRET_NAMES, values))
        return values

    def test_absent_credentials_explicitly_select_ad_hoc(self):
        with patch.object(signing, "run") as run:
            signing.prepare()
        run.assert_not_called()
        text = (self.root / "github-env").read_text()
        self.assertIn("ASAGIRI_SIGN_IDENTITY=-\n", text)
        self.assertIn("ASAGIRI_APPLE_SIGNING=false\n", text)

    def test_partial_credentials_fail_before_keychain_mutation(self):
        os.environ[signing.SECRET_NAMES[0]] = "present"
        with patch.object(signing, "run") as run, self.assertRaisesRegex(ValueError, "Incomplete"):
            signing.prepare()
        run.assert_not_called()
        self.assertFalse((self.root / "asagiri-signing").exists())

    def test_required_signing_cannot_silently_fall_back(self):
        os.environ["ASAGIRI_REQUIRE_APPLE_SIGNING"] = "true"
        with self.assertRaisesRegex(ValueError, "required"):
            signing.configuration()

    def test_personal_mac_keychains_are_never_changed(self):
        os.environ.pop("GITHUB_ACTIONS")
        with patch.object(signing, "run") as run, self.assertRaisesRegex(RuntimeError, "Actions"):
            signing.prepare()
        run.assert_not_called()

    def test_preparation_exports_identity_but_no_private_material(self):
        credentials = self.credentials()
        fingerprint = "A" * 40

        def response(*args):
            if args[1] == "list-keychains" and "-s" not in args:
                return '"/original/login.keychain-db"\n'
            if args[1] == "find-identity":
                return f'1) {fingerprint} "Developer ID Application: Test (1234567890)"\n'
            return ""

        output = io.StringIO()
        with patch.object(signing, "run", side_effect=response) as run, contextlib.redirect_stdout(output):
            signing.prepare()
        text = (self.root / "github-env").read_text()
        self.assertIn(f"ASAGIRI_SIGN_IDENTITY={fingerprint}\n", text)
        self.assertIn("ASAGIRI_APPLE_SIGNING=true\n", text)
        for secret in credentials:
            self.assertNotIn(secret, text + output.getvalue())
        directory = self.root / "asagiri-signing"
        self.assertFalse((directory / "certificate.p12").exists())
        self.assertFalse((directory / "AuthKey.p8").exists())
        calls = [call.args for call in run.call_args_list]
        self.assertTrue(any(args[:3] == ("xcrun", "notarytool", "store-credentials") for args in calls))
        with patch.object(signing, "run") as run:
            signing.cleanup()
        self.assertEqual(run.call_args_list[0].args,
                         ("security", "list-keychains", "-d", "user", "-s", "/original/login.keychain-db"))
        self.assertFalse(directory.exists())

    def test_final_artifacts_include_tickets_before_checksums(self):
        directory = self.root / "asagiri-signing"
        directory.mkdir()
        (directory / "state.json").write_text(json.dumps({"keychain": "/temporary/keychain"}))
        os.environ["ASAGIRI_APPLE_SIGNING"] = "true"
        app = self.root / "Asagiri.app"

        def response(*args):
            if args[:3] == ("xcrun", "notarytool", "submit"):
                return '{"status":"Accepted","id":"12345678-1234-1234-1234-123456789012"}'
            if args[0] == "ditto":
                Path(args[-1]).write_bytes(b"notary upload")
            return ""

        with patch.object(signing, "run", side_effect=response) as run:
            signing.finalize(app, "arm64", "0.1.0")
        calls = [call.args for call in run.call_args_list]
        staple_app = calls.index(("xcrun", "stapler", "staple", str(app)))
        build_dmg = calls.index(("cargo", "xtask", "dmg", "--arch", "arm64"))
        staple_dmg = next(i for i, args in enumerate(calls)
                          if args[:3] == ("xcrun", "stapler", "staple") and args[-1].endswith(".dmg"))
        pack = calls.index(("cargo", "xtask", "pack", "--arch", "arm64"))
        self.assertLess(staple_app, build_dmg)
        self.assertLess(build_dmg, staple_dmg)
        self.assertLess(staple_dmg, pack)
        self.assertEqual(sum(args[:3] == ("xcrun", "notarytool", "submit") for args in calls), 2)
        self.assertFalse((directory / "notary-app.zip").exists())
        self.assertTrue(any(args[:4] == ("spctl", "--assess", "--type", "execute") for args in calls))

    def test_apple_rejection_stops_packaging_and_preserves_diagnostics(self):
        directory = self.root / "asagiri-signing"
        directory.mkdir()
        (directory / "state.json").write_text(json.dumps({"keychain": "/temporary/keychain"}))
        os.environ["ASAGIRI_APPLE_SIGNING"] = "true"

        def response(*args):
            if args[:3] == ("xcrun", "notarytool", "submit"):
                return '{"status":"Invalid","id":"12345678-1234-1234-1234-123456789012"}'
            return "{}"

        with patch.object(signing, "run", side_effect=response) as run, self.assertRaisesRegex(RuntimeError, "did not accept"):
            signing.finalize(self.root / "Asagiri.app", "arm64", "0.1.0")
        self.assertFalse(any(call.args[0] == "cargo" for call in run.call_args_list))
        self.assertTrue((directory / "app-failure.json").exists())

    def test_nonzero_submission_exit_preserves_id_and_rejection_log(self):
        directory = self.root / "asagiri-signing"
        directory.mkdir()
        output = '{"status":"Invalid","id":"12345678-1234-1234-1234-123456789012"}'
        with patch.object(signing, "run", side_effect=[
            signing.CommandFailure("xcrun", 1, output), '{"issues":[]}']):
            with self.assertRaisesRegex(RuntimeError, "did not accept"):
                signing.submit(Path("app.zip"), Path("temporary.keychain"), "app")
        self.assertEqual(json.loads((directory / "app-submission.json").read_text())["status"], "Invalid")
        self.assertTrue((directory / "app-failure.json").exists())

    def test_wait_timeout_retains_submission_when_log_is_unavailable(self):
        directory = self.root / "asagiri-signing"
        directory.mkdir()
        output = '{"status":"In Progress","id":"12345678-1234-1234-1234-123456789012"}'
        with patch.object(signing, "run", side_effect=[
            signing.CommandFailure("xcrun", 1, output),
            signing.CommandFailure("xcrun", 1, "log not available")]):
            with self.assertRaisesRegex(RuntimeError, "did not accept"):
                signing.submit(Path("app.zip"), Path("temporary.keychain"), "app")
        self.assertEqual(json.loads((directory / "app-submission.json").read_text())["status"], "In Progress")
        self.assertFalse((directory / "app-failure.json").exists())


if __name__ == "__main__":
    unittest.main()
