#!/usr/bin/env python3
"""Bind the Mac app, its executable, and its update archive to one release."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path, PurePosixPath
import plistlib
import re
import subprocess
import tarfile


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def metadata(info, version):
    match = re.fullmatch(r"(\d+\.\d+\.\d+)(?:-dev\.\d+)?", version)
    if not match:
        raise ValueError("Invalid release version")
    expected = {
        "CFBundleIdentifier": "dev.asagiri.app",
        "CFBundleExecutable": "asagiri",
        "CFBundleShortVersionString": match[1],
        "CFBundleVersion": match[1],
        "AsagiriVersion": version,
    }
    for key, value in expected.items():
        if info.get(key) != value:
            raise ValueError(f"{key} does not identify release {version}: {info.get(key)!r}")


def output(command):
    return subprocess.check_output(command, text=True, timeout=20).strip()


def verify_archive(archive, expected):
    with tarfile.open(archive, "r:gz") as tar:
        seen = {}
        paths = {}
        identity_names = {name.casefold(): name for name in expected}
        for member in tar.getmembers():
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("The update archive contains an unsafe archive path")
            # tar accepts ./ and repeated separators; a default Mac filesystem
            # also treats differently cased names as the same destination.
            name = path.as_posix()
            key = name.casefold()
            if key in paths or (key in identity_names and name != identity_names[key]):
                raise ValueError("The update archive repeats or redirects a path")
            paths[key] = member.isdir()
            if name in expected:
                if not member.isfile():
                    raise ValueError("The update archive repeats or redirects an identity file")
                with tar.extractfile(member) as stream:
                    seen[name] = hashlib.file_digest(stream, "sha256").hexdigest()
        # A link at any ancestor can redirect later writes through an alias.
        # Check all entries after reading, since the redirect may come last.
        for name in paths:
            if any(paths.get(parent.as_posix()) is False for parent in PurePosixPath(name).parents):
                raise ValueError("The update archive repeats or redirects a path")
        if seen != expected:
            raise ValueError("The update archive does not contain the verified app identity")


def verify(app, archive, version, arch):
    if arch not in ("arm64", "x86_64"):
        raise ValueError("Unsupported Mac architecture")
    plist = app / "Contents/Info.plist"
    executable = app / "Contents/MacOS/asagiri"
    metadata(plistlib.loads(plist.read_bytes()), version)
    if output(["/usr/bin/lipo", "-archs", str(executable)]).split() != [arch]:
        raise ValueError("The packaged executable has the wrong architecture")
    reported = output([str(executable), "--version"])
    if reported != f"Asagiri {version}":
        raise ValueError(f"The packaged executable reports a different version: {reported!r}")
    expected = {
        "Asagiri.app/Contents/Info.plist": digest(plist),
        "Asagiri.app/Contents/MacOS/asagiri": digest(executable),
    }
    verify_archive(archive, expected)
    return {
        "schema": 1, "version": version, "architecture": arch,
        "cli_version": reported, "identity_files": expected,
        "archive_sha256": digest(archive),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--arch", choices=("arm64", "x86_64"), required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = verify(args.app, args.archive, args.version, args.arch)
    result["verified_at"] = datetime.now(timezone.utc).isoformat()
    text = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text)
    print(text, end="")


if __name__ == "__main__":
    main()
