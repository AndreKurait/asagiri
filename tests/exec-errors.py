#!/usr/bin/env python3
"""Compare real run/exec failure codes, including scripts and --init."""
import json
import subprocess
import sys
import uuid


def check(path, image):
    docker = ["docker", "--host", "unix://" + path]
    token = "asagiri-exec-errors-" + uuid.uuid4().hex[:10]
    container, volume = token, token + "-scripts"
    made_container = made_volume = False
    results = []

    def command(*args):
        return subprocess.run([*docker, *args], text=True, capture_output=True, timeout=60)

    def require(*args):
        result = command(*args)
        assert result.returncode == 0, (args, result.returncode, result.stdout, result.stderr)
        return result.stdout.strip()

    try:
        require("volume", "create", volume)
        made_volume = True
        setup = ("printf '%s\\n' 'echo no-shebang-ok' > /scripts/nosb; "
                 "printf '%s\\n' '#!/missing-interpreter' 'echo wrong' > /scripts/badsb; "
                 "chmod 755 /scripts/nosb /scripts/badsb")
        require("run", "--rm", "--network", "none", "-v", volume + ":/scripts", image, "sh", "-ec", setup)
        require("run", "-d", "--name", container, "--network", "none", "-v", volume + ":/scripts", image, "sleep", "600")
        made_container = True
        cases = [
            ("run file", 126, "permission denied", ["run", "--rm", "--network", "none", image, "/etc/passwd"]),
            ("run directory", 126, "is a directory", ["run", "--rm", "--network", "none", image, "/etc"]),
            ("run missing", 127, "no such file", ["run", "--rm", "--network", "none", image, "/no/such/file"]),
            ("exec file", 126, "permission denied", ["exec", container, "/etc/passwd"]),
            ("exec directory", 126, "is a directory", ["exec", container, "/etc"]),
            ("exec missing", 127, "no such file", ["exec", container, "/no/such/file"]),
        ]
        for filename, message in [("nosb", "exec format error"), ("badsb", "no such file")]:
            cases.append(("run " + filename, 255, message,
                          ["run", "--rm", "--network", "none", "-v", volume + ":/scripts", image, "/scripts/" + filename]))
            cases.append(("exec " + filename, 255, message, ["exec", container, "/scripts/" + filename]))
        cases.extend([
            ("init no shebang", 0, "no-shebang-ok",
             ["run", "--rm", "--init", "--network", "none", "-v", volume + ":/scripts", image, "/scripts/nosb"]),
            ("init missing interpreter", 127, "no such file",
             ["run", "--rm", "--init", "--network", "none", "-v", volume + ":/scripts", image, "/scripts/badsb"]),
        ])
        for name, expected, message, args in cases:
            result = command(*args)
            text = result.stdout + result.stderr
            # Docker's tini capitalizes strerror; compare the error's meaning
            # while still requiring the exact run/exec exit status.
            ok = result.returncode == expected and message in text.lower()
            results.append({"case": name, "exit": result.returncode, "expected": expected, "passed": ok})
            print(("PASS " if ok else "FAIL ") + name + ": " + str(result.returncode), flush=True)
            if not ok:
                print(text[:1200], flush=True)
        assert all(item["passed"] for item in results), json.dumps(results)
        print(json.dumps({"probe": "exec-errors", "cases": results}), flush=True)
    finally:
        errors = []
        if made_container:
            result = command("rm", "-f", container)
            if result.returncode:
                errors.append(result.stderr)
        if made_volume:
            result = command("volume", "rm", volume)
            if result.returncode:
                errors.append(result.stderr)
        if errors:
            print("Cleanup failed:", errors, file=sys.stderr)
            if sys.exc_info()[0] is None:
                raise RuntimeError("Could not remove exec-errors test resources")


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
