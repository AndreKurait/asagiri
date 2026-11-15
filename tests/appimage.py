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
import subprocess
import tempfile
import time
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("appimage", type=Path)
parser.add_argument("binaries", type=Path)
parser.add_argument("window", type=Path)
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
    return subprocess.check_output(command, stderr=subprocess.STDOUT, timeout=60, **kwargs)


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
        expected_version = run(str(args.binaries.resolve() / "asagiri"), "--version").decode().strip()
        assert version in expected_version
        env = dict(os.environ, APPIMAGE_EXTRACT_AND_RUN="1")
        record["cli_version"] = run(str(image), "--version", env=env).decode().strip()
        assert record["cli_version"] == expected_version
        assert run(str(app / "usr/bin/asagiri-window"), "--version").decode().strip() == expected_version
        record["window_libraries"] = run("ldd", str(app / "usr/bin/asagiri-window")).decode()
        assert "not found" not in record["window_libraries"]
        record["runtime_version"] = run(str(image), "--appimage-version").decode().strip()
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
                        break
                    assert time.monotonic() < deadline, (scratch / "gui.log").read_text()
                    time.sleep(.1)
                assert not Path("/run/asagiri.sock").exists()
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
        record["status"] = "passed"
except BaseException as error:
    record["status"] = "failed"
    record["error"] = repr(error)
    raise
finally:
    record["completed_at"] = datetime.now(timezone.utc).isoformat()
    args.output.write_text(json.dumps(record, indent=2) + "\n")
print(json.dumps(record, indent=2))
