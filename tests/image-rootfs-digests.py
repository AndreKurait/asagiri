#!/usr/bin/env python3
"""Compare image inspection to hashes of uncompressed saved layer bytes."""
import gzip
import hashlib
import io
import json
import subprocess
import sys
import tarfile
import uuid


def check(path, image):
    docker = ["docker", "--host", "unix://" + path]

    def run(*args, data=None):
        return subprocess.run(docker + list(args), input=data, check=True,
                              capture_output=True, timeout=60).stdout

    tag = "asagiri-diff-id-" + uuid.uuid4().hex[:12] + ":test"
    run("tag", image, tag)
    try:
        saved = run("save", tag)
        with tarfile.open(fileobj=io.BytesIO(saved)) as archive:
            manifest = json.load(archive.extractfile("manifest.json"))[0]
            config = json.load(archive.extractfile(manifest["Config"]))
            expected = []
            for name in manifest["Layers"]:
                blob = archive.extractfile(name).read()
                plain = gzip.decompress(blob) if blob.startswith(b"\x1f\x8b") else blob
                expected.append("sha256:" + hashlib.sha256(plain).hexdigest())
        assert expected, "Use an image with actual filesystem layers"
        assert config["rootfs"]["diff_ids"] == expected, (config["rootfs"], expected)
        for phase in ("pulled", "reloaded"):
            if phase == "reloaded":
                run("image", "rm", tag)
                run("load", data=saved)
            inspected = json.loads(run("image", "inspect", tag))[0]
            assert inspected["RootFS"] == {"Type": "layers", "Layers": expected}, (
                phase, inspected["RootFS"], expected)
            print("PASS", phase, "RootFS.Layers contains uncompressed layer digests", flush=True)
    finally:
        run("image", "rm", tag)


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21")
