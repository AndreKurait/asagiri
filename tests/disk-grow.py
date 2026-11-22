#!/usr/bin/env python3
"""VM disk growth on macOS, in a disposable daemon with its own data and state.

cargo xtask host && cargo xtask guest
python3 tests/disk-grow.py [target/release/asagiri] [--guest-dir guest/out] --image-archive ALPINE_TAR --image-id SHA256_ID
"""
import argparse
import os
from pathlib import Path
import platform
import plistlib
import secrets
import shutil
import subprocess
import tempfile
import time


def main():
    if platform.system() != "Darwin":
        print("SKIP VM disk growth: macOS only")
        return
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default=repo / "target/release/asagiri", type=Path)
    parser.add_argument("--guest-dir", default=repo / "guest/out", type=Path)
    parser.add_argument("--image-archive", required=True, type=Path, help="docker save archive containing Alpine for the guest architecture")
    parser.add_argument("--image-id", required=True, help="expected sha256 image ID from the archive")
    args = parser.parse_args()
    feature = "disk" + secrets.token_hex(4)
    env = os.environ.copy()
    for key in ["DOCKER_CONTEXT", "DOCKER_HOST", "ASAGIRI_DISK_GB"]:
        env.pop(key, None)
    with tempfile.TemporaryDirectory(prefix="asagiri-disk-", dir="/tmp") as directory:
        work = Path(directory).resolve()
        env["HOME"] = str(work)
        contents = work / "Asagiri Disk.app/Contents"
        (contents / "MacOS").mkdir(parents=True)
        binary = contents / "MacOS/asagiri"
        shutil.copy2(args.binary.resolve(), binary)
        (contents / "Info.plist").write_bytes(plistlib.dumps({
            "CFBundleIdentifier": "dev.asagiri.app." + feature,
            "CFBundleExecutable": "asagiri",
        }))
        socket = work / "docker.sock"
        env.update(ASAGIRI_SOCKET=str(socket), ASAGIRI_DATA_DIR=str(work / "data"),
                   ASAGIRI_GUEST_DIR=str(args.guest_dir.resolve()), ASAGIRI_NO_FINDER="1",
                   ASAGIRI_IDLE_SECS="0", ASAGIRI_MEMORY_MIB="1024")
        proc = None
        log = (work / "daemon.log").open("w+")

        def docker(*command):
            return subprocess.run(["docker", "--host", "unix://" + str(socket), *command],
                                  env=env, capture_output=True, text=True, check=True,
                                  timeout=90).stdout.strip()

        def stop():
            nonlocal proc
            if proc is not None:
                proc.terminate()
                try:
                    proc.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
                    raise AssertionError("daemon did not stop cleanly")
                proc = None

        def start(size):
            nonlocal proc
            settings = dict(env)
            if size is not None:
                settings["ASAGIRI_DISK_GB"] = str(size)
            proc = subprocess.Popen([str(binary), "daemon"], env=settings, stdout=log, stderr=log)
            for _ in range(100):
                assert proc.poll() is None, "daemon exited before becoming ready"
                if socket.exists():
                    try:
                        docker("info")
                        return
                    except subprocess.CalledProcessError:
                        pass
                time.sleep(0.1)
            raise AssertionError("daemon did not become ready")

        def volume(command):
            return docker("run", "--rm", "-v", "disk-grow:/data", "--network", "none", args.image_id, "sh", "-c", command)

        def check_capacity(size, checksum):
            assert volume("sha256sum /data/keep").split()[0] == checksum, "volume data changed"
            blocks, block_size = map(int, volume("stat -f -c '%b %S' /data").split())
            actual = blocks * block_size
            assert size * 0.9 * (1 << 30) < actual <= size * (1 << 30), actual
            image = work / "data/vm/data.img"
            info = image.stat()
            assert info.st_size == size * (1 << 30), info.st_size
            assert info.st_blocks * 512 < info.st_size // 8, "image is no longer sparse"
            print(f"PASS {size} GiB image and filesystem; volume data preserved", flush=True)

        try:
            start(None)
            docker("load", "-i", str(args.image_archive.resolve()))
            checksum = volume("dd if=/dev/urandom of=/data/keep bs=1048576 count=1 2>/dev/null; sha256sum /data/keep").split()[0]
            check_capacity(64, checksum)
            contender = subprocess.run(
                [binary, "raw", "run", args.image_id, "true"],
                env=dict(env, ASAGIRI_DISK_GB="128"), capture_output=True, text=True, timeout=30,
            )
            assert contender.returncode and "lock VM data directory" in contender.stderr, contender.stderr
            check_capacity(64, checksum)
            stop()
            for setting in [128, 96, None]:
                start(setting)
                check_capacity(128, checksum)
                stop()
            print("PASS growth, smaller setting, and removed setting across VM restarts", flush=True)
            # A fresh install must grow its copy of the template as well.
            env["ASAGIRI_DATA_DIR"] = str(work / "fresh")
            start(128)
            docker("load", "-i", str(args.image_archive.resolve()))
            volume("echo fresh > /data/keep")
            fresh = (work / "fresh/vm/data.img").stat()
            blocks, block_size = map(int, volume("stat -f -c '%b %S' /data").split())
            assert fresh.st_size == 128 << 30
            assert blocks * block_size > 115 << 30
            print("PASS new disk starts at configured capacity", flush=True)
        except Exception:
            log.flush()
            print((work / "daemon.log").read_text())
            raise
        finally:
            stop()
            log.close()


if __name__ == "__main__":
    main()
