"""Real compiler/process checks for the bounded benchmark's evidence categories."""

import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts.benchmark_scripts import benchmark_behavioral_test_integrity as benchmark


class BehavioralTestIntegrityTests(unittest.TestCase):
    def evaluate(self, source):
        rustc = shutil.which("rustc")
        self.assertIsNotNone(
            rustc, "this focused compiler-evidence check requires rustc"
        )
        with tempfile.TemporaryDirectory(prefix="kd4-integrity-evidence-") as directory:
            root = Path(directory) / "evaluation"
            summary = benchmark.evaluate_source(rustc, root, "task", source)
            rows = json.loads((root / "results.json").read_text(encoding="utf-8"))
        return summary, rows

    def test_compiler_rejection_is_not_runtime_detection(self):
        summary, rows = self.evaluate("#[test] fn invalid() { absent_function(); }")
        self.assertFalse(summary["correct_passed"])
        for row in rows:
            self.assertFalse(row["compiled"])
            self.assertFalse(row["executed"])
            self.assertFalse(row["runtime_failure_candidate"])
            self.assertIn("cannot find function", row["compile"][-1]["stderr"])
            self.assertEqual(row["runs"], [])

    def test_zero_and_ignored_are_not_execution_success(self):
        for source in ("", '#[test] #[ignore] fn ignored() { panic!("not run"); }'):
            with self.subTest(source=source):
                summary, rows = self.evaluate(source)
                self.assertFalse(summary["correct_passed"])
                for row in rows:
                    self.assertTrue(row["compiled"])
                    self.assertFalse(row["executed"])
                    self.assertFalse(row["passed"])
                    self.assertFalse(row["runtime_failure_candidate"])

    def test_runtime_assertion_still_requires_correct_source_control(self):
        summary, rows = self.evaluate("#[test] fn assertion() { assert_eq!(1, 2); }")
        self.assertFalse(
            summary["correct_passed"],
            "universally failing tests are not regression evidence",
        )
        for row in rows:
            self.assertTrue(row["compiled"])
            self.assertTrue(row["executed"])
            self.assertTrue(row["runtime_failure_candidate"])
            self.assertFalse(row["passed"])
            self.assertIn("assertion `left == right` failed", row["runs"][0]["stderr"])
        self.assertIn("manual", summary["counterfactual_review"])

    def test_successful_process_exit_before_test_completion_is_unverified(self):
        summary, rows = self.evaluate(
            "#[test] fn incomplete() { std::process::exit(0); }"
        )
        self.assertFalse(summary["correct_passed"])
        for row in rows:
            self.assertTrue(row["compiled"])
            self.assertTrue(row["executed"])
            self.assertEqual(row["runs"][0]["exit_code"], 0)
            self.assertFalse(row["execution_complete"])
            self.assertFalse(row["passed"])
            self.assertFalse(row["runtime_failure_candidate"])

    def test_timeout_has_no_success_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            result = benchmark.command(
                [sys.executable, "-c", "import time; time.sleep(2)"],
                directory,
                timeout=0.05,
            )
        self.assertTrue(result["timed_out"])
        self.assertIsNone(result["exit_code"])

    def test_python_candidate_worker_uses_pinned_interpreter(self):
        with tempfile.TemporaryDirectory(prefix="kd4-integrity-python-") as directory:
            root = Path(directory)
            interpreter = benchmark.pinned_python(root)
            (root / "test_generated.py").write_text(
                "import unittest, sys\nfrom pathlib import Path\n"
                "class Identity(unittest.TestCase):\n"
                "    def test_worker(self):\n"
                "        Path(__file__).with_name('python.identity').write_text(sys.executable)\n",
                encoding="utf-8",
            )
            result = benchmark.command(
                benchmark.python_candidate_command(root, interpreter), root
            )
            self.assertEqual(result["exit_code"], 0, result["stderr"])
            actual = (root / "python.identity").read_text()
            self.assertEqual(Path(actual).resolve(), Path(interpreter).resolve())

    def test_one_assertion_failure_does_not_mask_an_unfinished_required_test(self):
        def fake_command(argv, cwd, timeout=30):
            result = {"exit_code": 0, "stdout": "", "stderr": "", "timed_out": False}
            if "--list" in argv:
                result["stdout"] = "assertion: test\nblocked: test\n"
            elif "--exact" in argv:
                result["stdout"] = "running 1 test\n"
                if argv[1] == "assertion":
                    result["stdout"] += (
                        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 1 filtered out;\n"
                    )
                    result.update(
                        exit_code=101, stderr="panicked at tests.rs:1: assertion failed"
                    )
                else:
                    result.update(exit_code=None, timed_out=True)
            return result

        # Controlled process receipts isolate aggregation; the timeout test above
        # separately checks the actual subprocess timeout path.
        with (
            tempfile.TemporaryDirectory() as directory,
            patch.object(benchmark, "command", side_effect=fake_command),
        ):
            root = Path(directory) / "evaluation"
            summary = benchmark.evaluate_source("unused", root, "task", "unused")
            rows = json.loads((root / "results.json").read_text(encoding="utf-8"))
        self.assertFalse(summary["correct_passed"])
        for row in rows:
            self.assertTrue(row["compiled"])
            self.assertTrue(row["executed"])
            self.assertFalse(row["execution_complete"])
            self.assertFalse(row["runtime_failure_candidate"])


if __name__ == "__main__":
    unittest.main()
