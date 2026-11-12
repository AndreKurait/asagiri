#!/usr/bin/env python3
"""Test installer selection and rejection paths without installing any app."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


def metadata(arch="arm64"):
    version = "0.1.0-dev.37"
    return (f"version {version}\n"
            f"url https://github.com/AndreKurait/asagiri/releases/download/v{version}/Asagiri-{version}-{arch}.tar.gz\n"
            f"sha256 {'a' * 64}\n")


class InstallerTests(unittest.TestCase):
    def run_installer(self, *, arch="arm64", system="Darwin", rosetta=False,
                      version="15.0", body=None, args=()):
        with tempfile.TemporaryDirectory(prefix="asagiri-installer-test-") as directory:
            folder = Path(directory)
            config = folder / "config.json"
            calls = folder / "calls.jsonl"
            config.write_text(json.dumps({
                "arch": arch, "system": system, "rosetta": rosetta, "version": version,
                "body": metadata(arch) if body is None else body,
            }))
            program = f"""#!{sys.executable}
import json, os, sys
from pathlib import Path
config=json.loads(Path(os.environ["ASAGIRI_TEST_CONFIG"]).read_text())
name=Path(sys.argv[0]).name
if name=="uname": print(config["arch"] if sys.argv[1]=="-m" else config["system"])
elif name=="sysctl": print("1" if config["rosetta"] else "0")
elif name=="sw_vers": print(config["version"])
elif name=="curl":
    with Path(os.environ["ASAGIRI_TEST_CALLS"]).open("a") as log:
        log.write(json.dumps(sys.argv[1:])+"\\n")
    assert "--proto" in sys.argv and "=https" in sys.argv
    Path(sys.argv[sys.argv.index("-o")+1]).write_text(config["body"])
else: raise SystemExit("Unexpected test command")
"""
            for name in ("uname", "sysctl", "sw_vers", "curl"):
                path = folder / name
                path.write_text(program)
                path.chmod(0o755)
            env = {**os.environ, "PATH": str(folder) + os.pathsep + os.environ["PATH"],
                   "ASAGIRI_TEST_CONFIG": str(config), "ASAGIRI_TEST_CALLS": str(calls),
                   "TMPDIR": str(folder)}
            # Dry-run is unconditional: these tests never install or start a
            # native Mac app, even if a test expectation is wrong.
            result = subprocess.run(["sh", str(ROOT / "site/install.sh"), "--dry-run", *args],
                                    env=env, text=True, capture_output=True, timeout=15)
            requests = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
            self.assertFalse(any(folder.glob("asagiri-install.*")), "Installer leaked its temporary directory")
            return result, requests

    def test_native_mac_chooses_each_architecture(self):
        for arch in ("arm64", "x86_64"):
            with self.subTest(arch=arch):
                result, requests = self.run_installer(arch=arch)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"macOS {arch}", result.stdout)
                self.assertEqual(len(requests), 1, "Dry-run must not download an app")
                self.assertIn(f"https://andrekurait.github.io/asagiri/downloads/latest/macos-{arch}.txt", requests[0])

    def test_rosetta_terminal_selects_the_native_arm_app(self):
        result, requests = self.run_installer(arch="x86_64", rosetta=True, body=metadata("arm64"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("macOS arm64", result.stdout)
        self.assertIn("https://andrekurait.github.io/asagiri/downloads/latest/macos-arm64.txt", requests[0])

    def test_explicit_channel_selects_its_metadata(self):
        result, requests = self.run_installer(args=("--channel", "preview"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("https://andrekurait.github.io/asagiri/downloads/preview/macos-arm64.txt", requests[0])

    def test_invalid_options_and_channels_stop_before_network_access(self):
        for args in (("--channel", "wrong"), ("--channel",), ("--unknown",)):
            with self.subTest(args=args):
                result, requests = self.run_installer(args=args)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(requests, [])

    def test_old_macos_and_unsupported_platforms_stop_before_download(self):
        for options in ({"version": "13.6"}, {"arch": "ppc"}, {"system": "FreeBSD"}):
            with self.subTest(options=options):
                result, requests = self.run_installer(**options)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(requests, [])

    def test_untrusted_urls_versions_and_digests_are_rejected(self):
        bodies = [
            metadata().replace("https://github.com/AndreKurait/asagiri/", "https://example.com/"),
            metadata().replace("sha256 " + "a" * 64, "sha256 " + "a" * 63),
            metadata().replace("version 0.1.0-dev.37", "version $(touch /tmp/should-never-run)"),
            metadata() + "version 0.1.0-dev.38\n",
            metadata().replace("version 0.1.0-dev.37\n", ""),
        ]
        for body in bodies:
            with self.subTest(body=body):
                result, requests = self.run_installer(body=body)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(requests), 1, "Invalid metadata must never trigger an app download")


if __name__ == "__main__":
    unittest.main()
