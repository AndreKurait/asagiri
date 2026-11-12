#!/usr/bin/env python3
"""Check release trust boundaries, gate completeness, and version stamping."""
import copy
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "release", Path(__file__).resolve().parents[1] / "scripts/ci/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)
COMMIT = "a" * 40
REPO = "AndreKurait/asagiri"


class ReleaseTests(unittest.TestCase):
    def event(self):
        return {"repository": {"full_name": REPO}, "after": COMMIT, "deleted": False,
                "ref": "refs/heads/main"}

    def test_only_main_and_version_tag_source_events_are_trusted(self):
        release.validate_event("push", "refs/heads/main", self.event(), REPO, COMMIT)
        release.validate_event("push", "refs/tags/v1.0.0",
                               {**self.event(), "ref": "refs/tags/v1.0.0", "after": "c" * 40},
                               REPO, COMMIT)
        release.validate_event("workflow_dispatch", "refs/heads/main", self.event(), REPO, COMMIT)
        for field, value in [("repository", {"full_name": "fork/asagiri"}),
                             ("after", "b" * 40), ("deleted", True), ("ref", "refs/heads/feature")]:
            event = self.event()
            event[field] = value
            with self.subTest(field=field), self.assertRaises(SystemExit):
                release.validate_event("push", "refs/heads/main", event, REPO, COMMIT)
        for kind, ref in [("workflow_dispatch", "refs/heads/feature"), ("push", "refs/heads/feature"),
                          ("pull_request", "refs/heads/main"), ("workflow_run", "refs/heads/main")]:
            with self.subTest(kind=kind, ref=ref), self.assertRaises(SystemExit):
                release.validate_event(kind, ref, self.event(), REPO, COMMIT)

    def test_main_prerelease_versions_and_stable_tag_versions(self):
        for kind in ("push", "workflow_dispatch"):
            self.assertEqual(release.release_version(kind, "refs/heads/main", "34", "0.1.0"),
                             ("0.1.0-dev.34", True))
        self.assertEqual(release.release_version("push", "refs/tags/v1.2.3", "35", "0.1.0"),
                         ("1.2.3", False))
        for ref in ("refs/tags/v1", "refs/tags/v01.2.3", "refs/tags/v1.2.3-dev.1"):
            with self.subTest(ref=ref), self.assertRaises(SystemExit):
                release.release_version("push", ref, "35", "0.1.0")
        for number in ("", "0", "01", "1\nready=true"):
            with self.subTest(number=number), self.assertRaises(SystemExit):
                release.release_version("push", "refs/heads/main", number, "0.1.0")

    def checks(self):
        return [{"id": number, "name": name, "head_sha": COMMIT, "head_branch": "main",
                 "event": "push", "status": "completed", "conclusion": "success", "run_number": 10}
                for number, name in enumerate(sorted(release.REQUIRED), 1)]

    def test_main_packaging_starts_without_waiting_for_other_workflows(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "RELEASE_VERSION").write_text("0.1.0\n")
            event_path = root / "event.json"
            event_path.write_text(json.dumps(self.event()))
            env = {"GITHUB_EVENT_PATH": str(event_path), "GITHUB_EVENT_NAME": "push",
                   "GITHUB_REF": "refs/heads/main", "GITHUB_RUN_NUMBER": "34"}
            with patch.object(release, "ROOT", root), patch.dict(release.os.environ, env), \
                    patch.object(release, "run", side_effect=[
                        SimpleNamespace(stdout=COMMIT), SimpleNamespace(stdout=""),
                        SimpleNamespace(returncode=1)]), \
                    patch.object(release, "api") as api, patch.object(release, "emit") as emit:
                release.plan(SimpleNamespace(repo=REPO))
            api.assert_not_called()
            emit.assert_called_once_with(ready=True, commit=COMMIT, version="0.1.0-dev.34",
                                         tag="v0.1.0-dev.34", prerelease=True)

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

    def test_publication_waits_for_ci_and_ignores_website_deployment(self):
        complete = self.checks()
        queued = copy.deepcopy(complete)
        queued[0].update(status="queued", conclusion=None)
        website = {"id": 100, "name": "Website", "head_sha": COMMIT, "head_branch": "main",
                   "event": "push", "status": "queued", "conclusion": None}
        with patch.object(release, "api", side_effect=[
                {"workflow_runs": queued}, {"workflow_runs": [*complete, website]}]), \
                patch.object(release.time, "sleep") as sleep:
            release.require_checks(REPO, COMMIT, 60)
        sleep.assert_called_once_with(15)

    def test_failed_cancelled_or_missing_ci_cannot_publish(self):
        for conclusion in ("failure", "cancelled", "skipped", "timed_out"):
            runs = self.checks()
            runs[0]["conclusion"] = conclusion
            with self.subTest(conclusion=conclusion), \
                    patch.object(release, "api", return_value={"workflow_runs": runs}), \
                    self.assertRaises(SystemExit):
                release.require_checks(REPO, COMMIT, 60)
        with patch.object(release, "api", return_value={"workflow_runs": []}), \
                self.assertRaises(SystemExit):
            release.require_checks(REPO, COMMIT, 0)

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

    def publication(self, root, *, existing=False, corruption=None):
        """Model GitHub's draft lifecycle: the tag endpoint sees only published releases."""
        dist = root / "dist"
        dist.mkdir()
        version = "0.1.0-dev.34"
        tag = "v" + version
        (root / "docs/releases").mkdir(parents=True)
        (root / "docs/releases/asagiri.md").write_text("Release notes\n")
        (dist / "release-manifest.json").write_text(json.dumps(
            {"version": version, "commit": COMMIT, "repository": REPO, "assets": []}))
        (dist / "payload.tar.gz").write_bytes(b"verified release payload")
        state = {"exists": existing, "created": False, "uploaded": False, "draft_read": False}
        remote = {"id": 123, "draft": True, "tag_name": tag, "target_commitish": COMMIT,
                  "prerelease": True, "assets": [],
                  "html_url": f"https://github.com/{REPO}/releases/tag/{tag}"}

        def command(*args, **_):
            self.assertEqual(args[:2], ("gh", "release"))
            self.assertEqual(args[3], tag)
            action = args[2]
            if action == "view":
                if not state["exists"]:
                    return SimpleNamespace(returncode=1, stdout="")
                fields = args[args.index("--json") + 1].split(",")
                data = {"databaseId": remote["id"], "isDraft": remote["draft"],
                        "targetCommitish": COMMIT, "tagName": tag}
                return SimpleNamespace(returncode=0, stdout=json.dumps({key: data[key] for key in fields}))
            if action == "create":
                self.assertIn("--draft", args)
                self.assertFalse(state["exists"])
                state.update(exists=True, created=True)
            elif action == "upload":
                self.assertTrue(state["exists"])
                state["uploaded"] = True
                remote["assets"] = [{"name": path.name, "size": path.stat().st_size,
                                     "digest": "sha256:" + release.sha256(path)} for path in sorted(dist.iterdir())]
                if corruption == "digest":
                    remote["assets"][0]["digest"] = "sha256:" + "0" * 64
                elif corruption == "size":
                    remote["assets"][0]["size"] += 1
                elif corruption == "missing":
                    remote["assets"].pop()
                elif corruption == "source":
                    remote["target_commitish"] = "b" * 40
            elif action == "edit":
                self.assertTrue(state["uploaded"] and state["draft_read"])
                self.assertIn("--draft=false", args)
                self.assertIn("--latest=false", args)
                remote["draft"] = False
            else:
                self.fail(f"Unexpected release command: {action}")
            return SimpleNamespace(returncode=0, stdout="")

        def api(repo, path):
            self.assertEqual(repo, REPO)
            if path == f"releases/tags/{tag}":
                if remote["draft"]:
                    raise subprocess.CalledProcessError(
                        1, ["gh", "api", path], stderr="HTTP 404: draft tags are not published")
            else:
                self.assertEqual(path, "releases/123")
                if remote["draft"]:
                    state["draft_read"] = True
            return copy.deepcopy(remote)

        return dist, state, remote, command, api

    def test_first_publication_and_draft_retries_verify_uploads_by_release_id(self):
        for existing in (False, True):
            with self.subTest(existing=existing), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                dist, state, remote, command, api = self.publication(root, existing=existing)
                with patch.object(release, "ROOT", root), patch.object(release, "run", side_effect=command), \
                        patch.object(release, "api", side_effect=api), patch.object(release, "emit") as emit:
                    release.publish(SimpleNamespace(directory=dist, repo=REPO))
                self.assertEqual(state["created"], not existing)
                self.assertFalse(remote["draft"])
                emit.assert_called_once_with(url=remote["html_url"])

    def test_damaged_uploads_or_changed_source_stay_drafts(self):
        for corruption in ("digest", "size", "missing", "source"):
            with self.subTest(corruption=corruption), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                dist, _, remote, command, api = self.publication(root, corruption=corruption)
                with patch.object(release, "ROOT", root), patch.object(release, "run", side_effect=command), \
                        patch.object(release, "api", side_effect=api), self.assertRaises(SystemExit):
                    release.publish(SimpleNamespace(directory=dist, repo=REPO))
                self.assertTrue(remote["draft"])

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
