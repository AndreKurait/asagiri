#!/usr/bin/env python3
"""Exercise the packaged runtime in an owned Linux container, without FUSE.

The root setup check uses an absent account and stops before any installation.
Its preexisting AppRun fixture is non-executable data, never a test program.
Other fixtures execute a small test AppRun under the actual packaged runtime.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import struct
import subprocess
import tempfile
import time
import uuid

CASES = (
    "privileged-setup-ignores-shared-cache",
    "privileged-setup-ignores-attacker-tmpdir",
    "concurrent-unprivileged-extraction",
    "failed-child-exit-cleanup",
    "signalled-child-waits-and-cleans",
    "cancelled-extraction-cleans-before-launch",
    "writable-apprun-rejected",
    "linked-apprun-rejected",
    "writable-tmpdir-ancestor-rejected",
    "foreign-owned-tmpdir-ancestor-rejected",
)
TOOL_HASHES = {
    "x86_64": "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0",
    "aarch64": "f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158",
}
PREEXISTING_DATA = b"Asagiri extraction isolation fixture; non-executable data.\n"
PADDING_BYTES = 536870912
FIXTURE = b"""#!/usr/bin/python3
import json, os, signal, sys, time
from pathlib import Path
signalled = False
def received(number, frame):
    global signalled
    signalled = True
    path = Path(os.environ["ASAGIRI_TEST_SIGNAL"])
    path.with_suffix(".writing").write_text(str(number))
    path.with_suffix(".writing").replace(path)
signal.signal(signal.SIGTERM, received)
ready = Path(os.environ["ASAGIRI_TEST_READY"])
ready.with_suffix(".writing").write_text(json.dumps({
    "appdir": os.environ["APPDIR"], "uid": os.geteuid(), "pid": os.getpid(),
    "argv": sys.argv[1:], "token": os.environ["ASAGIRI_TEST_TOKEN"],
}))
ready.with_suffix(".writing").replace(ready)
if sys.argv[1:] == ["hold"]:
    while not Path(os.environ["ASAGIRI_TEST_RELEASE"]).exists():
        time.sleep(.01)
    sys.exit(23 if signalled else 0)
if sys.argv[1:] == ["exit"]:
    sys.exit(42)
