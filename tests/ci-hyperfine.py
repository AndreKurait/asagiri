#!/usr/bin/env python3
"""Exercise the real legacy report and shared Hyperfine 1/2 statistics reader."""
from copy import deepcopy
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/hyperfine"


class HyperfineStatistics(unittest.TestCase):
    def reader(self, data, expression="hyperfine_wall_clock"):
        return subprocess.run(
            ["jq", "-L", str(ROOT / "bench"), "-c", 'include "hyperfine"; ' + expression],
            input=json.dumps(data), text=True, capture_output=True)

    def test_legacy_seconds(self):
        result = self.reader(json.loads((FIXTURES / "v1.json").read_text()))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["median"], 0.25)

    def test_actual_version_two_export(self):
        data = json.loads((FIXTURES / "v2.json").read_text())
        self.assertEqual(data["schema_version"], 2)
        result = self.reader(data)
        self.assertEqual(result.returncode, 0, result.stderr)
        stats = json.loads(result.stdout)
        self.assertEqual(stats, data["results"][0]["summary"]["time_wall_clock"])
        self.assertEqual(stats["unit"], "second")
        self.assertEqual(stats["count"], len(data["results"][0]["measurements"]))

    def test_wall_clock_is_independent_of_primary_metric(self):
        data = json.loads((FIXTURES / "v2.json").read_text())
        data["primary_metric"] = "memory_peak_resident"
        result = self.reader(data, r'hyperfine_wall_clock | "\(.median*1000|round) \(.min*1000|round)"')
        stats = data["results"][0]["summary"]["time_wall_clock"]
        expected = f"{math.floor(stats['median'] * 1000 + 0.5)} {math.floor(stats['min'] * 1000 + 0.5)}"
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), expected)

    def test_wrong_units_and_schemas_fail_instead_of_using_legacy_fields(self):
        base = json.loads((FIXTURES / "v2.json").read_text())
        variants = []
        for unit in ("millisecond", "byte", None):
            data = deepcopy(base)
            data["results"][0]["summary"]["time_wall_clock"]["unit"] = unit
            data["results"][0].update(median=0.25, min=0.125)
            variants.append(data)
        data = deepcopy(base)
        data["schema_version"] = 3
        variants.append(data)
        data = deepcopy(base)
        del data["results"][0]["summary"]["time_wall_clock"]
        variants.append(data)
        variants += [{}, {"results": []}, {"results": None}]
        for data in variants:
            with self.subTest(data=data):
                result = self.reader(data)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(result.stdout.strip())

    def test_invalid_statistics_fail(self):
        base = json.loads((FIXTURES / "v1.json").read_text())
        for field, value in [
            ("median", 0), ("median", -1), ("median", None), ("median", "0.25"),
            ("min", -1), ("min", None), ("min", 0.5),
        ]:
            with self.subTest(field=field, value=value):
                data = deepcopy(base)
                data["results"][0][field] = value
                result = self.reader(data)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(result.stdout.strip())

    def test_real_report_reads_both_layouts_and_decimal_megabytes(self):
        with tempfile.TemporaryDirectory(prefix="asagiri-hyperfine-") as directory:
            root = Path(directory)
            # Resolve the jq module relative to the script, even with spaces
            # and a working directory outside the repository.
            scripts = root / "bench scripts"
            scripts.mkdir()
            for name in ("report.sh", "hyperfine.jq"):
                shutil.copyfile(ROOT / "bench" / name, scripts / name)
            samples = root / "samples"
            samples.mkdir()
            medians = {}
            for version in ("v1", "v2"):
                data = json.loads((FIXTURES / (version + ".json")).read_text())
                medians[version] = (data["results"][0]["median"] if version == "v1" else
                                    data["results"][0]["summary"]["time_wall_clock"]["median"])
                (samples / (version + ".net.json")).write_text(json.dumps({"label": version, "net_gbps": 1.25}))
                for case in ("run", "exec", "stdout", "stdin", "bind-write", "bind-read", "build", "pull"):
                    (samples / f"{version}.{case}.json").write_text(json.dumps(data))
            report = subprocess.run(["bash", str(scripts / "report.sh")], cwd=root,
                                    env=dict(os.environ, OUT=str(samples)), text=True, capture_output=True)
            self.assertEqual(report.returncode, 0, report.stderr)
            self.assertFalse(report.stderr)
            self.assertNotIn("n/a", report.stdout)
            for version, median in medians.items():
                line = next(line for line in report.stdout.splitlines() if line.startswith("| " + version + " |"))
                fields = [value.strip() for value in line.split("|")[1:-1]]
                milliseconds = str(math.floor(median * 1000 + 0.5))
                stream = str(math.floor(200000000 / 1000000 / median + 0.5))
                bind = str(math.floor(1073741824 / 1000000 / median + 0.5))
                self.assertEqual(fields, [version, milliseconds, milliseconds, stream, stream,
                                          bind, bind, "1.25", milliseconds, milliseconds])

    def test_empty_report_has_only_headers(self):
        with tempfile.TemporaryDirectory(prefix="asagiri-hyperfine-empty-") as directory:
            report = subprocess.run(["bash", str(ROOT / "bench/report.sh")],
                                    env=dict(os.environ, OUT=directory), text=True, capture_output=True)
            self.assertEqual(report.returncode, 0)
            self.assertFalse(report.stderr)
            self.assertEqual(len(report.stdout.splitlines()), 2)


if __name__ == "__main__":
    unittest.main()
