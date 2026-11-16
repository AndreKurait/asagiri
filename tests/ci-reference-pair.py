#!/usr/bin/env python3
"""Check pairing and raw-sample integrity without launching engines."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("reference_pair", ROOT / "scripts/ci/reference-pair.py")
pair = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pair)


class PairTests(unittest.TestCase):
    def setUp(self):
        fixture = json.loads((ROOT / "docs/benchmarks/engine-comparison-2026-11-16-c610.json").read_text())
        self.sources = {"baseline": "1" * 40, "candidate": "2" * 40}
        self.binaries = {
            "baseline": {"asagiri": "3" * 64, "asagiri-init": "4" * 64},
            "candidate": {"asagiri": "5" * 64, "asagiri-init": "6" * 64},
        }
        self.inputs = {key: fixture["provenance"][key] for key in pair.INPUTS}
        self.pins = {pin: fixture["provenance"]["hosts"][engine]["image"]
                     for engine, pin in zip(pair.ENGINES, ("asagiri_host", "docker", "podman"))}
        self.context = {key: fixture["provenance"][key]
                        for key in ("repository", "run_id", "run_attempt", "event")}
        self.legs = []
        for index, variant in enumerate(pair.ORDER):
            result = copy.deepcopy(fixture)
            result["measured_at"] = f"2026-11-16T00:0{index}:00+00:00"
            result["provenance"].update(commit=self.sources[variant], binaries=self.binaries[variant],
                                        asagiri_build_kind="musl-portable")
            values = ([4.] * 360 + [300.] * 40) if variant == "baseline" else [2.] * 400
            result["samples_ms"]["Asagiri"]["run"] = values
            result["metrics_ms"]["Asagiri"]["run"] = pair.summary(values)
            for engine in pair.ENGINES:
                identifier = hashlib.sha256(f"synthetic-{index}-{engine}".encode()).hexdigest()
                result["cleanup"][engine]["containers_verified_absent"] = [identifier]
            self.legs.append({"variant": variant, "result": result,
                              "comparison_sha256": hashlib.sha256(str(index).encode()).hexdigest()})

    def compare(self, legs=None):
        return pair.compare(self.legs if legs is None else legs, self.sources, self.binaries,
                            self.inputs, self.pins, self.context)

    def test_pair_reports_observed_median_and_tail_without_claiming_a_speedup_gate(self):
        value = self.compare()
        self.assertEqual(value["samples"], 24000)
        self.assertEqual(len(value["metrics"]), 15)
        row = next(row for row in value["metrics"] if row["engine"] == "Asagiri" and row["workload"] == "run")
        self.assertEqual(row["median_ratio"], .5)
        self.assertEqual((row["baseline"]["samples"], row["candidate"]["samples"]), (800, 800))
        self.assertEqual((row["baseline"]["p95"], row["candidate"]["p95"]), (300., 2.))
        self.assertEqual((row["baseline"]["over_240_ms"], row["candidate"]["over_240_ms"]), (80, 0))
        self.assertIn("does not assert a speedup", value["limits"])
        self.assertIn("Asagiri: 80 / 0", pair.report(value))

    def test_slower_candidate_is_reported_honestly(self):
        for leg in self.legs:
            if leg["variant"] == "candidate":
                values = [800.] * 400
                leg["result"]["samples_ms"]["Asagiri"]["run"] = values
                leg["result"]["metrics_ms"]["Asagiri"]["run"] = pair.summary(values)
        row = next(row for row in self.compare()["metrics"]
                   if row["engine"] == "Asagiri" and row["workload"] == "run")
        self.assertEqual(row["median_ratio"], 200.)
        self.assertEqual(row["candidate"]["over_240_ms"], 800)

    def test_wrong_order_missing_leg_and_replayed_measurements_are_rejected(self):
        wrong_order = copy.deepcopy(self.legs)
        wrong_order[0], wrong_order[1] = wrong_order[1], wrong_order[0]
        replay = copy.deepcopy(self.legs)
        replay[3] = copy.deepcopy(replay[0])
        for legs in (self.legs[:3], wrong_order, replay):
            with self.subTest(legs=len(legs)), self.assertRaises(ValueError):
                self.compare(legs)

    def test_mismatched_provenance_hosts_workloads_and_cleanup_are_rejected(self):
        changes = [
            (("provenance", "commit"), "9" * 40),
            (("provenance", "run_id"), "123"),
            (("provenance", "binaries", "asagiri-init"), "8" * 64),
            (("provenance", "runner_sha256"), "7" * 64),
            (("provenance", "hosts", "Docker", "image"), "docker:latest"),
            (("provenance", "hosts", "Docker", "published_ports"), True),
            (("host", "kernel"), "another-kernel"),
            (("host", "cpus"), True),
            (("workload",), "docker-api-reference-v2+instrumented"),
            (("warmup_runs",), 0),
            (("cleanup", "Asagiri", "volume_verified_absent"), False),
            (("cleanup", "Docker", "containers_verified_absent"), []),
            (("engines", "Podman", "image", "RootFS", "Layers"), ["sha256:" + "9" * 64]),
            (("engines", "Docker", "version", "Version"), "changed"),
            (("engines", "Podman", "version", "AsagiriVersion"), "0.0.0"),
            (("measured_at",), self.legs[0]["result"]["measured_at"]),
            (("samples_ms", "Asagiri", "run"), [2.] * 399),
            (("samples_ms", "Asagiri", "run"), [2.] * 399 + [True]),
            (("samples_ms", "Asagiri", "run"), [2.] * 399 + [float("nan")]),
            (("metrics_ms", "Asagiri", "run", "p95"), 100.),
        ]
        for path, value in changes:
            with self.subTest(path=path):
                legs = copy.deepcopy(self.legs)
                target = legs[1]["result"]
                for key in path[:-1]:
                    target = target[key]
                target[path[-1]] = value
                with self.assertRaises(ValueError):
                    self.compare(legs)

    def test_raw_statistics_use_nearest_rank_and_reject_nonfinite_values(self):
        self.assertEqual(pair.summary([1.] * 19 + [999.])["p95"], 1.)
        for values in ([], [True], [-1.], [float("inf")], [float("nan")]):
            with self.subTest(values=values), self.assertRaises(ValueError):
                pair.summary(values)

    def test_baseline_text_cannot_become_a_git_option_or_shell_program(self):
        environment = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                       "GITHUB_REPOSITORY": "AndreKurait/asagiri"}
        for baseline in ("main", "A" * 40, "--help", "../outside", "$(touch should-not-exist)"):
            arguments = ["reference-pair.py", "--baseline=" + baseline, "--out=/unused/out",
                         "--target=/unused/target", "--registry=/unused/registry"]
            with self.subTest(baseline=baseline), patch.dict(pair.os.environ, environment), \
                    patch.object(pair.platform, "system", return_value="Linux"), \
                    patch.object(pair.platform, "machine", return_value="x86_64"), \
                    patch("sys.argv", arguments), patch.object(pair, "git") as git:
                with self.assertRaises(ValueError):
                    pair.main()
                git.assert_not_called()

    def test_controller_refuses_a_user_machine_before_git_or_engine_operations(self):
        arguments = ["reference-pair.py", "--baseline=" + "1" * 40, "--out=/unused/out",
                     "--target=/unused/target", "--registry=/unused/registry"]
        with patch.dict(pair.os.environ, {"GITHUB_ACTIONS": "false"}), \
                patch.object(pair.platform, "system", return_value="Linux"), \
                patch.object(pair.platform, "machine", return_value="x86_64"), \
                patch("sys.argv", arguments), patch.object(pair, "git") as git:
            with self.assertRaises(ValueError):
                pair.main()
            git.assert_not_called()


if __name__ == "__main__":
    unittest.main()