print("Asagiri runtime fixture")
"""


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def runtime_prefix(image):
    data = image.read_bytes()
    assert data[:6] == b"\x7fELF\x02\x01" and data[8:11] == b"AI\x02"
    offset = struct.unpack_from("<Q", data, 40)[0]
    entry_size, count = struct.unpack_from("<HH", data, 58)
    offset += entry_size * count
    assert data[offset:offset + 4] == b"hsqs"
    return data[:offset]


class Case:
    def __init__(self, name, directory, output):
        self.name, self.directory, self.output = name, directory, output
        directory.mkdir(mode=0o755)
        self.home = directory / "user"
        self.home.mkdir(mode=0o700)
        os.chown(self.home, 65534, 65534)
        self.tmp = self.home / "tmp"
        self.tmp.mkdir(mode=0o700)
        os.chown(self.tmp, 65534, 65534)
        self.token = uuid.uuid4().hex
        self.processes, self.owned, self.children = [], [], []
        self.process_start = {}
        self.record = {"name": name, "status": "running", "commands": [], "cleanup": {}}
        self.env = {key: value for key, value in os.environ.items()
                    if key not in ("TARGET_APPIMAGE", "NO_CLEANUP")}
        self.env.update(APPIMAGE_EXTRACT_AND_RUN="1", TMPDIR=str(self.tmp),
                        ASAGIRI_TEST_TOKEN=self.token, VERBOSE="1",
                        ASAGIRI_TEST_READY=str(self.home / "ready.json"),
                        ASAGIRI_TEST_SIGNAL=str(self.home / "signal"),
                        ASAGIRI_TEST_RELEASE=str(self.home / "release"))

    def spawn(self, command, *, env=None, root=False):
        log = self.output.with_name(self.output.stem + "-" + self.name
                                    + f"-{len(self.processes) + 1}.txt")
        identity = {} if root else {"user": 65534, "group": 65534, "extra_groups": []}
        with log.open("wb") as file:
            process = subprocess.Popen(command, env=env or self.env, stdout=file,
                                       stderr=file, start_new_session=True, **identity)
        self.processes.append((process, log))
        fields = Path(f"/proc/{process.pid}/stat").read_text().rsplit(")", 1)[1].split()
        self.process_start[process.pid] = fields[19]
        self.record["commands"].append({"argv": list(map(str, command)), "uid": 0 if root else 65534,
                                        "log": log.name})
        return process, log

    def command(self, command, *, env=None, root=False):
        process, log = self.spawn(command, env=env, root=root)
        process.wait(timeout=60)
        return process.returncode, log.read_text(errors="replace")

    def send_signal(self, process, number, *, group=False):
        """Signal as the invocation's UID, without adding CAP_KILL to the test."""
        index = next(i for i, (owned, _) in enumerate(self.processes) if owned is process)
        uid = self.record["commands"][index]["uid"]
        anchors = [(process.pid, self.process_start[process.pid])]
        anchors += [(pid, start) for pid, start, pgid in self.children if pgid == process.pid]
        anchor = None
        for pid, start in anchors:
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                if fields[19] == start and fields[0] != "Z":
                    anchor = (pid, start)
                    break
            except FileNotFoundError:
                pass
        if anchor is None:
            return
        # A pidfd binds the individual target; for a process group, the live
        # recorded anchor must still be a member of this invocation's session.
        helper = (
            "import json,os,signal,sys\nfrom pathlib import Path\n"
            "pid,start,number,group=map(int,sys.argv[1:])\n"
            "try:\n"
            " fd=os.pidfd_open(pid)\n"
            "except ProcessLookupError:\n"
            " sys.exit(0)\n"
            "try:\n"
            " fields=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()\n"
            " assert int(fields[19])==start\n"
            " if group:\n"
            "  assert int(fields[2])==group and int(fields[3])==group\n"
            "  os.killpg(group,number)\n"
            " else:\n"
            "  signal.pidfd_send_signal(fd,number)\n"
            " print(json.dumps({'uid':os.geteuid(),'pid':pid,'start':start,'signal':number,'group':group,'sent':True}))\n"
            "except (FileNotFoundError,ProcessLookupError):\n"
            " pass\n"
            "finally:\n"
            " os.close(fd)\n"
        )
        signals = self.record.setdefault("signals", [])
        log = self.output.with_name(self.output.stem + "-" + self.name + f"-signal-{len(signals) + 1}.txt")
        command = ["python3", "-c", helper, str(anchor[0]), anchor[1],
                   str(number), str(process.pid if group else 0)]
        identity = {} if uid == 0 else {"user": uid, "group": uid, "extra_groups": []}
        with log.open("wb") as file:
            result = subprocess.run(command, stdout=file, stderr=file, timeout=10, **identity)
        signals.append({"uid": uid, "target_pid": anchor[0], "target_start": anchor[1],
                        "signal": number, "group": process.pid if group else 0,
                        "command": command, "exit": result.returncode,
                        "log": log.name, "log_sha256": digest(log)})
        assert result.returncode == 0, log.read_text()

    def own(self, path, uid, *, private=True):
        value = path.lstat()
        assert stat.S_ISDIR(value.st_mode) and value.st_uid == uid, str(path)
        if private:
            assert stat.S_IMODE(value.st_mode) == 0o700, str(path)
        self.owned.append((path, value.st_dev, value.st_ino, value.st_uid))
        return {"path": str(path), "device": value.st_dev, "inode": value.st_ino,
                "uid": value.st_uid, "mode": stat.S_IMODE(value.st_mode)}

    def ready(self, process, path=None, *, uid=65534):
        ready = path or self.home / "ready.json"
        deadline = time.monotonic() + 15
        while not ready.exists():
            assert process.poll() is None, "AppRun exited before writing its ready record"
            assert time.monotonic() < deadline, "AppRun ready record timed out"
            time.sleep(.01)
        value = json.loads(ready.read_text())
        assert value["token"] == self.token and value["uid"] == uid
        assert os.getpgid(value["pid"]) == process.pid
        child_stat = Path(f"/proc/{value['pid']}/stat").read_text().rsplit(")", 1)[1].split()
        self.children.append((value["pid"], child_stat[19], process.pid))
        appdir = Path(value["appdir"])
        allowed = Path("/tmp") if uid == 0 else self.tmp
        assert appdir.parent == allowed and re.fullmatch(
            r"(?:asagiri-appimage-[a-zA-Z0-9]{6}|appimage_extracted_[a-f0-9]{32})", appdir.name)
        return value

    def live_child_groups(self):
        live_groups = set()
        for pid, start, group in self.children:
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                if fields[19] == start and int(fields[2]) == group and fields[0] != "Z":
                    live_groups.add(group)
            except FileNotFoundError:
                pass
        return live_groups

    def cleanup(self):
        errors = []
        # Release a live fixture even when an old launcher already exited. Its
        # recorded start time keeps a reused PID from authorizing group cleanup.
        (self.home / "release").touch()
        live_groups = self.live_child_groups()
        # Stop only process groups created by this case, before removing files.
        for process, _ in self.processes:
            try:
                if process.poll() is None or process.pid in live_groups:
                    self.send_signal(process, signal.SIGTERM, group=True)
                    try:
                        process.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        self.send_signal(process, signal.SIGKILL, group=True)
                        process.wait(timeout=10)
            except ProcessLookupError:
                process.wait(timeout=10)
            except BaseException as error:
                errors.append(repr(error))
        # A launcher can exit before its recorded child. Waiting on that
        # Popen alone does not make the child's files safe to remove.
        try:
            for number in (None, signal.SIGKILL):
                live_groups = self.live_child_groups()
                if number is not None:
                    for process, _ in self.processes:
                        if process.pid in live_groups:
                            self.send_signal(process, number, group=True)
                deadline = time.monotonic() + 3
                while self.live_child_groups() and time.monotonic() < deadline:
                    time.sleep(.01)
                if not self.live_child_groups():
                    break
            assert not self.live_child_groups(), "An owned descendant is still using its extracted files"
        except BaseException as error:
            errors.append(repr(error))
        safe_to_remove = not self.live_child_groups() and all(p.poll() is not None for p, _ in self.processes)
        assert shutil.rmtree.avoids_symlink_attacks
        for path, device, inode, owner in self.owned if safe_to_remove else []:
            try:
                try:
                    value = path.lstat()
                except FileNotFoundError:
                    continue
                assert (value.st_dev, value.st_ino, value.st_uid) == (device, inode, owner), str(path)
                assert stat.S_ISDIR(value.st_mode), str(path)
                shutil.rmtree(path)
            except BaseException as error:
                errors.append(repr(error))
        for entry, (process, log) in zip(self.record["commands"], self.processes, strict=True):
            entry.update(exit=process.returncode, log_sha256=digest(log))
        self.record["cleanup"] = {"processes_reaped": all(p.poll() is not None for p, _ in self.processes),
                                  "descendants_exited": not self.live_child_groups(),
                                  "owned_directories_removed": all(not p.exists() for p, *_ in self.owned),
                                  "errors": errors}
        assert not errors and all(self.record["cleanup"][key] for key in
                                  ("processes_reaped", "descendants_exited", "owned_directories_removed")), errors


