#!/usr/bin/env python3
"""Exercise release selection and distribution integrity boundaries."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "distribution", Path(__file__).resolve().parents[1] / "scripts/ci/distribution.py")
distribution = importlib.util.module_from_spec(spec)
spec.loader.exec_module(distribution)


def release(tag, *, draft=False, prerelease=None):
    return {"tag_name": tag, "draft": draft,
            "prerelease": "-dev." in tag if prerelease is None else prerelease}


class DistributionTests(unittest.TestCase):
    def test_empty_repository_has_no_fake_downloads(self):
        self.assertEqual(distribution.channels([]), {"latest": None, "stable": None, "preview": None})

    def test_latest_uses_preview_until_a_stable_release_exists(self):
        preview = release("v0.1.0-dev.37")
        self.assertEqual(distribution.channels([preview])["latest"], preview)
        stable = release("v0.1.0")
        newer_preview = release("v0.2.0-dev.99")
        result = distribution.channels([newer_preview, stable, preview])
        self.assertEqual(result, {"latest": stable, "stable": stable, "preview": newer_preview})

    def test_late_completion_of_an_old_build_cannot_downgrade_installs(self):
        first = release("v0.1.0-dev.9")
        newer = release("v0.1.0-dev.10")
        self.assertEqual(distribution.channels([first, newer])["preview"], newer)
        self.assertEqual(distribution.channels([release("v0.1.9"), release("v0.1.10")])["stable"],
                         release("v0.1.10"))

    def test_drafts_invalid_tags_and_mislabeled_channels_are_excluded(self):
        bad = [release("v0.1.0", draft=True), release("v01.2.3"), release("v1.0.0-rc.1"),
               release("v1.0.0-dev.1", prerelease=False), release("v1.0.0", prerelease=True)]
        self.assertEqual(distribution.channels(bad)["latest"], None)

    def test_downloads_must_use_the_exact_versioned_repository_url_and_digest(self):
        with tempfile.TemporaryDirectory() as folder:
            good = {"name": "Asagiri-0.1.0-arm64.dmg", "size": 1,
                    "digest": "sha256:" + "a" * 64,
                    "browser_download_url": "https://example.com/Asagiri-0.1.0-arm64.dmg"}
            with patch.object(distribution.urllib.request, "urlopen") as request, self.assertRaises(ValueError):
                distribution.download(good, "v0.1.0", Path(folder))
            request.assert_not_called()
            good["browser_download_url"] = distribution.asset_url("v0.1.0", good["name"])
            good["digest"] = None
            with self.assertRaises((ValueError, TypeError)):
                distribution.download(good, "v0.1.0", Path(folder))

    def test_asset_paths_cannot_escape_the_download_directory(self):
        for path in ("..", "../secret", "/tmp/secret", "nested/file", "name\nbad"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                distribution.asset_url("v0.1.0", path)

    def test_installer_pin_matches_the_committed_public_key(self):
        root = Path(__file__).resolve().parents[1]
        digest = distribution.sha256(root / "packaging/keys/asagiri-rpm.asc")
        self.assertIn("key_sha256=" + digest, (root / "site/install.sh").read_text())

    def test_release_source_outside_main_history_is_rejected(self):
        commit = "a" * 40
        tag = "v0.1.0"
        with tempfile.TemporaryDirectory() as folder:
            cache = Path(folder)
            (cache / tag).mkdir()
            (cache / tag / "release-manifest.json").write_text(json.dumps({
                "repository": distribution.REPO, "version": tag[1:], "commit": commit, "assets": [],
            }))
            candidate = {**release(tag), "assets": [
                {"name": name} for name in ("release-manifest.json", "SHA256SUMS", "attestation.sigstore.json")
            ]}
            with (patch.object(distribution, "download"),
                  patch.object(distribution, "api", side_effect=[
                      {"sha": commit}, {"merge_base_commit": {"sha": "b" * 40}},
                  ]), patch.object(distribution, "command") as command):
                with self.assertRaisesRegex(ValueError, "reviewed main history"):
                    distribution.verified_release(candidate, cache)
                command.assert_not_called()


if __name__ == "__main__":
    unittest.main()
