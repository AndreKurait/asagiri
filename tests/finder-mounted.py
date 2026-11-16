#!/usr/bin/env python3
"""Check the installed Mac's real Finder mount using only owned test resources."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid


def check(args):
    assert sys.platform == "darwin", "requires the real macOS NFS client"
    assert Path(args.socket).is_socket(), args.socket
    mounted = Path(args.mount)
    assert mounted.is_dir() and os.path.ismount(mounted), "the existing Asagiri Finder mount must be available"
    token = "asagiri-files-" + uuid.uuid4().hex
    name = token + "-container"
    volume = token + "-volume"
    owned_container = None
    owned_volume = False
    record = {"probe": "finder-mounted", "name": name, "checks": []}

    with tempfile.TemporaryDirectory(prefix=token + "-", dir=Path.home()) as directory:
        root = Path(directory)
        bind = root / "bind"
        bind.mkdir()
        payload = (token + " from host\n").encode()
        (bind / "host.txt").write_bytes(payload)
        config = root / "docker-config"
        config.mkdir(mode=0o700)
        env = {**os.environ, "DOCKER_CONFIG": str(config), "DOCKER_HOST": "unix://" + args.socket}
        env.pop("DOCKER_CONTEXT", None)
        command = ["docker", "--host", "unix://" + args.socket, "--config", str(config)]

        def cli(*items, check=True):
            result = subprocess.run([*command, *items], env=env, capture_output=True, text=True, timeout=60)
            if check:
                assert result.returncode == 0, (items, result.returncode, result.stdout, result.stderr)
            return result

        def await_file(path, expected):
            deadline = time.monotonic() + 20
            last = None
            while True:
                try:
                    last = path.read_bytes()
                    if last == expected:
                        return
                except OSError as error:
                    last = repr(error)
                assert time.monotonic() < deadline, (str(path), expected, last)
                time.sleep(0.2)

        try:
            version = json.loads(cli("version", "--format", "{{json .Server}}").stdout)
            assert "asagiri" in version.get("Version", "").lower(), version
            record["server"] = version
            assert cli("volume", "create", volume).stdout.strip() == volume
            owned_volume = True
            created = cli("create", "--name", name, "--network", "none",
                          "--mount", f"type=volume,source={volume},target=/data",
                          "--mount", f"type=bind,source={bind},target=/bind,readonly",
                          args.image, "sh", "-ec",
                          "printf '%s' volume-payload > /data/volume.txt; "
                          "mkdir -p /plain; printf '%s' runtime-payload > /plain/runtime.txt; "
                          "exec sleep 300").stdout.strip()
            assert len(created) == 64 and all(c in "0123456789abcdef" for c in created), created
            owned_container = created
            cli("start", owned_container)
            visible = mounted / name
            for relative, expected in (("data/volume.txt", b"volume-payload"),
                                       ("bind/host.txt", payload),
                                       ("plain/runtime.txt", b"runtime-payload")):
                path = visible / relative
                await_file(path, expected)
                assert path.name in [entry.name for entry in path.parent.iterdir()]
                assert path.stat().st_size == len(expected)
                record["checks"].append({"file": relative, "bytes": len(expected)})
                print("PASS Finder reads " + relative, flush=True)
            cli("exec", owned_container, "sh", "-ec", "printf '%s' updated-volume > /data/volume.txt")
            await_file(visible / "data/volume.txt", b"updated-volume")
            record["checks"].append({"fresh_volume_write": True})
            cli("stop", "--time", "0", owned_container)
            deadline = time.monotonic() + 20
            while visible.exists():
                assert time.monotonic() < deadline, "stopped container remains browsable"
                time.sleep(0.2)
            record["checks"].append({"stopped_container_unavailable": True})
        finally:
            primary_failure = sys.exc_info()[0] is not None
            errors = []
            if owned_container is not None:
                result = cli("rm", "-f", owned_container, check=False)
                if result.returncode:
                    errors.append(result.stderr)
            if owned_volume:
                result = cli("volume", "rm", volume, check=False)
                if result.returncode:
                    errors.append(result.stderr)
            record["cleanup_errors"] = errors
            print(json.dumps(record), flush=True)
            if errors and not primary_failure:
                raise AssertionError(errors)
    print("Finder mounted-volume checks passed", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("mount")
    parser.add_argument("image", nargs="?", default="alpine:3.21")
    check(parser.parse_args())
