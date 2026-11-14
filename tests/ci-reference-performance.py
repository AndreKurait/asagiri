#!/usr/bin/env python3
"""Protect comparison semantics and cleanup of user-selected endpoints."""
import http.client
import importlib.util
import json
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

        engine.conn = Broken()
        with patch.object(bench, "UnixHTTP", lambda path: Broken()):
            with self.assertRaises(http.client.RemoteDisconnected):
                engine.call("POST", "/containers/create", {})
        self.assertEqual(len(calls), 1)

    def test_cleanup_checks_labels_when_server_ignores_filters(self):
        engine = bench.Engine("Asagiri=/test/socket", "our-run")
        ours, theirs = "a" * 64, "b" * 64
        removed = []

        def call(method, path, **kwargs):
            if method == "GET":
                return json.dumps([{"Id": ours, "Labels": {bench.LABEL: "our-run"}},
                                   {"Id": theirs, "Labels": {bench.LABEL: "another-run"}},
                                   {"Id": "c" * 64, "Labels": None},
                                   {"Id": "../unsafe", "Labels": {bench.LABEL: "our-run"}}]).encode()
            removed.append(path)
            return b""

        engine.call = call
        engine.cleanup()
        self.assertEqual(removed, [f"/containers/{ours}?force=1"])

    def test_comparison_preserves_raw_samples_and_accepts_different_image_id_schemes(self):
        instances = []

        class Fake:
            def __init__(self, spec, nonce):
                self.name, self.path = spec.split("=")
                self.list_path = "/containers/json"
                self.runs, self.cleaned = 0, False
                instances.append(self)

            def prepare(self):
                return {"image": {"Id": self.name, "Architecture": "amd64", "Os": "linux",
                                  "RootFS": {"Layers": ["sha256:" + "a" * 64]}}}

            def run_once(self):
                self.runs += 1

            def call(self, *args):
                return b"[]"

            def cleanup(self):
                self.cleaned = True

        with patch.object(bench, "Engine", Fake), patch.object(bench.time, "sleep"):
            result = bench.measure(["Docker=/first", "Asagiri=/second"], 20)
        self.assertTrue(all(i.cleaned and i.runs == 30 for i in instances))
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
