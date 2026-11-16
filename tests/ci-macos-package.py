#!/usr/bin/env python3
"""Reject stale preview metadata, executable identities, and update archives."""
import importlib.util
from pathlib import Path
import plistlib
import re
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("macos_package", ROOT / "scripts/ci/macos-package.py")
PACKAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE)


def production_plist(version):
    # Exercise xtask's actual XML literal without building another complete app.
    templates = re.findall(r'r#"(<\?xml[^\n]*\n<!DOCTYPE plist[^\n]*\n<plist.*?</plist>\n)"#',
                           (ROOT / "xtask/src/main.rs").read_text(), re.DOTALL)
    assert len(templates) == 1
    return templates[0].format(plain=version.split("-", 1)[0], version=version).encode()


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.app = self.root / "Asagiri.app"
        self.plist = self.app / "Contents/Info.plist"
        self.binary = self.app / "Contents/MacOS/asagiri"
        self.binary.parent.mkdir(parents=True)
        self.version = "0.1.6-dev.98"
        self.plist.write_bytes(production_plist(self.version))
        self.binary.write_bytes(b"fixture executable identity")
        self.archive = self.root / "app.tar.gz"
        self.pack()

    def pack(self, duplicate=False):
        with tarfile.open(self.archive, "w:gz") as tar:
            for path in (self.plist, self.binary):
                tar.add(path, arcname=str(path.relative_to(self.root)), recursive=False)
            if duplicate:
                tar.add(self.plist, arcname=str(self.plist.relative_to(self.root)), recursive=False)

    def verify(self, arch="arm64", cli=None):
        with patch.object(PACKAGE, "output", side_effect=[arch, cli or f"Asagiri {self.version}"]):
            return PACKAGE.verify(self.app, self.archive, self.version, "arm64")

    def test_actual_xtask_template_retains_stable_and_preview_versions(self):
        for version in ("0.1.5", "0.1.6-dev.98"):
            info = plistlib.loads(production_plist(version))
            PACKAGE.metadata(info, version)
            self.assertEqual(info["AsagiriVersion"], version)

    def test_old_preview_metadata_is_rejected(self):
        info = plistlib.loads(self.plist.read_bytes())
        del info["AsagiriVersion"]
        with self.assertRaisesRegex(ValueError, "AsagiriVersion"):
            PACKAGE.metadata(info, self.version)

    def test_other_release_or_bundle_identity_is_rejected(self):
        for key, wrong in (
            ("AsagiriVersion", "0.1.6-dev.97"),
            ("CFBundleShortVersionString", "0.1.5"),
            ("CFBundleVersion", "0.1.5"),
            ("CFBundleIdentifier", "dev.unrelated.app"),
        ):
            info = plistlib.loads(self.plist.read_bytes())
            info[key] = wrong
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, key):
                PACKAGE.metadata(info, self.version)

    def test_matching_executable_and_archive_are_accepted(self):
        result = self.verify()
        self.assertEqual(result["version"], self.version)
        self.assertEqual(result["archive_sha256"], PACKAGE.digest(self.archive))

    def test_stale_executable_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "different version"):
            self.verify(cli="Asagiri 0.1.6-dev.97")

    def test_wrong_architecture_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "wrong architecture"):
            self.verify(arch="x86_64")

    def test_stale_archived_executable_is_rejected(self):
        self.binary.write_bytes(b"a newer executable")
        with self.assertRaisesRegex(ValueError, "verified app identity"):
            self.verify()

    def test_stale_archived_metadata_is_rejected(self):
        self.plist.write_bytes(self.plist.read_bytes() + b"\n")
        with self.assertRaisesRegex(ValueError, "verified app identity"):
            self.verify()

    def test_duplicate_archive_identity_is_rejected(self):
        self.pack(duplicate=True)
        with self.assertRaisesRegex(ValueError, "repeats or redirects"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
