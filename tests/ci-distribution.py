#!/usr/bin/env python3
"""Exercise release selection and distribution integrity boundaries."""
import importlib.util
import copy
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
    def test_recovery_distinguishes_signed_workflow_and_package_identities(self):
        source, publisher = "a" * 40, "b" * 40
        origin = "https://github.com/" + distribution.REPO
        manifest = {"commit": source, "version": "0.1.1-dev.62"}
        statement = {"predicate": {
            "buildDefinition": {
                "externalParameters": {"workflow": {"repository": origin, "path": ".github/workflows/release.yml", "ref": "refs/heads/main"}},
                "resolvedDependencies": [
                    {"uri": "git+" + origin + "@refs/heads/main", "digest": {"gitCommit": publisher}, "annotations": {"role": "build workflow"}},
                    {"uri": "git+" + origin + "@" + source, "digest": {"gitCommit": source}, "annotations": {"role": "package source"}},
                ],
            },
            "runDetails": {"metadata": {"invocationId": origin + "/actions/runs/123/attempts/1"}},
        }}
        self.assertEqual(distribution.publication_identity(statement, manifest), (publisher, "123"))
        changes = [
            lambda d: d["buildDefinition"]["resolvedDependencies"][1]["digest"].update(gitCommit=publisher),
            lambda d: d["buildDefinition"]["resolvedDependencies"].append(d["buildDefinition"]["resolvedDependencies"][0]),
            lambda d: d["buildDefinition"]["externalParameters"]["workflow"].update(repository="https://github.com/other/fork"),
            lambda d: d["buildDefinition"]["externalParameters"]["workflow"].update(ref="refs/heads/unreviewed"),
            lambda d: d["runDetails"]["metadata"].update(invocationId="https://github.com/other/fork/actions/runs/123/attempts/1"),
        ]
        for change in changes:
            bad = copy.deepcopy(statement)
            change(bad["predicate"])
            with self.assertRaises(ValueError):
                distribution.publication_identity(bad, manifest)

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

    def test_rpm_repository_uses_canonical_release_names_and_retains_original_checksums(self):
        for version in ("0.1.0-dev.34", "0.1.0"):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                destination = root / "rpm" / "x86_64"
                names = [f"{package}-{version.replace('-', '.', 1)}-1.x86_64.rpm"
                         for package in ("asagiri", "asagiri-lite")]
                assets = {name: {"name": name, "digest": "sha256:" + "a" * 64} for name in names}
                for name in names:
                    (root / name).write_bytes(b"original package bytes")
                def download(asset, tag, cache):
                    self.assertEqual(tag, "v" + version)
                    return root / asset["name"]
                def command(*args):
                    if args[0] == "rpmkeys":
                        return "digests signatures OK"
                    if args[0] == "rpmsign":
                        with Path(args[-1]).open("ab") as stream:
                            stream.write(b"signature")
                    return ""
                with patch.object(distribution, "download", side_effect=download), \
                        patch.object(distribution, "command", side_effect=command):
                    distribution.rpm_repo(
                        {**release("v" + version), "html_url": "release"},
                        {"version": version, "commit": "b" * 40}, assets, root,
                        destination, "c" * 40, root / "public.asc")
                metadata = json.loads((destination / "provenance.json").read_text())
                self.assertEqual({p["name"] for p in metadata["packages"]}, set(names))
                for package in metadata["packages"]:
                    self.assertEqual(package["original_release_sha256"], "a" * 64)
                    self.assertEqual((root / package["name"]).read_bytes(), b"original package bytes")
                    self.assertEqual(package["sha256"], distribution.sha256(destination / "Packages" / package["name"]))

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
