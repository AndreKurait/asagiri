#!/usr/bin/env python3
"""A stopped-container archive reply must wait for its private overlay to disappear."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import http.client
import io
import json
import os
from pathlib import Path
import platform
import signal
import socket
import subprocess
import tarfile
import time


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=40)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image, expect_race=False):
    assert platform.system() == "Linux" and os.environ.get("ASAGIRI_TEST_ENGINE") == "1"
    assert Path("/.dockerenv").is_file()
    docker = ["docker", "-H", "unix://" + path]
    cid = subprocess.check_output(docker + ["create", "--network", "none", image, "sleep", "120"],
                                  text=True, stderr=subprocess.STDOUT, timeout=30).strip()
    tracer = None
    pool = ThreadPoolExecutor(max_workers=1)
    future = None
    trace_log = Path("/tmp/asagiri-archive-completion.trace")

    def request(method, target, body=None):
        connection = Connection(path)
        try:
            connection.request(method, target, body, {"Content-Type": "application/x-tar"})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    try:
        agents = []
        for item in Path("/proc").iterdir():
            if not item.name.isdigit():
                continue
            try:
                fields = (item / "stat").read_text().rsplit(")", 1)[1].split()
                if (item / "comm").read_text().strip() == "asagiri-init" and fields[0] != "Z" and int(fields[1]) == 1:
                    agents.append(int(item.name))
            except (OSError, ValueError):
                pass
        assert len(agents) == 1, agents
        agent = agents[0]
        with trace_log.open("wb") as log:
            tracer = subprocess.Popen([
                "strace", "-f", "-e", "trace=exit_group", "-e", "inject=exit_group:delay_enter=5s",
                "-p", str(agent),
            ], stdout=log, stderr=log)
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if f"TracerPid:\t{tracer.pid}\n" in Path("/proc", str(agent), "status").read_text():
                    break
                time.sleep(0.02)
            else:
                raise AssertionError("strace did not attach")
            data = io.BytesIO()
            with tarfile.open(fileobj=data, mode="w") as archive:
                entry = tarfile.TarInfo("completion-proof")
                entry.size = 9
                archive.addfile(entry, io.BytesIO(b"preserved"))
            began = time.monotonic()
            future = pool.submit(request, "PUT", f"/containers/{cid}/archive?path=/tmp", data.getvalue())
            deadline = began + 10
            worker = None
            while time.monotonic() < deadline:
                for item in Path("/proc").iterdir():
                    if not item.name.isdigit():
                        continue
                    try:
                        if f"TracerPid:\t{tracer.pid}\n" not in (item / "status").read_text():
                            continue
                        syscall = (item / "syscall").read_text().split()
                        if syscall and syscall[0] == {"x86_64": "231", "aarch64": "94"}[platform.machine()]:
                            if os.readlink(item / "ns/mnt") != os.readlink(Path("/proc", str(agent), "ns/mnt")):
                                worker = int(item.name)
                                break
                    except OSError:
                        pass
                if worker is not None:
                    break
                time.sleep(0.01)
            assert worker is not None, "No delayed private-mount worker exit was observed"
            assert " - overlay " in Path("/proc", str(worker), "mountinfo").read_text()
            time.sleep(0.2)
            early_start = None
            if expect_race:
                assert future.result(timeout=1)[0] == 200
                status, body = request("POST", f"/containers/{cid}/start")
                # Some kernels allow overlapping overlay mounts; others
                # reject the same upper/work directories with EBUSY. The
                # premature completion is observable on either kernel.
                assert status == 204 or (status >= 400 and b"busy" in body.lower()), (status, body)
                early_start = {"status": status, "error": body.decode(errors="replace")}
            else:
                assert not future.done(), "Archive upload acknowledged while its overlay was still mounted"
                assert request("GET", "/_ping") == (200, b"OK")
                status, body = future.result(timeout=15)
                assert status == 200, (status, body)
                assert time.monotonic() - began >= 4.5
                status, body = request("POST", f"/containers/{cid}/start")
                assert status == 204, (status, body)
            tracer.send_signal(signal.SIGINT)
            tracer.wait(timeout=20)
            tracer = None
        if expect_race:
            subprocess.run(docker + ["start", cid], check=True, capture_output=True, timeout=30)
        assert subprocess.check_output(docker + ["exec", cid, "cat", "/tmp/completion-proof"],
                                       timeout=30) == b"preserved"
        print(json.dumps({"probe": "archive-completion", "delayedWorker": worker,
                          "expectedOldRace": expect_race, "uploadSeconds": time.monotonic() - began,
                          "startBeforeWorkerExit": early_start,
                          "immediateStartAndWritableData": "passed"}), flush=True)
    finally:
        if tracer is not None and tracer.poll() is None:
            tracer.send_signal(signal.SIGINT)
            tracer.wait(timeout=20)
        pool.shutdown(wait=True, cancel_futures=True)
        subprocess.run(docker + ["rm", "-f", cid], capture_output=True, timeout=30)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("socket")
    parser.add_argument("image")
    parser.add_argument("--expect-race", action="store_true")
    args = parser.parse_args()
    check(args.socket, args.image, args.expect_race)
