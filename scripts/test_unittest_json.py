"""Exercise the real worker in isolated subprocesses, without third-party tools."""

from __future__ import annotations

import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

from scripts import unittest_json


RUNNER = Path(__file__).with_name("unittest_json.py").resolve()


class UnittestJsonTests(unittest.TestCase):
    def run_fixture(self, source, *args, files=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "test_example.py").write_text(
                textwrap.dedent(source), encoding="utf-8"
            )
            for name, content in (files or {}).items():
                (root / name).write_text(textwrap.dedent(content), encoding="utf-8")
            result = subprocess.run(
                [sys.executable, "-B", str(RUNNER), *args],
                cwd=root,
                env={**os.environ, "PYTHONIOENCODING": "utf-8"},
                capture_output=True,
                text=True,
                encoding="utf-8",
                timeout=30,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
        self.assertTrue(result.stdout.strip(), result.stderr)
        report = json.loads(result.stdout)
        unittest_json.validate_report(report)
        return result, report

    def test_outcomes_preserve_all_diagnostics_and_subtests(self):
        result, report = self.run_fixture(
            """
            import unittest
            class Cases(unittest.TestCase):
                def test_pass(self):
                    self.assertEqual(2 + 2, 4)
                def test_fail(self): self.assertEqual(2 + 2, 5, "assertion details")
                def test_error(self): raise RuntimeError("environment unavailable")
                @unittest.skip("skip reason")
                def test_skip(self): self.fail()
                @unittest.expectedFailure
                def test_expected(self): self.fail("expected details")
                @unittest.expectedFailure
                def test_unexpected(self): pass
                def test_subtests(self):
                    with self.subTest(n=1): self.assertEqual(1, 1)
                    with self.subTest(n=2): self.fail("subtest assertion")
                    with self.subTest(n=3): raise ValueError("subtest exception")
                    with self.subTest(n=4): self.skipTest("subtest skip")
        """,
            "test_example",
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertTrue(report["complete"])
        self.assertFalse(report["successful"])
        self.assertEqual(report["tests_run"], 7)
        records = {row["name"].rsplit(".", 1)[-1]: row for row in report["tests"]}
        self.assertEqual(
            {name: row["outcome"] for name, row in records.items()},
            {
                "test_pass": "success",
                "test_fail": "failure",
                "test_error": "error",
                "test_skip": "skipped",
                "test_expected": "expected_failure",
                "test_unexpected": "unexpected_success",
                "test_subtests": "error",
            },
        )
        self.assertEqual(records["test_fail"]["failure_type"], "assertion")
        self.assertEqual(records["test_error"]["failure_type"], "exception")
        self.assertEqual(records["test_skip"]["events"][0]["reason"], "skip reason")
        subtests = records["test_subtests"]["events"]
        self.assertEqual(
            [event["outcome"] for event in subtests],
            ["success", "failure", "error", "skipped"],
        )
        self.assertIn("(n=2)", subtests[1]["name"])
        self.assertEqual(subtests[3]["reason"], "subtest skip")
        for event, message in (
            (
                records["test_fail"]["events"][0],
                "AssertionError: 4 != 5 : assertion details",
            ),
            (
                records["test_error"]["events"][0],
                "RuntimeError: environment unavailable",
            ),
            (records["test_expected"]["events"][0], "AssertionError: expected details"),
            (subtests[1], "AssertionError: subtest assertion"),
            (subtests[2], "ValueError: subtest exception"),
        ):
            self.assertIn("Traceback (most recent call last):", event["traceback"])
            self.assertIn(message, event["traceback"])
        authored = "    def test_pass(self):\n        self.assertEqual(2 + 2, 4)\n"
        identity = records["test_pass"]["content_before"]
        self.assertEqual(
            identity["sha256"], hashlib.sha256(authored.encode()).hexdigest()
        )
        for record in records.values():
            self.assertEqual(record["content_before"], record["content_after"])
        self.assertEqual(report["content_identity_scope"], unittest_json.CONTENT_SCOPE)
        self.assertFalse(report["reusable_validation_receipt"])

    def test_stdout_is_json_even_for_raw_native_children_and_atexit_output(self):
        result, report = self.run_fixture(
            """
            import atexit, os, subprocess, sys, unittest
            print("import diagnostic")
            atexit.register(lambda: print("atexit diagnostic"))
            class Cases(unittest.TestCase):
                def test_output(self):
                    print("Unicode diagnostic: 雪")
                    os.write(1, b"raw stdout\\n")
                    sys.__stdout__.write("original stdout\\n")
                    sys.__stdout__.flush()
                    subprocess.run([sys.executable, "-c", "print('child stdout')"], check=True)
        """,
            "test_example",
            "-v",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["successful"])
        for message in (
            "import diagnostic",
            "Unicode diagnostic: 雪",
            "raw stdout",
            "original stdout",
            "child stdout",
            "atexit diagnostic",
        ):
            self.assertIn(message, result.stderr)
            self.assertNotIn(message, result.stdout)

    def test_named_selection_and_filter_use_actual_unittest_loader(self):
        source = """
            import unittest
            class Cases(unittest.TestCase):
                def test_pass(self): pass
                def test_other(self): self.fail("not selected")
        """
        for args in (("test_example.Cases.test_pass",), ("-k", "pass", "test_example")):
            with self.subTest(args=args):
                result, report = self.run_fixture(source, *args)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(report["selection"]["requested"], list(args))
                self.assertEqual(
                    report["selection"]["names"], ["test_example.Cases.test_pass"]
                )
                self.assertEqual(report["selection"]["selected_count"], 1)
                self.assertEqual(report["selection"]["unrun"], [])

    def test_zero_tests_and_invalid_selection_never_succeed(self):
        for args in (
            (),
            ("discover",),
            ("test_example",),
            ("-k", "absent", "test_example"),
            ("does_not_exist",),
            ("--bad-option",),
        ):
            with self.subTest(args=args):
                source = (
                    ""
                    if args in (("discover",), ("test_example",))
                    else (
                        "import unittest\nclass Cases(unittest.TestCase):\n    def test_pass(self): pass\n"
                    )
                )
                result, report = self.run_fixture(source, *args)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(report["complete"])
                self.assertFalse(report["successful"])
                self.assertEqual(report["tests_run"], 0)
                self.assertTrue(report["errors"])

    def test_duplicate_selection_is_explicit_and_not_executed(self):
        result, report = self.run_fixture(
            """
            import unittest
            class Cases(unittest.TestCase):
                def test_pass(self): raise AssertionError("must not execute")
        """,
            "test_example",
            "test_example.Cases.test_pass",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["tests_run"], 0)
        self.assertEqual(report["selection"]["selected_count"], 2)
        self.assertEqual(
            report["selection"]["duplicates"], ["test_example.Cases.test_pass"]
        )
        self.assertEqual(len(report["selection"]["unrun"]), 2)
        self.assertIn("Duplicate", report["errors"][0]["message"])

    def test_setup_and_teardown_errors_keep_fixture_and_test_events(self):
        cases = (
            (
                "def setUpModule(): raise RuntimeError('module setup')",
                "",
                "module setup",
                0,
                False,
            ),
            (
                "def tearDownModule(): raise RuntimeError('module teardown')",
                "",
                "module teardown",
                1,
                True,
            ),
            (
                "",
                "@classmethod\n    def setUpClass(cls): raise RuntimeError('class setup')",
                "class setup",
                0,
                False,
            ),
            (
                "",
                "@classmethod\n    def tearDownClass(cls): raise RuntimeError('class teardown')",
                "class teardown",
                1,
                True,
            ),
            (
                "",
                "def setUp(self): raise RuntimeError('test setup')",
                "test setup",
                1,
                True,
            ),
            (
                "",
                "def tearDown(self): raise RuntimeError('test teardown')",
                "test teardown",
                1,
                True,
            ),
        )
        for module_fixture, class_fixture, message, count, complete in cases:
            with self.subTest(message=message):
                source = (
                    f"import unittest\n{module_fixture}\nclass Cases(unittest.TestCase):\n"
                    f"    {class_fixture}\n    def test_pass(self): pass\n"
                )
                result, report = self.run_fixture(source, "discover")
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(report["tests_run"], count)
                self.assertEqual(report["complete"], complete)
                self.assertFalse(report["successful"])
                traces = [
                    event["traceback"]
                    for row in report["tests"]
                    for event in row["events"]
                    if event["traceback"]
                ]
                self.assertTrue(
                    any(f"RuntimeError: {message}" in trace for trace in traces)
                )
                self.assertEqual(len(report["selection"]["unrun"]), 1 - count)

    def test_multiple_failure_events_are_not_overwritten(self):
        _, report = self.run_fixture(
            """
            import unittest
            class Cases(unittest.TestCase):
                def test_both(self): self.fail("body failure")
                def tearDown(self): raise RuntimeError("teardown failure")
        """,
            "test_example",
        )
        record = report["tests"][0]
        self.assertEqual(record["outcome"], "error")
        self.assertEqual(
            [event["outcome"] for event in record["events"]], ["failure", "error"]
        )
        self.assertIn("body failure", record["events"][0]["traceback"])
        self.assertIn("teardown failure", record["events"][1]["traceback"])

    def test_failfast_has_explicit_unrun_coverage_and_buffered_details(self):
        result, report = self.run_fixture(
            """
            import sys, unittest
            class Cases(unittest.TestCase):
                def test_a_fail(self):
                    print("buffered stdout")
                    print("buffered stderr", file=sys.stderr)
                    self.fail("buffered failure")
                def test_z_unrun(self): self.fail("never executed")
        """,
            "-b",
            "-f",
            "test_example",
        )
        self.assertEqual(result.returncode, 1)
        self.assertFalse(report["complete"])
        self.assertEqual(report["tests_run"], 1)
        self.assertEqual(
            report["selection"]["unrun"], ["test_example.Cases.test_z_unrun"]
        )
        for text in (
            "buffered stdout",
            "buffered stderr",
            "AssertionError: buffered failure",
        ):
            self.assertIn(text, report["tests"][0]["events"][0]["traceback"])
            self.assertIn(text, result.stderr)

    def test_interrupt_never_produces_successful_complete_report(self):
        result, report = self.run_fixture(
            """
            import unittest
            class Cases(unittest.TestCase):
                def test_a_interrupt(self): raise KeyboardInterrupt()
                def test_z_unrun(self): pass
        """,
            "test_example",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(report["complete"])
        self.assertFalse(report["successful"])
        self.assertEqual(report["errors"][0]["kind"], "interrupted")
        self.assertEqual(report["tests"][0]["outcome"], "incomplete")
        self.assertEqual(
            report["selection"]["unrun"], ["test_example.Cases.test_z_unrun"]
        )

    def test_body_hash_observes_same_size_restored_mtime_change(self):
        result, report = self.run_fixture(
            """
            import os, pathlib, unittest
            class Cases(unittest.TestCase):
                def test_edit(self):
                    path = pathlib.Path(__file__)
                    stat = path.stat()
                    text = path.read_text()
                    path.write_text(text.replace("# before", "# after!"))
                    os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
                    # before
        """,
            "test_example",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        row = report["tests"][0]
        self.assertNotEqual(
            row["content_before"]["sha256"], row["content_after"]["sha256"]
        )
        self.assertFalse(report["reusable_validation_receipt"])

    def test_expected_failures_and_skips_can_succeed_without_losing_details(self):
        result, report = self.run_fixture(
            """
            import unittest
            class Cases(unittest.TestCase):
                @unittest.expectedFailure
                def test_expected(self): raise RuntimeError("expected exception")
                @unittest.skip("documented skip")
                def test_skip(self): pass
        """,
            "test_example",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["successful"])
        event = report["tests"][0]["events"][0]
        self.assertEqual(event["failure_type"], "exception")
        self.assertIn("RuntimeError: expected exception", event["traceback"])

    def test_stop_and_system_exit_cannot_report_success(self):
        for statement in ("self._outcome.result.stop()", "raise SystemExit(0)"):
            with self.subTest(statement=statement):
                result, report = self.run_fixture(
                    f"import unittest\nclass Cases(unittest.TestCase):\n    def test_stop(self): {statement}\n",
                    "test_example",
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(report["successful"])
                if statement.startswith("self"):
                    self.assertFalse(report["complete"])
                    self.assertEqual(report["errors"][0]["kind"], "interrupted")
                else:
                    # unittest classifies SystemExit in a test as an error.
                    self.assertIn(
                        "SystemExit: 0", report["tests"][0]["events"][0]["traceback"]
                    )

    def test_validator_rejects_malformed_or_inconsistent_reports(self):
        _, report = self.run_fixture(
            """
            import unittest
            class Cases(unittest.TestCase):
                def test_pass(self): pass
        """,
            "test_example",
        )
        changes = (
            lambda p: p.update(tests_run=0),
            lambda p: p.update(tests_run=True),
            lambda p: p.update(successful=False),
            lambda p: p.update(complete=False),
            lambda p: p.update(reusable_validation_receipt=True),
            lambda p: p["selection"].update(selected_count=2),
            lambda p: p["selection"].update(unrun=["missing"]),
            lambda p: p["tests"][0].update(events=[]),
            lambda p: p["tests"][0]["events"][0].update(outcome="error"),
            lambda p: p["tests"][0]["content_before"].update(sha256="not a hash"),
            lambda p: p["tests"][0].update(outcome=[]),
            lambda p: p["tests"][0].update(failure_type={}),
            lambda p: p["tests"][0]["events"][0].update(outcome=[]),
            lambda p: p["tests"][0]["events"][0].update(failure_type=[]),
            lambda p: p["errors"].append(
                {"kind": [], "message": "invalid", "traceback": None}
            ),
            lambda p: p["tests"][0].update(outcome="error"),
        )
        for change in changes:
            with self.subTest(change=change):
                altered = copy.deepcopy(report)
                change(altered)
                with self.assertRaises(ValueError):
                    unittest_json.validate_report(altered)


if __name__ == "__main__":
    unittest.main()
