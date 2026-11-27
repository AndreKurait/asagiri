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
                   "GITHUB_REF": "refs/heads/main", "GITHUB_RUN_NUMBER": "34", "GITHUB_RUN_ATTEMPT": "1"}
            with patch.object(release, "ROOT", root), patch.dict(release.os.environ, env), \
                    patch.object(release, "run", side_effect=[
                        SimpleNamespace(stdout=COMMIT), SimpleNamespace(stdout=""),
                        SimpleNamespace(returncode=1)]), \
                    patch.object(release, "api") as api, patch.object(release, "emit") as emit:
                release.plan(SimpleNamespace(repo=REPO))
            api.assert_not_called()
            emit.assert_called_once_with(ready=True, commit=COMMIT, version="0.1.0-dev.34",
                                         tag="v0.1.0-dev.34", prerelease=True,
                                         artifact_run="", artifact_attempt="1")

    def test_every_check_must_pass_on_the_exact_source_commit(self):
        runs = self.checks()
        self.assertEqual(release.checked_runs(runs, COMMIT)[1], [])
        for field, value in [("head_sha", "b" * 40), ("event", "pull_request"),
                             ("head_branch", "feature"), ("conclusion", "failure"),
                             ("conclusion", None), ("status", "in_progress")]:
            changed = copy.deepcopy(runs)
            changed[0][field] = value
            with self.subTest(field=field, value=value):
                self.assertEqual(release.checked_runs(changed, COMMIT)[1], [runs[0]["name"]])
        self.assertEqual(release.checked_runs(runs[1:], COMMIT)[1], [runs[0]["name"]])

    def test_manual_recovery_plans_the_original_packages_from_the_trusted_workflow(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "event.json"
            path.write_text(json.dumps({**self.event(), "inputs": {"recover_run": "123"}}))
            env = {"GITHUB_EVENT_PATH": str(path), "GITHUB_EVENT_NAME": "workflow_dispatch",
                   "GITHUB_REF": "refs/heads/main", "GITHUB_RUN_NUMBER": "99", "GITHUB_RUN_ATTEMPT": "1"}
            with patch.dict(release.os.environ, env), patch.object(release, "run", side_effect=[
                    SimpleNamespace(stdout="b" * 40), SimpleNamespace(stdout=""),
                    SimpleNamespace(returncode=1)]), \
                    patch.object(release, "recover_packages", return_value=(COMMIT, "0.1.0-dev.62", True, 2)) as recover, \
                    patch.object(release, "emit") as emit:
                release.plan(SimpleNamespace(repo=REPO))
            recover.assert_called_once_with(REPO, "123")
            emit.assert_called_once_with(ready=True, commit=COMMIT, version="0.1.0-dev.62",
                                         tag="v0.1.0-dev.62", prerelease=True,
                                         artifact_run="123", artifact_attempt=2)

    def test_a_failed_rerun_supersedes_an_earlier_success(self):
        runs = self.checks()
        rerun = {**runs[0], "id": 100, "conclusion": "failure"}
        self.assertEqual(release.checked_runs([*runs, rerun], COMMIT)[1], [rerun["name"]])

    def test_release_waits_for_the_three_engine_comparison(self):
        complete = self.checks()
        absent = [run for run in complete if run["name"] != "Engine comparison"]
        self.assertEqual(release.checked_runs(absent, COMMIT)[1], ["Engine comparison"])
        comparison = next(run for run in complete if run["name"] == "Engine comparison")
        for changed in (dict(comparison, status="in_progress", conclusion=None),
                        dict(comparison, conclusion="failure"),
                        dict(comparison, head_sha="b" * 40)):
            self.assertEqual(release.checked_runs([*absent, changed], COMMIT)[1], ["Engine comparison"])
        event = {**self.retry_event(), "workflow_run": {
            **comparison, "head_repository": {"full_name": REPO},
        }}
        self.assertEqual(release.retry_source(event, REPO), COMMIT)

    def test_manual_speed_experiments_cannot_replace_the_main_push_gate(self):
        runs = self.checks()
        speed = next(item for item in runs if item["name"] == "Speed")
        for conclusion in ("success", "failure", None):
            experiment = {**speed, "id": 100, "event": "workflow_dispatch", "conclusion": conclusion,
                          "status": "in_progress" if conclusion is None else "completed"}
            with self.subTest(manual_conclusion=conclusion):
                checked, missing = release.checked_runs([*runs, experiment], COMMIT)
                self.assertEqual(missing, [])
                self.assertEqual(checked["Speed"]["id"], speed["id"])
                failed = [dict(item, conclusion="failure") if item is speed else item for item in runs]
                self.assertEqual(release.checked_runs([*failed, experiment], COMMIT)[1], ["Speed"])
                absent = [item for item in runs if item is not speed]
                self.assertEqual(release.checked_runs([*absent, experiment], COMMIT)[1], ["Speed"])
        rerun = {**speed, "id": 101, "conclusion": "failure"}
        self.assertEqual(release.checked_runs([*runs, rerun], COMMIT)[1], ["Speed"])

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

    def retry_event(self):
        return {"action": "completed", "repository": {"full_name": REPO},
                "workflow_run": {**self.checks()[0], "head_repository": {"full_name": REPO}}}

    def release_run(self):
        return {"id": 123, "name": "Release", "path": release.RELEASE_WORKFLOW,
                "head_sha": COMMIT, "head_branch": "main", "head_repository": {"full_name": REPO},
                "event": "push", "status": "completed", "conclusion": "failure",
                "run_attempt": 1, "run_number": 62}

    def release_jobs(self):
        return [{"name": "Linux packages (x86_64)", "status": "completed", "conclusion": "success"},
                {"name": release.PUBLISH_JOB, "status": "completed", "conclusion": "failure",
                 "steps": [{"name": release.CHECK_STEP, "conclusion": "failure"}]}]

    def retry(self, runs=None, jobs=None, current=None, event=None):
        """Replay delayed CI completion without contacting or mutating GitHub."""
        runs = [*self.checks(), self.release_run()] if runs is None else runs
        jobs = self.release_jobs() if jobs is None else jobs
        current = self.release_run() if current is None else current
        event = self.retry_event() if event is None else event

        def api(repo, path):
            self.assertEqual(repo, REPO)
            if path == f"actions/runs?head_sha={COMMIT}&per_page=100&page=1":
                return {"workflow_runs": runs}
            if path == "actions/runs/123":
                return current
            if path == "actions/runs/123/jobs?filter=latest&per_page=100&page=1":
                return {"jobs": jobs}
            self.fail(f"Unexpected retry API request: {path}")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "event.json"
            path.write_text(json.dumps(event))
            with patch.dict(release.os.environ, {"GITHUB_EVENT_NAME": "workflow_run",
                                                 "GITHUB_EVENT_PATH": str(path)}), \
                    patch.object(release, "api", side_effect=api), patch.object(release, "run") as run:
                release.retry(SimpleNamespace(repo=REPO))
        return run

    def test_delayed_success_resumes_only_failed_publication_and_keeps_the_release_run(self):
        for event in (self.retry_event(), {**self.retry_event(), "workflow_run": self.release_run()}):
            with self.subTest(trigger=event["workflow_run"]["name"]):
                run = self.retry(event=event)
                self.assertEqual(run.call_args_list[0].args,
                                 ("git", "merge-base", "--is-ancestor", COMMIT, "origin/main"))
                run.assert_called_with("gh", "api", "--method", "POST",
                                       f"repos/{REPO}/actions/runs/123/rerun-failed-jobs")
                self.assertEqual(run.call_count, 2)

    def test_recovery_waits_for_all_checks_and_rejects_newer_failures(self):
        for runs in ([self.release_run()], [*self.checks(), self.release_run(),
                     {**self.checks()[0], "id": 200, "conclusion": "failure"}]):
            with self.subTest(runs=runs):
                self.assertEqual(self.retry(runs=runs).call_count, 1)

    def test_recovery_does_not_retry_build_upload_or_cancelled_failures(self):
        variants = []
        for field, value in (("conclusion", "failure"), ("status", "queued")):
            jobs = self.release_jobs()
            jobs[0][field] = value
            variants.append(jobs)
        for step in ("Attest the release artifacts", "Upload, verify every GitHub digest, then publish"):
            jobs = self.release_jobs()
            jobs[1]["steps"][0]["name"] = step
            variants.append(jobs)
        jobs = self.release_jobs()
        jobs[1]["conclusion"] = "cancelled"
        variants.append(jobs)
        variants += [[], self.release_jobs()[1:]]
        for jobs in variants:
            with self.subTest(jobs=jobs):
                self.assertEqual(self.retry(jobs=jobs).call_count, 1)

    def test_active_retries_and_attempt_limit_prevent_duplicate_or_endless_runs(self):
        for current in ({**self.release_run(), "status": "in_progress", "conclusion": None},
                        {**self.release_run(), "run_attempt": release.MAX_RELEASE_ATTEMPTS}):
            with self.subTest(current=current):
                self.assertEqual(self.retry(current=current).call_count, 1)

    def test_untrusted_completion_events_and_release_candidates_cannot_retry(self):
        for field, value in (("head_repository", {"full_name": "fork/asagiri"}),
                             ("event", "pull_request"), ("head_branch", "feature"),
                             ("head_sha", "invalid"), ("conclusion", "failure")):
            event = self.retry_event()
            event["workflow_run"][field] = value
            with self.subTest(event_field=field), self.assertRaises(SystemExit):
                self.retry(event=event)
        for field, value in (("head_repository", {"full_name": "fork/asagiri"}),
                             ("event", "pull_request"), ("head_branch", "feature"),
                             ("head_sha", "b" * 40), ("path", ".github/workflows/other.yml"),
                             ("conclusion", "cancelled"), ("conclusion", "success")):
            candidate = {**self.release_run(), field: value}
            with self.subTest(candidate_field=field, value=value):
                self.assertEqual(self.retry(runs=[*self.checks(), candidate]).call_count, 1)
        event = self.retry_event()
        event["action"] = "requested"
        with self.assertRaises(SystemExit):
            self.retry(event=event)

    def test_stable_release_retry_requires_a_valid_tag_and_main_checks(self):
        candidate = {**self.release_run(), "head_branch": "v0.1.1"}
        event = {**self.retry_event(), "workflow_run": candidate}
        self.assertEqual(self.retry(runs=[*self.checks(), candidate], event=event).call_count, 2)
        for branch in ("v01.1.1", "v0.1.1-dev.3", "feature"):
            event["workflow_run"] = {**candidate, "head_branch": branch}
            with self.subTest(branch=branch), self.assertRaises(SystemExit):
                self.retry(event=event)

    def test_check_pagination_does_not_hide_earlier_required_workflows(self):
        with patch.object(release, "api", side_effect=[
                {"workflow_runs": [self.release_run()] * 100},
                {"workflow_runs": self.checks()}]) as api:
            release.require_checks(REPO, COMMIT, 0)
        self.assertEqual(api.call_count, 2)
        self.assertIn("page=2", api.call_args.args[1])

    def provenance_environment(self):
        return {"GITHUB_WORKFLOW_SHA": "b" * 40,
               "GITHUB_SHA": "b" * 40,
               "GITHUB_WORKFLOW_REF": REPO + "/.github/workflows/release.yml@refs/heads/main",
               "GITHUB_SERVER_URL": "https://github.com", "GITHUB_REF": "refs/heads/main",
               "GITHUB_EVENT_NAME": "workflow_run", "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1",
               "GITHUB_REPOSITORY_ID": "456", "GITHUB_REPOSITORY_OWNER_ID": "789",
               "RUNNER_ENVIRONMENT": "github-hosted"}

    def test_provenance_distinguishes_source_from_a_newer_workflow(self):
        env = self.provenance_environment()
        predicate = release.provenance(REPO, COMMIT, "0.1.0-dev.12", env)
        dependencies = predicate["buildDefinition"]["resolvedDependencies"]
        self.assertEqual(dependencies[0]["uri"], f"git+https://github.com/{REPO}@refs/heads/main")
        self.assertEqual(dependencies[0]["digest"]["gitCommit"], "b" * 40)
        self.assertEqual(dependencies[1]["digest"]["gitCommit"], COMMIT)
        self.assertEqual(dependencies[1]["annotations"]["buildInvocation"],
                          f"https://github.com/{REPO}/actions/runs/123")
        self.assertEqual(dependencies[1]["annotations"]["artifactSelectionAttempt"], "1")
        self.assertEqual(predicate["buildDefinition"]["externalParameters"]["source"]["commit"], COMMIT)
        self.assertEqual(predicate["buildDefinition"]["internalParameters"]["github"], {
            "event_name": "workflow_run", "repository_id": "456", "repository_owner_id": "789",
            "runner_environment": "github-hosted"})
        recovered = release.provenance(REPO, COMMIT, "0.1.0-dev.62", {
            **env, "GITHUB_EVENT_NAME": "workflow_dispatch",
            "ASAGIRI_PACKAGE_RUN_ID": "62", "ASAGIRI_PACKAGE_RUN_ATTEMPT": "2"})
        self.assertEqual(recovered["buildDefinition"]["resolvedDependencies"][1]["annotations"]["buildInvocation"],
                          f"https://github.com/{REPO}/actions/runs/62")
        self.assertEqual(recovered["buildDefinition"]["resolvedDependencies"][1]["annotations"]["artifactSelectionAttempt"], "2")
        for field in ("GITHUB_REPOSITORY_ID", "GITHUB_REPOSITORY_OWNER_ID", "RUNNER_ENVIRONMENT",
                      "GITHUB_SHA", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "ASAGIRI_PACKAGE_RUN_ID"):
            for value in ("", "untrusted"):
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    release.provenance(REPO, COMMIT, "0.1.0-dev.12", {**env, field: value})

    def test_same_run_recovery_selects_mixed_attempt_packages_without_using_the_old_plan(self):
        env = {**self.provenance_environment(), "GITHUB_RUN_ATTEMPT": "3",
               "ASAGIRI_PACKAGE_RUN_ID": "123", "ASAGIRI_PACKAGE_RUN_ATTEMPT": "1"}
        predicate = release.provenance(REPO, COMMIT, "0.1.0-dev.12", env)
        annotations = predicate["buildDefinition"]["resolvedDependencies"][1]["annotations"]
        # Packages retained from attempt 1 and rebuilt in attempt 2 belong to
        # the run. The publisher selects the whole set in attempt 3.
        self.assertEqual(annotations, {
            "role": "package source",
            "buildInvocation": f"https://github.com/{REPO}/actions/runs/123",
            "artifactSelectionAttempt": "3",
        })
        self.assertEqual(predicate["runDetails"]["metadata"]["invocationId"],
                         f"https://github.com/{REPO}/actions/runs/123/attempts/3")

    def test_other_run_recovery_preserves_its_verified_selection_attempt(self):
        env = {**self.provenance_environment(), "GITHUB_RUN_ATTEMPT": "3",
               "ASAGIRI_PACKAGE_RUN_ID": "62", "ASAGIRI_PACKAGE_RUN_ATTEMPT": "2"}
        predicate = release.provenance(REPO, COMMIT, "0.1.0-dev.62", env)
        annotations = predicate["buildDefinition"]["resolvedDependencies"][1]["annotations"]
        self.assertEqual(annotations["buildInvocation"],
                         f"https://github.com/{REPO}/actions/runs/62")
        self.assertEqual(annotations["artifactSelectionAttempt"], "2")
        self.assertEqual(predicate["runDetails"]["metadata"]["invocationId"],
                         f"https://github.com/{REPO}/actions/runs/123/attempts/3")
        for value in ("", "0", "-1", "untrusted"):
            with self.subTest(attempt=value), self.assertRaises(ValueError):
                release.provenance(REPO, COMMIT, "0.1.0-dev.62",
                                   {**env, "ASAGIRI_PACKAGE_RUN_ATTEMPT": value})
        del env["ASAGIRI_PACKAGE_RUN_ATTEMPT"]
        with self.assertRaises(ValueError):
            release.provenance(REPO, COMMIT, "0.1.0-dev.62", env)

    def package_recovery(self, *, candidate=None, jobs=None, checks=None, qemu=False, jammy=False, windows_tests=False, checkpoint=False):
        candidate = self.release_run() if candidate is None else candidate
        markers = {release.QEMU_RELEASE: (qemu, {release.QEMU_JOB}),
                   release.JAMMY_SCRIPT: (jammy, {release.JAMMY_JOB}),
                   release.WINDOWS_TEST_SCRIPT: (windows_tests, {release.WINDOWS_TEST_JOB}),
                   release.CHECKPOINT_RECIPE: (checkpoint, release.CHECKPOINT_JOBS)}
        if jobs is None:
            required = release.PACKAGE_JOBS.union(*(names for enabled, names in markers.values() if enabled))
            jobs = [{"name": name, "status": "completed", "conclusion": "success"} for name in sorted(required)]
            jobs.append({"name": release.PUBLISH_JOB, "status": "completed", "conclusion": "failure",
                         "steps": [{"name": "Attest the release artifacts", "conclusion": "failure"}]})
        checks = self.checks() if checks is None else checks
        def api(_, path):
            if path == "actions/runs/123":
                return candidate
            if "/jobs?" in path:
                return {"jobs": jobs}
            if path.startswith("actions/runs?"):
                return {"workflow_runs": checks}
            self.fail(f"Unexpected package recovery request: {path}")
        with patch.object(release, "api", side_effect=api), \
                patch.object(release, "run", side_effect=[
                    SimpleNamespace(stdout=""),
                    SimpleNamespace(stdout="\n".join(path for path, (enabled, _) in markers.items() if enabled)),
                    SimpleNamespace(stdout="0.1.0\n")]) as run:
            result = release.recover_packages(REPO, "123")
        self.assertEqual(run.call_args_list[0].args,
                         ("git", "merge-base", "--is-ancestor", COMMIT, "origin/main"))
        return result

    def test_checkpoint_recovery_requires_sources_for_the_packaged_revision(self):
        self.assertEqual(self.package_recovery(checkpoint=True), (COMMIT, "0.1.0-dev.62", True, 1))
        for missing in release.CHECKPOINT_JOBS:
            required = (release.PACKAGE_JOBS | release.CHECKPOINT_JOBS) - {missing}
            jobs = [{"name": name, "status": "completed", "conclusion": "success"} for name in required]
            jobs.append({"name": release.PUBLISH_JOB, "status": "completed", "conclusion": "failure",
                         "steps": [{"name": "Attest the release artifacts", "conclusion": "failure"}]})
            with self.subTest(missing=missing), self.assertRaisesRegex(SystemExit, "every native packaging"):
                self.package_recovery(checkpoint=True, jobs=jobs)

    def test_package_recovery_retains_original_version_and_source(self):
        self.assertEqual(self.package_recovery(), (COMMIT, "0.1.0-dev.62", True, 1))

    def test_package_recovery_requires_emulator_job_when_the_source_ships_it(self):
        self.assertEqual(self.package_recovery(qemu=True), (COMMIT, "0.1.0-dev.62", True, 1))
        legacy = [{"name": name, "status": "completed", "conclusion": "success"}
                  for name in sorted(release.PACKAGE_JOBS)] + [
                      {"name": release.PUBLISH_JOB, "status": "completed", "conclusion": "failure",
                       "steps": [{"name": "Attest the release artifacts", "conclusion": "failure"}]}]
        with self.assertRaisesRegex(SystemExit, "every native"):
            self.package_recovery(qemu=True, jobs=legacy)
        unexpected = legacy + [{"name": release.QEMU_JOB, "status": "completed", "conclusion": "success"}]
        with self.assertRaisesRegex(SystemExit, "every native"):
            self.package_recovery(jobs=unexpected)

    def test_package_recovery_matches_jammy_job_to_source(self):
        self.assertEqual(self.package_recovery(qemu=True, jammy=True), (COMMIT, "0.1.0-dev.62", True, 1))
        with self.assertRaisesRegex(SystemExit, "every native"):
            self.package_recovery(jammy=True, jobs=self.release_jobs())

    def test_package_recovery_matches_windows_tests_to_source(self):
        self.assertEqual(self.package_recovery(windows_tests=True), (COMMIT, "0.1.0-dev.62", True, 1))
        with self.assertRaisesRegex(SystemExit, "every native"):
            self.package_recovery(windows_tests=True, jobs=self.release_jobs())

    def test_package_recovery_rejects_untrusted_or_active_runs(self):
        for field, value in (("head_repository", {"full_name": "fork/asagiri"}),
                             ("path", ".github/workflows/untrusted.yml"), ("event", "pull_request"),
                             ("head_branch", "feature"), ("head_sha", "bad"), ("id", 456),
                             ("status", "in_progress"), ("conclusion", "success")):
            with self.subTest(field=field), self.assertRaises(SystemExit):
                self.package_recovery(candidate={**self.release_run(), field: value})
        for run_id in ("0", "-1", "x", "123\nbad-output=true"):
            with self.subTest(run_id=run_id), patch.object(release, "api") as api, \
                    self.assertRaises(SystemExit):
                release.recover_packages(REPO, run_id)
            api.assert_not_called()

    def test_package_recovery_requires_all_native_jobs_and_source_checks(self):
        with self.assertRaises(SystemExit):
            self.package_recovery(jobs=self.release_jobs())
        jobs = [{"name": name, "status": "completed", "conclusion": "success"}
                for name in sorted(release.PACKAGE_JOBS)] + [
                    {"name": release.PUBLISH_JOB, "status": "completed", "conclusion": "failure",
                     "steps": [{"name": "Attest the release artifacts", "conclusion": "failure"}]}]
        for conclusion in ("failure", "skipped", "cancelled"):
            changed = copy.deepcopy(jobs)
            changed[0]["conclusion"] = conclusion
            with self.subTest(conclusion=conclusion), self.assertRaises(SystemExit):
                self.package_recovery(jobs=changed)
        changed = copy.deepcopy(jobs)
        changed[-1]["steps"][0]["name"] = "Upload, verify every GitHub digest, then publish"
        with self.assertRaises(SystemExit):
            self.package_recovery(jobs=changed)
        with self.assertRaises(SystemExit):
            self.package_recovery(checks=self.checks()[1:])

    def package_assets(self, dist, version="0.1.0-dev.34", *, normalized=False, qemu=True, appimage=True, checkpoint=True):
        package_version = version.replace("-", "." if normalized else "~", 1)
        names = {"asagiri-kernel-source.tar.xz"}
        if checkpoint:
            names.add(release.CHECKPOINT_SOURCE)
        if qemu:
            names.add(release.QEMU_SOURCE)
        for arch, deb in (("x86_64", "amd64"), ("aarch64", "arm64")):
            names.add(f"asagiri-{version}-linux-{arch}.tar.gz")
            if appimage:
                names.add(f"Asagiri-{version}-{arch}.AppImage")
            for package in ("asagiri", "asagiri-lite"):
                names.update({f"{package}_{package_version}_{deb}.deb",
                              f"{package}-{package_version}-1.{arch}.rpm"})
        for arch in ("arm64", "x86_64"):
            names.update({f"Asagiri-{version}-{arch}.{extension}" for extension in ("dmg", "msi", "tar.gz")})
            names.add(f"Asagiri-{version}-windows-{arch}.zip")
        for name in names:
            (dist / name).write_bytes(b"package contents keep their internal version")
        for platform, extension in (("mac", "tar.gz"), ("windows", "msi")):
            for arch in ("arm64", "x86_64"):
                name = f"Asagiri-{version}-{arch}.{extension}"
                (dist / f"latest-{platform}-{arch}.json").write_text(json.dumps({
                    "version": version, "sha256": release.sha256(dist / name),
                    "url": f"https://github.com/{REPO}/releases/download/v{version}/{name}",
                }))

    def test_asset_names_match_github_before_checksums_and_attestation(self):
        for version, normalized in (("0.1.0-dev.34", False), ("0.1.0-dev.34", True), ("0.1.0", False)):
            with self.subTest(version=version, normalized=normalized), tempfile.TemporaryDirectory() as folder:
                dist = Path(folder)
                self.package_assets(dist, version, normalized=normalized)
                original = {path.name.replace("~", "."): path.read_bytes() for path in dist.iterdir()}
                with patch.dict(release.os.environ, {"GITHUB_ACTIONS": "false"}):
                    release.collect(SimpleNamespace(directory=dist, version=version, commit=COMMIT, repo=REPO))
                manifest = json.loads((dist / "release-manifest.json").read_text())
                self.assertEqual(manifest["commit"], COMMIT)
                self.assertEqual(manifest["version"], version)
                self.assertEqual(len(manifest["assets"]), 27)
                self.assertEqual({asset["name"] for asset in manifest["assets"]}, set(original))
                self.assertNotIn("~", (dist / "SHA256SUMS").read_text())
                for asset in manifest["assets"]:
                    path = dist / asset["name"]
                    self.assertEqual(path.read_bytes(), original[path.name])
                    self.assertEqual(asset["sha256"], release.sha256(path))
                    self.assertIn(f"{asset['sha256']}  {path.name}\n", (dist / "SHA256SUMS").read_text())

    def test_legacy_source_asset_contract_is_preserved_by_a_newer_publisher(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            dist = root / "dist"
            dist.mkdir()
            self.package_assets(dist, qemu=False, appimage=False, checkpoint=False)
            with patch.dict(release.os.environ, {"GITHUB_ACTIONS": "false"}):
                release.collect(SimpleNamespace(directory=dist, version="0.1.0-dev.34",
                                                commit=COMMIT, repo=REPO, source_root=root))
            assets = json.loads((dist / "release-manifest.json").read_text())["assets"]
            self.assertEqual(len(assets), 23)
            self.assertNotIn(release.QEMU_SOURCE, {row["name"] for row in assets})
            self.assertFalse(any(row["name"].endswith(".AppImage") for row in assets))

    def test_declared_appimages_are_required_before_publication(self):
        with tempfile.TemporaryDirectory() as folder:
            dist = Path(folder)
            self.package_assets(dist, appimage=False)
            original = {path.name: path.read_bytes() for path in dist.iterdir()}
            with self.assertRaisesRegex(SystemExit, "AppImage"):
                release.collect(SimpleNamespace(directory=dist, version="0.1.0-dev.34",
                                                commit=COMMIT, repo=REPO))
            self.assertEqual({path.name: path.read_bytes() for path in dist.iterdir()}, original)

    def test_checkpoint_release_requires_source_archive(self):
        with tempfile.TemporaryDirectory() as folder:
            dist = Path(folder)
            self.package_assets(dist, checkpoint=False)
            with self.assertRaisesRegex(SystemExit, release.CHECKPOINT_SOURCE):
                release.collect(SimpleNamespace(directory=dist, version="0.1.0-dev.34", commit=COMMIT, repo=REPO))

    def test_emulator_cannot_publish_without_its_source_archive(self):
        with tempfile.TemporaryDirectory() as folder:
            dist = Path(folder)
            self.package_assets(dist, qemu=False)
            with self.assertRaisesRegex(SystemExit, release.QEMU_SOURCE):
                release.collect(SimpleNamespace(directory=dist, version="0.1.0-dev.34",
                                                commit=COMMIT, repo=REPO))

    def test_asset_normalization_rejects_collisions_and_unexpected_aliases_without_mutating_inputs(self):
        for name in ("asagiri-0.1.0.dev.34-1.x86_64.rpm", "asagiri-0~1.0~dev.34-1.x86_64.rpm"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as folder:
                dist = Path(folder)
                self.package_assets(dist)
                (dist / name).write_bytes(b"unexpected second package")
                original = {path.name: path.read_bytes() for path in dist.iterdir()}
                with self.assertRaisesRegex(SystemExit, "asset mismatch"):
                    release.collect(SimpleNamespace(directory=dist, version="0.1.0-dev.34", commit=COMMIT, repo=REPO))
                self.assertEqual({path.name: path.read_bytes() for path in dist.iterdir()}, original)

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
                    release.publish(SimpleNamespace(directory=dist, repo=REPO, source_root=root))
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
                    release.publish(SimpleNamespace(directory=dist, repo=REPO, source_root=root))
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
