#!/usr/bin/env python3
"""Plan, version, assemble, and publish verified Asagiri releases."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
import tomllib

ROOT = Path(__file__).resolve().parents[2]
REQUIRED = {"Platform builds", "UI tests", "Security", "Speed", "CodeQL"}
VERSION = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-dev\.[1-9]\d*)?")
RELEASE_WORKFLOW = ".github/workflows/release.yml"
PUBLISH_JOB = "Verify uploads and publish"
CHECK_STEP = "Require successful CI for this exact source commit"
MAX_RELEASE_ATTEMPTS = 3
PACKAGE_JOBS = {
    "plan", "Guest kernels and corresponding source",
    "Linux packages (aarch64)", "Linux packages (x86_64)",
    "macOS packages (arm64)", "macOS packages (x86_64)",
    "Windows packages (arm64)", "Windows packages (x86_64)",
    "Windows Arm installer check",
}
QEMU_JOB = "Intel emulator and corresponding source"
QEMU_RELEASE = "packaging/qemu/release.json"
QEMU_SOURCE = "asagiri-qemu-source.tar.xz"


def run(*args, check=True):
    return subprocess.run(args, check=check, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def api(repo, path):
    return json.loads(run("gh", "api", f"repos/{repo}/{path}").stdout)


def pages(repo, path, key):
    """Read every page so repeated CI runs cannot hide a source check."""
    result = []
    separator = "&" if "?" in path else "?"
    page = 1
    while True:
        items = api(repo, f"{path}{separator}per_page=100&page={page}")[key]
        result.extend(items)
        if len(items) < 100:
            return result
        page += 1


def checked_runs(runs, commit):
    latest = {}
    for item in sorted(runs, key=lambda item: item["id"], reverse=True):
        # Manual Speed runs can compare an arbitrary ancestor, including the
        # same commit as a noise control. They are archived experiments, not
        # substitutes for this main push's required regression comparison.
        if item["name"] == "Speed" and item["event"] != "push":
            continue
        if (item["head_sha"] == commit and item["head_branch"] == "main"
                and item["event"] in ("push", "workflow_dispatch") and item["name"] in REQUIRED):
            latest.setdefault(item["name"], item)
    missing = sorted(name for name in REQUIRED if name not in latest
                     or latest[name]["status"] != "completed"
                     or latest[name]["conclusion"] != "success")
    return latest, missing


def retry_source(event, repo):
    """Only a completed source check or failed official release can request a retry."""
    trigger = event.get("workflow_run", {})
    if (event.get("action") != "completed"
            or event.get("repository", {}).get("full_name") != repo
            or trigger.get("head_repository", {}).get("full_name") != repo
            or trigger.get("status") != "completed"
            or trigger.get("event") not in ("push", "workflow_dispatch")):
        raise SystemExit("Release retries require a completed same-repository workflow")
    branch = trigger.get("head_branch", "")
    if trigger.get("name") in REQUIRED:
        allowed = branch == "main" and trigger.get("conclusion") == "success"
    else:
        allowed = (trigger.get("name") == "Release" and trigger.get("path") == RELEASE_WORKFLOW
                   and trigger.get("conclusion") == "failure"
                   and (branch == "main" or (trigger["event"] == "push"
                        and re.fullmatch(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", branch))))
    commit = trigger.get("head_sha", "")
    if not allowed or not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise SystemExit("This workflow cannot request a release retry")
    return commit


def gate_only_failure(jobs):
    """Successful native packages may be reused only after a failed CI gate."""
    publishers = [job for job in jobs if job["name"] == PUBLISH_JOB]
    if len(publishers) != 1 or len(jobs) < 2:
        return False
    publisher = publishers[0]
    if (publisher["status"] != "completed" or publisher["conclusion"] != "failure"
            or any(job["status"] != "completed" or job["conclusion"] != "success"
                   for job in jobs if job is not publisher)):
        return False
    failures = [step["name"] for step in publisher["steps"] if step["conclusion"] == "failure"]
    return failures == [CHECK_STEP]


def retry(args):
    if os.environ.get("GITHUB_EVENT_NAME") != "workflow_run":
        raise SystemExit("Release retries require a workflow completion event")
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    commit = retry_source(event, args.repo)
    # This checkout is the trusted workflow revision, never the event's source.
    # Fetch-depth: 0 gives us main's history, including earlier tagged sources.
    run("git", "merge-base", "--is-ancestor", commit, "origin/main")
    runs = pages(args.repo, f"actions/runs?head_sha={commit}", "workflow_runs")
    _, missing = checked_runs(runs, commit)
    if missing:
        print("No release retry: source checks are incomplete: " + ", ".join(missing))
        return
    for candidate in runs:
        branch = candidate.get("head_branch", "")
        if (candidate.get("name") != "Release" or candidate.get("path") != RELEASE_WORKFLOW
                or candidate.get("head_sha") != commit
                or candidate.get("head_repository", {}).get("full_name") != args.repo
                or candidate.get("event") not in ("push", "workflow_dispatch")
                or not (branch == "main" or (candidate["event"] == "push"
                    and re.fullmatch(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", branch)))
                or candidate.get("status") != "completed" or candidate.get("conclusion") != "failure"):
            continue
        run_id = candidate["id"]
        if type(run_id) is not int or run_id <= 0:
            raise SystemExit("Invalid release run ID")
        # Re-read after the list request: a manual retry might already be running.
        current = api(args.repo, f"actions/runs/{run_id}")
        if (current["status"] != "completed" or current["conclusion"] != "failure"
                or current["run_attempt"] >= MAX_RELEASE_ATTEMPTS):
            continue
        jobs = pages(args.repo, f"actions/runs/{run_id}/jobs?filter=latest", "jobs")
        if not gate_only_failure(jobs):
            continue
        run("gh", "api", "--method", "POST",
            f"repos/{args.repo}/actions/runs/{run_id}/rerun-failed-jobs")
        print(f"Resumed release run {run_id} for {commit}; successful packages and version retained",
              flush=True)


def emit(**outputs):
    text = "".join(f"{key}={str(value).lower() if isinstance(value, bool) else value}\n"
                   for key, value in outputs.items())
    print(text, end="", flush=True)
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
            stream.write(text)


def validate_event(kind, ref, event, repo, commit):
    if event["repository"]["full_name"] != repo:
        raise SystemExit("Releases must originate in this repository")
    if kind == "workflow_dispatch" and ref == "refs/heads/main":
        return
    if kind != "push" or event.get("deleted") or event.get("ref") != ref:
        raise SystemExit("Releases require a main push, a version tag, or a manual main run")
    if ref == "refs/heads/main" and event.get("after") != commit:
        raise SystemExit("The source checkout differs from the main push")
    if ref != "refs/heads/main" and not ref.startswith("refs/tags/v"):
        raise SystemExit("Unsupported release trigger")


def require_checks(repo, commit, wait):
    if not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise SystemExit("Invalid source commit")
    deadline = time.monotonic() + wait
    while True:
        runs = pages(repo, f"actions/runs?head_sha={commit}", "workflow_runs")
        checked, missing = checked_runs(runs, commit)
        if not missing:
            print(f"All release checks passed for {commit}", flush=True)
            return
        failed = [name for name in missing if name in checked
                  and checked[name]["status"] == "completed"]
        if failed:
            raise SystemExit("Release checks did not succeed: " + ", ".join(failed))
        print("Waiting for successful main checks: " + ", ".join(missing), flush=True)
        if time.monotonic() >= deadline:
            raise SystemExit("Release checks are incomplete; no release published")
        time.sleep(15)


def release_version(kind, ref, run_number, base):
    if kind == "push" and ref.startswith("refs/tags/v"):
        version = ref.removeprefix("refs/tags/v")
        if not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version):
            raise SystemExit("Stable tags must have the form vMAJOR.MINOR.PATCH")
        return version, False
    if not VERSION.fullmatch(base) or "-" in base:
        raise SystemExit("RELEASE_VERSION must be MAJOR.MINOR.PATCH")
    if not re.fullmatch(r"[1-9]\d*", run_number):
        raise SystemExit("The prerelease requires a positive Release workflow run number")
    return f"{base}-dev.{run_number}", True


def recover_packages(repo, run_id):
    """Reuse only complete packages from a failed official main/tag release."""
    if not re.fullmatch(r"[1-9]\d*", run_id):
        raise SystemExit("Recovery requires a positive Release workflow run ID")
    previous = api(repo, f"actions/runs/{run_id}")
    branch = previous.get("head_branch", "")
    commit = previous.get("head_sha", "")
    if (previous.get("id") != int(run_id) or previous.get("name") != "Release"
            or previous.get("path") != RELEASE_WORKFLOW
            or previous.get("head_repository", {}).get("full_name") != repo
            or previous.get("event") not in ("push", "workflow_dispatch")
            or previous.get("status") != "completed" or previous.get("conclusion") != "failure"
            or not re.fullmatch(r"[a-f0-9]{40}", commit)
            or not (branch == "main" or (previous["event"] == "push"
                and re.fullmatch(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", branch)))):
        raise SystemExit("Only a failed official main or version-tag release can supply packages")
    run("git", "merge-base", "--is-ancestor", commit, "origin/main")
    jobs = pages(repo, f"actions/runs/{run_id}/jobs?filter=latest", "jobs")
    source_marker = run("git", "ls-tree", "--name-only", commit, QEMU_RELEASE).stdout.strip()
    if source_marker not in ("", QEMU_RELEASE):
        raise SystemExit("Unexpected QEMU release declaration")
    expected_jobs = PACKAGE_JOBS | ({QEMU_JOB} if source_marker else set())
    if len(jobs) != len(expected_jobs) + 1 or {job["name"] for job in jobs} != expected_jobs | {PUBLISH_JOB}:
        raise SystemExit("Recovery requires every native packaging and installer job")
    for job in jobs:
        expected = "failure" if job["name"] == PUBLISH_JOB else "success"
        if job["status"] != "completed" or job["conclusion"] != expected:
            raise SystemExit("Recovery cannot reuse failed, skipped, or incomplete packages")
    publisher = next(job for job in jobs if job["name"] == PUBLISH_JOB)
    failures = [step["name"] for step in publisher["steps"] if step["conclusion"] == "failure"]
    if failures not in ([CHECK_STEP], ["Attest the release artifacts"]):
        raise SystemExit("Recovery is limited to CI-gate or attestation failures before publication")
    require_checks(repo, commit, 0)
    base = run("git", "show", f"{commit}:RELEASE_VERSION").stdout.strip()
    ref = "refs/heads/main" if branch == "main" else "refs/tags/" + branch
    if branch != "main" and api(repo, f"commits/{branch}")["sha"] != commit:
        raise SystemExit("The source release tag has changed")
    version, prerelease = release_version(previous["event"], ref, str(previous["run_number"]), base)
    return commit, version, prerelease, previous["run_attempt"]


def plan(args):
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    kind = os.environ["GITHUB_EVENT_NAME"]
    ref = os.environ["GITHUB_REF"]
    commit = run("git", "rev-parse", "HEAD").stdout.strip()
    validate_event(kind, ref, event, args.repo, commit)
    run("git", "merge-base", "--is-ancestor", commit, "origin/main")
    if ref.startswith("refs/tags/") and run("git", "rev-parse", ref + "^{commit}").stdout.strip() != commit:
        raise SystemExit("The version tag differs from the source checkout")
    artifact_run = (event.get("inputs") or {}).get("recover_run") or ""
    artifact_attempt = os.environ["GITHUB_RUN_ATTEMPT"]
    if artifact_run:
        if kind != "workflow_dispatch" or ref != "refs/heads/main":
            raise SystemExit("Package recovery requires a manual run of the main workflow")
        commit, version, prerelease, artifact_attempt = recover_packages(args.repo, artifact_run)
    else:
        version, prerelease = release_version(
            kind, ref, os.environ["GITHUB_RUN_NUMBER"], (ROOT / "RELEASE_VERSION").read_text().strip())
    tag = "v" + version
    previous = run("gh", "release", "view", tag, "--repo", args.repo,
                   "--json", "isDraft,targetCommitish", check=False)
    if previous.returncode == 0:
        release = json.loads(previous.stdout)
        target = (release["targetCommitish"] if release["isDraft"]
                  else api(args.repo, f"commits/{tag}")["sha"])
        if target != commit:
            raise SystemExit(f"{tag} already names a different commit")
        if not release["isDraft"]:
            print(f"{tag} is already published for {commit}")
            emit(ready=False)
            return
    emit(ready=True, commit=commit, version=version, tag=tag, prerelease=prerelease,
         artifact_run=artifact_run, artifact_attempt=artifact_attempt)


def stamp(root, version):
    if not VERSION.fullmatch(version):
        raise ValueError(f"Invalid release version: {version}")
    manifest = root / "Cargo.toml"
    text = manifest.read_text()
    old = tomllib.loads(text)["workspace"]["package"]["version"]
    text, count = re.subn(r'(?m)^version = "' + re.escape(old) + r'"$',
                          f'version = "{version}"', text, count=1)
    if count != 1:
        raise ValueError("Could not set the workspace version")
    names = {tomllib.loads(path.read_text())["package"]["name"]
             for path in [*root.glob("crates/*/Cargo.toml"), root / "xtask/Cargo.toml"]}
    lock = root / "Cargo.lock"
    blocks = lock.read_text().split("[[package]]")
    changed = set()
    for index, block in enumerate(blocks[1:], 1):
        name = re.search(r'(?m)^name = "([^"]+)"$', block)
        if name and name[1] in names and not re.search(r"(?m)^source =", block):
            blocks[index], count = re.subn(r'(?m)^version = "[^"]+"$',
                                           f'version = "{version}"', block, count=1)
            if count != 1:
                raise ValueError(f"Could not set lockfile version for {name[1]}")
            changed.add(name[1])
    if changed != names:
        raise ValueError(f"Workspace packages absent from lockfile: {names - changed}")
    manifest.write_text(text)
    lock.write_text("[[package]]".join(blocks))
    print(f"Stamped {len(changed)} workspace packages as {version}; dependency versions preserved")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def provenance(repo, commit, version, env):
    # A workflow_run executes the workflow from the default branch, which
    # may have advanced beyond the checked source commit. Record both
    # revisions explicitly instead of attesting the workflow SHA as if it
    # were the package source.
    workflow_sha = env["GITHUB_WORKFLOW_SHA"]
    workflow_ref = env["GITHUB_WORKFLOW_REF"]
    server = env["GITHUB_SERVER_URL"]
    if not re.fullmatch(r"[a-f0-9]{40}", workflow_sha):
        raise ValueError("Invalid build workflow commit")
    if not workflow_ref.startswith(repo + "/.github/workflows/"):
        raise ValueError("Build workflow belongs to a different repository")
    if env["GITHUB_SHA"] != workflow_sha:
        raise ValueError("Publication must execute the workflow revision it attests")
    repository_id = env.get("GITHUB_REPOSITORY_ID", "")
    owner_id = env.get("GITHUB_REPOSITORY_OWNER_ID", "")
    runner = env.get("RUNNER_ENVIRONMENT", "")
    if not all(re.fullmatch(r"[1-9]\d*", value) for value in (repository_id, owner_id)):
        raise ValueError("GitHub provenance requires repository and owner IDs")
    if runner != "github-hosted":
        raise ValueError("Release provenance requires a GitHub-hosted runner")
    return {
        "buildDefinition": {
            "buildType": "https://actions.github.io/buildtypes/workflow/v1",
            "externalParameters": {
                "workflow": {"ref": env["GITHUB_REF"], "repository": f"{server}/{repo}",
                             "path": workflow_ref.split("@")[0].removeprefix(repo + "/")},
                "source": {"commit": commit, "version": version},
            },
            # GitHub's SLSA build type requires these identity fields even for
            # a custom predicate. Omitting them makes attestation persistence fail.
            "internalParameters": {
                "github": {"event_name": env["GITHUB_EVENT_NAME"],
                           "repository_id": repository_id, "repository_owner_id": owner_id,
                           "runner_environment": runner},
            },
            "resolvedDependencies": [
                # GitHub checks the first dependency against the OIDC ref and
                # SHA. The URI must name that ref, not a commit-shaped ref.
                {"uri": f"git+{server}/{repo}@{env['GITHUB_REF']}", "digest": {"gitCommit": workflow_sha},
                 "annotations": {"role": "build workflow"}},
                {"uri": f"git+{server}/{repo}@{commit}", "digest": {"gitCommit": commit},
                 "annotations": {
                     "role": "package source",
                     "buildInvocation": f"{server}/{repo}/actions/runs/"
                         f"{env.get('ASAGIRI_PACKAGE_RUN_ID', env['GITHUB_RUN_ID'])}/attempts/"
                         f"{env.get('ASAGIRI_PACKAGE_RUN_ATTEMPT', env['GITHUB_RUN_ATTEMPT'])}",
                 }},
            ],
        },
        "runDetails": {
            "builder": {"id": f"{server}/{workflow_ref}"},
            "metadata": {"invocationId": f"{server}/{repo}/actions/runs/{env['GITHUB_RUN_ID']}/attempts/{env['GITHUB_RUN_ATTEMPT']}"},
        },
    }


def collect(args):
    dist = args.directory
    version = args.version
    if not VERSION.fullmatch(version) or not re.fullmatch(r"[a-f0-9]{40}", args.commit):
        raise SystemExit("Invalid release version or source commit")
    expected = set()
    package_version = version.replace("-", "~", 1)
    for arch, deb in (("x86_64", "amd64"), ("aarch64", "arm64")):
        expected.add(f"asagiri-{version}-linux-{arch}.tar.gz")
        for name in ("asagiri", "asagiri-lite"):
            expected.add(f"{name}_{package_version}_{deb}.deb")
            expected.add(f"{name}-{package_version}-1.{arch}.rpm")
    for arch in ("arm64", "x86_64"):
        expected.update({f"Asagiri-{version}-{arch}.dmg", f"Asagiri-{version}-{arch}.tar.gz",
                         f"Asagiri-{version}-{arch}.msi", f"Asagiri-{version}-windows-{arch}.zip",
                         f"latest-mac-{arch}.json"})
    expected.update({"latest-windows-x86_64.json", "latest-windows-arm64.json", "asagiri-kernel-source.tar.xz"})
    appimage_marker = getattr(args, "source_root", ROOT) / "packaging/linux/appimage-release.json"
    if appimage_marker.exists():
        if appimage_marker.is_symlink() or json.loads(appimage_marker.read_text()) != {
                "schema": 1, "architectures": ["x86_64", "aarch64"]}:
            raise SystemExit("Invalid AppImage release declaration")
        expected.update(f"Asagiri-{version}-{arch}.AppImage" for arch in ("x86_64", "aarch64"))
    # A newer publisher can recover an older source release. Its package
    # contract comes from that source checkout, not the publisher's revision.
    marker = getattr(args, "source_root", ROOT) / QEMU_RELEASE
    if marker.exists():
        if marker.is_symlink() or json.loads(marker.read_text()) != {"schema": 1, "source_asset": QEMU_SOURCE}:
            raise SystemExit("Invalid QEMU release declaration")
        expected.add(QEMU_SOURCE)
    # GitHub changes '~' to '.' in uploaded release filenames. Keep '~' in
    # the packages' own versions (where it sorts before a stable release),
    # but normalize filenames before hashing and attesting their names.
    # Accept original build artifacts and already-normalized artifacts;
    # reject collisions rather than overwrite either input.
    aliases = {name.replace("~", "."): name for name in expected}
    actual = {p.name for p in dist.iterdir()}
    matches = {name: sorted(actual & {name, original}) for name, original in aliases.items()}
    missing = sorted(name for name, sources in matches.items() if not sources)
    duplicates = sorted(name for name, sources in matches.items() if len(sources) != 1 and sources)
    extra = sorted(actual - expected - aliases.keys())
    if missing or extra or duplicates:
        raise SystemExit(f"Release asset mismatch: missing={missing}, extra={extra}, collisions={duplicates}")
    for sources in matches.values():
        path = dist / sources[0]
        if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
            raise SystemExit(f"Invalid release asset: {path.name}")
    for name, sources in matches.items():
        if sources[0] != name:
            (dist / sources[0]).rename(dist / name)
    expected = set(aliases)
    assets = []
    for name in sorted(expected):
        path = dist / name
        if path.is_symlink() or path.stat().st_size == 0:
            raise SystemExit(f"Invalid release asset: {name}")
        assets.append({"name": name, "size": path.stat().st_size, "sha256": sha256(path)})
    for name in sorted(expected):
        if name.startswith("latest-"):
            feed = json.loads((dist / name).read_text())
            asset_name = feed["url"].rsplit("/", 1)[-1]
            asset = next((item for item in assets if item["name"] == asset_name), None)
            if (not asset or feed["version"] != version or feed["sha256"] != asset["sha256"]
                    or feed["url"] != f"https://github.com/{args.repo}/releases/download/v{version}/{asset_name}"):
                raise SystemExit(f"Invalid update-feed metadata: {name}")
    manifest = dist / "release-manifest.json"
    manifest.write_text(json.dumps({"version": version, "commit": args.commit,
                                   "repository": args.repo, "assets": assets}, indent=2) + "\n")
    sums = [f"{item['sha256']}  {item['name']}\n" for item in assets]
    sums.append(f"{sha256(manifest)}  {manifest.name}\n")
    (dist / "SHA256SUMS").write_text("".join(sums))
    if os.environ.get("GITHUB_ACTIONS") == "true":
        if run("git", "-C", str(args.source_root), "rev-parse", "HEAD").stdout.strip() != args.commit:
            raise SystemExit("Publication checkout differs from the package source commit")
        predicate = ROOT / "target/release-provenance.json"
        predicate.parent.mkdir(exist_ok=True)
        predicate.write_text(json.dumps(provenance(args.repo, args.commit, version, os.environ), indent=2) + "\n")
    print(f"Verified {len(assets)} complete release assets and their update metadata")


def publish(args):
    dist = args.directory
    manifest = json.loads((dist / "release-manifest.json").read_text())
    version, commit = manifest["version"], manifest["commit"]
    tag = "v" + version
    prerelease = "-dev." in version
    notes = ROOT / "target/release-notes.md"
    notes.parent.mkdir(exist_ok=True)
    intro = (args.source_root / "docs/releases/asagiri.md").read_text()
    notes.write_text(f"{intro}\n\nSource: `{commit}`\n\n"
                     "Each download has a SHA-256 in `SHA256SUMS`; `release-manifest.json` "
                     "records its size and source commit. GitHub build attestations are also attached.\n")
    existing = run("gh", "release", "view", tag, "--repo", args.repo, "--json",
                   "isDraft,targetCommitish", check=False)
    if existing.returncode == 0:
        current = json.loads(existing.stdout)
        target = (current["targetCommitish"] if current["isDraft"]
                  else api(args.repo, f"commits/{tag}")["sha"])
        if target != commit:
            raise SystemExit("Refusing to replace a tag for a different source commit")
        if not current["isDraft"]:
            raise SystemExit("Refusing to overwrite an already published release")
    else:
        command = ["gh", "release", "create", tag, "--repo", args.repo, "--target", commit,
                   "--draft", "--title", f"Asagiri {version}", "--notes-file", str(notes)]
        command += ["--prerelease"] if prerelease else ["--verify-tag"]
        run(*command)
    files = sorted(p for p in dist.iterdir() if p.is_file())
    run("gh", "release", "upload", tag, "--repo", args.repo, "--clobber", *map(str, files))
    # REST's tag endpoint finds published releases only. `gh release view`
    # also resolves pending draft tags through GraphQL, giving us their REST ID.
    release_id = json.loads(run("gh", "release", "view", tag, "--repo", args.repo,
                                "--json", "databaseId").stdout)["databaseId"]
    if type(release_id) is not int or release_id <= 0:
        raise SystemExit("GitHub returned an invalid draft release ID")
    release_path = f"releases/{release_id}"
    release = api(args.repo, release_path)
    if (release["id"] != release_id or not release["draft"] or release["tag_name"] != tag
            or release["target_commitish"] != commit):
        raise SystemExit("The draft identity or source changed before upload verification")
    remote = {asset["name"]: asset for asset in release["assets"]}
    if set(remote) != {path.name for path in files}:
        raise SystemExit("Uploaded release has unexpected or missing files; leaving it as a draft")
    for path in files:
        asset = remote[path.name]
        if asset["size"] != path.stat().st_size or asset.get("digest") != "sha256:" + sha256(path):
            raise SystemExit(f"GitHub upload integrity check failed: {path.name}; leaving the release as a draft")
    command = ["gh", "release", "edit", tag, "--repo", args.repo, "--draft=false",
               "--notes-file", str(notes), "--latest=false" if prerelease else "--latest"]
    if prerelease:
        command.append("--prerelease")
    run(*command)
    release = api(args.repo, release_path)
    if release["draft"] or release["prerelease"] != prerelease:
        raise SystemExit("Release publication state did not match the requested channel")
    emit(url=release["html_url"])
    print(f"Published and verified {tag}: {len(files)} assets")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", "AndreKurait/asagiri"))
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("plan")
    commands.add_parser("retry")
    checks = commands.add_parser("checks")
    checks.add_argument("commit")
    checks.add_argument("--wait", type=int, default=2700)
    version = commands.add_parser("stamp")
    version.add_argument("version")
    version.add_argument("--root", type=Path, default=ROOT)
    assemble = commands.add_parser("collect")
    assemble.add_argument("directory", type=Path)
    assemble.add_argument("version")
    assemble.add_argument("commit")
    assemble.add_argument("--source-root", type=Path, default=ROOT)
    publication = commands.add_parser("publish")
    publication.add_argument("directory", type=Path)
    publication.add_argument("--source-root", type=Path, default=ROOT)
    args = parser.parse_args()
    if args.command == "plan":
        plan(args)
    elif args.command == "retry":
        retry(args)
    elif args.command == "checks":
        require_checks(args.repo, args.commit, args.wait)
    elif args.command == "stamp":
        stamp(args.root, args.version)
    elif args.command == "collect":
        collect(args)
    else:
        publish(args)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr or str(error)) from error
