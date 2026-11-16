#!/usr/bin/env python3
"""Bind the Mac app, its executable, and its update archive to one release."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
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
    with tarfile.open(archive, "r:gz") as tar:
        seen = {}
        for member in tar.getmembers():
            if member.name in expected:
                if member.name in seen or not member.isfile():
                    raise ValueError("The update archive repeats or redirects an identity file")
                with tar.extractfile(member) as stream:
                    seen[member.name] = hashlib.file_digest(stream, "sha256").hexdigest()
        if seen != expected:
            raise ValueError("The update archive does not contain the verified app identity")
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
