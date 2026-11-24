#!/usr/bin/env python3
"""Keep a real native BuildKit solve alive across both daemon idle watches."""
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=20)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    build_base = os.environ.get("ASAGIRI_TEST_BUILD_BASE") or image
    assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1", "requires a disposable test engine"
    assert "ASAGIRI_BUILDER_IDLE_SECS" not in os.environ, "test changes the private engine's setting"
    token = uuid.uuid4().hex
    observations = []

    def api(method, target, body=None):
        connection = Connection(path)
        try:
            # Observe power state without resetting the engine's idle clock.
            connection.request(method, target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json", "X-Asagiri-Background": "1"})
            response = connection.getresponse()
            data = response.read()
            assert response.status in (200, 204), (method, target, response.status, data)
            return json.loads(data) if data else None
        finally:
            connection.close()

    original = api("GET", "/asagiri/builder")
    managed = original["container"]

    def state():
        return api("GET", "/containers/" + managed + "/json")["State"]

    def setting(seconds):
        api("POST", "/asagiri/builder", {"idle_secs": seconds})
        assert api("GET", "/asagiri/builder")["idle_secs"] == seconds

    with tempfile.TemporaryDirectory(prefix="asagiri-builder-idle-") as directory:
        root = Path(directory)
        config = root / "docker-config"
        config.mkdir(mode=0o700)
        context = root / "context"
        context.mkdir()
        env = {**os.environ, "DOCKER_CONFIG": str(config), "DOCKER_HOST": "unix://" + path,
               "BUILDX_BUILDER": "default", "DOCKER_BUILDKIT": "1"}
        env.pop("DOCKER_CONTEXT", None)
        env.pop("BUILDX_CONFIG", None)
        command = ["docker", "--host", "unix://" + path, "--config", str(config),
                   "buildx", "build", "--builder", "default", "--no-cache", "--progress", "plain"]

        def build(name, hold):
            proof = token + "-" + name
            (context / "Dockerfile").write_text(
                f"FROM {build_base}\nRUN echo {proof}; sleep {hold}; printf '%s' {proof} > /idle-proof\n")
            target = root / (name + ".tar")
            log = root / (name + ".log")
            started = time.monotonic()
            with log.open("w") as output:
                process = subprocess.Popen(
                    [*command, "--output", "type=tar,dest=" + str(target), str(context)],
                    env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    deadline = time.monotonic() + 60
                    while not re.search(r"(?m)^#\d+ \d+\.\d+ " + re.escape(proof) + r"\s*$", log.read_text()):
                        assert process.poll() is None, log.read_text()
                        assert time.monotonic() < deadline, "RUN did not start\n" + log.read_text()
                        time.sleep(0.1)
                    first = state()
                    assert first["Running"] and not first["Paused"] and first["Pid"] > 0, first
                    samples = 0
                    active_since = time.monotonic()
                    while process.poll() is None:
                        current = state()
                        assert current["Running"] and not current["Paused"] and current["Pid"] == first["Pid"], (
                            current, first, log.read_text())
                        assert time.monotonic() < deadline, "native build stalled\n" + log.read_text()
                        samples += 1
                        time.sleep(0.25)
                    assert process.returncode == 0, log.read_text()
                    elapsed = time.monotonic() - active_since
                    if hold:
                        assert elapsed >= 18 and samples >= 50, (elapsed, samples, log.read_text())
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=10)
            assert 'using docker driver' in log.read_text(), log.read_text()
            with tarfile.open(target) as archive:
                member = next(item for item in archive if item.name.lstrip("./") == "idle-proof")
                assert member.isfile() and member.size == len(proof)
                assert archive.extractfile(member).read() == proof.encode()
            observations.append({"build": name, "pid": first["Pid"], "samples": samples,
                                 "seconds": time.monotonic() - started})
            return first["Pid"]

        try:
            setting(1)
            pid = build("active", 22)
            print("PASS native Docker driver stays running across both idle watches", flush=True)

            # A completed connection must release its busy lease; otherwise
            # fixing active builds would silently disable idle power saving.
            started = time.monotonic()
            while True:
                current = state()
                assert current["Running"] and current["Pid"] == pid, current
                if current["Paused"]:
                    break
                assert time.monotonic() - started < 35, "finished native build never became idle"
                time.sleep(0.25)
            observations.append({"idle_pause_seconds": time.monotonic() - started, "pid": pid})
            print("PASS completed native build releases its lease and pauses", flush=True)

            setting(0)
            assert build("resumed", 0) == pid, "paused backend was replaced instead of resumed"
            started = time.monotonic()
            samples = 0
            while time.monotonic() - started < 18:
                current = state()
                assert current["Running"] and not current["Paused"] and current["Pid"] == pid, current
                samples += 1
                time.sleep(0.25)
            observations.append({"disabled_idle_seconds": time.monotonic() - started, "samples": samples})
            print("PASS zero idle timeout keeps the resumed builder running", flush=True)
        finally:
            setting(original["idle_secs"])
    print(json.dumps({"probe": "builder-idle", "checks": observations}), flush=True)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
