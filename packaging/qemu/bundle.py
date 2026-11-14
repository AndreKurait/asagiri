#!/usr/bin/env python3
"""Verify and install the source-built Intel guest emulator without running it."""
import argparse
import csv
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import tarfile
import tempfile

RECIPE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("qemu_build", RECIPE / "build.py")
build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build)
FILES = {"qemu-aarch64", "COPYING", "README.md", "SOURCE.md", "manifest.csv", "build.json", "SHA256SUMS"}
LIMIT = 128 << 20


def relative(name):
    path = PurePosixPath(name)
    build.require(not path.is_absolute() and ".." not in path.parts and "\\" not in name,
                  "Unsafe emulator bundle path")
    return path


def verify(directory):
    build.require(directory.is_dir() and not directory.is_symlink(), "Expected an emulator directory")
    entries = list(directory.rglob("*"))
    build.require(len(entries) <= 1024
                  and all(not p.is_symlink() and (p.is_file() or p.is_dir()) for p in entries),
                  "Unexpected emulator bundle entries")
    files = {p.relative_to(directory).as_posix(): p for p in entries if p.is_file()}
    build.require(FILES <= files.keys() and sum(p.stat().st_size for p in files.values()) <= LIMIT,
                  "Incomplete or oversized emulator bundle")
    build.require(all(name in FILES or name.startswith("licenses/") for name in files),
                  "Unexpected emulator bundle file")
    checksums = {}
    for line in (directory / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split("  ", 1)
        relative(name)
        build.require(re.fullmatch(r"[a-f0-9]{64}", digest) and name not in checksums,
                      "Invalid or duplicate emulator checksum")
        checksums[name] = digest
    build.require(checksums.keys() == files.keys() - {"SHA256SUMS"}, "Incomplete emulator checksums")
    for name, digest in checksums.items():
        build.require(build.sha(files[name]) == digest, "Emulator bundle checksum mismatch: " + name)
    record = json.loads((directory / "build.json").read_text())
    build.require(record["sources"] == build.PINS
                  and record["recipe"] == {name: build.sha(RECIPE / name) for name in build.RECIPE_FILES},
                  "Emulator was built with different source pins or recipe")
    build.require(build.sha(directory / "qemu-aarch64") == record["emulator_sha256"],
                  "Emulator differs from its source build record")
    build.require((directory / "qemu-aarch64").stat().st_mode & 0o7777 == 0o755,
                  "Emulator must have ordinary executable permissions")
    build.static_x86_64(directory / "qemu-aarch64")
    with (directory / "manifest.csv").open(newline="") as stream:
        packages = list(csv.DictReader(stream))
    build.require(any(row["PACKAGE"] == "qemu" and row["VERSION"] == build.PINS["qemu"]["version"]
                      for row in packages), "Missing emulator license manifest")
    for row in packages:
        build.require(row["LICENSE"] and "unknown" not in row["LICENSE"].lower()
                      and row["LICENSE FILES"], "Missing target license")
        for name in row["LICENSE FILES"].split():
            path = PurePosixPath("licenses", row["PACKAGE"] + "-" + row["VERSION"], name)
            relative(path.as_posix())
            build.require(path.as_posix() in files, "Missing target license text")
    return record


def install(products, destination):
    build.verify(products)
    outer = json.loads((products / "build.json").read_text())
    if destination.exists():
        current = verify(destination)
        build.require(current["emulator_sha256"] == outer["emulator_sha256"],
                      "The existing emulator differs; use a fresh destination")
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".qemu-", dir=destination.parent) as temporary:
        staging = Path(temporary)
        seen, size = set(), 0
        with tarfile.open(products / "asagiri-qemu-guest.tar.gz") as archive:
            for member in archive:
                path = relative(member.name)
                if path.as_posix() == ".":
                    build.require(member.isdir(), "Unexpected bundle root")
                    continue
                build.require(path.parts[0] == "qemu" and path.as_posix() not in seen,
                              "Unexpected or duplicate emulator archive path")
                seen.add(path.as_posix())
                build.require(len(seen) <= 1024 and (member.isdir() or member.isfile()),
                              "Unsafe emulator archive entry")
                target = staging.joinpath(*path.parts)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                size += member.size
                build.require(size <= LIMIT, "Oversized emulator archive")
                target.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(member) as source, target.open("xb") as output:
                    shutil.copyfileobj(source, output)
                target.chmod(0o755 if path.name == "qemu-aarch64" else 0o644)
        record = verify(staging / "qemu")
        build.require(record == {key: value for key, value in outer.items() if key != "artifacts"},
                      "Emulator bundle and complete-source records differ")
        os.rename(staging / "qemu", destination)
    print("Installed the verified static emulator, source record and target licenses")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    add = commands.add_parser("install")
    add.add_argument("--products", type=Path, required=True)
    add.add_argument("--destination", type=Path, required=True)
    check = commands.add_parser("verify")
    check.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "install":
        install(args.products.resolve(), args.destination.absolute())
    else:
        verify(args.directory.absolute())
        print("Verified the static emulator and bundled licenses without execution")


if __name__ == "__main__":
    main()
