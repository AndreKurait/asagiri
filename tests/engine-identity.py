#!/usr/bin/env python3
"""Check /info in the daemon's host namespace, optionally after a restart."""
import json
import os
import socket
import subprocess
import sys
import uuid


def check(path):
    def info():
        return json.loads(subprocess.run(
            ["docker", "--host", "unix://" + path, "info", "--format", "{{json .}}"],
            check=True, capture_output=True, timeout=30).stdout)

    first, second = info(), info()
    assert first["Name"] == socket.gethostname(), (first["Name"], socket.gethostname())
    assert first["ID"] == second["ID"] and first["ID"] != "asagiri", (first["ID"], second["ID"])
    uuid.UUID(first["ID"])
    expected = os.environ.get("EXPECTED_ENGINE_ID")
    if expected:
        assert first["ID"] == expected, (first["ID"], expected)
    print("PASS host name and stable daemon ID" + (" after restart" if expected else ""), flush=True)


if __name__ == "__main__":
    check(sys.argv[1])