def privileged_setup(case, image, apprun_size, hostile_tmpdir):
    # Check path isolation with non-executable data. Only the legitimate
    # packaged setup command may execute; its absent account prevents install.
    image_md5 = hashlib.md5(image.read_bytes(), usedforsecurity=False).hexdigest()
    parent = case.directory / "attacker"
    parent.mkdir(mode=0o777)
    os.chmod(parent, 0o777)
    os.chown(parent, 65534, 65534)
    cache = (parent if hostile_tmpdir else Path("/tmp")) / ("appimage_extracted_" + image_md5)
    cache.mkdir(mode=0o755)  # exclusive: never borrow or overwrite another cache
    case.own(cache, 0, private=False)
    os.chown(cache, 65534, 65534)
    path, device, inode, _ = case.owned[-1]
    case.owned[-1] = (path, device, inode, 65534)
    payload = PREEXISTING_DATA
    assert len(payload) < apprun_size
    payload += b"x" * (apprun_size - len(payload))
    (cache / "AppRun").write_bytes(payload)
    (cache / "AppRun").chmod(0o644)
    os.chown(cache / "AppRun", 65534, 65534)
    before = (cache / "AppRun").lstat()
    identity = {"device": before.st_dev, "inode": before.st_ino, "uid": before.st_uid,
                "mode": stat.S_IMODE(before.st_mode), "size": before.st_size}
    assert stat.S_ISREG(before.st_mode) and identity["mode"] == 0o644
    assert before.st_size == apprun_size and before.st_uid == 65534
    env = dict(case.env, NO_CLEANUP="1")
    if hostile_tmpdir:
        env["TMPDIR"] = str(parent)
    else:
        env.pop("TMPDIR", None)
    user = "asagiri-absent-" + case.token
    code, output = case.command([str(image), "--appimage-extract-and-run", "host-access", user],
                                env=env, root=True)
    paths = set(re.findall(r"(?m)^(/tmp/asagiri-appimage-[a-zA-Z0-9]{6})/AppRun$", output))
    for path in paths:
        if Path(path).exists():
            case.own(Path(path), 0)
    case.record.update(preseeded_size=apprun_size, preseeded_sha256=hashlib.sha256(payload).hexdigest(),
                       preseeded_uid=65534, preseeded_kind="non-executable-data",
                       preseeded_identity=identity, setup_exit=code,
                       legitimate_setup_reached=f"no account named {user}" in output,
                       validation_scope="path isolation and unchanged non-executable data")
    assert code == 1 and f"no account named {user}" in output, output
    assert digest(cache / "AppRun") == hashlib.sha256(payload).hexdigest()
    after = (cache / "AppRun").lstat()
    after_identity = {"device": after.st_dev, "inode": after.st_ino, "uid": after.st_uid,
                      "mode": stat.S_IMODE(after.st_mode), "size": after.st_size}
    assert identity == after_identity
    assert len(paths) == 1, output
    assert all(not Path(path).exists() for path in paths), "Root extraction survived child exit"
    case.record.update(privileged_tmpdir="/tmp",
                       root_no_cleanup_ignored=True, runtime_directories_removed=True,
                       preseeded_file_unchanged=True, preseeded_identity_after=after_identity,
                       preseeded_sha256_after=digest(cache / "AppRun"))


