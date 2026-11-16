#!/usr/bin/env python3
"""Protect comparison semantics and cleanup of user-selected endpoints."""
import http.client
import importlib.util
import json
import struct
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("reference", ROOT / "scripts/reference-bench.py")
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)


class ReferenceTests(unittest.TestCase):
    def test_nearest_rank_p95_and_finite_samples(self):
        self.assertEqual(bench.summary(list(range(1, 20)) + [1000])["p95"], 19)
        for values in ([], [-1], [float("nan")], [float("inf")]):
            with self.assertRaises(ValueError):
                bench.summary(values)

    def test_mutation_is_not_retried_after_losing_response(self):
        engine = bench.Engine("Asagiri=/test/socket", "nonce")
        calls = []

        class Broken:
            def request(self, *args, **kwargs):
                calls.append(args)
                raise http.client.RemoteDisconnected()

            def close(self):
                pass

        with patch.object(bench, "UnixHTTP", lambda path: Broken()):
            for method, path, kwargs in (
                ("POST", "/containers/create", {"body": {}}),
                ("PUT", "/containers/id/archive?path=/data", {"raw": bench.copy_archive()}),
                ("DELETE", "/containers/id", {}),
            ):
                engine.conn = Broken()
                calls.clear()
                with self.assertRaises(http.client.RemoteDisconnected):
                    engine.call(method, path, **kwargs)
                self.assertEqual(len(calls), 1)

    def test_success_status_does_not_replace_ping_and_empty_list_semantics(self):
        engine = bench.Engine("Asagiri=/test/socket", "our-run")
        for metric, good, bad in (
            ("ping", b"OK\n", b"not ready"),
            ("list", b"[]", b'[{"Id":"unexpected-container"}]'),
        ):
            engine.call = lambda *args, **kwargs: good
            engine.operation(metric)()
            engine.call = lambda *args, **kwargs: bad
            with self.assertRaises(ValueError):
                engine.operation(metric)()
        engine.conn.close()

    def test_cleanup_checks_labels_when_server_ignores_filters(self):
        engine = bench.Engine("Asagiri=/test/socket", "our-run")
        ours, theirs = "a" * 64, "b" * 64
        removed, inspected = [], []

        def call(method, path, **kwargs):
            if method == "GET" and path == engine.list_path:
                return json.dumps([{"Id": ours, "Labels": {bench.LABEL: "our-run"}},
                                   {"Id": theirs, "Labels": {bench.LABEL: "another-run"}},
                                   {"Id": "c" * 64, "Labels": None},
                                   {"Id": "../unsafe", "Labels": {bench.LABEL: "our-run"}}]).encode()
            if method == "GET":
                self.assertEqual(kwargs["statuses"], (404,))
                inspected.append(path)
                return b"{}"
            removed.append(path)
            return b""

        engine.call = call
        engine.cleanup()
        self.assertEqual(removed, [f"/containers/{ours}?force=1"])
        self.assertEqual(inspected, [f"/containers/{ours}/json"])

    def test_volume_cleanup_requires_its_current_name_and_label(self):
        for volume in ({"Name": "our-volume", "Labels": {bench.LABEL: "another-run"}},
                       {"Name": "another-volume", "Labels": {bench.LABEL: "our-run"}}):
            engine = bench.Engine("Asagiri=/test/socket", "our-run")
            engine.volume = "our-volume"
            removed = []

            def call(method, path, **kwargs):
                if method == "GET":
                    return json.dumps([] if path == engine.list_path else volume).encode()
                removed.append(path)
                return b""

            engine.call = call
            with self.assertRaisesRegex(RuntimeError, "ownership differs"):
                engine.cleanup()
            self.assertEqual(removed, [])

    def test_copy_workload_rejects_changed_bytes_even_when_length_matches(self):
        bench.verify_copy(bench.copy_archive())
        changed = bytes([bench.COPY_PAYLOAD[0] ^ 1]) + bench.COPY_PAYLOAD[1:]
        with self.assertRaisesRegex(ValueError, "changed the file bytes"):
            bench.verify_copy(bench.copy_archive(changed))
        with self.assertRaisesRegex(ValueError, "wrong file length"):
            bench.verify_copy(bench.copy_archive(b"short"))

    def test_exec_requires_complete_docker_stream_frames(self):
        first, second = bench.EXEC_OUTPUT[:8], bench.EXEC_OUTPUT[8:]
        frame = lambda value: b"\x01\0\0\0" + struct.pack(">I", len(value)) + value
        data = frame(first) + frame(second)
        self.assertEqual(bench.stream_output(data), bench.EXEC_OUTPUT)
        for malformed in (data[:-1], b"\x01\0", b"\x01BAD" + data[4:]):
            with self.assertRaises(ValueError):
                bench.stream_output(malformed)

    def test_exec_nonzero_exit_is_not_a_successful_timing_sample(self):
        engine = bench.Engine("Asagiri=/test/socket", "nonce")
        engine.service = "a" * 64
        marker = b"\x01\0\0\0" + struct.pack(">I", len(bench.EXEC_OUTPUT)) + bench.EXEC_OUTPUT

        def call(method, path, *args, **kwargs):
            if path.endswith("/exec"):
                return json.dumps({"Id": "b" * 64}).encode()
            if path.endswith("/start"):
                return marker
            return json.dumps({"Running": False, "ExitCode": 1}).encode()

        engine.call = call
        with self.assertRaisesRegex(RuntimeError, "measured exec failed"):
            engine.exec_once()
        engine.conn.close()

    def test_comparison_preserves_raw_samples_and_accepts_different_image_id_schemes(self):
        instances = []

        class Fake:
            def __init__(self, spec, nonce):
                self.name, self.path = spec.split("=")
                self.list_path = "/containers/json"
                self.runs, self.cleaned = 0, False
                self.service = False
                self.calls = {name: 0 for name in bench.WORKLOADS}
                instances.append(self)

            def prepare(self, *, pull=True):
                return {"image": {"Id": self.name, "Architecture": "amd64", "Os": "linux",
                                  "RootFS": {"Layers": ["sha256:" + "a" * 64]}}}

            def prepare_service(self):
                self.service = True

            def operation(self, metric):
                def action():
                    if (metric in ("exec", "copy")) != self.service:
                        raise AssertionError("service fixtures changed the empty-list phase")
                    self.calls[metric] += 1
                    if metric == "run":
                        self.runs += 1
                return action

            def cleanup(self):
                self.cleaned = True
                return {"volume_verified_absent": True}

        with patch.object(bench, "Engine", Fake), patch.object(bench.time, "sleep"):
            result = bench.measure(["Docker=/first", "Asagiri=/second"], 20)
        self.assertTrue(all(i.cleaned and i.runs == 30 for i in instances))
        self.assertTrue(all(set(i.calls.values()) == {30} for i in instances))
        self.assertEqual(set(result["workloads"]), {"ping", "list", "run", "exec", "copy"})
        self.assertEqual(set(result["cleanup"]), {"Docker", "Asagiri"})
        for name, row in result["samples_ms"].items():
            for metric, values in row.items():
                self.assertEqual(len(values), 20)
                self.assertEqual(bench.summary(values), result["metrics_ms"][name][metric])

    def test_duplicate_endpoint_and_unsafe_engine_names_rejected(self):
        for spec in ("bad|name=/socket", "Docker=tcp://localhost:2375"):
            with self.assertRaises(ValueError):
                bench.Engine(spec, "nonce")
        with self.assertRaisesRegex(ValueError, "different socket"):
            bench.measure(["One=/socket", "Two=unix:///socket"], 20)


if __name__ == "__main__":
    unittest.main()
