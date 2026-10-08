"""Regression coverage for client-observed model inference diagnostics."""

from __future__ import annotations

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_model_attempt_analysis as analysis
from scripts import kd4_perf_snapshot as snapshot


def attempt(identity="a", **overrides):
    record = {
        "event.name": "codex.model_attempt",
        "sampling_request_id": identity,
        "attempt_id": identity,
        "retry_index": 0,
        "outcome": "success",
        "model": "fixture",
        "provider": "fixture-provider",
        "transport": "responses_http",
        "request_kind": "initial",
        "dispatch_ready_us": 100,
        "stream_established_us": 150,
        "first_provider_event_us": 200,
        "first_model_output_us": 400,
        "first_actionable_output_us": 500,
        "first_visible_output_us": 450,
        "completed_us": 1000,
        "request_construction_us": 80,
        "queue_us": 10,
        "connection_setup_us": 10,
        "transport_us": 60,
        "input_token_count": 1000,
        "cached_input_token_count": 900,
        "uncached_input_token_count": 100,
    }
    record.update(overrides)
    return record


class ModelInferenceTest(unittest.TestCase):
    def test_phases_use_offsets_not_absolute_timestamps_or_prefill_labels(self):
        report = analysis.analyze([attempt()])
        phases = report["phaseTiming"]
        metrics = phases["groups"][0]["metrics"]
        for field, expected in {
            "request_construction_us": 80,
            "queue_us": 10,
            "connection_setup_us": 10,
            "transport_us": 60,
            "dispatchToFirstProviderEventUs": 100,
            "dispatchToFirstModelOutputUs": 300,
            "dispatchToFirstActionableOutputUs": 400,
            "dispatchToFirstVisibleOutputUs": 350,
            "dispatchToCompletionUs": 900,
            "firstModelOutputToCompletionUs": 600,
        }.items():
            self.assertEqual(metrics[field], {
                "count": 1, "p50": expected, "p95": expected, "unavailableCount": 0,
            })
        self.assertIsNone(phases["serverPrefillUs"])
        self.assertIsNone(phases["serverDecodeTokensPerSecond"])
        human = analysis.render(report)
        self.assertIn("not server inference phases", human)
        self.assertIn("consumer backpressure", human)
        self.assertIn("dispatchToFirstModelOutputUs", human)

    def test_missing_usage_and_failed_attempts_retain_phases_separately(self):
        records = [
            attempt(input_token_count=None),
            attempt("b", outcome="failed", first_model_output_us=None,
                    first_actionable_output_us=None, first_visible_output_us=None),
            attempt("c", outcome="cancelled"),
        ]
        report = analysis.analyze(records)
        self.assertEqual(report["includedLogicalRequests"], 0)
        groups = report["phaseTiming"]["groups"]
        self.assertEqual({group["outcome"] for group in groups}, {"success", "failed", "cancelled"})
        failed = next(group for group in groups if group["outcome"] == "failed")
        self.assertEqual(failed["metrics"]["dispatchToCompletionUs"]["count"], 1)
        self.assertEqual(failed["metrics"]["dispatchToFirstModelOutputUs"], {
            "count": 0, "p50": None, "p95": None, "unavailableCount": 1,
        })

    def test_cohorts_do_not_pool_provider_tier_or_connection_state(self):
        records = [attempt()]
        for index, dimension in enumerate(("provider", "service_tier", "connection_reused")):
            records.append(attempt(str(index), **{dimension: True if dimension == "connection_reused" else "other"}))
        report = analysis.analyze(records)
        self.assertEqual(len(report["groups"]), 4)
        self.assertEqual(len(report["phaseTiming"]["groups"]), 4)
        self.assertTrue(all(group["sampleCount"] == 1 for group in report["groups"]))

    def test_invalid_offset_order_is_excluded_but_direct_measurements_survive(self):
        for overrides in (
            {"completed_us": 300}, {"first_model_output_us": 50},
            {"first_visible_output_us": 1001}, {"stream_established_us": 201},
            {"first_actionable_output_us": float("nan")},
            {"completed_us": -1}, {"dispatch_ready_us": True},
        ):
            with self.subTest(overrides=overrides):
                report = analysis.analyze([attempt(**overrides)])
                self.assertEqual(report["includedLogicalRequests"], 0)
                group = report["phaseTiming"]["groups"][0]
                self.assertEqual(group["invalidOffsetAttempts"], 1)
                self.assertEqual(group["metrics"]["dispatchToCompletionUs"]["count"], 0)
                self.assertEqual(group["metrics"]["request_construction_us"]["count"], 1)

    def test_visible_and_actionable_are_not_totally_ordered(self):
        for visible in (450, 550, None):
            self.assertEqual(analysis.analyze([attempt(first_visible_output_us=visible)])["includedLogicalRequests"], 1)
        legacy = attempt()
        for key in ("completed_us", "first_model_output_us", "first_provider_event_us", "stream_established_us"):
            del legacy[key]
        self.assertEqual(analysis.analyze([legacy])["includedLogicalRequests"], 1)

    def test_retries_remain_lower_bounds_and_phases_are_physical(self):
        records = [attempt(outcome="failed"), attempt(attempt_id="b", retry_index=1)]
        report = analysis.analyze(records)
        self.assertEqual(report["unmeasuredInterAttemptGaps"], 1)
        self.assertEqual(report["rows"][0]["decision_latency_us"], 1300)
        self.assertEqual(sum(group["physicalAttemptCount"] for group in report["phaseTiming"]["groups"]), 2)
        partial = analysis.analyze([attempt(retry_index=3)])
        self.assertEqual(partial["groups"], [])
        self.assertEqual(partial["phaseTiming"]["groups"][0]["physicalAttemptCount"], 1)

    def test_native_reuse_counter_has_precedence_with_legacy_fallback(self):
        records = [
            {"local_component_reuse_count": 7},
            {"local_component_reuse_count": 0, "component_cache_hits": 99},
            {"component_cache_hits": 3},
        ]
        self.assertEqual(analysis.analyze(records)["stableContext"]["componentCacheHits"], 10)

    def test_distribution_sorts_once_and_preserves_interpolation(self):
        for values in ([], [7], [30, 10], [4, 4, 0, 100]):
            expected = {
                "count": len(values),
                "p50": round(analysis.percentile(values, .5), 3) if values else None,
                "p95": round(analysis.percentile(values, .95), 3) if values else None,
            }
            with mock.patch("builtins.sorted", wraps=sorted) as sorting:
                self.assertEqual(analysis._distribution(values), expected)
                self.assertEqual(sorting.call_count, 1)

    def test_analysis_only_cli_roundtrip_launches_no_workflows_or_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output, human = root / "input.jsonl", root / "report.json", root / "report.txt"
            source.write_text(json.dumps(attempt()) + "\n", encoding="utf-8")
            stdout = io.StringIO()
            with mock.patch.object(snapshot, "environment_metadata", side_effect=AssertionError("metadata probe")), mock.patch.object(snapshot, "measure_scenario", side_effect=AssertionError("workflow launch")), contextlib.redirect_stdout(stdout):
                result = snapshot.main([
                    "--analysis-only", "--model-attempt-jsonl", str(source),
                    "--output", str(output), "--model-attempt-report", str(human), "--json",
                ])
            self.assertEqual(result, 0)
            payload = json.loads(stdout.getvalue())
            self.assertTrue(payload["analysisOnly"])
            self.assertIsNone(payload["environment"])
            self.assertEqual(payload["results"], [])
            self.assertEqual(json.loads(output.read_text(encoding="utf-8")), payload)
            self.assertIn("physical-attempt phases", human.read_text(encoding="utf-8"))

    def test_analysis_only_rejects_missing_input_and_conflicting_flags(self):
        for args in ([], ["--scenario", "python-startup"], ["--hash-binary"]):
            with self.subTest(args=args), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
                snapshot.main(["--analysis-only", *args])
            self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
