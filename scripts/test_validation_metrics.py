"""Validation ledger regression tests; no Cargo builds or test binaries."""
from __future__ import annotations

import contextlib
import copy
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import rust_build_status, rust_test_runner as runner, validation_metrics as metrics
from scripts.test_rust_test_runner import RunnerTestCase


def record(run_id="run-1", **updates):
    ledger = metrics.ValidationMetrics(["run-target", "example"], clock=lambda: 10)
    value = ledger.record
    value.update(run_id=run_id, started_at="2026-10-04T10:00:00+00:00",
                 finished_at="2026-10-04T10:00:10+00:00",
                 outcome="passed", lifecycle_seconds=10)
    value["proof"]["obligations"] = ["example"]
    value.update(updates)
    return value


class SummaryTest(unittest.TestCase):
    def test_unknown_is_not_zero_or_current_proof(self):
        value = record()
        value["commands"] = [{"wall_seconds": 7, "reported_build_seconds": 5,
                              "reported_test_seconds": .5}]
        value["phases"]["admission"] = 1
        report = metrics.summarize([value, copy.deepcopy(value)])
        self.assertEqual(report["launches"], 1)
        self.assertEqual(report["duplicate_records"], 1)
        self.assertEqual(report["work"]["wall_seconds"]["seconds"], 7)
        self.assertEqual(report["work"]["unattributed_lifecycle"]["seconds"], 2)
        self.assertEqual(report["work"]["reported_build_seconds"]["seconds"], 5)
        self.assertIsNone(report["work"]["compile_seconds"]["seconds"])
        self.assertEqual(report["work"]["cleanup_seconds"]["unknown"], 1)
        self.assertEqual(report["proof_freshness"], {"unknown": 1})
        self.assertEqual(report["launches_producing_current_proof"], 0)
        self.assertEqual(report["reasons"], {"unknown": 1})

    def test_conflicting_identity_and_bad_duration_fail_closed(self):
        value = record()
        conflict = copy.deepcopy(value)
        conflict["outcome"] = "failed"
        with self.assertRaisesRegex(ValueError, "conflicting"):
            metrics.summarize([value, conflict])
        for duration in (-1, float("nan"), float("inf"), True, "1"):
            with self.subTest(duration=duration), self.assertRaises(ValueError):
                metrics.summarize([record(lifecycle_seconds=duration)])

    def test_repeat_observations_do_not_infer_causal_reason_from_incomplete_history(self):
        for outcome in ("failed", "cancelled", "timed_out"):
            previous = record(outcome=outcome)
            current = record("run-2", started_at="2026-10-04T10:01:00+00:00",
                             finished_at="2026-10-04T10:01:10+00:00")
            report = metrics.summarize([current, previous])
            self.assertEqual(report["reasons"], {"unknown": 2})
            self.assertEqual(report["justified_reruns"], 0)
            self.assertEqual(report["repeat_observations"][0]["previous_outcome"], outcome)
            current["proof"]["obligations"] = ["unrelated"]
            self.assertEqual(metrics.summarize([previous, current])["repeat_observations"], [])
            current["proof"]["obligations"] = ["example"]
            previous["finished_at"] = "2026-10-04T10:02:00+00:00"
            self.assertEqual(metrics.summarize([previous, current])["repeat_observations"], [])

    def test_current_proof_redundancy_and_reuse_are_separate_dimensions(self):
        previous = record(input_coverage="complete", input_digest="a" * 64, prerequisite_ids=[])
        previous["proof"].update(status="passed", freshness="current", coverage="verified",
                                 freshness_basis={"revision": "r1"}, covered_paths=["src"])
        current = copy.deepcopy(previous)
        current.update(run_id="run-2", started_at="2026-10-04T10:01:00+00:00",
                       finished_at="2026-10-04T10:01:10+00:00", reason="redundant",
                       reason_basis={"prior_run_id": "run-1", "prior_proof_current_at_launch": True,
                                     "no_repeat_requirement": True})
        report = metrics.summarize([current, previous])
        self.assertEqual(report["redundant_reruns"], 1)
        self.assertEqual(report["launches_producing_current_proof"], 1)
        self.assertEqual(report["work"]["redundant_lifecycle"]["seconds"], 10)
        current["input_digest"] = "b" * 64
        self.assertEqual(metrics.summarize([current, previous])["redundant_reruns"], 0)
        current["input_digest"] = previous["input_digest"]
        previous["input_coverage"] = "declared_not_exhaustive"
        self.assertEqual(metrics.summarize([current, previous])["redundant_reruns"], 0)
        current["mode"] = "reused"
        current["proof"]["reused_from_run_id"] = "run-1"
        report = metrics.summarize([current, previous])
        self.assertEqual((report["launches"], report["reuse_decisions"]), (1, 1))

    def test_current_label_alone_does_not_establish_current_proof(self):
        value = record()
        value["proof"].update(status="passed", freshness="current")
        report = metrics.summarize([value])
        self.assertEqual(report["launches_producing_current_proof"], 0)
        self.assertEqual(report["proof_freshness"], {"unknown": 1})

    def test_reason_and_proof_are_independent_and_timestamps_are_normalized(self):
        value = record(reason="inputs_changed", reason_basis={"changed_inputs": ["src/a"]})
        report = metrics.summarize([value])
        self.assertEqual(report["justified_reruns"], 1)
        self.assertEqual(report["launches_producing_current_proof"], 0)
        second = record("run-2", started_at="2026-10-04T06:01:00-04:00",
                        finished_at="2026-10-04T06:01:10-04:00")
        report = metrics.summarize([second, value])
        self.assertEqual([r["run_id"] for r in report["records"]], ["run-1", "run-2"])
        for malformed in (None, [], {"kind": metrics.KIND}, record(reason_basis=[]),
                          record(started_at="2026-10-04T10:00:00")):
            with self.subTest(malformed=malformed), self.assertRaises((ValueError, KeyError)):
                metrics.summarize([malformed])


