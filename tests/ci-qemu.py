#!/usr/bin/env python3
"""Check that incomplete source bundles cannot pass release verification."""
import copy
import csv
import importlib.util
import json
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("qemu_build", ROOT / "packaging/qemu/build.py")
qemu = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qemu)
BUNDLE_SPEC = importlib.util.spec_from_file_location("qemu_bundle", ROOT / "packaging/qemu/bundle.py")
bundle = importlib.util.module_from_spec(BUNDLE_SPEC)
BUNDLE_SPEC.loader.exec_module(bundle)


class Sources(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.legal = self.root / "legal-info"
        self.legal.mkdir()
        self.archive = self.root / "buildroot.tar.xz"
        self.archive.write_bytes(b"test-only Buildroot source")
        pins = copy.deepcopy(qemu.PINS)
        pins["buildroot"]["sha256"] = qemu.sha(self.archive)
        change = patch.object(qemu, "PINS", pins)
        change.start()
        self.addCleanup(change.stop)
        self.rows = []
        for name, version in (("qemu", pins["qemu"]["version"]), ("musl", "1.2.6"), ("gcc-final", "15.2.0")):
            archive = name + ".tar.xz"
            folder = self.legal / "sources" / (name + "-" + version)
            folder.mkdir(parents=True)
            (folder / archive).write_bytes(b"source")
            license_dir = self.legal / "licenses" / folder.name
            license_dir.mkdir(parents=True)
            (license_dir / "COPYING").write_text("license")
            self.rows.append({"PACKAGE": name, "VERSION": version, "LICENSE": "GPL-2.0",
                              "LICENSE FILES": "COPYING", "SOURCE ARCHIVE": archive})
        self.warning = "WARNING: the Buildroot source code has not been saved\n"
        self.write()

    def write(self):
        for filename, rows in (("manifest.csv", self.rows), ("host-manifest.csv", [
            {"PACKAGE": "buildroot", "VERSION": qemu.PINS["buildroot"]["version"], "LICENSE": "GPL-2.0+",
             "LICENSE FILES": "COPYING", "SOURCE ARCHIVE": "not saved"},
        ])):
            with (self.legal / filename).open("w", newline="") as file:
                writer = csv.DictWriter(file, fieldnames=list(self.rows[0]))
                writer.writeheader()
                writer.writerows(rows)
        (self.legal / "README").write_text(self.warning)
        hashes = self.legal / "legal-info.sha256"
        hashes.unlink(missing_ok=True)
        hashes.write_text("".join(qemu.sha(p) + "  " + str(p.relative_to(self.legal)) + "\n"
                                 for p in sorted(self.legal.rglob("*")) if p.is_file()))

    def test_complete_source_with_explicit_buildroot_archive(self):
        self.assertEqual(len(qemu.legal_sources(self.legal, self.archive)), 3)

    def test_missing_static_library_source_rejected(self):
        (self.legal / "sources/musl-1.2.6/musl.tar.xz").unlink()
        self.write()  # A self-consistent manifest still cannot omit a dependency.
        with self.assertRaisesRegex(RuntimeError, "Source archive was not collected: musl"):
            qemu.legal_sources(self.legal, self.archive)

    def test_unknown_license_rejected(self):
        self.rows[1]["LICENSE"] = "unknown"
        self.write()
        with self.assertRaisesRegex(RuntimeError, "Missing license"):
            qemu.legal_sources(self.legal, self.archive)

    def test_additional_collection_warning_rejected(self):
        self.warning += "WARNING: a dependency source could not be saved\n"
        self.write()
        with self.assertRaisesRegex(RuntimeError, "Unresolved source"):
            qemu.legal_sources(self.legal, self.archive)

    def test_changed_buildroot_source_rejected(self):
        self.archive.write_bytes(b"changed")
        with self.assertRaisesRegex(RuntimeError, "Missing verified Buildroot"):
            qemu.legal_sources(self.legal, self.archive)

    def test_changed_collected_source_rejected(self):
        (self.legal / "sources/musl-1.2.6/musl.tar.xz").write_bytes(b"changed")
        with self.assertRaises(Exception):
            qemu.legal_sources(self.legal, self.archive)

    def test_dynamic_emulator_rejected_before_execution(self):
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", header, 18, 62)
        struct.pack_into("<Q", header, 32, 64)
        struct.pack_into("<HH", header, 54, 56, 1)
        program = bytearray(56)
        struct.pack_into("<I", program, 0, 3)
        binary = self.root / "qemu-aarch64"
        binary.write_bytes(header + program)
        with self.assertRaisesRegex(RuntimeError, "dynamic interpreter"):
            qemu.static_x86_64(binary)

    def test_shared_library_dependency_rejected_without_a_loader_segment(self):
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", header, 18, 62)
        struct.pack_into("<Q", header, 32, 64)
        struct.pack_into("<HH", header, 54, 56, 1)
        program = struct.pack("<IIQQQQQQ", 2, 0, 120, 0, 0, 16, 16, 8)
        binary = self.root / "qemu-dynamic"
        binary.write_bytes(header + program + struct.pack("<QQ", 1, 0))
        with self.assertRaisesRegex(RuntimeError, "shared libraries"):
            qemu.static_x86_64(binary)

    def test_cached_binary_substitution_rejected(self):
        output = self.root / "products"
        output.mkdir()
        artifacts = {}
        for name in ("asagiri-qemu-guest.tar.gz", "asagiri-qemu-source.tar.xz"):
            (output / name).write_bytes(b"original")
            artifacts[name] = qemu.sha(output / name)
        record = {"recipe": {name: qemu.sha(qemu.RECIPE / name) for name in qemu.RECIPE_FILES},
                  "sources": qemu.PINS, "artifacts": artifacts}
        (output / "build.json").write_text(json.dumps(record))
        qemu.verify(output)
        (output / "asagiri-qemu-guest.tar.gz").write_bytes(b"substituted")
        with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
            qemu.verify(output)


class Bundles(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.guest = self.root / "qemu"
        self.guest.mkdir()
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", header, 18, 62)
        struct.pack_into("<Q", header, 32, 64)
        struct.pack_into("<HH", header, 54, 56, 1)
        (self.guest / "qemu-aarch64").write_bytes(header + struct.pack("<IIQQQQQQ", 1, 0, 0, 0, 0, 0, 0, 0))
        (self.guest / "qemu-aarch64").chmod(0o755)
        self.record = {"schema": 1, "sources": qemu.PINS,
                       "recipe": {name: qemu.sha(qemu.RECIPE / name) for name in qemu.RECIPE_FILES},
                       "emulator_sha256": qemu.sha(self.guest / "qemu-aarch64")}
        (self.guest / "build.json").write_text(json.dumps(self.record))
        for name in ("COPYING", "README.md", "SOURCE.md"):
            (self.guest / name).write_text("fixture text\n")
        licenses = self.guest / "licenses" / ("qemu-" + qemu.PINS["qemu"]["version"])
        licenses.mkdir(parents=True)
        (licenses / "COPYING").write_text("fixture GPL text\n")
        with (self.guest / "manifest.csv").open("w", newline="") as output:
            writer = csv.DictWriter(output, fieldnames=["PACKAGE", "VERSION", "LICENSE", "LICENSE FILES"])
            writer.writeheader()
            writer.writerow({"PACKAGE": "qemu", "VERSION": qemu.PINS["qemu"]["version"],
                             "LICENSE": "GPL-2.0", "LICENSE FILES": "COPYING"})
        qemu.checksums(self.guest)
        self.products = self.root / "products"
        self.products.mkdir()
        (self.products / "asagiri-qemu-source.tar.xz").write_bytes(b"fixture corresponding source")
        self.archive()

    def archive(self, malicious=None):
        with tarfile.open(self.products / "asagiri-qemu-guest.tar.gz", "w:gz") as output:
            output.add(self.guest, arcname="qemu")
            if malicious:
                output.addfile(malicious)
        self.outer = {**self.record, "artifacts": {
            name: qemu.sha(self.products / name)
            for name in ("asagiri-qemu-guest.tar.gz", "asagiri-qemu-source.tar.xz")}}
        (self.products / "build.json").write_text(json.dumps(self.outer))

    def test_install_checks_all_contents_without_running_the_emulator(self):
        destination = self.root / "installed"
        with patch.object(bundle.build, "run", side_effect=AssertionError("Must not execute the bundle")):
            bundle.install(self.products, destination)
            self.assertEqual(bundle.verify(destination), self.record)
            bundle.install(self.products, destination)
        self.assertEqual((destination / "qemu-aarch64").stat().st_mode & 0o777, 0o755)

    def test_missing_license_rejected_even_with_rewritten_checksums(self):
        next((self.guest / "licenses").rglob("COPYING")).unlink()
        qemu.checksums(self.guest)
        with self.assertRaisesRegex(RuntimeError, "Missing target license"):
            bundle.verify(self.guest)

    def test_archive_symlink_and_parent_escape_are_rejected_before_installation(self):
        for name, kind, target in (("qemu/escape", tarfile.SYMTYPE, "/tmp"),
                                   ("qemu/../escape", tarfile.REGTYPE, "")):
            malicious = tarfile.TarInfo(name)
            malicious.type, malicious.linkname = kind, target
            self.archive(malicious)
            destination = self.root / "untrusted"
            with self.subTest(name=name), self.assertRaisesRegex(RuntimeError, "Unsafe"):
                bundle.install(self.products, destination)
            self.assertFalse(destination.exists())

    def test_conflicting_source_and_emulator_records_cannot_install(self):
        self.outer["emulator_sha256"] = "0" * 64
        (self.products / "build.json").write_text(json.dumps(self.outer))
        destination = self.root / "mismatch"
        with self.assertRaisesRegex(RuntimeError, "records differ"):
            bundle.install(self.products, destination)
        self.assertFalse(destination.exists())

    def test_modified_emulator_cannot_pass_bundle_verification(self):
        (self.guest / "qemu-aarch64").write_bytes(b"substituted")
        with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
            bundle.verify(self.guest)


if __name__ == "__main__":
    unittest.main()
