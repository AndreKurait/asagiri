#!/usr/bin/env python3
"""Check release trust boundaries, gate completeness, and version stamping."""
import copy
import importlib.util
from pathlib import Path
import tempfile
import tomllib
import unittest

spec = importlib.util.spec_from_file_location(
    "release", Path(__file__).resolve().parents[1] / "scripts/ci/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)
COMMIT = "a" * 40
REPO = "AndreKurait/asagiri"


class ReleaseTests(unittest.TestCase):
    def event(self):
        return {"workflow_run": {"head_repository": {"full_name": REPO}, "head_branch": "main",
                                 "head_sha": COMMIT, "event": "push", "conclusion": "success"}}

    def test_only_successful_main_source_events_are_trusted(self):
        release.validate_event("workflow_run", "refs/heads/main", self.event(), REPO, COMMIT)
        for field, value in [("head_repository", {"full_name": "fork/asagiri"}),
                             ("head_branch", "feature"), ("head_sha", "b" * 40),
                             ("event", "pull_request"), ("conclusion", "failure")]:
            event = self.event()
            event["workflow_run"][field] = value
            with self.subTest(field=field), self.assertRaises(SystemExit):
                release.validate_event("workflow_run", "refs/heads/main", event, REPO, COMMIT)
        with self.assertRaises(SystemExit):
            release.validate_event("workflow_dispatch", "refs/heads/feature", {}, REPO, COMMIT)

    def checks(self):
        return [{"id": number, "name": name, "head_sha": COMMIT, "head_branch": "main",
                 "event": "push", "conclusion": "success", "run_number": 10}
                for number, name in enumerate(sorted(release.REQUIRED), 1)]

    def test_every_check_must_pass_on_the_exact_source_commit(self):
        runs = self.checks()
        self.assertEqual(release.checked_runs(runs, COMMIT)[1], [])
        for field, value in [("head_sha", "b" * 40), ("event", "pull_request"),
                             ("head_branch", "feature"), ("conclusion", "failure"),
                             ("conclusion", None)]:
            changed = copy.deepcopy(runs)
            changed[0][field] = value
            with self.subTest(field=field, value=value):
                self.assertEqual(release.checked_runs(changed, COMMIT)[1], [runs[0]["name"]])
        self.assertEqual(release.checked_runs(runs[1:], COMMIT)[1], [runs[0]["name"]])

    def test_a_failed_rerun_supersedes_an_earlier_success(self):
        runs = self.checks()
        rerun = {**runs[0], "id": 100, "conclusion": "failure"}
        self.assertEqual(release.checked_runs([*runs, rerun], COMMIT)[1], [rerun["name"]])

    def test_provenance_distinguishes_source_from_a_newer_workflow(self):
        env = {"GITHUB_WORKFLOW_SHA": "b" * 40,
               "GITHUB_WORKFLOW_REF": REPO + "/.github/workflows/release.yml@refs/heads/main",
               "GITHUB_SERVER_URL": "https://github.com", "GITHUB_REF": "refs/heads/main",
               "GITHUB_EVENT_NAME": "workflow_run", "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1"}
        predicate = release.provenance(REPO, COMMIT, "0.1.0-dev.12", env)
        dependencies = predicate["buildDefinition"]["resolvedDependencies"]
        self.assertEqual(dependencies[0]["digest"]["gitCommit"], COMMIT)
        self.assertEqual(dependencies[1]["digest"]["gitCommit"], "b" * 40)
        self.assertEqual(predicate["buildDefinition"]["externalParameters"]["source"]["commit"], COMMIT)

    def test_stamping_preserves_dependency_versions_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "crates/asagiri").mkdir(parents=True)
            (root / "xtask").mkdir()
            (root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.0.0"\n')
            for name, path in [("asagiri", "crates/asagiri"), ("xtask", "xtask")]:
                (root / path / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion.workspace = true\n')
            (root / "Cargo.lock").write_text(
                'version = 4\n\n[[package]]\nname = "asagiri"\nversion = "0.0.0"\n'
                '\n[[package]]\nname = "xtask"\nversion = "0.0.0"\n'
                '\n[[package]]\nname = "dependency"\nversion = "0.0.0"\nsource = "registry+example"\n')
            release.stamp(root, "0.1.0-dev.12")
            once = (root / "Cargo.lock").read_text()
            release.stamp(root, "0.1.0-dev.12")
            self.assertEqual(once, (root / "Cargo.lock").read_text())
            packages = {p["name"]: p["version"] for p in tomllib.loads(once)["package"]}
            self.assertEqual(packages, {"asagiri": "0.1.0-dev.12", "xtask": "0.1.0-dev.12", "dependency": "0.0.0"})
            with self.assertRaises(ValueError):
                release.stamp(root, "1.0.0\nmalicious-output=true")


if __name__ == "__main__":
    unittest.main()
