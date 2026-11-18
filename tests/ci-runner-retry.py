#!/usr/bin/env python3
"""Replay a real hosted-runner cancellation and reject unsafe or duplicate retries."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("runner_retry", ROOT / "scripts/ci/runner-retry.py")
retry = importlib.util.module_from_spec(spec)
spec.loader.exec_module(retry)
FIXTURE = json.loads((ROOT / "tests/fixtures/ci-hosted-mac-capacity.json").read_text())


class RunnerRetryTests(unittest.TestCase):
    def event(self):
        return {"action": "completed", "repository": {"full_name": retry.REPO},
                "workflow_run": copy.deepcopy(FIXTURE["run"])}

    def replay(self, event=None, jobs=None, annotations=None, currents=None,
               ancestor=True, post_error=False, batches=None):
        """Mock every GitHub/Git operation; no test may make a real retry request."""
        event = self.event() if event is None else event
        trigger = event["workflow_run"]
        jobs = copy.deepcopy(FIXTURE["jobs"]) if jobs is None else jobs
        annotations = copy.deepcopy(FIXTURE["annotations"]) if annotations is None else annotations
        currents = [copy.deepcopy(trigger)] * 3 if currents is None else list(currents)
        requests = []
        commands = []
        run_path = f"actions/runs/{trigger['id']}"
        job_path = f"{run_path}/attempts/{trigger['run_attempt']}/jobs?per_page=100&page="
        check_path = f"check-runs/{FIXTURE['jobs'][0]['id']}/annotations?per_page=100"

        def api(path):
            requests.append(path)
            if path == run_path:
                self.assertTrue(currents, "Unexpected extra current-attempt read")
                return currents.pop(0)
            if path.startswith(job_path):
                page = int(path.removeprefix(job_path))
                return {"jobs": batches[page - 1] if batches is not None else jobs}
            if path == check_path:
                return annotations
            self.fail(f"Unexpected API request: {path}")

        def run(*args):
            commands.append(args)
            if args == ("git", "merge-base", "--is-ancestor", trigger["head_sha"], "origin/main"):
                if not ancestor:
                    raise subprocess.CalledProcessError(1, args)
            elif args == ("gh", "api", "--method", "POST",
                          f"repos/{retry.REPO}/actions/jobs/{FIXTURE['jobs'][0]['id']}/rerun"):
                if post_error:
                    raise subprocess.CalledProcessError(1, args, stderr="HTTP 403")
            else:
                self.fail(f"Unexpected command: {args}")
            return SimpleNamespace(stdout="", returncode=0)

        with patch.object(retry, "api", side_effect=api), patch.object(retry, "run", side_effect=run):
            result = retry.recover(event)
        return result, commands, requests

    def assert_no_post(self, result):
        accepted, commands, _ = result
        self.assertFalse(accepted)
        self.assertFalse(any("POST" in args for args in commands))

    def test_real_second_cancellation_retries_only_the_mac_job(self):
        accepted, commands, requests = self.replay()
        self.assertTrue(accepted)
        self.assertEqual(len(commands), 2)
        self.assertEqual(commands[1][-1],
                         f"repos/{retry.REPO}/actions/jobs/135060282393/rerun")
        self.assertEqual(requests.count("actions/runs/44366503879"), 2)
        self.assertEqual(sum(job["conclusion"] == "success" for job in FIXTURE["jobs"]), 3)

    def test_release_capacity_recovery_keeps_packages_and_retries_skipped_dependents(self):
        for branch, kind in (("main", "push"), ("main", "workflow_dispatch"), ("v0.1.14", "push")):
            event = self.event()
            event["workflow_run"].update(name="Release", path=".github/workflows/release.yml",
                                         head_branch=branch, event=kind)
            jobs = copy.deepcopy(FIXTURE["jobs"])
            jobs[0]["name"] = "macOS packages (arm64)"
            jobs.append({**jobs[1], "id": 901, "name": "Verify uploads and publish",
                         "conclusion": "skipped"})
            with self.subTest(branch=branch, event=kind):
                accepted, commands, _ = self.replay(event=event, jobs=jobs)
                self.assertTrue(accepted)
                self.assertEqual(len(commands), 2)
                jobs[-1].update(conclusion="failure", steps=[
                    {"name": "Upload, verify every GitHub digest, then publish", "conclusion": "failure"}])
                self.assert_no_post(self.replay(event=event, jobs=jobs))

    def test_first_failed_attempt_with_a_cancelled_mac_job_is_eligible(self):
        event = self.event()
        event["workflow_run"]["run_attempt"] = 1
        jobs = copy.deepcopy(FIXTURE["jobs"])
        for job in jobs:
            job["run_attempt"] = 1
        self.assertTrue(self.replay(event=event, jobs=jobs)[0])

    def test_only_completed_official_main_or_stable_events_are_accepted(self):
        changes = [
            ("head_repository", {"full_name": "fork/asagiri"}),
            ("head_repository", None), ("event", "pull_request"),
            ("event", "pull_request_target"), ("event", "workflow_run"),
            ("head_branch", "feature"), ("head_sha", "not-a-sha"),
            ("name", "Untrusted"), ("path", ".github/workflows/other.yml"),
            ("status", "in_progress"), ("conclusion", "success"),
            ("conclusion", "cancelled"),
            ("id", True), ("id", 0), ("run_attempt", True), ("run_attempt", 0),
        ]
        for field, value in changes:
            event = self.event()
            event["workflow_run"][field] = value
            with self.subTest(field=field, value=value), self.assertRaises(SystemExit):
                self.replay(event=event)
        for field, value in (("action", "requested"), ("repository", {"full_name": "fork/asagiri"})):
            event = self.event()
            event[field] = value
            with self.subTest(field=field), self.assertRaises(SystemExit):
                self.replay(event=event)
        for branch, kind in (("v01.1.1", "push"), ("v0.1.14-dev.131", "push"),
                             ("feature", "push"), ("v0.1.14", "workflow_dispatch")):
            event = self.event()
            event["workflow_run"].update(name="Release", path=".github/workflows/release.yml",
                                         head_branch=branch, event=kind)
            with self.subTest(branch=branch, event=kind), self.assertRaises(SystemExit):
                self.replay(event=event)

    def test_source_outside_main_history_cannot_retry(self):
        with self.assertRaises(subprocess.CalledProcessError):
            self.replay(ancestor=False)

    def test_attempt_limit_does_not_make_even_a_read_request(self):
        for attempt in (3, 4, 100):
            event = self.event()
            event["workflow_run"]["run_attempt"] = attempt
            accepted, commands, requests = self.replay(event=event)
            self.assertFalse(accepted)
            self.assertEqual(commands, [])
            self.assertEqual(requests, [])

    def test_live_attempt_recheck_rejects_stale_or_changed_metadata(self):
        for field, value in (("status", "queued"), ("run_attempt", 3), ("conclusion", "success"),
                             ("conclusion", "cancelled"),
                             ("head_sha", "b" * 40), ("id", 123),
                             ("head_branch", "feature"), ("name", "Release"),
                             ("path", ".github/workflows/other.yml"),
                             ("head_repository", {"full_name": "fork/asagiri"})):
            changed = {**FIXTURE["run"], field: value}
            for currents in ([changed], [FIXTURE["run"], changed]):
                with self.subTest(field=field, reads=len(currents)):
                    self.assert_no_post(self.replay(currents=currents))

    def test_assigned_or_started_jobs_never_qualify(self):
        for field, value in (("runner_id", 1), ("runner_id", None), ("runner_id", False),
                             ("runner_name", "assigned-runner"),
                             ("steps", [{"name": "Set up job", "conclusion": "cancelled"}]),
                             ("steps", None), ("conclusion", "failure"),
                             ("name", "Linux"), ("id", True),
                             ("run_id", 123), ("run_attempt", 1), ("head_sha", "b" * 40)):
            jobs = copy.deepcopy(FIXTURE["jobs"])
            jobs[0][field] = value
            with self.subTest(field=field, value=value):
                self.assert_no_post(self.replay(jobs=jobs))

    def test_failed_timed_out_or_active_sibling_jobs_prevent_recovery(self):
        for field, value in (("conclusion", "failure"), ("conclusion", "timed_out"),
                             ("conclusion", "cancelled"), ("status", "in_progress"),
                             ("run_id", 123), ("run_attempt", 1)):
            jobs = copy.deepcopy(FIXTURE["jobs"])
            jobs[1][field] = value
            with self.subTest(field=field, value=value):
                self.assert_no_post(self.replay(jobs=jobs))
        self.assert_no_post(self.replay(jobs=[]))
        self.assert_no_post(self.replay(jobs=copy.deepcopy(FIXTURE["jobs"][1:])))

    def test_manual_cancellations_and_incomplete_diagnostics_do_not_retry(self):
        variants = [
            [], FIXTURE["annotations"][1:], FIXTURE["annotations"][:1],
            [{"annotation_level": "failure", "message": "The operation was canceled."}],
            [*FIXTURE["annotations"], {"annotation_level": "failure", "message": "Build failed"}],
            FIXTURE["annotations"] * 25, [None],
        ]
        for annotations in variants:
            with self.subTest(annotations=annotations):
                self.assert_no_post(self.replay(annotations=annotations))

    def test_annotation_urls_cannot_redirect_api_calls(self):
        for url in (None, "", "https://example.com/check-runs/1",
                    "https://api.github.com/repos/fork/asagiri/check-runs/1",
                    "https://api.github.com/repos/AndreKurait/asagiri/check-runs/1/../2",
                    "https://api.github.com/repos/AndreKurait/asagiri/check-runs/1?other=2"):
            jobs = copy.deepcopy(FIXTURE["jobs"])
            jobs[0]["check_run_url"] = url
            with self.subTest(url=url):
                self.assert_no_post(self.replay(jobs=jobs))

    def test_attempt_jobs_are_paginated_without_hiding_failures(self):
        successful = [{**FIXTURE["jobs"][1], "id": 1000 + i} for i in range(100)]
        self.assertTrue(self.replay(batches=[successful, [FIXTURE["jobs"][0]]])[0])
        self.assert_no_post(self.replay(batches=[
            successful, [FIXTURE["jobs"][0], {**FIXTURE["jobs"][2], "conclusion": "failure"}]]))

    def test_race_with_manual_retry_does_not_loop_or_repeat_the_post(self):
        active = {**FIXTURE["run"], "status": "queued", "run_attempt": 3, "conclusion": None}
        accepted, commands, _ = self.replay(
            currents=[FIXTURE["run"], FIXTURE["run"], active], post_error=True)
        self.assertFalse(accepted)
        self.assertEqual(sum("POST" in args for args in commands), 1)
        with self.assertRaises(subprocess.CalledProcessError):
            self.replay(post_error=True)


if __name__ == "__main__":
    unittest.main()
