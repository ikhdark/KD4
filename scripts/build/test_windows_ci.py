"""Evidence adapter failure contracts. Fixtures do not build or run Rust."""

import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

from scripts.build import windows_ci


class WindowsEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        directory = Path(self.temp.name)
        self.environment = mock.patch.dict(os.environ, {
            "KD4_CI_EVIDENCE": str(directory / "evidence"),
            "KD4_CI_TARGET": str(directory / "target"),
            "KD4_STEP_RESULTS": json.dumps({name: {"outcome": "success"} for name in (
                "checkout", "initialize", "cache", "setup", "build", "test")}),
            "GITHUB_STEP_SUMMARY": "",
        })
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.evidence = windows_ci.Evidence()
        self.evidence.check_source = mock.Mock()

    def fixture(self):
        evidence = self.evidence
        test = {"binary": "codex-test", "test": "module::selected"}
        evidence.data["selection"] = {"expected_tests": [test]}
        operations = evidence.directory / "operations"
        operations.mkdir()
        receipt = {"kind": "codex_test_execution_v1", "exit_code": 0,
                   "executed_tests": 1, "executions": [test]}
        (operations / "tests.stdout.log").write_text(json.dumps(receipt) + "\n")
        (operations / "tests.stderr.log").write_text("")
        evidence.data["operations"] = [{"name": "rust-gates", "status": "passed", "exit_code": 0,
                                       "stdout": "operations/tests.stdout.log", "stderr": "operations/tests.stderr.log"}]
        logs = evidence.target / "test-runner-logs"
        logs.mkdir(parents=True)
        windows_ci.write_json(logs / "validation-fixture.json", {"outcome": "passed"})
        metadata = {"crate": "codex-test", "test_binary": "codex_test", "kind": "lib"}
        name = "codex-test::codex_test$module::selected"
        events = [
            {"type": "suite", "event": "started", "test_count": 1, "nextest": metadata},
            {"type": "test", "event": "started", "name": name},
            {"type": "test", "event": "ok", "name": name, "exec_time": 0.01},
            {"type": "suite", "event": "ok", "passed": 1, "failed": 0, "ignored": 0,
             "measured": 0, "filtered_out": 0, "exec_time": 0.01, "nextest": metadata},
        ]
        (logs / "rust-test-stdout-fixture.log").write_text("\n".join(map(json.dumps, events)))
        output = evidence.target / "dev-small"
        (output / "deps").mkdir(parents=True)
        for executable in ("codex-command-runner.exe", "codex-windows-sandbox-setup.exe", "deps/codex_test-fixture.exe"):
            (output / executable).write_bytes(b"fixture, not executable code")
        return operations, logs

    def finalize(self):
        with contextlib.redirect_stdout(io.StringIO()):
            result = self.evidence.finalize()
        summary = json.loads(self.evidence.path.read_text())
        return result, summary

    def test_complete_consistent_evidence_is_accepted(self):
        self.fixture()
        result, summary = self.finalize()
        self.assertEqual(result, 0)
        self.assertEqual(summary["status"], "passed")
        self.assertEqual(summary["tests"]["confirmed_pass_count"], 1)
        self.assertEqual(len(summary["executables"]), 3)
        for record in summary["evidence_files"]:
            self.assertEqual(record["sha256"], windows_ci.file_hash(self.evidence.directory / record["path"]))

    def test_zero_test_receipt_cannot_pass(self):
        operations, _ = self.fixture()
        (operations / "tests.stdout.log").write_text(json.dumps({
            "kind": "codex_test_execution_v1", "exit_code": 0, "executed_tests": 0, "executions": []}))
        result, summary = self.finalize()
        self.assertEqual(result, 1)
        self.assertEqual(len(summary["tests"]["unconfirmed"]), 1)

    def test_missing_reporter_log_cannot_pass(self):
        _, logs = self.fixture()
        (logs / "rust-test-stdout-fixture.log").unlink()
        result, summary = self.finalize()
        self.assertEqual(result, 1)
        self.assertIsNone(summary["tests"]["confirmed_failure_count"])

    def test_missing_command_log_cannot_pass(self):
        operations, _ = self.fixture()
        (operations / "tests.stdout.log").unlink()
        result, summary = self.finalize()
        self.assertEqual(result, 1)
        self.assertTrue(any("Missing stdout log" in problem for problem in summary["problems"]))

    def test_skipped_setup_cannot_pass(self):
        self.fixture()
        with mock.patch.dict(os.environ, {"KD4_STEP_RESULTS": "{}"}):
            result, summary = self.finalize()
        self.assertEqual(result, 1)
        self.assertTrue(any("Workflow step setup: missing" in problem for problem in summary["problems"]))

    def test_oversized_logs_are_explicit_and_fail(self):
        operations, _ = self.fixture()
        path = operations / "large.stderr.log"
        path.write_bytes(b"prefix" * 100 + b"tail")
        with mock.patch.object(windows_ci, "MAX_FILE_BYTES", 512):
            result, summary = self.finalize()
        self.assertEqual(result, 1)
        self.assertTrue(any("retention bound" in problem for problem in summary["problems"]))
        self.assertEqual(path.stat().st_size, 512)
        self.assertTrue(path.read_bytes().endswith(b"tail"))

    def test_nonzero_exit_retains_original_code_and_logs(self):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "exited 23"):
                self.evidence.run("failure-probe", [sys.executable, "-c",
                    "import sys; print('diagnostic', file=sys.stderr); sys.exit(23)"])
        operation = self.evidence.data["operations"][0]
        self.assertEqual(operation["exit_code"], 23)
        self.assertEqual(operation["status"], "failed")
        self.assertIsNotNone(operation["ended_at"])
        self.assertIn("diagnostic", (self.evidence.directory / operation["stderr"]).read_text())


if __name__ == "__main__":
    unittest.main()
