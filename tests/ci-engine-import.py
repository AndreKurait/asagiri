#!/usr/bin/env python3
"""Image-content checks for the real two-engine migration fixture."""
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("engine_import", ROOT / "scripts/ci/engine-import.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def tar(files):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for name, data in files:
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    return output.getvalue()


def image(*, blob_paths=False, compressed=False, command="sh", corrupt_layer=False,
          missing_layer=False, duplicate_config=False, unsafe_path=False):
    layer = tar([("payload", b"preserve the actual layer bytes\n")])
    digest = hashlib.sha256(layer).hexdigest()
    config = json.dumps({"os": "linux", "architecture": "amd64", "config": {"Cmd": [command]},
                         "rootfs": {"type": "layers", "diff_ids": ["sha256:" + digest]}}).encode()
    config_name = "blobs/sha256/" + hashlib.sha256(config).hexdigest() if blob_paths else "config.json"
    layer_name = "blobs/sha256/" + digest if blob_paths else "layer/layer.tar"
    if unsafe_path:
        config_name = "../config.json"
    manifest = [{"Config": config_name, "RepoTags": ["fixture:owned"], "Layers": [layer_name]}]
    files = [("manifest.json", json.dumps(manifest).encode()), (config_name, config)]
    if duplicate_config:
        files.append((config_name, config))
    if not missing_layer:
        payload = layer + b"changed" if corrupt_layer else layer
        files.append((layer_name, gzip.compress(payload, mtime=0) if compressed else payload))
    if blob_paths:
        files.extend([("oci-layout", b'{"imageLayoutVersion":"1.0.0"}'), ("index.json", b'{"manifests":[]}')])
    return tar(files)


class ContentIdentity(unittest.TestCase):
    def test_classic_and_blob_layout_preserve_configuration_and_layer_bytes(self):
        first = fixture.archive_identity(image())
        second = fixture.archive_identity(image(blob_paths=True))
        self.assertNotEqual(first["archive_sha256"], second["archive_sha256"])
        fixture.same_image_content(first, second)

    def test_layer_compression_does_not_change_the_content_identity(self):
        first = fixture.archive_identity(image())
        second = fixture.archive_identity(image(blob_paths=True, compressed=True))
        self.assertNotEqual(first["layers"][0]["blob_sha256"], second["layers"][0]["blob_sha256"])
        fixture.same_image_content(first, second)

    def test_configuration_change_is_rejected_even_with_identical_layers(self):
        first = fixture.archive_identity(image())
        second = fixture.archive_identity(image(command="different"))
        self.assertEqual(first["diff_ids"], second["diff_ids"])
        with self.assertRaisesRegex(AssertionError, "config_sha256"):
            fixture.same_image_content(first, second)

    def test_corrupt_layer_cannot_rely_on_the_declared_config_digest(self):
        for compressed in (False, True):
            with self.subTest(compressed=compressed), self.assertRaisesRegex(AssertionError, "contents"):
                fixture.archive_identity(image(corrupt_layer=True, compressed=compressed))

    def test_missing_layer_is_rejected(self):
        with self.assertRaisesRegex(AssertionError, "missing fixture archive member"):
            fixture.archive_identity(image(missing_layer=True))

    def test_duplicate_member_is_rejected(self):
        with self.assertRaisesRegex(AssertionError, "duplicate fixture archive member"):
            fixture.archive_identity(image(duplicate_config=True))

    def test_unsafe_member_is_rejected_without_extraction(self):
        with self.assertRaises(AssertionError):
            fixture.archive_identity(image(unsafe_path=True))


if __name__ == "__main__":
    unittest.main()