def run_fixture(case, fixture, mode):
    process, _ = case.spawn([str(fixture), mode])
    ready = case.ready(process)
    appdir = Path(ready["appdir"])
    # Preserve the old runtime's directory too if the assertion below fails.
    identity = case.own(appdir, 65534, private=False)
    assert os.getpgid(ready["pid"]) == process.pid
    return process, appdir, identity, ready


def concurrent(case, fixture):
    first, appdir, identity, _ = run_fixture(case, fixture, "hold")
    before = digest(appdir / "AppRun")
    second_ready = case.home / "second.json"
    code, output = case.command([str(fixture), "--version"],
                                env=dict(case.env, ASAGIRI_TEST_READY=str(second_ready)))
    assert code == 0 and "Asagiri runtime fixture" in output, output
    second = json.loads(second_ready.read_text())
    assert second["token"] == case.token and second["uid"] == 65534 and second["argv"] == ["--version"]
    if Path(second["appdir"]).exists() and second["appdir"] != str(appdir):
        assert Path(second["appdir"]).parent == case.tmp
        case.own(Path(second["appdir"]), 65534, private=False)
    case.record.update(first_identity=identity, second_appdir=second["appdir"],
                       first_alive_after_second=first.poll() is None,
                       first_exists_after_second=appdir.exists())
    assert first.poll() is None and appdir.exists(), "The second launch removed the first APPDIR"
    value = appdir.lstat()
    assert (value.st_dev, value.st_ino) == (identity["device"], identity["inode"])
    assert second["appdir"] != str(appdir), "A second invocation reused a live APPDIR"
    assert not Path(second["appdir"]).exists(), "Completed extraction was not removed"
    assert digest(appdir / "AppRun") == before and stat.S_IMODE(value.st_mode) == 0o700
    (case.home / "release").touch()
    assert first.wait(timeout=10) == 0 and not appdir.exists()
    case.record.update(uid=65534,
                       first_preserved_after_second=True, distinct_directories=True,
                       runtime_directories_removed=True)


