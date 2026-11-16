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
    command = ["sh", "-c", 'printf "%s:%s\\n" "$(id -u)" "$(id -g)"; grep "^Groups:" /proc/self/status']

    def run(*args):
        return subprocess.run(docker + list(args), check=True, capture_output=True,
                              text=True, timeout=30).stdout.strip()

    def identity(actual, expected, operation):
        lines = actual.splitlines()
        assert len(lines) == 2 and lines[0] == expected, (operation, actual, expected)
        assert lines[1].startswith("Groups:"), (operation, actual)
        groups = {int(value) for value in lines[1].split()[1:]}
        assert int(expected.split(":")[1]) in groups, (operation, actual, "primary group missing")
        return groups

    parent = run("run", "-d", "--network", "none", image, "sleep", "300")
    try:
        for user, expected in cases:
            options = ["--user", user] if user else []
            actual = run("run", "--rm", "--network", "none", *options, image, *command)
            identity(actual, expected, ("run", user))
            actual = run("exec", *options, parent, *command)
            identity(actual, expected, ("exec", user))
            print("PASS run and exec user", repr(user), "resolve to", expected, "with primary supplementary group", flush=True)
        groups = identity(run("run", "--rm", "--network", "none", "--user", "1001:1002",
                              "--group-add", "2000", "--group-add", "1002",
                              image, *command), "1001:1002", "explicit group-add")
        assert groups == {1002, 2000}, groups
        print("PASS explicit primary group and group-add memberships", flush=True)
    finally:
        run("rm", "-f", parent)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
