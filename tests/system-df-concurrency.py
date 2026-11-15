#!/usr/bin/env python3
"""Real disk-usage replies alongside container and archive worker processes."""
from concurrent.futures import ThreadPoolExecutor
import http.client
import io
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tarfile
import threading
import time
import uuid


def check(path, image):
    class Connection(http.client.HTTPConnection):
        def connect(self):
            self.sock = socket.socket(socket.AF_UNIX)
            self.sock.settimeout(self.timeout)
            self.sock.connect(path)

    def api(method, target, body=None, status=200):
        conn = Connection("localhost", timeout=60)
        try:
            binary = isinstance(body, bytes)
            conn.request(method, "/v1.41" + target,
                         body if binary else json.dumps(body).encode() if body is not None else None,
                         {"Content-Type": "application/x-tar" if binary else "application/json"})
            response = conn.getresponse()
            data = response.read()
            assert response.status == status, (method, target, response.status, data[:2000])
            return data
        finally:
            conn.close()

    def archive(files):
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w") as tar:
            for name, data in files:
                item = tarfile.TarInfo(name)
                item.size = len(data)
                item.mode = 0o644
                tar.addfile(item, io.BytesIO(data))
        return buf.getvalue()

    prefix = "df-concurrent-" + uuid.uuid4().hex[:10]
    volume = prefix + "-data"
    containers = []
    stop = threading.Event()
    observed_threads = []
    observer_errors = []
    pid_file = os.environ.get("ASAGIRI_AGENT_PID_FILE")
    pid = None
    docker = ["docker", "--host", "unix://" + path]

    def monitor():
        while not stop.is_set():
            try:
                observed_threads.append(len(list(Path(f"/proc/{pid}/task").iterdir())))
            except Exception as error:
                observer_errors.append(error)
                return
            stop.wait(0.001)

    watcher = None
    try:
        api("POST", "/volumes/create", {"Name": volume}, 201)
        cid = json.loads(api("POST", "/containers/create", {
            "Image": image, "Cmd": ["sh", "-c", "sleep 300"],
            "HostConfig": {"NetworkMode": "none", "Binds": [volume + ":/data"]},
        }, 201))["Id"]
        containers.append(cid)
        api("POST", f"/containers/{cid}/start", status=204)
        payload = b"usage-proof\n" * 65536
        # Enough directory entries to observe the measurement while it runs.
        files = [(f"entry-{i}", b"x") for i in range(12000)]
        files.append(("payload", payload))
        api("PUT", f"/containers/{cid}/archive?path=/data", archive(files))
        if pid_file:
            # Agent startup is lazy; a prior engine's lock file can exist until
            # the first container starts the current agent.
            pid = int(Path(pid_file).read_text().strip())
            watcher = threading.Thread(target=monitor)
            watcher.start()

        def measure():
            for _ in range(8):
                result = json.loads(api("GET", "/system/df"))
                row, = [v for v in result["Volumes"] if v["Name"] == volume]
                assert row["UsageData"]["Size"] >= len(payload), row
                assert row["UsageData"]["RefCount"] >= 1, row
                assert any(c["Id"] == cid for c in result["Containers"]), result["Containers"]

        def lifecycle():
            for i in range(12):
                child = json.loads(api("POST", "/containers/create", {
                    "Image": image, "Cmd": ["sh", "-c", f"printf 'child-{i}'"],
                    "HostConfig": {"NetworkMode": "none"},
                }, 201))["Id"]
                containers.append(child)
                api("POST", f"/containers/{child}/start", status=204)
                state = json.loads(api("POST", f"/containers/{child}/wait"))
                assert state["StatusCode"] == 0, state
                output = subprocess.run(docker + ["logs", child], check=True,
                                        capture_output=True, timeout=30)
                assert output.stdout == f"child-{i}".encode(), output
                api("DELETE", f"/containers/{child}", status=204)
                containers.remove(child)

        def copies_and_exec():
            for i in range(12):
                expected = f"copy-{i}".encode()
                api("PUT", f"/containers/{cid}/archive?path=/data",
                    archive([(f"probe-{i}", expected)]))
                data = api("GET", f"/containers/{cid}/archive?path=/data/probe-{i}")
                with tarfile.open(fileobj=io.BytesIO(data)) as tar:
                    assert tar.extractfile(tar.getmembers()[0]).read() == expected
                result = subprocess.run(docker + ["exec", cid, "cat", f"/data/probe-{i}"],
                                        check=True, capture_output=True, timeout=30)
                assert result.stdout == expected, result

        def failed_measurement():
            """Only the isolated engine exposes its agent PID for fault injection."""
            children = Path(f"/proc/{pid}/task/{pid}/children")
            before = set(children.read_text().split())
            executable = os.readlink(f"/proc/{pid}/exe")
            handle = None
            started = time.monotonic()
            with ThreadPoolExecutor(max_workers=1) as pool:
                result = pool.submit(api, "GET", "/system/df", status=500)
                try:
                    while time.monotonic() - started < 5 and handle is None:
                        for child in set(children.read_text().split()) - before:
                            try:
                                if os.readlink(f"/proc/{child}/exe") != executable:
                                    continue
                                handle = os.pidfd_open(int(child))
                                signal.pidfd_send_signal(handle, signal.SIGSTOP)
                                break
                            except ProcessLookupError:
                                if handle is not None:
                                    os.close(handle)
                                    handle = None
                            except FileNotFoundError:
                                continue
                        if result.done():
                            result.result()
                            raise AssertionError("usage worker escaped fault injection")
                        time.sleep(0.001)
                    assert handle is not None, "usage worker did not appear"
                    # Its blocked reply must not hold up an unrelated command.
                    probe = subprocess.run(docker + ["exec", cid, "printf", "still-running"],
                                           check=True, capture_output=True, timeout=5)
                    assert probe.stdout == b"still-running", probe
                    signal.pidfd_send_signal(handle, signal.SIGKILL)
                    error = json.loads(result.result(timeout=5))
                    assert "usage worker" in error["message"].lower(), error
                    assert int(Path(pid_file).read_text().strip()) == pid, "agent restarted"
                    probe = subprocess.run(docker + ["exec", cid, "cat", "/data/payload"],
                                           check=True, capture_output=True, timeout=5)
                    assert probe.stdout == payload, probe
                    measured = json.loads(api("GET", "/system/df"))
                    row, = [v for v in measured["Volumes"] if v["Name"] == volume]
                    assert row["UsageData"]["Size"] >= len(payload), row
                    print("PASS killed usage worker reports HTTP 500; agent, exec and "
                          "subsequent measurements survive", flush=True)
                finally:
                    if handle is not None:
                        try:
                            signal.pidfd_send_signal(handle, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        os.close(handle)

        if pid_file:
            failed_measurement()
        started = time.monotonic()
        with ThreadPoolExecutor(max_workers=3) as pool:
            futures = [pool.submit(f) for f in (measure, lifecycle, copies_and_exec)]
            for future in futures:
                future.result()
        stop.set()
        if watcher:
            watcher.join()
            assert not observer_errors, observer_errors
            assert observed_threads and set(observed_threads) == {1}, sorted(set(observed_threads))
            print(f"PASS agent stayed single-threaded in {len(observed_threads)} samples", flush=True)
        print(f"PASS measured volume bytes, 12 lifecycles and 12 copy/exec round trips "
              f"alongside 8 usage scans in {time.monotonic() - started:.3f}s", flush=True)
    finally:
        stop.set()
        if watcher and watcher.ident:
            watcher.join()
        failed = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(containers):
            try:
                api("DELETE", f"/containers/{cid}?force=true", status=204)
            except Exception as error:
                errors.append(error)
        try:
            api("DELETE", "/volumes/" + volume, status=204)
        except Exception as error:
            errors.append(error)
        if errors:
            print("Cleanup errors:", errors, file=sys.stderr)
            if not failed:
                raise RuntimeError("disk usage test cleanup failed") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
