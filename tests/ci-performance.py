#!/usr/bin/env python3
"""Regression gates, evidence integrity, and the untrusted artifact boundary."""
import copy
import importlib.util
import io
import json
from pathlib import Path
import stat
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


history = module("history", "scripts/ci/performance.py")
perf = module("perf", "scripts/perf.py")


def fixture():
    samples = {metric: {side: list(range(1, 21)) for side in ("base", "head")}
               for metric in ("run", "ping", "list")}
    results = {
        side: {**{key: {"median": 10.5, "p95": 19, "n": 20} for key in ("run", "ping", "list")},
               "rss_idle": 4096, "rss_after": 6144, "syscalls": 300}
        for side in ("base", "head")
    }
    results["samples_ms"] = samples
    sizes = {side: {"engine": 5000000, "init": 1200000} for side in ("base", "head")}
    files = {"results.json": history.encoded(results), "sizes.json": history.encoded(sizes),
             "report.md": b"Test fixture: identical builds.\n"}
    item = {
        "schema": 1, "repository": history.REPO, "commit": "b" * 40, "baseline": "a" * 40,
        "harness": "a" * 40, "run_id": 123, "run_attempt": 1, "event": "push",
        "measured_at": "2026-11-14T12:00:00+00:00", "status": "within-limits", "regressions": [],
        "workload": {"id": "linux-engine-v2", "image": perf.IMAGE, "runs": 20, "warmup_runs": 10, "syscall_runs": 20},
        "environment": {"system": "Linux", "architecture": "x86_64", "cpu_model": "fixture CPU",
                        "cpus": 4, "memory_kib": 16000000, "kernel": "fixture kernel",
                        "runner_image": "ubuntu24", "runner_version": "fixture version",
                        "python": "3.12.0", "docker": "28.0.4", "rust": "1.99.1"},
        "binaries": {side: {key: {"sha256": "c" * 64, "bytes": size} for key, size in sizes[side].items()}
                     for side in ("base", "head")},
        "metrics": history.measurements(results, sizes), "samples_ms": samples,
        "limits": copy.deepcopy(history.LEGACY_LIMITS),
        "files": {name: history.digest(data) for name, data in files.items()},
    }
    item["series"] = history.series(item)
    files["record.json"] = history.encoded(item)
    return item, files


def archive(files, extra=None):
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as stream:
        for name, data in files.items():
            stream.writestr(name, data)
        if extra:
            stream.writestr(*extra)
    return output.getvalue()


