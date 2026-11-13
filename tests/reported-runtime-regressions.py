#!/usr/bin/env python3
"""Regression probes for reported Linux behavior, against a disposable engine.

The supplied socket must belong to the private test engine. No host ports are
published by the outer harness; this script's port stays inside that harness.
Native macOS/WSL behavior is deliberately not inferred from these results.
"""
import json
import os
from pathlib import Path
import pty
import socket
import stat
import subprocess
import sys
import tempfile
import uuid


def check(path, image):
    prefix = "asagiri-reported-" + uuid.uuid4().hex[:10]
    containers, images = [], []
    with tempfile.TemporaryDirectory(prefix=prefix) as directory:
        env = dict(os.environ, DOCKER_CONFIG=directory)
        env.pop("DOCKER_CONTEXT", None)

        def cli(*args, data=None):
            result = subprocess.run(
                ["docker", "--host", "unix://" + path, *args], env=env,
                input=data, capture_output=True, text=True, timeout=90)
            assert result.returncode == 0, (args, result.stdout, result.stderr)
            return result.stdout.strip()

        try:
            version = json.loads(cli("version", "--format", "{{json .Server}}"))
            assert version["KernelVersion"] == os.uname().release, version
            print("PASS #79 Linux reports its actual kernel", flush=True)

            assert cli("run", "--rm", image, "cat",
                       "/proc/sys/net/ipv4/ip_unprivileged_port_start") == "0"
            assert cli("run", "--rm", "--sysctl",
                       "net.ipv4.ip_unprivileged_port_start=1024", image, "cat",
                       "/proc/sys/net/ipv4/ip_unprivileged_port_start") == "1024"
            host_value = Path("/proc/sys/net/ipv4/ip_unprivileged_port_start").read_text().strip()
            assert cli("run", "--rm", "--network", "host", image, "cat",
                       "/proc/sys/net/ipv4/ip_unprivileged_port_start") == host_value
            cli("run", "--rm", "--user", "1000", image, "sh", "-ec",
                "nc -l -p 80 >/dev/null & child=$!; sleep 0.2; kill -0 $child; kill $child")
            assert Path("/proc/sys/net/ipv4/ip_unprivileged_port_start").read_text().strip() == host_value
            print("PASS #18 non-root port binding, explicit override, and host namespace preservation", flush=True)

            assert cli("run", "--rm", "-i", "--user", "1000", image, "sh", "-ec",
                       "read value </proc/self/fd/0; "
                       "printf '%s' \"$value\" >/dev/stdout; "
                       "printf 'stderr-ok' >/dev/stderr; : >/proc/self/fd/2",
                       data="stdin-ok\n") == "stdin-ok"
            print("PASS #98 non-root stdin/stdout/stderr can be reopened", flush=True)

            dev_before = [(os.stat(p).st_mode, os.stat(p).st_rdev)
                          for p in ("/dev/ptmx", "/dev/pts/ptmx")]
            cli("run", "--rm", "--privileged", image, "sh", "-ec",
                "test -c /dev/null; test -c /dev/ptmx; test -c /dev/pts/ptmx")
            assert [(os.stat(p).st_mode, os.stat(p).st_rdev)
                    for p in ("/dev/ptmx", "/dev/pts/ptmx")] == dev_before
            master, slave = pty.openpty()
            try:
                assert stat.S_ISCHR(os.fstat(slave).st_mode)
                os.write(slave, b"host-pty-ok\n")
                assert b"host-pty-ok" in os.read(master, 64)
            finally:
                os.close(master)
                os.close(slave)
            print("PASS #26 privileged start preserves the test host's PTY devices", flush=True)

            source = Path(directory) / "source"
            source.mkdir()
            (source / "sentinel").write_text("through-symlink")
            link = Path(directory) / "linked"
            link.symlink_to(source, target_is_directory=True)
            assert cli("run", "--rm", "-v", f"{link}:/data:ro", image,
                       "cat", "/data/sentinel") == "through-symlink"
            print("PASS #170 bind source symlink resolves on Linux (Mac path mapping remains separate)", flush=True)

            prep = prefix + "-prepare"
            containers.append(prep)
            cli("run", "--name", prep, image, "sh", "-ec",
                "mkdir -p /app/deps; echo image-data >/app/deps/origin")
            derived = prefix + ":copyup"
            images.append(derived)
            cli("commit", prep, derived)
            (source / "deps").mkdir()
            (source / "deps/origin").write_text("host-data")
            assert cli("run", "--rm", "-v", f"{source}:/app",
                       "--mount", "type=volume,destination=/app/deps",
                       derived, "cat", "/app/deps/origin") == "image-data"
            print("PASS #30 nested anonymous volume copies image data rather than host bind data", flush=True)

            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                port = probe.getsockname()[1]
            for suffix in ("first", "replacement"):
                name = prefix + "-" + suffix
                containers.append(name)
                cli("run", "-d", "--name", name, "-p",
                    f"127.0.0.1:{port}:80", image, "sleep", "120")
                cli("kill", name)
                cli("rm", name)
                containers.remove(name)
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", port))
            print("PASS #170 killed/removed containers release their published port", flush=True)
        finally:
            for name in reversed(containers):
                subprocess.run(["docker", "--host", "unix://" + path, "rm", "-fv", name],
                               env=env, capture_output=True, timeout=30)
            for name in reversed(images):
                subprocess.run(["docker", "--host", "unix://" + path, "image", "rm", name],
                               env=env, capture_output=True, timeout=30)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2])
