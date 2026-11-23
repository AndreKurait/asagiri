#!/usr/bin/env python3
"""Measure three separate private Linux engines on the same runner."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("reference", ROOT / "scripts/reference-bench.py")
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)
OWNER = "dev.asagiri.reference-run"


def run(*args, **kwargs):
    return subprocess.run([str(a) for a in args], check=True, **kwargs)


def checksum(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=400)
    parser.add_argument("--native", action="store_true",
                        help="Measure the GNU package engine in the pinned GNU host image")
    args = parser.parse_args()
    if platform.system() != "Linux":
        raise RuntimeError("The private runner requires Linux; reference-bench.py also accepts macOS engine sockets")
    bins, output = args.binaries.resolve(), args.out.resolve()
    if shutil.disk_usage(output.parent).free < 3 * 1024**3:
        raise RuntimeError("Build guard: fewer than 3 GiB free")
    output.mkdir(mode=0o700)
    for line in (bins / "SHA256SUMS").read_text().splitlines():
        digest, name = line.split()
        if Path(name).name != name or checksum(bins / name) != digest:
            raise RuntimeError("Asagiri binary checksum mismatch")
    commit = (bins / "SOURCE_COMMIT").read_text().strip()
    # The benchmark runs as root to access its private engine sockets.
    # Trust only this checkout for this read, without changing Git settings.
    head = run("git", "-c", "safe.directory=" + str(ROOT), "-C", ROOT,
               "rev-parse", "HEAD", text=True, capture_output=True).stdout.strip()
    if commit != head:
        raise RuntimeError("Compile the measured Asagiri binaries from this checkout")
    if args.native:
        elf = json.loads((bins / "native-elf.json").read_text())
        assert (elf["schema"], elf["status"], elf["source"]) == (
            "asagiri.native-linux-elf.v1", "passed", head)
        assert elf["engine"]["sha256"] == checksum(bins / "asagiri") and elf["engine"]["interpreter"]
        assert elf["init"]["sha256"] == checksum(bins / "asagiri-init") and not elf["init"]["interpreter"]
    images = json.loads((ROOT / "scripts/ci/reference-images.json").read_text())
    nonce = uuid.uuid4().hex[:16]
    names = {engine: "asagiri-reference-" + nonce + "-" + engine.lower()
             for engine in ("Asagiri", "Docker", "Podman")}
    ids = {}
    with tempfile.TemporaryDirectory(prefix="asagiri-reference-") as temporary:
        work = Path(temporary)
        folders = {engine: work / engine.lower() for engine in names}
        for folder in folders.values():
            folder.mkdir(mode=0o700)
        # Use the kernel overlay driver in this disposable Podman store.
        # Its container image otherwise selects a FUSE mount helper.
        storage = work / "storage.conf"
        storage.write_text('[storage]\ndriver="overlay"\nrunroot="/run/containers/storage"\n'
                           'graphroot="/var/lib/containers/storage"\n[storage.options.overlay]\nmount_program=""\n')
        try:
            common = ("--privileged", "--cgroupns=private", "--tmpfs", "/run", "--tmpfs", "/tmp",
                      "--label", OWNER + "=" + nonce)
            commands = {
                "Asagiri": [*common,
                            # The benchmark mounts binaries directly, bypassing package dependencies.
                            "--mount", "type=bind,src=/etc/ssl/certs/ca-certificates.crt,"
                            "dst=/run/reference-ca.pem,readonly",
                            "-e", "SSL_CERT_FILE=/run/reference-ca.pem", "-v", str(bins) + ":/opt/asagiri:ro",
                            "-v", str(folders["Asagiri"]) + ":/run/asagiri",
                            "-v", "/var/lib/asagiri", "-v", "/test-store",
                            "-e", "ASAGIRI_SOCKET=/run/asagiri/docker.sock",
                            "-e", "ASAGIRI_DATA_DIR=/test-store", "-e", "ASAGIRI_TEST_ENGINE=1",
                            "--entrypoint", "/opt/asagiri/asagiri",
                            images["asagiri_native_host" if args.native else "asagiri_host"], "daemon"],
                "Docker": [*common, "-e", "DOCKER_TLS_CERTDIR=",
                           "-v", str(folders["Docker"]) + ":/sockets", "-v", "/var/lib/docker",
                           images["docker"], "dockerd", "--host=unix:///sockets/docker.sock"],
                "Podman": [*common, "--user", "0", "-v", str(folders["Podman"]) + ":/sockets",
                           "-v", "/var/lib/containers", "-v", str(storage) + ":/etc/containers/storage.conf:ro",
                           "--entrypoint", "/usr/bin/podman", images["podman"],
                           "--cgroup-manager=cgroupfs", "--storage-driver=overlay", "--events-backend=file",
                           "system", "service", "--time=0", "unix:///sockets/podman.sock"],
            }
            sockets = {"Asagiri": "/run/asagiri/docker.sock", "Docker": "/sockets/docker.sock",
                       "Podman": "/sockets/podman.sock"}
            specs, settings = [], {}
            for engine, command in commands.items():
                run("docker", "run", "-d", "--name", names[engine], *command, timeout=300,
                    stdout=subprocess.DEVNULL)
                info = json.loads(run("docker", "inspect", names[engine], capture_output=True, text=True).stdout)[0]
                if info["Config"]["Labels"].get(OWNER) != nonce or info["HostConfig"]["PortBindings"]:
                    raise RuntimeError("The reference host has unexpected ownership or published ports")
                ids[engine] = info["Id"]
                path = folders[engine] / Path(sockets[engine]).name
                deadline = time.monotonic() + 120
                while time.monotonic() < deadline:
                    if path.exists():
                        # The parent directory is private to this runner user.
                        # Grant this user access to this disposable socket only.
                        run("docker", "exec", "--user", "0", ids[engine], "chown",
                            f"{os.getuid()}:{os.getgid()}", sockets[engine], capture_output=True)
                        run("docker", "exec", "--user", "0", ids[engine], "chmod", "600",
                            sockets[engine], capture_output=True)
                        client = benchmark.UnixHTTP(str(path))
                        try:
                            client.request("GET", "/_ping")
                            response = client.getresponse()
                            if response.status == 200 and response.read(32).strip() == b"OK":
                                break
                        except OSError:
                            pass
                        finally:
                            client.close()
                    time.sleep(.5)
                else:
                    raise RuntimeError(engine + " did not become ready")
                specs.append(engine + "=" + str(path))
                settings[engine] = {"image": info["Config"]["Image"], "image_id": info["Image"],
                                    "privileged": True, "published_ports": False,
                                    "command": info["Config"]["Cmd"]}
            provenance = {
                "repository": os.environ.get("GITHUB_REPOSITORY"), "commit": commit,
                "run_id": os.environ.get("GITHUB_RUN_ID"), "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
                "event": os.environ.get("GITHUB_EVENT_NAME"), "hosts": settings,
                "asagiri_build_kind": "gnu-native" if args.native else "musl-portable",
                "binaries": {name: checksum(bins / name) for name in ("asagiri", "asagiri-init")},
                "harness_sha256": checksum(ROOT / "scripts/reference-bench.py"),
                "runner_sha256": checksum(Path(__file__)), "images_sha256": checksum(ROOT / "scripts/ci/reference-images.json"),
            }
            result = benchmark.measure(specs, args.runs, provenance)
            (output / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")
            (output / "report.md").write_text(benchmark.report(result))
            print(benchmark.report(result), flush=True)
        finally:
            errors = []
            for engine, name in names.items():
                try:
                    inspected = subprocess.run(["docker", "inspect", name], capture_output=True, text=True)
                    if inspected.returncode:
                        continue
                    info = json.loads(inspected.stdout)[0]
                    if (info["Config"]["Labels"] or {}).get(OWNER) != nonce:
                        raise RuntimeError("Refuse to remove an engine belonging to another run")
                    container = info["Id"]
                    logs = subprocess.run(["docker", "logs", container], capture_output=True)
                    (output / (engine.lower() + "-host.log")).write_bytes(logs.stdout + logs.stderr)
                    run("docker", "rm", "-fv", container, stdout=subprocess.DEVNULL)
                except Exception as error:
                    errors.append(f"{engine}: {error}")
            if errors:
                raise RuntimeError("Reference host cleanup failed: " + "; ".join(errors))


if __name__ == "__main__":
    main()
