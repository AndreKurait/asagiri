#!/usr/bin/env python3
"""Check Linux host writes reach a bind-mounted container's inotify watch.

Run beside the daemon so the temporary directory is on its Linux host.
This does not exercise propagation through macOS Virtualization.framework.
"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid


def check(path, image):
    name = "asagiri-inotify-" + uuid.uuid4().hex[:12]
    env = dict(os.environ)
    env.pop("DOCKER_CONTEXT", None)

    def cli(*args, ok=True):
        p = subprocess.run(["docker", "--host", "unix://" + path, *args],
                           env=env, capture_output=True, text=True, timeout=30)
        if ok:
            assert p.returncode == 0, (args, p.stdout, p.stderr)
        return p

    with tempfile.TemporaryDirectory(prefix=name) as directory:
        try:
            cli("run", "-d", "--name", name, "-v", directory + ":/watched", image,
                "sh", "-c", "inotifyd - /watched:wcnd & echo ready; wait")
            deadline = time.monotonic() + 10
            while "ready" not in cli("logs", name).stdout:
                assert time.monotonic() < deadline, "watcher did not start"
                time.sleep(0.05)
            # Each new file is a fresh event even if the first write races
            # the child process installing its watch.
            for attempt in range(40):
                file = Path(directory) / ("probe-" + str(attempt))
                file.write_text("created\n")
                with file.open("a") as out:
                    out.write("changed\n")
                time.sleep(0.1)
                logs = cli("logs", name).stdout
                if "/watched" in logs and "probe-" in logs:
                    print("PASS Linux host writes produce inotify events in a bind-mounted container")
                    return
            raise AssertionError("no inotify event for Linux host writes: " + logs)
        finally:
            cli("rm", "-f", name, ok=False)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