class PerformanceTests(unittest.TestCase):
    def test_quantile_uses_nearest_rank_even_for_small_samples(self):
        self.assertEqual(perf.summary(range(1, 11)), {"median": 5.5, "p95": 10, "n": 10})
        self.assertEqual(perf.summary(range(1, 401)), {"median": 200.5, "p95": 380, "n": 400})
        for values in ([], [float("nan")], [float("inf")], [-1, 2]):
            with self.assertRaises(ValueError):
                perf.summary(values)

    def test_gate_requires_both_relative_and_absolute_regression(self):
        item, _ = fixture()
        metric = item["metrics"]["run_ms"]
        for base, head, failed in ((1, 1.2, False), (100, 105, False), (100, 111, True),
                                  (0, 0.6, True), (100, 80, False)):
            metric[:] = [base, head]
            self.assertEqual("run" in history.regressions(item["metrics"], item["limits"]), failed)

    def test_complete_paired_record_and_artifact_round_trip(self):
        item, files = fixture()
        self.assertEqual(history.validate(item), item)
        self.assertEqual(history.unpack(archive(files)), item)

    def test_rejects_incomplete_or_fabricated_measurements(self):
        original, _ = fixture()
        mutations = [
            lambda value: value["metrics"]["run_ms"].__setitem__(1, float("nan")),
            lambda value: value["metrics"]["rss_idle_kib"].__setitem__(0, -1),
            lambda value: value["metrics"]["run_ms"].__setitem__(1, 500),
            lambda value: value["samples_ms"]["run"]["head"].pop(),
            lambda value: value["metrics"].pop("syscalls"),
            lambda value: value["binaries"]["head"]["engine"].__setitem__("bytes", 42),
            lambda value: value.__setitem__("harness", "f" * 40),
            lambda value: value.__setitem__("status", "regression"),
        ]
        for mutate in mutations:
            item = copy.deepcopy(original)
            mutate(item)
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                history.validate(item)

    def test_changed_hardware_and_workload_do_not_join_one_series(self):
        item, _ = fixture()
        original = item["series"]
        for field, new in (("cpu_model", "another CPU"), ("cpus", 8),
                           ("architecture", "aarch64"), ("runner_image", "ubuntu26")):
            changed = copy.deepcopy(item)
            changed["environment"][field] = new
            self.assertNotEqual(history.series(changed), original)
        item["workload"]["image"] = perf.IMAGE[:-1] + "0"
        self.assertNotEqual(history.series(item), original)

    def test_legacy_harness_is_labeled_without_inventing_raw_samples(self):
        item, _ = fixture()
        item["workload"].update(id="linux-engine-v1", image="alpine:3.21")
        item["samples_ms"] = None
        item["series"] = history.series(item)
        history.validate(item)
        item["workload"]["image"] = perf.IMAGE
        with self.assertRaises(ValueError):
            history.validate(item)

    def test_duplicate_json_and_nonfinite_values_are_rejected(self):
        for value in (b'{"schema":1,"schema":2}', b'{"value":NaN}', b'{"value":Infinity}'):
            with self.assertRaises(ValueError):
                history.document(value)

    def test_artifact_cannot_add_paths_symlinks_or_change_evidence(self):
        _, files = fixture()
        cases = [
            archive(files, ("../../script.py", b"not executed")),
            archive({**files, "sizes.json": b"changed"}),
            archive({**files, "unrelated.txt": b"extra"}),
        ]
        link = zipfile.ZipInfo("record.json")
        link.create_system = 3
        link.external_attr = (stat.S_IFLNK | 0o777) << 16
        cases.append(archive({k: v for k, v in files.items() if k != "record.json"}, (link, b"../outside")))
        for data in cases:
            with self.assertRaises(ValueError):
                history.unpack(data)

    def test_forks_prs_unrelated_workflows_and_incomplete_runs_cannot_publish(self):
        valid = {"repository": {"full_name": history.REPO}, "head_repository": {"full_name": history.REPO},
                 "head_branch": "main", "event": "push", "path": ".github/workflows/perf.yml",
                 "name": "Speed", "status": "completed", "conclusion": "success"}
        history.trusted_run(valid)
        history.trusted_run({**valid, "conclusion": "failure"})  # regressions belong in history too
        for key, value in (("head_repository", {"full_name": "someone/fork"}), ("event", "pull_request"),
                           ("path", ".github/workflows/other.yml"), ("head_branch", "feature"),
                           ("status", "in_progress"), ("conclusion", "cancelled")):
            with self.assertRaises(ValueError):
                history.trusted_run({**valid, key: value})

    def test_history_is_ordered_idempotent_and_cannot_rewrite_a_run(self):
        first, _ = fixture()
        second = copy.deepcopy(first)
        second.update(run_id=124, measured_at="2026-11-14T13:00:00+00:00")
        empty = {"schema": 1, "repository": history.REPO, "entries": []}
        newer_first = history.update_index(empty, second)
        both = history.update_index(newer_first, first)
        self.assertEqual([row["run_id"] for row in both["entries"]], [123, 124])
        self.assertEqual(history.update_index(both, first), both)
        first["metrics"]["run_ms"][0] = 0.5
        with self.assertRaises(ValueError):
            history.update_index(both, first)

    def test_history_limits_the_index_without_changing_archive_paths(self):
        item, _ = fixture()
        original = history.summary(item)
        previous = {"schema": 1, "repository": history.REPO, "entries": []}
        for index in range(history.MAX_HISTORY):
            row = copy.deepcopy(original)
            row.update(run_id=index + 1, path=f"benchmarks/v1/runs/{index + 1}-1.json")
            previous["entries"].append(row)
        item["run_id"] = 900
        item["measured_at"] = "2026-11-14T13:00:00+00:00"
        updated = history.update_index(previous, item)
        self.assertEqual(len(updated["entries"]), history.MAX_HISTORY)
        self.assertEqual(updated["entries"][-1]["path"], "benchmarks/v1/runs/900-1.json")
        previous["entries"][0]["path"] = "../../outside"
        with self.assertRaises(ValueError):
            history.update_index(previous, item)

    def test_local_process_cannot_publish_automatic_commits(self):
        item, _ = fixture()
        with patch.dict(history.os.environ, {}, clear=True), self.assertRaises(ValueError):
            history.publish(item)


if __name__ == "__main__":
    unittest.main()
