#!/usr/bin/env python3
"""Exercise the Linux WSL bridge and startup script in one disposable container."""
import argparse
from datetime import datetime, timezone
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import re
import selectors
import signal
import struct
import subprocess
import time
import traceback
import uuid

ROOT = Path(__file__).resolve().parents[1]
SOCKET = "/var/lib/asagiri-daemon/.asagiri/run/docker.sock"
LABEL = "dev.asagiri.wsl-bridge-test"
NO_START = "asagiri daemon exited as it started; see /var/log/asagiri.log"
NO_ANSWER = "it started, then stopped before answering"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class Fixture:
    def __init__(self, args, record):
        self.args, self.record = args, record
        self.name = "asagiri-bridge-start-" + uuid.uuid4().hex[:16]
        record["fixture"] = self.name
        args.output.write_text(json.dumps(record, indent=2) + "\n")

    def command(self, *args, input=None, timeout=30):
        result = subprocess.run([self.args.docker, *args], input=input,
                                capture_output=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError((args, result.returncode,
                                result.stdout.decode(errors="replace")[-2000:],
                                result.stderr.decode(errors="replace")[-2000:]))
        return result.stdout

    def inside(self, script, *, input=None):
        return self.command("exec", "-i", self.name, "sh", "-ceu", script, input=input)

    def create(self):
        args = self.args
        if not re.fullmatch(r"(?:[^\s]+@)?sha256:[a-f0-9]{64}", args.image):
            raise ValueError("Use a digest-pinned image or a verified local image ID")
        self.record["image_id"] = self.command("image", "inspect", args.image,
                                               "--format", "{{.Id}}").decode().strip()
        bindings = {
            args.bin / "asagiri": "/fixture/asagiri",
            args.bin / "asagiri-init": "/fixture/asagiri-init",
            args.fixture: "/fixture/daemon-fixture",
            ROOT / "contrib/wsl/asagiri-linux/asagiri-start": "/fixture/asagiri-start",
        }
        bridge = args.bridge or args.bin / "asagiri"
        # A native component probe may supply the production bridge module
        # separately. CI always runs the complete rebuilt Asagiri executable.
        if args.bridge:
            if args.bridge in bindings:
                raise ValueError("The component override must be a separate executable")
            bindings[args.bridge] = "/fixture/bridge"
        self.record["binary_sha256"] = {str(dst): digest(src) for src, dst in bindings.items()}
        self.record["bridge_kind"] = "production component override" if args.bridge else "complete Asagiri executable"
        self.record["bridge_sha256"] = digest(bridge)
        command = ["run", "-d", "--rm", "--pull", "never", "--privileged",
                   "--network", "none", "--tmpfs", "/run", "--name", self.name,
                   "--label", LABEL + "=" + self.name, "--entrypoint", "sh"]
        for source, destination in bindings.items():
            command += ["-v", str(source) + ":" + destination + ":ro"]
        bridge_source = "/fixture/bridge" if args.bridge else "/fixture/asagiri"
        command += [args.image, "-ceu",
                    "cp /fixture/asagiri /usr/local/bin/asagiri\n"
                    "cp /fixture/asagiri-init /usr/local/bin/asagiri-init\n"
                    "cp /fixture/asagiri-start /usr/local/bin/asagiri-start\n"
                    f"cp {bridge_source} /usr/local/bin/asagiri-bridge-probe\n"
                    "chmod 755 /usr/local/bin/asagiri /usr/local/bin/asagiri-init "
                    "/usr/local/bin/asagiri-start /usr/local/bin/asagiri-bridge-probe\n"
                    "exec sleep 600"]
        self.record["container_id"] = self.command(*command).decode().strip()
        self.record["daemon_version"] = self.inside("/usr/local/bin/asagiri --version").decode().strip()

    def install_start(self, script):
        self.inside("cat > /usr/local/bin/asagiri-start\nchmod 755 /usr/local/bin/asagiri-start",
                    input=script.encode())

    def connections(self, count, case, gap=0):
        stderr_path = self.args.output.parent / (case + ".bridge.log")
        with stderr_path.open("wb") as stderr:
            process = subprocess.Popen(
                [self.args.docker, "exec", "-i", self.name,
                 "/usr/local/bin/asagiri-bridge-probe", "bridge", "--socket", SOCKET],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                start_new_session=True)
            deadline = time.monotonic() + 45
            selector = selectors.DefaultSelector()
            selector.register(process.stdout, selectors.EVENT_READ)
            done, received = {}, {}

            def read_exact(size):
                data = bytearray()
                while len(data) < size:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not selector.select(remaining):
                        raise TimeoutError("The bridge did not finish its reply")
                    chunk = os.read(process.stdout.fileno(), size - len(data))
                    if not chunk:
                        raise RuntimeError("The bridge ended before replying")
                    data.extend(chunk)
                return bytes(data)

            def send(kind, cid, data=b""):
                process.stdin.write(struct.pack(">BII", kind, cid, len(data)) + data)
                process.stdin.flush()

            def wait(number):
                while len(done) < number:
                    kind, cid, size = struct.unpack(">BII", read_exact(9))
                    if size > 1 << 20:
                        raise ValueError("Oversized bridge frame")
                    data = read_exact(size)
                    if kind == 5:  # hello / returned request credit
                        continue
                    if not 1 <= cid <= count or cid in done:
                        raise ValueError("Reply for an unknown or completed connection")
                    if kind == 2:
                        received[cid] = received.get(cid, b"") + data
                        if len(received[cid]) > 1 << 20:
                            raise ValueError("Oversized ping response")
                    elif kind == 3:
                        done[cid] = {"kind": "eof", "reply": received.get(cid, b"")}
                    elif kind == 4:
                        done[cid] = {"kind": "close", "reason": data.decode()}
                    else:
                        raise ValueError("Unknown bridge frame kind")

            try:
                for cid in range(1, count + 1):
                    send(1, cid, b"\x01")
                    send(2, cid, b"GET /_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n")
                    send(3, cid)
                    if gap:
                        wait(cid)
                        time.sleep(gap)
                wait(count)
                process.stdin.close()
                if process.wait(timeout=5) != 0:
                    raise RuntimeError("The bridge did not close normally")
                return [done[cid] for cid in range(1, count + 1)]
            finally:
                selector.close()
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
                process.stdout.close()
                if not process.stdin.closed:
                    process.stdin.close()

    def cleanup(self):
        result = subprocess.run([self.args.docker, "inspect", self.name], capture_output=True, timeout=15)
        if result.returncode == 0:
            info = json.loads(result.stdout)[0]
            if (info["Config"].get("Labels") or {}).get(LABEL) != self.name:
                raise RuntimeError("Fixture ownership changed; it was left intact")
            self.record["container_image"] = info["Image"]
            self.record["container_network"] = info["HostConfig"]["NetworkMode"]
            self.command("rm", "-f", self.name)
            result = subprocess.run([self.args.docker, "inspect", self.name],
                                    capture_output=True, timeout=15)
        error = result.stderr.decode(errors="replace").lower()
        if result.returncode == 0 or not any(text in error for text in ("no such object", "no such container")):
            raise RuntimeError("Could not verify fixture removal: " + error)
        self.record["fixture_verified_absent"] = True


def is_ping(result):
    if result["kind"] != "eof" or not result["reply"]:
        return False
    class Socket:
        def makefile(self, *_args):
            return io.BytesIO(result["reply"])
    with http.client.HTTPResponse(Socket()) as response:
        response.begin()
        return response.status == 200 and response.read() == b"OK"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bin", type=lambda value: Path(value).resolve())
    parser.add_argument("fixture", type=lambda value: Path(value).resolve())
    parser.add_argument("image")
    parser.add_argument("--bridge", type=lambda value: Path(value).resolve())
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    if args.output.exists():
        raise ValueError("Use a new output path; inspect the existing run before retrying")
    record = {"schema": "asagiri.wsl.bridge-start.v1", "status": "running",
              "started_at": datetime.now(timezone.utc).isoformat(), "cases": [],
              "image": args.image, "pull": "never", "network": "none",
              "source_files": {name: digest(ROOT / name) for name in (
                  "crates/asagiri/src/bridge.rs", "tests/bridge-start.py",
                  "tests/wsl-daemon-fixture.rs", "contrib/wsl/asagiri-linux/asagiri-start")}}
    fixture = Fixture(args, record)
    try:
        fixture.create()
        # The compiled stand-in exits with an abandoned socket at the exact
        # startup path. Python remains on the host; the fixture needs no apk.
        fixture.inside("echo dies > /tmp/mode\n"
                       "ASAGIRI_TEST_WSL_START=1 /fixture/daemon-fixture daemon\n"
                       f"test -S {SOCKET}")
        replies = fixture.connections(3, "concurrent")
        assert all(is_ping(reply) for reply in replies), replies
        log = fixture.inside("cat /var/log/asagiri.log").decode()
        assert log.count("engine not answering on its socket; starting it") == 1, log
        assert "asagiri bridge: engine started" in log, log
        daemons = fixture.inside("pgrep -f '^/usr/local/bin/[a]sagiri daemon($| )'").splitlines()
        assert len(daemons) == 1, daemons
        record["cases"].append({"case": "three connections, one startup", "passed": True})
        assert all(is_ping(reply) for reply in fixture.connections(1, "already-running"))
        log = fixture.inside("cat /var/log/asagiri.log").decode()
        assert log.count("engine not answering on its socket; starting it") == 1, log
        record["cases"].append({"case": "already running, no startup", "passed": True})
        fixture.inside("pkill -9 -f '^/usr/local/bin/[a]sagiri daemon($| )'\nsleep 0.5")
        fixture.install_start(f"#!/bin/sh\necho x >>/tmp/starts\necho '{NO_START}' >&2\nexit 2\n")
        replies = fixture.connections(2, "failed-start")
        assert all(reply == {"kind": "close", "reason": NO_START} for reply in replies), replies
        assert fixture.inside("wc -l </tmp/starts").strip() == b"1"
        log = fixture.inside("cat /var/log/asagiri.log").decode()
        assert "asagiri bridge: engine did not start: " + NO_START in log, log
        record["cases"].append({"case": "failed startup reason and one attempt", "passed": True})
        fixture.install_start(
            "#!/bin/sh\nset -eu\necho x >>/tmp/boots\n"
            f"rm -f {SOCKET}\necho dies >/tmp/mode\n"
            "nohup env ASAGIRI_TEST_WSL_START=1 /fixture/daemon-fixture daemon >/dev/null 2>&1 &\n"
            f"i=0; while [ $i -lt 50 ]; do [ -S {SOCKET} ] && exit 0; sleep 0.1; i=$((i+1)); done\nexit 1\n")
        replies = fixture.connections(4, "dies-before-answer", gap=1)
        assert fixture.inside("wc -l </tmp/boots").strip() == b"1", replies
        assert sum(reply["kind"] == "close" and reply["reason"].startswith(NO_ANSWER)
                   for reply in replies) == 3, replies
        record["cases"].append({"case": "socket without an answer keeps startup backoff", "passed": True})
        record["status"] = "passed"
    except BaseException as error:
        record.update(status="failed", error=repr(error), traceback=traceback.format_exc())
    finally:
        try:
            record["daemon_log"] = fixture.inside("cat /var/log/asagiri.log 2>/dev/null || true").decode(errors="replace")[-16000:]
        except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
            record["log_error"] = repr(error)
        try:
            fixture.cleanup()
        except BaseException as error:
            record.update(status="failed", cleanup_error=repr(error))
        record["finished_at"] = datetime.now(timezone.utc).isoformat()
        args.output.write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps({key: value for key, value in record.items()
                      if key not in ("daemon_log", "traceback")}, indent=2))
    return 0 if record["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
