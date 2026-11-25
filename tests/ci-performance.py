#!/usr/bin/env python3
"""Regression gates, evidence integrity, and the untrusted artifact boundary."""
import base64
import copy
from contextlib import redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import stat
import tempfile
from types import SimpleNamespace
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


def fixture(workload="linux-engine-v2"):
    names = history.timing_names(workload)
    samples = {metric: {side: list(range(1, 21)) for side in ("base", "head")}
               for metric in names}
    results = {
        side: {**{key: {"median": 10.5, "p95": 19, "n": 20} for key in names},
               "rss_idle": 4096, "rss_after": 6144, "syscalls": 300}
        for side in ("base", "head")
    }
    results["samples_ms"] = samples
    results["workload"] = {"id": workload, "image": perf.IMAGE, "runs": 20, "warmup_runs": 10, "syscall_runs": 20}
    if workload == "linux-engine-v3":
        results["workload"]["fixtures"] = perf.fixtures("x86_64")[2]
    sizes = {side: {"engine": 5000000, "init": 1200000} for side in ("base", "head")}
    files = {"results.json": history.encoded(results), "sizes.json": history.encoded(sizes),
             "report.md": b"Test fixture: identical builds.\n"}
    item = {
        "schema": 1, "repository": history.REPO, "commit": "b" * 40, "baseline": "a" * 40,
        "harness": "a" * 40, "run_id": 123, "run_attempt": 1, "event": "push",
        "measured_at": "2026-11-14T12:00:00+00:00", "status": "within-limits", "regressions": [],
        "workload": copy.deepcopy(results["workload"]),
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
    def test_report_labels_alternating_execution_order(self):
        _, files = fixture()
        results = json.loads(files["results.json"])
        samples = results["samples_ms"]["run"]
        samples["base"] = [14, 2] * 10
        samples["head"] = [2, 14] * 10
        for side in ("base", "head"):
            results[side]["run"] = perf.summary(samples[side])
        output = io.StringIO()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "results.json"
            path.write_text(json.dumps(results))
            with redirect_stdout(output):
                self.assertEqual(perf.report(SimpleNamespace(results=path, sizes=None)), 0)
        self.assertIn("| Head − base, base first | Head − base, head first |", output.getvalue())
        self.assertIn("| run | +0.000 ms | 10/20 | -12.000 ms | +12.000 ms |", output.getvalue())

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

    def test_v3_fixtures_and_every_raw_summary_are_verified(self):
        item, files = fixture("linux-engine-v3")
        self.assertEqual(len(item["metrics"]), 15)
        self.assertEqual(history.unpack(archive(files)), item)
        for arch in ("x86_64", "aarch64"):
            value = copy.deepcopy(item)
            value["environment"]["architecture"] = arch
            value["workload"]["fixtures"] = perf.fixtures(arch)[2]
            value["series"] = history.series(value)
            history.validate(value)
        for mutate in (
                lambda value: value["workload"]["fixtures"].__setitem__("config_sha256", "0" * 64),
                lambda value: value["workload"]["fixtures"].__setitem__("copy_payload_bytes", 1),
                lambda value: value["samples_ms"]["create"]["head"].pop(),
                lambda value: value["metrics"]["copy_ms"].__setitem__(0, 999),
                lambda value: value["metrics"].pop("create_p95_ms"),
                lambda value: value["workload"].__setitem__("id", "linux-engine-v2")):
            value = copy.deepcopy(item)
            mutate(value)
            value["series"] = history.series(value)
            with self.assertRaises(ValueError):
                history.validate(value)

    def test_v3_and_older_runs_share_an_index_without_rewriting_old_data(self):
        older, _ = fixture()
        new, _ = fixture("linux-engine-v3")
        new["run_id"] += 1
        new["measured_at"] = "2026-11-15T00:00:00+00:00"
        previous = history.update_index({"schema": 2, "repository": history.REPO, "entries": []}, older)
        snapshot = copy.deepcopy(previous["entries"][0])
        updated = history.update_index(previous, new)
        self.assertEqual(updated["entries"][0], snapshot)
        self.assertNotEqual(updated["entries"][0]["series"], updated["entries"][1]["series"])
        self.assertEqual(len(updated["entries"][1]["metrics"]), 15)

    def test_new_workloads_preserve_all_existing_gates(self):
        item, _ = fixture("linux-engine-v3")
        item["metrics"]["create_ms"][1] *= 3
        item["metrics"]["copy_ms"][1] *= 3
        self.assertEqual(history.regressions(item["metrics"], item["limits"]), [])
        for key, metric in history.LIMIT_METRICS.items():
            share, amount = item["limits"][key]
            value = copy.deepcopy(item)
            base = value["metrics"][metric][0]
            value["metrics"][metric][1] = base + max(base * share, amount) + 1
            self.assertEqual(history.regressions(value["metrics"], value["limits"]), [key])

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

    def test_chart_groups_page_sized_memory_variations_but_separates_capacities(self):
        item, _ = fixture()
        item["environment"]["memory_kib"] = 16078420
        item["series"] = history.series(item)
        original = copy.deepcopy(item)
        second = copy.deepcopy(item)
        second["environment"]["memory_kib"] += 4
        second["series"] = history.series(second)
        self.assertNotEqual(item["series"], second["series"])
        self.assertEqual(history.summary(item)["series"], history.summary(second)["series"])
        for field, value in (("cpus", 8), ("memory_kib", 8 * 1024**2),
                             ("cpu_model", "different CPU"), ("architecture", "aarch64"),
                             ("runner_image", "ubuntu26")):
            changed = copy.deepcopy(item)
            changed["environment"][field] = value
            self.assertNotEqual(history.summary(item)["series"], history.summary(changed)["series"])
        changed = copy.deepcopy(item)
        changed["workload"]["id"] = "linux-engine-v1"
        self.assertNotEqual(history.summary(item)["series"], history.summary(changed)["series"])
        self.assertEqual(item, original)

    def test_index_upgrade_checks_original_records_and_preserves_measurements(self):
        first, _ = fixture()
        first["environment"]["memory_kib"] = 16078420
        first["series"] = history.series(first)
        second = copy.deepcopy(first)
        second.update(run_id=124, measured_at="2026-11-14T13:00:00+00:00")
        second["environment"]["memory_kib"] += 4
        second["series"] = history.series(second)
        records = {history.summary(item)["path"]: item for item in (first, second)}
        old = {"schema": 1, "repository": history.REPO,
               "entries": [history.summary(item, legacy=True) for item in (first, second)]}
        before = copy.deepcopy((old, records))
        updated = history.update_index(old, second, records.__getitem__)
        self.assertEqual(updated["schema"], 2)
        self.assertEqual(len({row["series"] for row in updated["entries"]}), 1)
        self.assertEqual([row["path"] for row in updated["entries"]], list(records))
        self.assertEqual([row["metrics"] for row in updated["entries"]],
                         [first["metrics"], second["metrics"]])
        self.assertEqual((old, records), before)
        self.assertEqual(history.update_index(updated, second), updated)
        with self.assertRaises(ValueError):
            history.update_index(old, second)
        old["entries"][0]["metrics"]["run_ms"][0] += 1
        with self.assertRaises(ValueError):
            history.update_index(old, second, records.__getitem__)

    def test_republishing_an_archived_run_upgrades_only_the_index(self):
        item, _ = fixture()
        path = history.summary(item)["path"]
        raw = history.encoded(item)
        files = {path: raw, history.INDEX: history.encoded({
            "schema": 1, "repository": history.REPO,
            "entries": [history.summary(item, legacy=True)]})}
        writes = []

        def command(args, **kwargs):
            if args[2] == "graphql":
                value = json.loads(kwargs["input"])
                changes = value["variables"]["input"]["fileChanges"]["additions"]
                writes.append(changes)
                for change in changes:
                    files[change["path"]] = base64.b64decode(change["contents"], validate=True)
                response = {"data": {"createCommitOnBranch": {"commit": {
                    "oid": "d" * 40, "url": "https://github.com/AndreKurait/asagiri/commit/" + "d" * 40,
                    "signature": {"__typename": "GpgSignature", "isValid": True, "wasSignedByGitHub": True}}}}}
                return SimpleNamespace(returncode=0, stdout=history.encoded(response), stderr=b"")
            return SimpleNamespace(returncode=0, stdout=b"{}", stderr=b"")

        environment = {"GITHUB_ACTIONS": "true", "GITHUB_REPOSITORY": history.REPO,
                       "GITHUB_REF": "refs/heads/main", "GITHUB_WORKFLOW": "Performance history"}
        with patch.dict(history.os.environ, environment, clear=True), \
                patch.object(history, "api", return_value={"object": {"sha": "d" * 40}}), \
                patch.object(history, "remote_file", side_effect=lambda name, _revision: files.get(name)), \
                patch.object(history.subprocess, "run", side_effect=command):
            history.publish(item)
            history.publish(item)
        self.assertEqual(len(writes), 1)
        self.assertEqual([change["path"] for change in writes[0]], [history.INDEX])
        self.assertEqual(json.loads(files[history.INDEX])["schema"], 2)
        self.assertEqual(files[path], raw)

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

    def test_experiment_steps_are_archived_without_becoming_push_checks(self):
        steps = [{"name": "Compare", "conclusion": "success"},
                 {"name": "Record measurements and source identities", "conclusion": "success"}]
        original = {"name": "speed", "steps": steps}
        experiment = {"name": "speed experiment", "steps": steps}
        expected = {step["name"]: step["conclusion"] for step in steps}
        self.assertEqual(history.step_outcomes([original], "push"), expected)
        self.assertEqual(history.step_outcomes([experiment], "push"), {})
        for job in (original, experiment):
            self.assertEqual(history.step_outcomes([job], "workflow_dispatch"), expected)
        with self.assertRaises(ValueError):
            history.step_outcomes([original, experiment], "workflow_dispatch")
        with self.assertRaises(ValueError):
            history.step_outcomes([dict(original, steps=[*steps, steps[0]])], "push")

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
        previous = {"schema": 2, "repository": history.REPO, "entries": []}
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
