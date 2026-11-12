#!/usr/bin/env python3
"""Reject corrupt cached executables and unsafe browser archives."""
import hashlib
import importlib.util
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location("browser", Path(__file__).resolve().parents[1] / "scripts/ci/browser.py")
browser = importlib.util.module_from_spec(spec)
spec.loader.exec_module(browser)


class BrowserTests(unittest.TestCase):
    def test_valid_cache_needs_no_network(self):
        with tempfile.TemporaryDirectory() as folder:
            archive = Path(folder) / "browser.zip"
            archive.write_bytes(b"verified download")
            metadata = {"size": archive.stat().st_size, "sha256": hashlib.sha256(archive.read_bytes()).hexdigest()}
            with patch.object(browser, "urlopen", side_effect=AssertionError("unexpected network request")):
                browser.download(archive, metadata)

    def test_rejects_corrupt_cache_even_when_size_matches(self):
        with tempfile.TemporaryDirectory() as folder:
            archive = Path(folder) / "browser.zip"
            archive.write_bytes(b"bad")
            self.assertFalse(browser.verified(archive, {"size": 3, "sha256": hashlib.sha256(b"yes").hexdigest()}))

    def test_rejects_traversal_and_links_before_extracting(self):
        for name, mode in [("../escape", stat.S_IFREG), ("/escape", stat.S_IFREG),
                           ("browser\\escape", stat.S_IFREG), ("link", stat.S_IFLNK)]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as folder:
                archive = Path(folder) / "browser.zip"
                member = zipfile.ZipInfo(name)
                member.external_attr = (mode | 0o755) << 16
                with zipfile.ZipFile(archive, "w") as output:
                    output.writestr("first-file", "must not be extracted")
                    output.writestr(member, "payload")
                destination = Path(folder) / "extracted"
                with self.assertRaises(ValueError):
                    browser.extract(archive, destination)
                self.assertFalse(destination.exists())

    def test_preserves_executable_permissions_without_special_bits(self):
        with tempfile.TemporaryDirectory() as folder:
            archive = Path(folder) / "browser.zip"
            member = zipfile.ZipInfo("browser")
            member.external_attr = (stat.S_IFREG | 0o6755) << 16
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr(member, "binary")
            destination = Path(folder) / "extracted"
            browser.extract(archive, destination)
            self.assertEqual(stat.S_IMODE((destination / "browser").stat().st_mode), 0o755)


if __name__ == "__main__":
    unittest.main()
