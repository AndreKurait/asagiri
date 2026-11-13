#!/usr/bin/env python3
"""Exercise private process groups and group signals against a disposable engine."""
import os
import subprocess
import sys
import tempfile


def check(path, image):
    with tempfile.TemporaryDirectory(prefix="asagiri-sessions-") as config:
        env = dict(os.environ, DOCKER_CONFIG=config)
        env.pop("DOCKER_CONTEXT", None)

        def cli(*args):
            result = subprocess.run(["docker", "--host", "unix://" + path, *args],
                                    env=env, capture_output=True, text=True, timeout=30)
            assert result.returncode == 0, (args, result.stdout, result.stderr)
            return result.stdout.strip()

        # Verify isolation before sending a group signal. A broken engine
        # fails the SID assertion without signalling its shared parent group.
        signal = ('sid=$(cut -d" " -f6 /proc/$$/stat); '
                  '[ "$sid" = "$$" ] || exit 42; '
                  'trap "exit 0" TERM; kill -TERM 0; sleep 1; exit 43')
        cli("run", "--rm", "--network", "none", image, "sh", "-c", signal)
        cid = cli("run", "-d", "--network", "none", image, "sleep", "120")
        try:
            cli("exec", cid, "sh", "-c", signal)
            cli("exec", cid, "echo", "container-survived")
            cli("info")
        finally:
            cli("rm", "-f", cid)
        print("PASS container and exec sessions isolate process-group termination from the engine", flush=True)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2])
