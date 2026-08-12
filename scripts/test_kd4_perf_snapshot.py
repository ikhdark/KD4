from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_perf_snapshot
from scripts import kd4_model_attempt_analysis


class Kd4PerfSnapshotTest(unittest.TestCase):
    def test_percentile_interpolates_sorted_values(self) -> None:
        self.assertEqual(kd4_perf_snapshot.percentile([4.0, 1.0, 3.0, 2.0], 0.5), 2.5)
        self.assertAlmostEqual(
            kd4_perf_snapshot.percentile([1.0, 2.0, 3.0, 4.0], 0.95),
            3.85,
        )

    def test_successful_scenario_records_cold_and_warm_samples(self) -> None:
        scenario = kd4_perf_snapshot.Scenario(
            name="fixture",
            command=(sys.executable, "-c", "print('ok')"),
            cwd=Path.cwd(),
            default_iterations=3,
            category="test",
        )

        result = kd4_perf_snapshot.measure_scenario(scenario)

        self.assertEqual(result.status, "passed")
        self.assertEqual(len(result.samples), 3)
        self.assertIsNotNone(result.cold_ms)
        self.assertIsNotNone(result.warm_p50_ms)
        self.assertGreater(result.samples[0].stdout_bytes, 0)

    def test_missing_executable_is_skipped(self) -> None:
        scenario = kd4_perf_snapshot.Scenario(
            name="missing",
            command=("definitely-not-a-kd4-command",),
            cwd=Path.cwd(),
            default_iterations=1,
            category="test",
        )

        result = kd4_perf_snapshot.measure_scenario(scenario)

        self.assertEqual(result.status, "skipped")
        self.assertTrue(result.passed)

    def test_phase0_profile_covers_required_baseline_categories(self) -> None:
        catalog = kd4_perf_snapshot.scenario_catalog()
        categories = {
            catalog[name].category
            for name in kd4_perf_snapshot.PROFILE_SCENARIOS["phase0"]
        }

        self.assertTrue(
            {
                "startup",
                "repository",
                "validation",
                "test",
                "build",
                "app-server",
                "desktop-publish",
            }
            <= categories
        )

    def test_phase0_profile_runs_source_tools_promotion_gates(self) -> None:
        catalog = kd4_perf_snapshot.scenario_catalog()
        phase0 = kd4_perf_snapshot.PROFILE_SCENARIOS["phase0"]

        self.assertIn("source-tools-runtime-test", phase0)
        self.assertIn("source-tools-bounded-search-test", phase0)
        self.assertEqual(
            catalog["source-tools-runtime-test"].command[-1],
            "test(source_tools_execute_search_and_read_end_to_end)",
        )
        self.assertEqual(
            catalog["source-tools-bounded-search-test"].command[-1],
            "test(representative_large_repository_search_stays_within_walk_and_output_bounds)",
        )

    def test_atomic_json_writer_replaces_target(self) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            target = Path(tempdir) / "snapshot.json"
            kd4_perf_snapshot.write_json_atomic(target, {"ok": True})

            self.assertEqual(
                json.loads(target.read_text(encoding="utf-8")), {"ok": True}
            )

    def test_atomic_json_writer_removes_temporary_file_on_serialization_error(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            target = Path(tempdir) / "snapshot.json"

            with self.assertRaises(TypeError):
                kd4_perf_snapshot.write_json_atomic(target, {"bad": object()})

            self.assertEqual(list(Path(tempdir).glob("*.tmp")), [])
            self.assertFalse(target.exists())

    def test_environment_metadata_distinguishes_git_failure_from_clean_tree(
        self,
    ) -> None:
        with mock.patch.object(kd4_perf_snapshot, "_git_text", return_value=None):
            metadata = kd4_perf_snapshot.environment_metadata(
                Path.cwd(), hash_binary=False
            )

        self.assertIsNone(metadata["dirtyPaths"])

    def test_environment_metadata_reports_zero_dirty_paths_for_clean_tree(
        self,
    ) -> None:
        with mock.patch.object(
            kd4_perf_snapshot, "_git_text", side_effect=["", "head", "main"]
        ):
            metadata = kd4_perf_snapshot.environment_metadata(
                Path.cwd(), hash_binary=False
            )

        self.assertEqual(metadata["dirtyPaths"], 0)

    def test_model_attempt_analysis_filters_groups_and_reconciles(self) -> None:
        def attempt(request_id: str, attempt_id: str, wait: int, **overrides: object) -> dict[str, object]:
            record: dict[str, object] = {
                "event.name": "codex.model_attempt",
                "sampling_request_id": request_id,
                "attempt_id": attempt_id,
                "retry_index": 0,
                "outcome": "success",
                "model": "gpt-test",
                "transport": "responses_http",
                "request_kind": "initial",
                "dispatch_ready_us": 10,
                "first_model_output_us": 10 + wait,
                "input_token_count": 10,
                "cached_input_token_count": 4,
                "uncached_input_token_count": 6,
                "reconciliation_residual_bytes": 2,
                "logical_request_bytes": 102,
                "base_instructions_bytes": 0,
                "tool_schemas_bytes": 100,
                "conversation_history_bytes": 0,
                "current_input_bytes": 0,
                "repository_context_bytes": 0,
                "memory_bytes": 0,
                "skills_bytes": 0,
                "other_injected_context_bytes": 0,
                "envelope_overhead_bytes": 0,
            }
            record.update(overrides)
            return record

        records = [
            attempt("clean-1", "a", 100),
            attempt("clean-2", "b", 200, cached_input_token_count=5, uncached_input_token_count=5),
            attempt("retry", "c", 300),
            attempt("retry", "d", 400, retry_index=1),
            attempt("failed", "e", 500, outcome="failed"),
            attempt("cancelled", "f", 600, outcome="cancelled"),
            attempt("missing", "g", 700, first_model_output_us=None),
        ]
        analysis = kd4_model_attempt_analysis.analyze(records, {"malformed_json": 1})

        self.assertEqual(analysis["totalPhysicalAttempts"], 7)
        self.assertEqual(analysis["includedLogicalRequests"], 2)
        self.assertEqual(analysis["outcomeCounts"], {"success": 5, "failed": 1, "cancelled": 1})
        self.assertEqual(analysis["exclusionCounts"]["multiple_physical_attempts"], 1)
        self.assertEqual(analysis["exclusionCounts"]["outcome_failed"], 1)
        self.assertEqual(analysis["exclusionCounts"]["outcome_cancelled"], 1)
        self.assertEqual(analysis["exclusionCounts"]["missing_first_model_output"], 1)
        group = analysis["groups"][0]
        self.assertEqual(group["sampleCount"], 2)
        self.assertEqual(group["firstOutputWaitUs"]["p50"], 150.0)
        bins = group["predictors"]["cached_input_token_count"]["quantileBins"]
        self.assertEqual(sum(item["count"] for item in bins), 2)
        self.assertEqual(analysis["componentReconciliation"]["coveredCount"], 2)
        self.assertEqual(analysis["componentReconciliation"]["withinToleranceCount"], 2)
        self.assertEqual(analysis["componentReconciliation"]["suppliedResidualMismatchCount"], 0)
        self.assertEqual(len(analysis["rows"]), 2)
        self.assertEqual(analysis["interpretation"], "observational and non-causal")
        human = kd4_model_attempt_analysis.render(analysis)
        self.assertIn("tokens p50/p95", human)
        self.assertIn("spearman=", human)
        self.assertIn("reconciliation:", human)

    def test_model_attempt_spearman_handles_ties(self) -> None:
        self.assertEqual(
            kd4_model_attempt_analysis.spearman([1.0, 1.0, 2.0], [1.0, 1.0, 3.0]),
            1.0,
        )
        self.assertIsNone(kd4_model_attempt_analysis.spearman([1.0, 1.0], [2.0, 3.0]))

    def test_model_attempt_jsonl_loader_and_parser_flags(self) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            path = Path(tempdir) / "attempts.jsonl"
            path.write_text(
                "not-json\n"
                + json.dumps({"event.name": "something.else"})
                + "\n"
                + json.dumps({"fields": {"event.name": "codex.model_attempt", "attempt_id": "a"}})
                + "\n",
                encoding="utf-8",
            )
            records, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
        self.assertEqual(len(records), 1)
        self.assertEqual(exclusions, {"malformed_json": 1, "not_model_attempt": 1})
        args = kd4_perf_snapshot.build_parser().parse_args(
            ["--model-attempt-jsonl", "attempts.jsonl", "--model-attempt-report", "report.txt"]
        )
        self.assertEqual(args.model_attempt_jsonl, [Path("attempts.jsonl")])
        self.assertEqual(args.model_attempt_report, Path("report.txt"))


if __name__ == "__main__":
    unittest.main()
