#!/usr/bin/env python3
"""Inspect the real AppImage and exercise CLI extraction and headless GTK launch."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import struct
import subprocess
import tempfile
import time
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("appimage", type=Path)
parser.add_argument("binaries", type=Path)
parser.add_argument("window", type=Path)
parser.add_argument("--appimagetool", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
image = args.appimage.resolve()
version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
arch = os.uname().machine
assert image.name == f"Asagiri-{version}-{arch}.AppImage"
assert os.environ.get("DISPLAY"), "Run the GTK check under a disposable X server"
assert Path("/.dockerenv").exists(), "Use the private AppImage build/test container"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(*command, **kwargs):
    kwargs.setdefault("timeout", 60)
    return subprocess.check_output(command, stderr=subprocess.STDOUT, **kwargs)


source_record = args.output.parent / "appimage-build-source.json"
source = json.loads(source_record.read_text())
for name, expected in source["files"].items():
    assert not Path(name).is_absolute() and ".." not in Path(name).parts
    assert digest(root / name) == expected, name
record = {
    "started_at": datetime.now(timezone.utc).isoformat(),
    "source": source["source"], "source_tree": source["source_tree"],
    "build_source_sha256": digest(source_record),
    "version": version, "architecture": arch, "appimage_sha256": digest(image),
    "build_image": json.loads((root / "packaging/linux/appimage-build.json").read_text())["image"],
    "gui_test": "private X server; software rendering; WebKit sandbox disabled only in this test process",
    "status": "running",
}
assert (args.binaries / "SOURCE_COMMIT").read_text().strip() == record["source"]
try:
    with tempfile.TemporaryDirectory(prefix="asagiri-appimage-test-") as directory:
        scratch = Path(directory)
        run(str(image), "--appimage-extract", cwd=scratch)
        app = scratch / "squashfs-root"
        for path in app.rglob("*"):
            mode = path.lstat().st_mode
            assert stat.S_ISREG(mode) or stat.S_ISDIR(mode) or stat.S_ISLNK(mode), path
            assert not mode & (stat.S_ISUID | stat.S_ISGID), path
            if path.is_symlink():
                assert path.resolve().is_relative_to(app), path
        expected = {
            "asagiri": args.binaries / "asagiri",
            "asagiri-init": args.binaries / "asagiri-init",
            "asagiri-window": args.window,
        }
        for name, path in expected.items():
            installed = app / "usr/bin" / name
            assert installed.is_file() and not installed.is_symlink()
            assert installed.stat().st_mode & 0o7777 == 0o755
            assert digest(installed) == digest(path), name
        record["binaries"] = {name: digest(path) for name, path in expected.items()}
        run("desktop-file-validate", str(app / "asagiri.desktop"))
        for name in ("LICENSE", "NOTICE"):
            assert digest(app / "usr/share/doc/asagiri" / name) == digest(root / name)
        bundled = app / "usr/share/doc/asagiri/appimage-runtime"
        manifest = json.loads((bundled / "appimage-sources.json").read_text())
        assert manifest == json.loads((root / "packaging/linux/appimage-sources.json").read_text())
        for source in manifest["sources"]:
            assert digest(bundled / "sources" / source["name"]) == source["sha256"]
            for license_path in source["licenses"]:
                assert (bundled / "licenses" / (source["component"] + "-" + Path(license_path).name)).stat().st_size
        record["runtime_source_archives"] = len(manifest["sources"])
        build_manifest_path = bundled / "appimage-runtime-build.json"
        build_manifest = json.loads(build_manifest_path.read_text())
        assert build_manifest == json.loads((root / "packaging/linux/appimage-runtime-build.json").read_text())
        assert build_manifest["compiler_image"] == record["build_image"]
        for source in build_manifest["tools"]:
            assert digest(bundled / "sources" / source["name"]) == source["sha256"]
            for license_path in source["licenses"]:
                assert (bundled / "licenses" / (source["component"] + "-" + Path(license_path).name)).stat().st_size
        record["runtime_tool_source_archives"] = len(build_manifest["tools"])
        assert record["runtime_source_archives"] == 7 and record["runtime_tool_source_archives"] == 2
        for name, expected_hash in build_manifest["inputs"].items():
            assert digest(bundled / name) == expected_hash == digest(root / "packaging/linux" / name)
        assert digest(bundled / "README.md") == digest(root / "packaging/linux/appimage-runtime-source.md")
        build = json.loads((bundled / "runtime-build.json").read_text())
        assert build["schema"] == "asagiri.appimage-runtime-build.v1" and build["status"] == "passed"
        assert build["architecture"] == arch and build["compiler_image"] == record["build_image"]
        assert build["runtime_source"] == manifest["runtime_commit"]
        assert build["inputs"] == build_manifest["inputs"]
        assert build["patched_runtime_sha256"] == build_manifest["patched_runtime_sha256"]
        assert build["runtime_version"] == build_manifest["version"]
        assert build["elf_type"] == "static-pie" and build["elf_interpreter"] is None
        assert build["elf_needed"] == [] and build["libc_static_pie_smoke"] == "passed"
        assert build["build_tools"] == {x["name"]: x["sha256"] for x in build_manifest["tools"]}
        assert build["linked_source_archives"] == {x["name"]: x["sha256"] for x in manifest["sources"]}
        offset = int(run(str(image), "--appimage-offset").strip())
        prefix = bytearray(image.read_bytes()[:offset])
        assert prefix[:6] == b"\x7fELF\x02\x01" and prefix[8:11] == b"AI\x02"
        assert struct.unpack_from("<H", prefix, 16)[0] == 3
        program_offset = struct.unpack_from("<Q", prefix, 32)[0]
        program_size, program_count = struct.unpack_from("<HH", prefix, 54)
        assert all(struct.unpack_from("<I", prefix, program_offset + i * program_size)[0] != 3
                   for i in range(program_count)), "Runtime has an ELF interpreter"
        section_offset = struct.unpack_from("<Q", prefix, 40)[0]
        section_size, section_count, strings_index = struct.unpack_from("<HHH", prefix, 58)
        assert offset == section_offset + section_size * section_count
        sections = [struct.unpack_from("<IIQQQQIIQQ", prefix, section_offset + i * section_size)
                    for i in range(section_count)]
        for section in sections:
            if section[1] == 6:
                assert section[5] % 16 == 0
                assert all(struct.unpack_from("<q", prefix, i)[0] != 1
                           for i in range(section[4], section[4] + section[5], 16)), "Runtime needs a shared library"
        strings = prefix[sections[strings_index][4]:sections[strings_index][4] + sections[strings_index][5]]
        md5_sections = [x for x in sections if strings[x[0]:].split(b"\0", 1)[0] == b".digest_md5"]
        assert len(md5_sections) == 1
        md5_offset, md5_size = md5_sections[0][4:6]
        assert build["packaging_md5_section"] == {
            "offset": md5_offset, "size": 16, "initial_bytes": bytes(16).hex(),
        } and md5_size == 16 and md5_offset + 16 <= offset
        record["runtime_prefix_sha256"] = hashlib.sha256(prefix).hexdigest()
        record["runtime_prefix_size"] = offset
        # appimagetool writes only this reserved 16-byte data section. Bind the
        # full unmodified runtime after restoring exactly its original bytes.
        prefix[md5_offset:md5_offset + 16] = bytes(16)
        record["runtime_normalized_prefix_sha256"] = hashlib.sha256(prefix).hexdigest()
        assert record["runtime_normalized_prefix_sha256"] == build["runtime_sha256"]
        record["runtime_build"] = build
        record["runtime_build_sha256"] = digest(bundled / "runtime-build.json")
        record["runtime_build_manifest_sha256"] = digest(build_manifest_path)
        record["runtime_source_files"] = {
            str(path.relative_to(bundled)): digest(path)
            for path in sorted(bundled.rglob("*")) if path.is_file()
        }
        # Retain the source-level ownership controls and actual packaged-runtime
        # isolation/lifetime/signal controls as independently hashed raw evidence.
        controls_path = args.output.with_name("appimage-extraction-controls.json")
        runtime_source = next(x for x in manifest["sources"] if x["component"] == "type2-runtime")
        run("python3", str(root / "tests/appimage-extraction.py"),
            str(bundled / "sources" / runtime_source["name"]), "--output", str(controls_path))
        controls = json.loads(controls_path.read_text())
        assert controls["status"] == "passed"
        record["extraction_source_controls"] = {"file": controls_path.name, "sha256": digest(controls_path)}
        runtime_path = args.output.with_name("appimage-runtime-verification.json")
        run("python3", str(root / "tests/appimage-runtime.py"), str(image),
            str(args.appimagetool.resolve()), "--apprun-size", str((app / "AppRun").stat().st_size),
            "--source", record["source"], "--source-tree", record["source_tree"],
            "--output", str(runtime_path), timeout=180)
        runtime_checks = json.loads(runtime_path.read_text())
        assert runtime_checks["status"] == "passed" and runtime_checks["mode"] == "complete"
        assert runtime_checks["appimage_sha256"] == record["appimage_sha256"]
        assert runtime_checks["runtime_prefix_sha256"] == record["runtime_prefix_sha256"]
        record["runtime_extraction_controls"] = {"file": runtime_path.name, "sha256": digest(runtime_path)}
        expected_version = run(str(args.binaries.resolve() / "asagiri"), "--version").decode().strip()
        assert expected_version == f"Asagiri {version}"
        env = dict(os.environ, APPIMAGE_EXTRACT_AND_RUN="1")
        record["cli_version"] = run(str(image), "--version", env=env).decode().strip()
        assert record["cli_version"] == expected_version
        assert run(str(image), "-V", env=env).decode().strip() == expected_version
        assert run(str(app / "usr/bin/asagiri-window"), "--version").decode().strip() == expected_version
        record["window_libraries"] = run("ldd", str(app / "usr/bin/asagiri-window")).decode()
        assert "not found" not in record["window_libraries"]
        record["runtime_version"] = run(str(image), "--appimage-version").decode().strip()
        assert record["runtime_version"] == "AppImage runtime version: " + build_manifest["version"]
        # Use the default launcher, so this also checks AppRun's GUI route.
        # The process has no host engine socket and no access to a user session.
        private_home = scratch / "home"
        private_home.mkdir()
        env.update(HOME=str(private_home), XDG_CONFIG_HOME=str(private_home / ".config"),
                   XDG_CACHE_HOME=str(private_home / ".cache"), LIBGL_ALWAYS_SOFTWARE="1",
                   WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS="1")
        with (scratch / "gui.log").open("wb") as log:
            process = subprocess.Popen([str(image)], env=env, stdout=log, stderr=log,
                                       start_new_session=True)
            try:
                deadline = time.monotonic() + 30
                while True:
                    assert process.poll() is None, (scratch / "gui.log").read_text()
                    found = subprocess.run(
                        ["xdotool", "search", "--onlyvisible", "--name", "^Asagiri$"],
                        capture_output=True, text=True, timeout=5)
                    if found.returncode == 0 and found.stdout.strip():
                        record["gtk_window_visible"] = True
                        window_id = found.stdout.splitlines()[0]
                        break
                    assert time.monotonic() < deadline, (scratch / "gui.log").read_text()
                    time.sleep(.1)
                assert not Path("/run/asagiri.sock").exists()
                window_pid = int(run("xdotool", "getwindowpid", window_id).strip())
                assert os.getpgid(window_pid) == process.pid
                environment = dict(entry.split(b"=", 1) for entry in
                                   Path(f"/proc/{window_pid}/environ").read_bytes().split(b"\0") if b"=" in entry)
                gui_appdir = Path(os.fsdecode(environment[b"APPDIR"]))
                assert gui_appdir.parent == Path("/tmp") and gui_appdir.name.startswith("asagiri-appimage-")
                identity = gui_appdir.lstat()
                assert identity.st_uid == 0 and stat.S_IMODE(identity.st_mode) == 0o700
                assert run(str(image), "--version", env=env).decode().strip() == expected_version
                after = gui_appdir.lstat()
                assert (after.st_dev, after.st_ino) == (identity.st_dev, identity.st_ino)
                assert process.poll() is None
                assert digest(gui_appdir / "usr/bin/asagiri-window") == record["binaries"]["asagiri-window"]
                record["gtk_appdir_preserved_after_cli"] = True
            finally:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=10)
        record["gui_log"] = (scratch / "gui.log").read_text()
        assert not gui_appdir.exists()
        record["gtk_appdir_removed_after_exit"] = True
        record["status"] = "passed"
except BaseException as error:
    record["status"] = "failed"
    record["error"] = repr(error)
    raise
finally:
    record["completed_at"] = datetime.now(timezone.utc).isoformat()
    args.output.write_text(json.dumps(record, indent=2) + "\n")
print(json.dumps(record, indent=2))