def child_exit(case, fixture):
    process, _ = case.spawn([str(fixture), "exit"])
    assert process.wait(timeout=10) == 42
    ready = json.loads((case.home / "ready.json").read_text())
    assert ready["token"] == case.token and ready["uid"] == 65534 and ready["argv"] == ["exit"]
    assert Path(ready["appdir"]).parent == case.tmp
    if Path(ready["appdir"]).exists():
        case.own(Path(ready["appdir"]), 65534, private=False)
    assert not Path(ready["appdir"]).exists()
    case.record.update(child_exit=42, runtime_directories_removed=True)


def signalled(case, fixture):
    process, appdir, identity, _ = run_fixture(case, fixture, "hold")
    case.send_signal(process, signal.SIGTERM)  # interrupt the launcher's actual waitpid
    signal_file = case.home / "signal"
    deadline = time.monotonic() + 10
    while not signal_file.exists():
        assert process.poll() is None and time.monotonic() < deadline
        time.sleep(.01)
    assert signal_file.read_text() == str(signal.SIGTERM)
    assert process.poll() is None and appdir.exists(), "Signal removed APPDIR before child exit"
    value = appdir.lstat()
    assert (value.st_dev, value.st_ino) == (identity["device"], identity["inode"])
    (case.home / "release").touch()
    assert process.wait(timeout=10) == 23 and not appdir.exists()
    case.record.update(signal=signal.SIGTERM, child_exit=23, forwarded_to_child=True,
                       directory_retained_until_child_exit=True, runtime_directories_removed=True)


def cancelled_extraction(case, fixture):
    process, _ = case.spawn([str(fixture), "--version"])
    deadline = time.monotonic() + 15
    while True:
        candidates = [path for path in case.tmp.iterdir()
                      if re.fullmatch(r"(?:asagiri-appimage-[a-zA-Z0-9]{6}|appimage_extracted_[a-f0-9]{32})",
                                      path.name) and (path / "padding").is_file()]
        if candidates:
            assert len(candidates) == 1
            appdir = candidates[0]
            identity = case.own(appdir, 65534, private=False)
            break
        assert process.poll() is None and time.monotonic() < deadline, "Extraction was not observed"
        time.sleep(.001)
    # Use the existing UID/start-time/pidfd helper; no CAP_KILL or foreign
    # process is involved. Prove extraction is unfinished while it is stopped.
    case.send_signal(process, signal.SIGSTOP)
    deadline = time.monotonic() + 3
    while True:
        fields = Path(f"/proc/{process.pid}/stat").read_text().rsplit(")", 1)[1].split()
        assert fields[19] == case.process_start[process.pid]
        if fields[0] == "T":
            break
        assert process.poll() is None and time.monotonic() < deadline
        time.sleep(.001)
    size = (appdir / "padding").stat().st_size
    assert 0 < size < PADDING_BYTES, "The launcher must be stopped during extraction"
    assert not (case.home / "ready.json").exists(), "AppRun started before cancellation"
    case.send_signal(process, signal.SIGTERM)
    pending = {key: int(value.strip(), 16) for line in Path(f"/proc/{process.pid}/status").read_text().splitlines()
               for key, value in [line.split(":", 1)] if key in ("SigPnd", "ShdPnd")}
    assert (pending["SigPnd"] | pending["ShdPnd"]) & (1 << (signal.SIGTERM - 1))
    case.record.update(signal=signal.SIGTERM, extraction_identity=identity,
                       stopped_during_extraction=True, launcher_pid=process.pid,
                       launcher_start=fields[19], launcher_state=fields[0],
                       extracted_bytes=size, padding_bytes=PADDING_BYTES, pending_signals=pending)
    case.send_signal(process, signal.SIGCONT)
    code = process.wait(timeout=30)
    assert code == -signal.SIGTERM
    assert not (case.home / "ready.json").exists(), "Cancelled extraction launched AppRun"
    case.record.update(exit=code, apprun_executed=False, runtime_directories_removed=not appdir.exists())
    assert case.record["runtime_directories_removed"], "Cancelled extraction left its private directory"


def rejected_apprun(case, fixture):
    code, output = case.command([str(fixture), "--version"])
    assert code != 0 and "Failed to extract a private, owned AppRun" in output, output
    assert not (case.home / "ready.json").exists()
    assert not list(case.tmp.iterdir())
    case.record.update(exit=code, apprun_executed=False, runtime_directories_removed=True)


