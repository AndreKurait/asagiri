#!/usr/bin/env python3
"""Check Docker user/group defaults for container startup and exec."""
import subprocess
import sys


def check(path, image):
    docker = ["docker", "--host", "unix://" + path]
    cases = [
        ("", "0:0"),
        ("root", "0:0"),
        ("1001", "1001:0"),
        ("1001:1002", "1001:1002"),
        ("1001:", "1001:0"),
        (":1002", "0:1002"),
        (":", "0:0"),
        ("nobody", "65534:65534"),
        ("65534", "65534:65534"),
        ("root:nobody", "0:65534"),
        ("1001:nobody", "1001:65534"),
    ]
    command = ["sh", "-c", 'printf "%s:%s\\n" "$(id -u)" "$(id -g)"']

    def run(*args):
        return subprocess.run(docker + list(args), check=True, capture_output=True,
                              text=True, timeout=30).stdout.strip()

    parent = run("run", "-d", "--network", "none", image, "sleep", "300")
    try:
        for user, expected in cases:
            options = ["--user", user] if user else []
            actual = run("run", "--rm", "--network", "none", *options, image, *command)
            assert actual == expected, ("run", user, actual, expected)
            actual = run("exec", *options, parent, *command)
            assert actual == expected, ("exec", user, actual, expected)
            print("PASS run and exec user", repr(user), "resolve to", expected, flush=True)
    finally:
        run("rm", "-f", parent)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
