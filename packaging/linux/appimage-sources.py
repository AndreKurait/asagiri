#!/usr/bin/env python3
"""Stage pinned AppImage runtime source archives and their license notices."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("cache", type=Path)
parser.add_argument("destination", type=Path)
args = parser.parse_args()
here = Path(__file__).resolve().parent
manifest_path = here / "appimage-sources.json"
manifest = json.loads(manifest_path.read_text())
build_path = here / "appimage-runtime-build.json"
build = json.loads(build_path.read_text())
args.cache.mkdir(parents=True, exist_ok=True)
source_dir = args.destination / "sources"
license_dir = args.destination / "licenses"
source_dir.mkdir(parents=True, exist_ok=True)
license_dir.mkdir(parents=True, exist_ok=True)
for source in manifest["sources"] + build["tools"]:
    name, digest = source["name"], source["sha256"]
    assert Path(name).name == name and re.fullmatch(r"[a-zA-Z0-9_.-]+", name)
    assert re.fullmatch(r"[a-f0-9]{64}", digest)
    assert source["url"].startswith("https://")
    cached = args.cache / name
    assert not cached.is_symlink(), "Source cache entries must be regular files"
    if not cached.exists():
        with tempfile.NamedTemporaryFile(dir=args.cache, prefix=name + ".", suffix=".part", delete=False) as file:
            temporary = Path(file.name)
        try:
            subprocess.run([
                "curl", "-fsSL", "--proto", "=https", "--proto-redir", "=https",
                "--retry", "3", "--max-filesize", "33554432", "-o", str(temporary), source["url"],
            ], check=True)
            assert hashlib.sha256(temporary.read_bytes()).hexdigest() == digest, name
            temporary.replace(cached)
        finally:
            temporary.unlink(missing_ok=True)
    assert cached.is_file() and cached.stat().st_size <= 32 * 1024 * 1024
    assert hashlib.sha256(cached.read_bytes()).hexdigest() == digest, name
    shutil.copyfile(cached, source_dir / name)
    with tarfile.open(cached) as archive:
        for member_name in source["licenses"]:
            member = archive.getmember(member_name)
            assert member.isfile() and 0 < member.size <= 1024 * 1024
            contents = archive.extractfile(member).read()
            assert len(contents) == member.size
            component = source["component"]
            assert re.fullmatch(r"[a-zA-Z0-9-]+", component)
            (license_dir / (component + "-" + Path(member_name).name)).write_bytes(contents)
shutil.copyfile(manifest_path, args.destination / manifest_path.name)
shutil.copyfile(build_path, args.destination / build_path.name)
for name, expected in build["inputs"].items():
    assert Path(name).name == name
    path = here / name
    assert path.is_file() and not path.is_symlink()
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected, name
    shutil.copyfile(path, args.destination / name)
shutil.copyfile(here / "appimage-runtime-source.md", args.destination / "README.md")
