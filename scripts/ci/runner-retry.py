#!/usr/bin/env python3
"""Retry an unassigned Mac job only after GitHub confirms hosted capacity failure."""
import json
import os
from pathlib import Path
import re
import subprocess

REPO = "AndreKurait/asagiri"
MAX_ATTEMPTS = 3
WORKFLOWS = {
    "Platform builds": (".github/workflows/platforms.yml", {"macOS"}),
    "Release": (".github/workflows/release.yml",
                {"macOS packages (arm64)", "macOS packages (x86_64)"}),
}
ACQUISITION_FAILURE = "The job was not acquired by Runner of type hosted even after multiple attempts"
CAPACITY_NOTICE = "Due to capacity constraints, jobs targeting macOS arm64 runners may experience longer queue times."


def run(*args):
    return subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def api(path):
    return json.loads(run("gh", "api", f"repos/{REPO}/{path}").stdout)


def positive_id(value):
    return type(value) is int and value > 0


def trusted_run(item):
    name = item.get("name")
    branch = item.get("head_branch", "")
    return (isinstance(name, str) and name in WORKFLOWS and item.get("path") == WORKFLOWS[name][0]
            and isinstance(item.get("head_repository"), dict)
            and item.get("head_repository", {}).get("full_name") == REPO
            and positive_id(item.get("id")) and positive_id(item.get("run_attempt"))
            and isinstance(item.get("head_sha"), str)
            and re.fullmatch(r"[a-f0-9]{40}", item["head_sha"]) is not None
            and item.get("event") in ("push", "workflow_dispatch")
            and (branch == "main" or (name == "Release" and item["event"] == "push"
                 and isinstance(branch, str)
                 and re.fullmatch(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", branch) is not None))
            and item.get("status") == "completed"
            # A manually cancelled workflow must stay cancelled, even if an
            # earlier Mac job also recorded a provider acquisition failure.
            and item.get("conclusion") == "failure")


def same_attempt(current, trigger):
    return trusted_run(current) and all(
        current.get(key) == trigger.get(key)
        for key in ("id", "name", "path", "head_sha", "head_branch", "event", "run_attempt", "conclusion"))


def retry_job(jobs, current):
    """Retain every successful job; do not retry real failures or ambiguous cancellations."""
    if not jobs or any(job.get("status") != "completed"
                       or job.get("conclusion") not in ("success", "skipped", "cancelled")
                       or job.get("run_id") != current["id"]
                       or job.get("head_sha") != current["head_sha"]
                       or job.get("run_attempt") != current["run_attempt"] for job in jobs):
        return None
    cancelled = [job for job in jobs if job["conclusion"] == "cancelled"]
    if len(cancelled) != 1:
        return None
    job = cancelled[0]
    if (job.get("name") not in WORKFLOWS[current["name"]][1]
            or not positive_id(job.get("id"))
            or type(job.get("runner_id")) is not int or job["runner_id"] != 0
            or job.get("runner_name") != "" or job.get("steps") != []):
        return None
    return job


def capacity_failure(annotations):
    if (not isinstance(annotations, list) or not 0 < len(annotations) < 100
            or any(not isinstance(item, dict) for item in annotations)):
        return False
    failures = [item.get("message") for item in annotations
                if item.get("annotation_level") == "failure"]
    return (failures == [ACQUISITION_FAILURE]
            and any(item.get("annotation_level") == "notice"
                    and item.get("message") == CAPACITY_NOTICE for item in annotations))


def recover(event):
    trigger = event.get("workflow_run", {})
    if (event.get("action") != "completed"
            or event.get("repository", {}).get("full_name") != REPO
            or not trusted_run(trigger)):
        raise SystemExit("Runner recovery requires a completed official main or stable release workflow")
    if trigger["run_attempt"] >= MAX_ATTEMPTS:
        print(f"No runner retry: limit of {MAX_ATTEMPTS} total attempts reached")
        return False
    # Only the trusted workflow checkout is executed. Its full history must
    # contain the source, including a stable tag or an older main release.
    run("git", "merge-base", "--is-ancestor", trigger["head_sha"], "origin/main")
    path = f"actions/runs/{trigger['id']}"
    current = api(path)
    if not same_attempt(current, trigger):
        print("No runner retry: the triggering attempt is no longer the current completed attempt")
        return False
    jobs = []
    page = 1
    while True:
        batch = api(f"{path}/attempts/{current['run_attempt']}/jobs?per_page=100&page={page}")["jobs"]
        jobs.extend(batch)
        if len(batch) < 100:
            break
        page += 1
    job = retry_job(jobs, current)
    if job is None:
        print("No runner retry: jobs include a real failure, assigned runner or ambiguous cancellation")
        return False
    check_url = job.get("check_run_url")
    check = re.fullmatch(
        rf"https://api\.github\.com/repos/{re.escape(REPO)}/check-runs/([1-9]\d*)",
        check_url) if isinstance(check_url, str) else None
    if check is None or not capacity_failure(
            api(f"check-runs/{check[1]}/annotations?per_page=100")):
        print("No runner retry: GitHub did not confirm an unassigned hosted capacity failure")
        return False
    # A manual retry may have started while jobs and annotations were inspected.
    if not same_attempt(api(path), current):
        print("No runner retry: another attempt has already started")
        return False
    try:
        run("gh", "api", "--method", "POST", f"repos/{REPO}/actions/jobs/{job['id']}/rerun")
    except subprocess.CalledProcessError:
        if not same_attempt(api(path), current):
            print("Another retry won the race; no duplicate request will be made")
            return False
        raise
    print(f"Retried unassigned {job['name']} job {job['id']} for {current['head_sha']}; "
          f"successful jobs retained, next attempt {current['run_attempt'] + 1}/{MAX_ATTEMPTS}")
    return True


if __name__ == "__main__":
    if os.environ.get("GITHUB_EVENT_NAME") != "workflow_run":
        raise SystemExit("Runner recovery requires a workflow completion event")
    recover(json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text()))