class PersistenceTest(unittest.TestCase):
    def test_snapshots_hash_bound_references_dedup_and_no_summary_mining(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ledger = metrics.ValidationMetrics(["test"])
            ledger.bind(root)
            self.assertEqual(json.loads(ledger.path.read_text())["outcome"], "running")
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                ledger.finish("passed")
            reference = json.loads(stderr.getvalue())
            raw = ledger.path.read_bytes()
            self.assertEqual(reference["sha256"], hashlib.sha256(raw).hexdigest())
            output = json.dumps({"stderr": stderr.getvalue(), "stdout": "Ran 123 tests in 12.0s\nOK"})
            rollout = root / "rollout.jsonl"
            rollout.write_text(json.dumps({"type": "response_item", "payload": {
                "type": "function_call_output", "call_id": "call-1", "output": output,
            }}) + "\n", encoding="utf-8")
            records, coverage, unresolved = metrics.read_inputs([root])
            self.assertEqual(len(coverage), 2)
            self.assertEqual(unresolved, [])
            self.assertEqual(metrics.summarize(records)["duplicate_records"], 1)
            self.assertEqual(metrics.summarize(records)["launches"], 1)
            ledger.path.write_text("{}")
            _, _, unresolved = metrics.read_inputs([rollout])
            self.assertEqual(len(unresolved), 1)
            self.assertIn("hash/size", unresolved[0]["error"])

    def test_retention_failure_preserves_inline_ledger(self):
        ledger = metrics.ValidationMetrics(["test"])
        with mock.patch.object(metrics, "write_json_atomic", side_effect=OSError("unavailable")):
            with contextlib.redirect_stderr(io.StringIO()):
                ledger.bind(Path("unused"))
        self.assertIsNone(ledger.path)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            ledger.finish("cancelled")
        value = json.loads(stderr.getvalue())
        self.assertEqual(value["outcome"], "cancelled")
        self.assertEqual(value["proof"]["status"], "cancelled")

    def test_cli_describes_and_reports_complete_json(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "validation-fixture.json"
            path.write_text(json.dumps(record()), encoding="utf-8")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(metrics.main([str(path), "--json"]), 0)
            report = json.loads(output.getvalue())
            self.assertEqual(report["launches"], 1)
            self.assertEqual(report["coverage"][0]["bytes"], path.stat().st_size)
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(metrics.main(["--describe"]), 0)

    def test_non_string_metadata_kinds_do_not_abort_rollout_extraction(self):
        kinds = [[], ["bin"], {"target": "bin"}, None, 1, True]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "rollout.jsonl"
            rows = []
            for index, kind in enumerate(kinds):
                rows.append({"type": "response_item", "payload": {
                    "type": "function_call_output",
                    "output": json.dumps({
                        "kind": kind,
                        "stdout": json.dumps(record(f"run-{index}")),
                        "arguments": json.dumps(record("not-an-output")),
                    }),
                }})
            path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(metrics.main([str(path), "--json"]), 0)
            report = json.loads(output.getvalue())
            self.assertEqual(report["launches"], len(kinds))
            self.assertEqual(report["coverage"][0]["records"], len(kinds))
            self.assertEqual(report["unresolved"], [])
            self.assertEqual(
                {row["run_id"] for row in report["records"]},
                {f"run-{index}" for index in range(len(kinds))},
            )

    def test_source_strings_are_not_metrics(self):
        value = record()
        self.assertEqual(list(metrics._packets({"arguments": json.dumps(value)})), [])
        self.assertEqual(list(metrics._packets("test result: ok. 4 passed; 0 failed")), [])


class RunnerMetricsTest(RunnerTestCase):
    def invoke(self, *, outcome="passed", admission_error=None):
        instance, _ = self.runner()
        manifest = self.temp_dir / "manifest.toml"
        manifest.write_text("fixture")
        stdout, stderr = io.StringIO(), io.StringIO()

        def execute(args, **kwargs):
            if outcome in {"cancelled", "timed_out", "cleanup_failed"}:
                raise runner.RunnerError("fixture", outcome=outcome)
            return subprocess.CompletedProcess(args, 1 if outcome == "failed" else 0,
                "", "Finished test profile in 2s\nSummary [ 0.2s] 1 tests run: 1 passed\n")

        instance.executor = execute

        def target(*args, **kwargs):
            instance._checked(["cargo", "nextest", "run"], env={}, capture=runner.CAPTURE_BOTH)
            return {"example": ["test"]}

        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(runner.Manifest, "load", return_value=instance.manifest))
            stack.enter_context(mock.patch.object(runner, "load_metadata", return_value=instance.metadata))
            stack.enter_context(mock.patch.object(runner, "RustTestRunner", return_value=instance))
            stack.enter_context(mock.patch.object(instance, "run_target", side_effect=target))
            stack.enter_context(mock.patch.object(runner, "execution_dependency_manifest",
                return_value={"coverage": "declared_not_exhaustive", "automatic_replay_allowed": False}))
            if admission_error is not None:
                stack.enter_context(mock.patch.object(rust_build_status, "reserve_rust_test_target", side_effect=admission_error))
            stack.enter_context(contextlib.redirect_stdout(stdout))
            stack.enter_context(contextlib.redirect_stderr(stderr))
            code = runner.main(["--manifest", str(manifest), "run-target", "core_all", "-E", "test(one)"])
        reference = next(json.loads(line) for line in stderr.getvalue().splitlines()
                         if line.startswith('{"bytes":') and metrics.REF_KIND in line)
        value = json.loads(Path(reference["path"]).read_text())
        return code, value, stdout.getvalue()

    def test_real_dispatch_links_success_receipt_and_lifecycle_without_claiming_freshness(self):
        code, value, stdout = self.invoke()
        self.assertEqual(code, 0)
        receipt = json.loads(stdout)
        self.assertEqual(receipt["validation_run_id"], value["run_id"])
        self.assertEqual(value["outcome"], "passed")
        self.assertEqual(value["proof"]["status"], "passed")
        self.assertEqual(value["proof"]["completed_tests"], receipt["completed_tests"])
        self.assertEqual(value["proof"]["freshness"], "unknown")
        self.assertEqual(value["input_coverage"], "declared_not_exhaustive")
        self.assertEqual(len(value["input_digest"]), 64)
        self.assertIsNotNone(value["phases"]["admission"])
        self.assertIsNotNone(value["phases"]["preparation"])
        self.assertEqual(value["commands"][0]["reported_build_seconds"], 2)
        self.assertEqual(value["commands"][0]["reported_test_seconds"], .2)
        self.assertIsNone(value["commands"][0]["cleanup_seconds"])
        self.assertFalse(rust_build_status.cargo_lock_is_busy(self.target_dir))

    def test_failure_cancellation_timeout_and_admission_failure_are_not_dropped(self):
        for outcome in ("failed", "cancelled", "timed_out", "cleanup_failed"):
            with self.subTest(outcome=outcome):
                # Cleanup-failed admission quarantines the target by design;
                # use independent targets rather than deleting quarantine.
                self.target_dir = self.temp_dir / outcome
                self.target_dir.mkdir()
                code, value, stdout = self.invoke(outcome=outcome)
                self.assertEqual(code, 2)
                self.assertEqual(value["outcome"], outcome)
                self.assertEqual(value["commands"][0]["outcome"], outcome)
                self.assertEqual(stdout, "")
        code, value, _ = self.invoke(admission_error=TimeoutError("fixture"))
        self.assertEqual(code, 2)
        self.assertEqual(value["outcome"], "timed_out")
        self.assertEqual(value["commands"], [])
        self.assertIsNotNone(value["phases"]["admission"])

    def test_default_executor_reports_wait_and_cleanup_on_success_and_timeout(self):
        for timeout in (False, True):
            timings = {}
            env = dict(os.environ, CODEX_RUST_TEST_LOG_DIR=str(self.temp_dir))
            if timeout:
                env["CODEX_RUST_TEST_TIMEOUT_SECS"] = ".05"
            with self.subTest(timeout=timeout):
                try:
                    runner._default_executor(
                        [sys.executable, "-c", "import time; time.sleep(5)" if timeout else "print('ok')"],
                        cwd=self.temp_dir, env=env, capture=runner.CAPTURE_BOTH, timings=timings,
                    )
                except runner.RunnerError as error:
                    self.assertTrue(timeout)
                    self.assertEqual(error.outcome, "timed_out")
                else:
                    self.assertFalse(timeout)
                self.assertGreaterEqual(timings["process_wait_seconds"], 0)
                self.assertGreaterEqual(timings["cleanup_seconds"], 0)


if __name__ == "__main__":
    unittest.main()

