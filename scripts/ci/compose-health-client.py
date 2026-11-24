#!/usr/bin/env python3
"""Fetch the checksum-pinned Compose client used by the live health suite."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    assert sys.platform in ("linux", "darwin"), "Unsupported Compose client platform"
    source = Path(__file__).resolve().parents[2] / "tests/fixtures/compose/client.json"
    pins = json.loads(source.read_text())
    asset = pins[sys.platform][platform.machine()]
    output = args.output.resolve()
    output.mkdir(mode=0o755, parents=False, exist_ok=False)
    binary = output / "docker-compose"
    url = f"https://github.com/docker/compose/releases/download/v{pins['version']}/{asset['name']}"
    digest = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(url, timeout=60) as response, binary.open("xb") as target:
        while block := response.read(128 << 10):
            size += len(block)
            assert size <= asset["size"], "Compose download exceeds the pinned size"
            digest.update(block)
            target.write(block)
    assert size == asset["size"] and digest.hexdigest() == asset["sha256"], "Compose download differs from its pin"
    binary.chmod(0o755)
    version = subprocess.check_output([str(binary), "version", "--short"], text=True, timeout=20).strip()
    assert version.removeprefix("v") == pins["version"], version
    (output / "client.json").write_text(json.dumps({
        "version": version, "url": url, "size": size, "sha256": digest.hexdigest(),
        "pins_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
        "downloader_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
    }, indent=2) + "\n")
    print(output)


if __name__ == "__main__":
    main()
