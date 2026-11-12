#!/usr/bin/env python3
"""Restore pinned build images, verifying their content IDs before use.

Only base images are saved. Containers, engine data, and test results are
never cached. A valid warm cache needs no registry access; a missing or
damaged archive falls back to a pull by the committed manifest digest.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess

MANIFEST = Path(__file__).with_name("images.json")


def docker(*args, check=True):
    return subprocess.run(["docker", *map(str, args)], check=check,
                          text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def present(record, arch):
    # Docker's classic store identifies images by config digest; its
    # containerd store uses the manifest/index digest. Accept only those
    # committed identities, with the requested platform in either case.
    expected = record["config"][arch]
    manifest = record["image"].split("@")[1]
    for reference in (record["image"], expected, manifest):
        result = docker("image", "inspect", reference, "--format", "{{json .}}", check=False)
        if result.returncode:
            continue
        image = json.loads(result.stdout)
        if image.get("Os") != "linux" or image.get("Architecture") != arch:
            continue
        if image.get("Id") == expected:
            return expected
        if image.get("Id") == manifest and image.get("Descriptor", {}).get("digest") == manifest:
            return manifest
    return None


def prepare(name, record, arch, cache):
    expected = record["config"][arch]
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", expected):
        raise ValueError(f"{name}: invalid image content ID")
    if not re.fullmatch(r"[^\s]+@sha256:[0-9a-f]{64}", record["image"]):
        raise ValueError(f"{name}: image must be pinned by manifest digest")
    archive = cache / f"{name}-{arch}-{expected[7:]}.tar"
    available = present(record, arch)
    restored = False
    if not available and archive.exists():
        result = docker("image", "load", "--input", archive, check=False)
        available = present(record, arch) if result.returncode == 0 else None
        restored = bool(available)
        if not available:
            archive.unlink()
            print(f"{name}: discarded invalid cache; pulling the pinned image", flush=True)
    if not available:
        print(f"{name}: pulling {record['image']}", flush=True)
        docker("pull", "--platform", f"linux/{arch}", record["image"])
        available = present(record, arch)
        if not available:
            raise RuntimeError(f"{name}: pulled image does not match its committed content ID")
    else:
        print(f"{name}: {'restored' if restored else 'already present'}, content ID verified", flush=True)
    if not archive.exists():
        temporary = archive.with_suffix(".tmp")
        try:
            docker("image", "save", "--output", temporary, "--platform", f"linux/{arch}", available)
            temporary.replace(archive)
        finally:
            temporary.unlink(missing_ok=True)
    return available


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", choices=("ci", "release"), default="ci")
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--github-env", type=Path)
    args = parser.parse_args()
    platform = json.loads(docker("info", "--format", "{{json .}}").stdout)
    if platform["OSType"] != "linux":
        parser.error("a Linux Docker engine is required")
    arch = {"x86_64": "amd64", "aarch64": "arm64"}.get(platform["Architecture"], platform["Architecture"])
    if arch not in ("amd64", "arm64"):
        parser.error(f"unsupported Docker architecture: {arch}")
    args.cache.mkdir(parents=True, exist_ok=True)
    environment = []
    for name, record in json.loads(MANIFEST.read_text()).items():
        if args.group in record["groups"]:
            image = prepare(name, record, arch, args.cache)
            environment.append(f"{record['env']}={image}\n")
    if args.github_env:
        with args.github_env.open("a") as stream:
            stream.writelines(environment)
    print(f"Verified {len(environment)} base images for linux/{arch}; no test-engine data cached.")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr or str(error)) from error
