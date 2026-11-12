#!/usr/bin/env python3
"""Build verified download metadata and a signed RPM repository for Pages."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import urllib.request

REPO = "AndreKurait/asagiri"
ROOT = Path(__file__).resolve().parents[2]
SITE = "https://andrekurait.github.io/asagiri"
VERSION = re.compile(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-dev\.([1-9]\d*))?")


def command(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout


def api(path):
    return json.loads(command("gh", "api", f"repos/{REPO}/{path}"))


def channels(releases):
    eligible = []
    for release in releases:
        match = VERSION.fullmatch(release["tag_name"])
        if release["draft"] or not match or bool(match[4]) != release["prerelease"]:
            continue
        # Completion order can differ from build order. Never downgrade the
        # index when an older main build finishes after a newer one.
        order = (*map(int, match.groups()[:3]), int(match[4] or 0))
        eligible.append((order, release))
    stable = max((item for item in eligible if not item[1]["prerelease"]), default=None)
    preview = max((item for item in eligible if item[1]["prerelease"]), default=None)
    return {name: item[1] if item else None
            for name, item in {"latest": stable or preview, "stable": stable, "preview": preview}.items()}


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def asset_url(tag, name):
    if (not VERSION.fullmatch(tag) or name in (".", "..")
            or not re.fullmatch(r"[A-Za-z0-9_.~+-]+", name)):
        raise ValueError("Invalid release asset path")
    return f"https://github.com/{REPO}/releases/download/{tag}/{name}"


def download(asset, tag, folder):
    name = asset["name"]
    url = asset_url(tag, name)
    digest = asset.get("digest", "")
    if (asset["browser_download_url"] != url or not isinstance(digest, str)
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest)):
        raise ValueError(f"Unverified GitHub asset metadata: {name}")
    path = folder / name
    if path.is_file() and path.stat().st_size == asset["size"] and sha256(path) == digest[7:]:
        return path
    if shutil.disk_usage(folder).free < asset["size"] + 3 * 1024**3:
        raise RuntimeError("Fewer than 3 GiB would remain after the download")
    partial = path.with_name(path.name + ".partial")
    # Public downloads need no credential. Never forward GH_TOKEN through an
    # asset redirect to another host.
    request = urllib.request.Request(url, headers={"User-Agent": "Asagiri-distribution"})
    with urllib.request.urlopen(request, timeout=120) as response, partial.open("wb") as stream:
        shutil.copyfileobj(response, stream)
    if partial.stat().st_size != asset["size"] or sha256(partial) != digest[7:]:
        partial.unlink()
        raise ValueError(f"Downloaded asset does not match GitHub's digest: {name}")
    partial.replace(path)
    return path


def verified_release(release, cache):
    tag = release["tag_name"]
    folder = cache / tag
    folder.mkdir(parents=True, exist_ok=True)
    assets = {asset["name"]: asset for asset in release["assets"]}
    if len(assets) != len(release["assets"]):
        raise ValueError("Duplicate asset names")
    for name in ("release-manifest.json", "SHA256SUMS", "attestation.sigstore.json"):
        download(assets[name], tag, folder)
    manifest = json.loads((folder / "release-manifest.json").read_text())
    if (manifest["repository"] != REPO or manifest["version"] != tag[1:]
            or manifest["commit"] != api(f"commits/{tag}")["sha"]):
        raise ValueError("Release manifest, tag, and repository do not agree")
    if api(f"compare/{manifest['commit']}...main")["merge_base_commit"]["sha"] != manifest["commit"]:
        raise ValueError("The release source is not part of reviewed main history")
    listed = {item["name"]: item for item in manifest["assets"]}
    for name, item in listed.items():
        remote = assets[name]
        if remote["size"] != item["size"] or remote["digest"] != "sha256:" + item["sha256"]:
            raise ValueError(f"Manifest digest differs from the published asset: {name}")
    verified = json.loads(command(
        "gh", "attestation", "verify", str(folder / "SHA256SUMS"),
        "--repo", REPO, "--signer-workflow", REPO + "/.github/workflows/release.yml",
        "--signer-digest", manifest["commit"], "--source-digest", manifest["commit"],
        "--deny-self-hosted-runners", "--bundle", str(folder / "attestation.sigstore.json"),
        "--format", "json",
    ))
    statements = [item["verificationResult"]["statement"] for item in verified]
    statement = next((item for item in statements if
                      item["predicateType"] == "https://slsa.dev/provenance/v1"
                      and item["predicate"]["buildDefinition"]["externalParameters"].get("source")
                      == {"commit": manifest["commit"], "version": manifest["version"]}), None)
    if statement is None:
        raise ValueError("Build provenance does not identify this source and version")
    subjects = {Path(item["name"]).name: item["digest"]["sha256"] for item in statement["subject"]}
    for name in [*listed, "release-manifest.json", "SHA256SUMS"]:
        if subjects.get(name) != assets[name]["digest"][7:]:
            raise ValueError(f"Build provenance does not cover the published asset: {name}")
    return manifest, assets, folder


def signing_key():
    public_key = ROOT / "packaging/keys/asagiri-rpm.asc"
    fingerprint = (ROOT / "packaging/keys/asagiri-rpm.fingerprint").read_text().strip()
    if not re.fullmatch(r"[A-F0-9]{40}", fingerprint):
        raise ValueError("Invalid RPM signing key fingerprint")
    secret = os.environ.get("ASAGIRI_RPM_SIGNING_KEY")
    if not secret:
        raise RuntimeError("ASAGIRI_RPM_SIGNING_KEY is required to publish the RPM repository")
    command("gpg", "--batch", "--import", input=secret)
    actual = command("gpg", "--batch", "--with-colons", "--fingerprint", fingerprint)
    if not any(line.startswith("fpr:") and line.split(":")[9] == fingerprint for line in actual.splitlines()):
        raise ValueError("RPM signing secret does not match the committed public key")
    return public_key, fingerprint


def rpm_repo(release, manifest, assets, cache, destination, fingerprint, public_key):
    version = manifest["version"].replace("-", "~", 1)
    destination.mkdir(parents=True, exist_ok=True)
    packages = destination / "Packages"
    packages.mkdir(exist_ok=True)
    arch = destination.name
    for package in ("asagiri", "asagiri-lite"):
        name = f"{package}-{version}-1.{arch}.rpm"
        original = download(assets[name], release["tag_name"], cache)
        target = packages / name
        shutil.copy2(original, target)
        command("rpmsign", "--define", f"_gpg_name {fingerprint}",
                "--define", f"__gpg {shutil.which('gpg')}",
                "--define", "_gpg_digest_algo sha256", "--addsign", str(target))
        # Verify the new RPM header signature against an isolated keyring.
        database = destination / ".verify-rpmdb"
        database.mkdir(exist_ok=True)
        command("rpm", "--dbpath", str(database), "--import", str(public_key))
        result = command("rpmkeys", "--dbpath", str(database), "--checksig", str(target))
        if "signatures OK" not in result:
            raise ValueError(f"Signed RPM verification failed: {name}: {result}")
        shutil.rmtree(database)
    command("createrepo_c", "--quiet", str(destination))
    # Record why the signed repository bytes differ from the original RPM's
    # release checksum: only the package header signature was added.
    (destination / "provenance.json").write_text(json.dumps({
        "release": release["html_url"], "source": manifest["commit"],
        "rpm_signing_fingerprint": fingerprint,
        "packages": [{"name": p.name, "sha256": sha256(p),
                      "original_release_sha256": assets[p.name]["digest"][7:]}
                     for p in sorted(packages.iterdir())],
    }, indent=2) + "\n")


def build(output):
    if not (output / "deployment.json").is_file():
        raise ValueError("Build the site first; distribution output must be its generated directory")
    for managed in ("rpm", "downloads"):
        if (output / managed).exists():
            shutil.rmtree(output / managed)
    # Use pagination so old stable versions remain discoverable when main
    # has produced more than one page of prereleases.
    pages = json.loads(command("gh", "api", "--paginate", "--slurp",
                               f"repos/{REPO}/releases?per_page=100"))
    selected = channels([item for page in pages for item in page])
    output.mkdir(parents=True, exist_ok=True)
    index = {"repository": REPO, "channels": {}}
    verified = {}
    cache = ROOT / "target/distribution-downloads"
    cache.mkdir(parents=True, exist_ok=True)
    public_key = ROOT / "packaging/keys/asagiri-rpm.asc"
    (output / "keys").mkdir(exist_ok=True)
    shutil.copy2(public_key, output / "keys/asagiri-rpm.asc")
    with tempfile.TemporaryDirectory(prefix="asagiri-sign-") as keyring:
        os.chmod(keyring, 0o700)
        previous_keyring = os.environ.get("GNUPGHOME")
        os.environ["GNUPGHOME"] = keyring
        try:
            key = signing_key() if any(selected.values()) else None
            for channel, release in selected.items():
                if release is None:
                    index["channels"][channel] = None
                    continue
                tag = release["tag_name"]
                if tag not in verified:
                    verified[tag] = verified_release(release, cache)
                manifest, assets, folder = verified[tag]
                downloads = {}
                for arch in ("arm64", "x86_64"):
                    dmg = assets[f"Asagiri-{manifest['version']}-{arch}.dmg"]
                    archive = assets[f"Asagiri-{manifest['version']}-{arch}.tar.gz"]
                    downloads["macos-" + arch] = {
                        "url": dmg["browser_download_url"], "sha256": dmg["digest"][7:],
                        "archive_url": archive["browser_download_url"], "archive_sha256": archive["digest"][7:],
                    }
                    metadata = output / "downloads" / channel
                    metadata.mkdir(parents=True, exist_ok=True)
                    (metadata / f"macos-{arch}.txt").write_text(
                        f"version {manifest['version']}\nurl {archive['browser_download_url']}\n"
                        f"sha256 {archive['digest'][7:]}\n")
                for arch in ("x86_64", "aarch64"):
                    dest = output / "rpm" / channel / arch
                    rpm_repo(release, manifest, assets, folder, dest, key[1], key[0])
                    name = f"asagiri-{manifest['version'].replace('-', '~', 1)}-1.{arch}.rpm"
                    downloads["linux-" + arch] = {
                        "url": f"{SITE}/rpm/{channel}/{arch}/Packages/{name}",
                        "sha256": sha256(dest / "Packages" / name),
                    }
                index["channels"][channel] = {
                    "version": manifest["version"], "tag": tag, "prerelease": release["prerelease"],
                    "commit": manifest["commit"], "release_url": release["html_url"], "downloads": downloads,
                }
        finally:
            if previous_keyring is None:
                os.environ.pop("GNUPGHOME", None)
            else:
                os.environ["GNUPGHOME"] = previous_keyring
    (output / "releases.json").write_text(json.dumps(index, indent=2) + "\n")
    print("Verified download index and signed RPM repositories:", output)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    build(parser.parse_args().output.resolve())
