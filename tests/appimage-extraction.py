#!/usr/bin/env python3
"""Compile actual runtime helpers and exercise owned temporary filesystem state.

No AppImage, GUI, daemon, mount, network request or privilege prompt is run.
Full packaged-runtime checks are separate in appimage.py.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("runtime_source_archive", type=Path)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
packaging = root / "packaging/linux"
build = json.loads((packaging / "appimage-runtime-build.json").read_text())
sources = json.loads((packaging / "appimage-sources.json").read_text())
source = next(x for x in sources["sources"] if x["component"] == "type2-runtime")
assert hashlib.sha256(args.runtime_source_archive.read_bytes()).hexdigest() == source["sha256"]
record = {
    "status": "running",
    "scope": "pure native C filesystem tests of actual source helpers; packaged AppImage and privilege flow are separate",
    "runtime_source_sha256": source["sha256"],
    "patched_runtime_sha256": build["patched_runtime_sha256"],
    "source_sha256": {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in [
            packaging / "appimage-runtime.patch", packaging / "appimage-extraction.h",
            root / "tests/appimage-extraction.c", Path(__file__),
        ]
    },
    "checks": [],
}
args.output.parent.mkdir(parents=True, exist_ok=True)


def remove_test_directories(output, fixture):
    """Remove only directories whose identities the C fixture recorded."""
    assert shutil.rmtree.avoids_symlink_attacks
    owned = []
    for line in output.splitlines():
        if not line.startswith("OWNED\t"):
            continue
        _, name, device, inode, owner = line.split("\t")
        path = Path(name)
        assert path.is_absolute() and path.parent in (fixture, Path("/tmp")), name
        assert re.fullmatch(r"asagiri-appimage-[a-zA-Z0-9]{6}(?:-retained)?", path.name), name
        assert int(owner) == os.geteuid(), name
        owned.append((path, int(device), int(inode), int(owner)))
    for path, device, inode, owner in owned:
        try:
            actual = path.lstat()
        except FileNotFoundError:
            continue
        if (actual.st_dev, actual.st_ino, actual.st_uid) != (device, inode, owner):
            # A replacement has its own separately recorded identity.
            continue
        assert stat.S_ISDIR(actual.st_mode) and stat.S_IMODE(actual.st_mode) == 0o700, path
        shutil.rmtree(path)
    assert all(not path.exists() and not path.is_symlink() for path, *_ in owned)


try:
    with tempfile.TemporaryDirectory(prefix="asagiri-extraction-source-") as temporary:
        directory = Path(temporary).resolve()
        with tarfile.open(args.runtime_source_archive) as archive:
            # This pinned runtime archive contains only directories and files.
            # Copy without archive ownership; works with the pinned Python 3.11.
            for member in archive:
                name = PurePosixPath(member.name)
                assert not name.is_absolute() and ".." not in name.parts
                path = directory / member.name
                if member.isdir():
                    path.mkdir(parents=True, exist_ok=True)
                else:
                    assert member.isfile()
                    path.parent.mkdir(parents=True, exist_ok=True)
                    with archive.extractfile(member) as source_file, path.open("xb") as output_file:
                        shutil.copyfileobj(source_file, output_file)
                    path.chmod(member.mode & 0o777)
        runtime = directory / ("type2-runtime-" + sources["runtime_commit"])
        subprocess.run([
            "patch", "--batch", "--forward", "-p1", "-i",
            str(packaging / "appimage-runtime.patch"),
        ], cwd=runtime, check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        text = (runtime / "src/runtime/runtime.c").read_text()
        assert hashlib.sha256(text.encode()).hexdigest() == build["patched_runtime_sha256"]
        cleanup = text[text.index("int rm_recursive_callback("):text.index("void build_mount_point(")]
        original_header = (packaging / "appimage-extraction.h").read_text()
        cases = [
            ("actual-source", original_header, cleanup, 0),
            ("negative-root-tmpdir", original_header.replace(
                "!elevated && requested && requested[0]", "requested && requested[0]"), cleanup, 1),
            ("negative-replaced-directory", original_header, cleanup.replace(
                "asagiri_owned_extraction(extraction) && rm_recursive(extraction->path)",
                "rm_recursive(extraction->path)"), 1),
            ("negative-unvalidated-apprun", original_header.replace(
                "return valid;", "return true;"), cleanup, 1),
        ]
        for name, header, implementation, expected in cases:
            case = directory / name
            case.mkdir(mode=0o700)
            (case / "appimage-extraction.h").write_text(header)
            (case / "appimage-cleanup-under-test.h").write_text(implementation)
            command = [
                os.environ.get("CC", "cc"), "-std=gnu99", "-D_GNU_SOURCE",
                "-Wall", "-Werror", "-Wno-unused-parameter", "-I", str(case),
                str(root / "tests/appimage-extraction.c"), "-o", str(case / "checks"),
            ]
            compiled = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=60)
            compile_log = args.output.with_name(args.output.stem + "-" + name + "-compile.txt")
            compile_log.write_text(compiled.stdout)
            assert compiled.returncode == 0, compiled.stdout
            fixture = case / "files"
            fixture.mkdir(mode=0o700)
            log = args.output.with_name(args.output.stem + "-" + name + ".txt")
            output = ""
            try:
                result = subprocess.run([str(case / "checks"), str(fixture)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=15)
                output = result.stdout
            except subprocess.TimeoutExpired as error:
                output = error.stdout or ""
                if isinstance(output, bytes):
                    output = output.decode(errors="replace")
                raise
            finally:
                try:
                    log.write_text(output)
                finally:
                    remove_test_directories(output, fixture)
            entry = {
                "name": name, "command": command, "compile_exit": compiled.returncode,
                "exit": result.returncode, "expected_exit": expected,
                "checks_passed": result.stdout.count("PASS: "),
                "log": str(log.resolve()), "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest(),
                "compile_log": str(compile_log.resolve()),
                "compile_log_sha256": hashlib.sha256(compile_log.read_bytes()).hexdigest(),
                "header_sha256": hashlib.sha256(header.encode()).hexdigest(),
                "cleanup_sha256": hashlib.sha256(implementation.encode()).hexdigest(),
            }
            record["checks"].append(entry)
            assert result.returncode == expected, result.stdout
        record["status"] = "passed"
finally:
    args.output.write_text(json.dumps(record, indent=2) + "\n")
print(json.dumps({"status": record["status"], "checks": len(record["checks"])}, indent=2))
