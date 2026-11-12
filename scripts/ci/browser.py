#!/usr/bin/env python3
"""Prepare a verified Chrome for Testing download for Linux CI browser checks."""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import platform
import re
import shlex
import shutil
import stat
from urllib.request import urlopen
import zipfile


def verified(archive, metadata):
    if not archive.is_file() or archive.stat().st_size != metadata["size"]:
        return False
    with archive.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest() == metadata["sha256"]


def download(archive, metadata):
    if verified(archive, metadata):
        print("Using the verified browser download cache", flush=True)
        return
    archive.parent.mkdir(parents=True, exist_ok=True)
    temporary = archive.with_suffix(".partial")
    try:
        with urlopen(metadata["url"], timeout=60) as response, temporary.open("wb") as output:
            total = 0
            while block := response.read(1024 * 1024):
                total += len(block)
                if total > metadata["size"]:
                    raise ValueError("Browser download exceeds its pinned size")
                output.write(block)
        if not verified(temporary, metadata):
            raise ValueError("Browser download does not match its pinned checksum and size")
        temporary.replace(archive)
    finally:
        temporary.unlink(missing_ok=True)


def extract(archive, destination):
    with zipfile.ZipFile(archive) as bundle:
        # Validate every member before writing anything; caches never supply executable state.
        for member in bundle.infolist():
            path = PurePosixPath(member.filename)
            mode = member.external_attr >> 16
            if (path.is_absolute() or ".." in path.parts or "\\" in member.filename or
                    stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR)):
                raise ValueError(f"Unsafe browser archive member: {member.filename}")
        for member in bundle.infolist():
            path = Path(bundle.extract(member, destination))
            if not member.is_dir():
                path.chmod(0o755 if (member.external_attr >> 16) & 0o111 else 0o644)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", required=True, type=Path)
    parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "amd64"):
        parser.error("This pinned browser is for Linux x64 CI only")
    metadata = json.loads(Path(__file__).with_name("browser.json").read_text())
    version = metadata["version"]
    if (not re.fullmatch(r"\d+\.\d+\.\d+\.\d+", version) or
            metadata["url"] != f"https://storage.googleapis.com/chrome-for-testing-public/{version}/linux64/chrome-headless-shell-linux64.zip" or
            not re.fullmatch(r"[0-9a-f]{64}", metadata["sha256"]) or
            not 0 < metadata["size"] < 250_000_000 or
            metadata["executable"] != "chrome-headless-shell-linux64/chrome-headless-shell"):
        raise ValueError("Invalid browser download manifest")
    archive = args.cache / "chrome.zip"
    download(archive, metadata)
    directory = args.directory.resolve()
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    extract(archive, directory)
    executable = directory / metadata["executable"]
    if not executable.is_file() or not executable.stat().st_mode & stat.S_IXUSR:
        raise ValueError("Verified archive is missing the browser executable")
    wrapper = directory / "chrome"
    # The runner is already an isolated unprivileged container. Chromium's own
    # namespace sandbox and its default small shared-memory mount are unavailable.
    wrapper.write_text("#!/bin/sh\nexec " + shlex.quote(str(executable)) +
                       ' --no-sandbox --disable-dev-shm-usage "$@"\n')
    wrapper.chmod(0o755)
    print(f"Prepared Chrome for Testing {version}: {wrapper}", flush=True)


if __name__ == "__main__":
    main()
