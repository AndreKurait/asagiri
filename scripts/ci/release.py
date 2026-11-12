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


def run(*args, check=True):
    return subprocess.run(args, check=check, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def api(repo, path):
    return json.loads(run("gh", "api", f"repos/{repo}/{path}").stdout)


def checked_runs(runs, commit):
    latest = {}
    for item in sorted(runs, key=lambda item: item["id"], reverse=True):
        if (item["head_sha"] == commit and item["head_branch"] == "main"
                and item["event"] in ("push", "workflow_dispatch") and item["name"] in REQUIRED):
            latest.setdefault(item["name"], item)
    missing = sorted(name for name in REQUIRED
                     if name not in latest or latest[name]["conclusion"] != "success")
    return latest, missing


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
        runs = api(repo, f"actions/runs?head_sha={commit}&per_page=100")["workflow_runs"]
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


def plan(args):
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    kind = os.environ["GITHUB_EVENT_NAME"]
    ref = os.environ["GITHUB_REF"]
    commit = run("git", "rev-parse", "HEAD").stdout.strip()
    validate_event(kind, ref, event, args.repo, commit)
    run("git", "merge-base", "--is-ancestor", commit, "origin/main")
    if ref.startswith("refs/tags/") and run("git", "rev-parse", ref + "^{commit}").stdout.strip() != commit:
        raise SystemExit("The version tag differs from the source checkout")
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
    emit(ready=True, commit=commit, version=version, tag=tag, prerelease=prerelease)


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
    return {
        "buildDefinition": {
            "buildType": "https://actions.github.io/buildtypes/workflow/v1",
            "externalParameters": {
                "workflow": {"ref": env["GITHUB_REF"], "repository": f"{server}/{repo}",
                             "path": workflow_ref.split("@")[0].removeprefix(repo + "/")},
                "source": {"commit": commit, "version": version},
            },
            "internalParameters": {"github": {"event_name": env["GITHUB_EVENT_NAME"]}},
            "resolvedDependencies": [
                {"uri": f"git+{server}/{repo}@{commit}", "digest": {"gitCommit": commit},
                 "annotations": {"role": "package source"}},
                {"uri": f"git+{server}/{repo}@{workflow_sha}", "digest": {"gitCommit": workflow_sha},
                 "annotations": {"role": "build workflow"}},
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
    actual = {p.name for p in dist.iterdir() if p.is_file()}
    if actual != expected:
        raise SystemExit(f"Release asset mismatch: missing={sorted(expected - actual)}, extra={sorted(actual - expected)}")
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
        if run("git", "rev-parse", "HEAD").stdout.strip() != args.commit:
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
    intro = (ROOT / "docs/releases/asagiri.md").read_text()
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
    release = api(args.repo, f"releases/tags/{tag}")
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
    release = api(args.repo, f"releases/tags/{tag}")
    if release["draft"] or release["prerelease"] != prerelease:
        raise SystemExit("Release publication state did not match the requested channel")
    emit(url=release["html_url"])
    print(f"Published and verified {tag}: {len(files)} assets")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", "AndreKurait/asagiri"))
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("plan")
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
    publication = commands.add_parser("publish")
    publication.add_argument("directory", type=Path)
    args = parser.parse_args()
    if args.command == "plan":
        plan(args)
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
