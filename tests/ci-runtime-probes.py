#!/usr/bin/env python3
"""Check process identity in runtime probes without starting an engine."""
import ast
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
PROBE = ROOT / "tests/network-dns-reuse.py"
TREE = ast.parse(PROBE.read_text())
FUNCTION = next(node for node in TREE.body if isinstance(node, ast.FunctionDef) and node.name == "require_agent")
CODE = compile(ast.Module(body=[FUNCTION], type_ignores=[]), str(PROBE), "exec")


class AgentIdentity(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="asagiri-probe-identity-")
        self.addCleanup(self.scratch.cleanup)
        self.lock = Path(self.scratch.name) / "agent.lock"
        self.lock.write_text("130\n")
        self.agent = {"pid": 130, "start_time": "297796", "sockets": ["socket:[349759]"]}
        self.worker = {"pid": 449, "start_time": "297913", "sockets": ["socket:[351090]"]}
        self.rows = [self.agent, self.worker, {"pid": 450, "start_time": "297913", "sockets": []}]
        self.namespace = {"Path": Path, "os": os, "agent_sockets": lambda: self.rows}
        exec(CODE, self.namespace)
        self.require = self.namespace["require_agent"]
        self.environment = patch.dict(os.environ, {"ASAGIRI_DATA_DIR": self.scratch.name})
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def test_forked_workers_do_not_replace_the_recorded_agent(self):
        self.assertEqual(self.require(), [self.agent])
        self.assertEqual(self.require(self.agent.copy()), [self.agent])

    def test_agent_socket_leaks_are_still_observable(self):
        before = {**self.agent, "sockets": list(self.agent["sockets"])}
        self.agent["sockets"].append("socket:[999999]")
        after = self.require(before)[0]
        self.assertEqual(set(after["sockets"]) - set(before["sockets"]), {"socket:[999999]"})

    def test_worker_cannot_substitute_for_an_unreadable_agent(self):
        self.rows = [self.worker]
        with self.assertRaises(AssertionError):
            self.require(self.agent)

    def test_changed_agent_pid_is_rejected(self):
        self.lock.write_text("449\n")
        with self.assertRaises(AssertionError):
            self.require(self.agent)

    def test_reused_pid_with_a_new_start_time_is_rejected(self):
        before = self.agent.copy()
        self.agent["start_time"] = "999999"
        with self.assertRaises(AssertionError):
            self.require(before)

    def test_pid_change_during_sampling_is_rejected(self):
        def changing():
            self.lock.write_text("449\n")
            return self.rows
        self.namespace["agent_sockets"] = changing
        with self.assertRaises(AssertionError):
            self.require(self.agent)

    def test_missing_or_invalid_record_never_falls_back_to_a_worker(self):
        for text in ("", "0", "-1", "130 449", "invalid"):
            self.lock.write_text(text)
            with self.subTest(text=text), self.assertRaises(AssertionError):
                self.require()
        self.lock.unlink()
        with self.assertRaises(FileNotFoundError):
            self.require()


if __name__ == "__main__":
    unittest.main()