def rejected_parent(case, fixture, foreign):
    parent = case.directory / "unsafe"
    parent.mkdir(mode=0o755 if foreign else 0o777)
    os.chmod(parent, 0o755 if foreign else 0o777)
    if foreign:
        os.chown(parent, 65533, 65533)
    child = parent / "private"
    child.mkdir(mode=0o700)
    os.chown(child, 65534, 65534)
    code, output = case.command([str(fixture), "--version"], env=dict(case.env, TMPDIR=str(child)))
    assert code != 0 and "Cannot create private extraction directory" in output, output
    assert not (case.home / "ready.json").exists() and not list(child.iterdir())
    case.record.update(exit=code, ancestor_uid=65533 if foreign else 0,
                       child_uid=65534, child_mode=0o700, apprun_executed=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("appimage", type=Path)
    parser.add_argument("appimagetool", type=Path)
    parser.add_argument("--apprun-size", type=int, required=True)
    parser.add_argument("--source", required=True)
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--only", choices=CASES)
    args = parser.parse_args()
    assert os.uname().sysname == "Linux" and os.geteuid() == 0 and Path("/.dockerenv").is_file()
    assert not args.output.exists(), "Keep earlier runtime evidence"
    assert digest(args.appimagetool) == TOOL_HASHES[os.uname().machine]
    assert re.fullmatch("[a-f0-9]{40}", args.source) and re.fullmatch("[a-f0-9]{40}", args.source_tree)
    prefix = runtime_prefix(args.appimage)
    record = {
        "schema": "asagiri.appimage-runtime-extraction.v1", "status": "running",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "source": args.source, "source_tree": args.source_tree, "architecture": os.uname().machine,
        "appimage_sha256": digest(args.appimage), "runtime_prefix_sha256": hashlib.sha256(prefix).hexdigest(),
        "appimagetool_sha256": digest(args.appimagetool), "qualifier_sha256": digest(Path(__file__)),
        "mode": "single-control" if args.only else "complete", "checks": [], "negative_controls": [],
        "limits": ["private container; no FUSE mount or polkit prompt",
                   "root setup validates a nonexistent account and exits before installation",
                   "lifetime/signal/malformed fixtures use the actual packaged runtime and test AppRun"],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    try:
        temporary = tempfile.mkdtemp(prefix="asagiri-runtime-test-")
        try:
            scratch = Path(temporary)
            scratch.chmod(0o755)
            tools = scratch / "tools"
            tools.mkdir(mode=0o700)
            private_tool = tools / "appimagetool"
            shutil.copyfile(args.appimagetool, private_tool)
            private_tool.chmod(0o755)
            assert digest(private_tool) == TOOL_HASHES[os.uname().machine]
            tool_log = args.output.with_name(args.output.stem + "-tool-extract.txt")
            with tool_log.open("wb") as log:
                subprocess.run([str(private_tool), "--appimage-extract"], cwd=tools,
                               stdout=log, stderr=log, timeout=60, check=True)
            candidates = list((tools / "squashfs-root").rglob("mksquashfs"))
            assert len(candidates) == 1 and candidates[0].is_file() and not candidates[0].is_symlink()
            mksquashfs = candidates[0]
            record["mksquashfs_sha256"] = digest(mksquashfs)
            fixtures = {}
            for kind in ("normal", "writable", "linked", "large"):
                appdir = scratch / (kind + "-input")
                appdir.mkdir()
                (appdir / "payload").write_bytes(FIXTURE)
                (appdir / "payload").chmod(0o755)
                if kind == "linked":
                    (appdir / "AppRun").symlink_to("payload")
                else:
                    (appdir / "AppRun").write_bytes(FIXTURE)
                    (appdir / "AppRun").chmod(0o777 if kind == "writable" else 0o755)
                if kind == "large":
                    # Sparse input keeps the fixture small on disk; the runtime
                    # writes it in chunks, allowing a real mid-extraction stop.
                    with (appdir / "padding").open("wb") as padding:
                        padding.truncate(PADDING_BYTES)
                squash = scratch / (kind + ".squashfs")
                log_path = args.output.with_name(args.output.stem + "-" + kind + "-squashfs.txt")
                with log_path.open("wb") as log:
                    subprocess.run([str(mksquashfs), str(appdir), str(squash), "-noappend",
                                    "-all-root", "-processors", "1", "-comp", "zstd"],
                                   stdout=log, stderr=log, check=True, timeout=60)
                fixture = scratch / (kind + ".AppImage")
                fixture.write_bytes(prefix + squash.read_bytes())
                fixture.chmod(0o755)
                fixtures[kind] = fixture
            # The SHA-pinned packaging tool includes the original upstream
            # runtime. Exercise its cancellation defect only as the test UID,
            # with our own fixture; never run a privileged extraction control.
            upstream_prefix = runtime_prefix(args.appimagetool)
            record["upstream_runtime_prefix_sha256"] = hashlib.sha256(upstream_prefix).hexdigest()
            upstream_fixture = scratch / "upstream-large.AppImage"
            upstream_fixture.write_bytes(upstream_prefix + (scratch / "large.squashfs").read_bytes())
            upstream_fixture.chmod(0o755)
            fixtures["upstream-large"] = upstream_fixture
            record["fixtures"] = {kind: digest(path) for kind, path in fixtures.items()}
            record["fixture_apprun_sha256"] = hashlib.sha256(FIXTURE).hexdigest()
            for name in (args.only,) if args.only else CASES:
                case = Case(name, scratch / name, args.output)
                try:
                    if name.startswith("privileged-setup"):
                        privileged_setup(case, args.appimage.resolve(), args.apprun_size,
                                         name.endswith("attacker-tmpdir"))
                    elif name == "concurrent-unprivileged-extraction":
                        concurrent(case, fixtures["normal"])
                    elif name == "failed-child-exit-cleanup":
                        child_exit(case, fixtures["normal"])
                    elif name == "signalled-child-waits-and-cleans":
                        signalled(case, fixtures["normal"])
                    elif name == "cancelled-extraction-cleans-before-launch":
                        cancelled_extraction(case, fixtures["large"])
                    elif name in ("writable-apprun-rejected", "linked-apprun-rejected"):
                        rejected_apprun(case, fixtures[name.split("-")[0]])
                    else:
                        rejected_parent(case, fixtures["normal"], name.startswith("foreign"))
                    case.record["status"] = "passed"
                except BaseException as error:
                    case.record.update(status="failed", error=repr(error))
                finally:
                    try:
                        case.cleanup()
                    finally:
                        record["checks"].append(case.record)
            assert all(case["status"] == "passed" for case in record["checks"])
            assert len(record["checks"]) == (1 if args.only else len(CASES))
            if not args.only:
                name = "upstream-cancelled-extraction-leaks"
                case = Case(name, scratch / name, args.output)
                try:
                    cancelled_extraction(case, upstream_fixture)
                except AssertionError as error:
                    assert str(error) == "Cancelled extraction left its private directory", repr(error)
                    case.record.update(status="expected-failure", error=str(error))
                else:
                    raise AssertionError("The upstream cancellation control did not reproduce its leak")
                finally:
                    try:
                        case.cleanup()
                    finally:
                        record["negative_controls"].append(case.record)
        finally:
            if all(case["cleanup"].get("processes_reaped") is True
                   and case["cleanup"].get("descendants_exited") is True
                   for case in record["checks"] + record["negative_controls"]):
                shutil.rmtree(temporary)
            else:
                # The enclosing owned container is the final cleanup boundary.
                # Never delete a fixture while a recorded descendant is alive.
                record["retained_scratch"] = temporary
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error))
        raise
    finally:
        record["completed_at"] = datetime.now(timezone.utc).isoformat()
        # Hash completed qualifier-owned logs only. An outer driver's stdout
        # may still be open here and is authenticated by its own final receipt.
        names = {args.output.stem + "-tool-extract.txt"}
        names.update(args.output.stem + "-" + kind + "-squashfs.txt" for kind in ("normal", "writable", "linked", "large"))
        for case in record["checks"] + record["negative_controls"]:
            names.update(command["log"] for command in case["commands"])
            names.update(entry["log"] for entry in case.get("signals", []))
        record["evidence"] = {name: digest(args.output.parent / name) for name in sorted(names)
                              if (args.output.parent / name).is_file()}
        args.output.write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps({"status": record["status"], "checks": len(record["checks"])}, indent=2))


if __name__ == "__main__":
    main()
