#!/usr/bin/env python3
"""Check native package ELF dependencies and the separate static guest init."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
LOADERS = {"x86_64": "/lib64/ld-linux-x86-64.so.2", "aarch64": "/lib/ld-linux-aarch64.so.1"}
MACHINES = {"x86_64": "Advanced Micro Devices X86-64", "aarch64": "AArch64"}
ALLOWED = {"libc.so.6", "libgcc_s.so.1", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1"}


def inspect(binary, architecture, native):
    def read(*arguments):
        return subprocess.check_output(["readelf", *arguments, str(binary)], text=True,
                                       env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"})
    header, program, dynamic, versions = [read(option) for option in ("-h", "-l", "-d", "--version-info")]
    machine, = re.findall(r"^\s*Machine:\s*(.+)$", header, re.M)
    assert machine == MACHINES[architecture]
    loader = re.findall(r"Requesting program interpreter: ([^\]]+)", program)
    needed = sorted(re.findall(r"\(NEEDED\).*Shared library: \[([^\]]+)\]", dynamic))
    symbols = sorted(set(re.findall(r"\bGLIBC_([A-Za-z0-9_.]+)", versions)))
    if native:
        assert loader == [LOADERS[architecture]], "Native packages require the host GNU loader and NSS"
        assert "libc.so.6" in needed and set(needed) <= ALLOWED | {Path(LOADERS[architecture]).name}, needed
        assert symbols and all(re.fullmatch(r"\d+\.\d+(?:\.\d+)?", name) for name in symbols), symbols
        maximum = json.loads((ROOT / "packaging/linux/native-build.json").read_text())["maximum_glibc"]
        bound = tuple(int(part) for part in maximum.split(".")) + (0,)
        assert all(tuple(int(part) for part in name.split(".")) + (0,) * (3 - len(name.split("."))) <= bound
                   for name in symbols), (maximum, symbols)
    else:
        assert not loader and not needed, "The guest init must remain static"
    with binary.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"sha256": digest, "bytes": binary.stat().st_size, "machine": machine,
            "interpreter": loader, "needed": needed, "glibc_versions": symbols}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--arch", choices=LOADERS, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = {
        "schema": "asagiri.native-linux-elf.v1", "status": "passed",
        "at": datetime.now(timezone.utc).isoformat(), "architecture": args.arch,
        "source": (args.binaries / "SOURCE_COMMIT").read_text().strip(),
        "engine": inspect(args.binaries / "asagiri", args.arch, True),
        "init": inspect(args.binaries / "asagiri-init", args.arch, False),
        "verifier_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "policy_sha256": hashlib.sha256((ROOT / "packaging/linux/native-build.json").read_bytes()).hexdigest(),
    }
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
