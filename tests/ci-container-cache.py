#!/usr/bin/env python3
"""Cache recovery and content verification, without requiring a registry."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "container_cache", Path(__file__).resolve().parents[1] / "scripts/ci/container-cache.py")
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)
EXPECTED = "sha256:" + "a" * 64
RECORD = {"image": "example.invalid/rust@sha256:" + "b" * 64, "config": {"amd64": EXPECTED}}


class CacheTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.archive = self.root / f"rust-amd64-{EXPECTED[7:]}.tar"
        self.calls = []

    def docker(self, *args, check=True):
        self.calls.append(args)
        if args[:2] == ("image", "save"):
            Path(args[3]).write_bytes(b"archive")
        return subprocess.CompletedProcess(args, 0, "", "")

    def test_warm_cache_is_verified_without_a_network_pull(self):
        self.archive.write_bytes(b"archive")
        with patch.object(cache, "present", side_effect=[None, EXPECTED]), patch.object(cache, "docker", self.docker):
            self.assertEqual(cache.prepare("rust", RECORD, "amd64", self.root), EXPECTED)
        self.assertEqual([c[:2] for c in self.calls], [("image", "load")])

    def test_wrong_cached_image_is_replaced_with_the_pinned_image(self):
        self.archive.write_bytes(b"wrong image")
        with patch.object(cache, "present", side_effect=[None, None, EXPECTED]), patch.object(cache, "docker", self.docker):
            self.assertEqual(cache.prepare("rust", RECORD, "amd64", self.root), EXPECTED)
        self.assertIn(("pull", "--platform", "linux/amd64", RECORD["image"]), self.calls)
        self.assertEqual(self.archive.read_bytes(), b"archive")

    def test_corrupt_archive_falls_back_to_registry(self):
        self.archive.write_bytes(b"broken")
        def docker(*args, check=True):
            if args[:2] == ("image", "load"):
                return subprocess.CompletedProcess(args, 1, "", "invalid tar")
            return self.docker(*args, check=check)
        with patch.object(cache, "present", side_effect=[None, EXPECTED]), patch.object(cache, "docker", docker):
            cache.prepare("rust", RECORD, "amd64", self.root)
        self.assertEqual(self.archive.read_bytes(), b"archive")

    def test_wrong_registry_image_is_not_accepted_or_saved(self):
        with patch.object(cache, "present", return_value=False), patch.object(cache, "docker", self.docker):
            with self.assertRaisesRegex(RuntimeError, "does not match"):
                cache.prepare("rust", RECORD, "amd64", self.root)
        self.assertFalse(self.archive.exists())
        self.assertFalse(any(c[:2] == ("image", "save") for c in self.calls))

    def test_both_docker_image_stores_verify_the_platform(self):
        import json
        manifest = RECORD["image"].split("@")[1]
        for identifier, descriptor in [(EXPECTED, {}), (manifest, {"digest": manifest})]:
            image = {"Id": identifier, "Os": "linux", "Architecture": "amd64", "Descriptor": descriptor}
            with patch.object(cache, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(image), "")):
                self.assertEqual(cache.present(RECORD, "amd64"), identifier)
            image["Architecture"] = "arm64"
            with patch.object(cache, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(image), "")):
                self.assertIsNone(cache.present(RECORD, "amd64"))


if __name__ == "__main__":
    unittest.main()
