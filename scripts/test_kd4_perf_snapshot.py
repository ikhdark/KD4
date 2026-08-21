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
        self.assertFalse(result.passed)
        self.assertTrue(result.required)

    def test_install_dir_override_is_independent_of_checkout_location(self) -> None:
        install_dir = Path("C:/custom/local-codex")

        catalog = kd4_perf_snapshot.scenario_catalog(
            Path("C:/unrelated/checkout"), install_dir=install_dir
        )

        self.assertEqual(
            Path(catalog["installed-codex-version"].command[0]).parent,
            install_dir,
        )

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
        def attempt(
            request_id: str, attempt_id: str, wait: int, **overrides: object
        ) -> dict[str, object]:
            record: dict[str, object] = {
                "event.name": "codex.model_attempt",
                "turn_id": "turn-1",
                "generation_index": 2,
                "generation_purpose": "implementation",
                "generation_disposition": "decision_bearing",
                "relevant_state_fingerprint": "state-1",
                "sampling_request_id": request_id,
                "attempt_id": attempt_id,
                "retry_index": 0,
                "outcome": "success",
                "model": "gpt-test",
                "transport": "responses_http",
                "request_kind": "initial",
                "dispatch_ready_us": 10,
                "first_model_output_us": 11,
                "first_actionable_output_us": 10 + wait,
                "completed_us": 10 + wait,
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
            attempt(
                "clean-2",
                "b",
                200,
                cached_input_token_count=5,
                uncached_input_token_count=5,
            ),
            attempt("retry", "c", 300, outcome="failed"),
            attempt("retry", "d", 400, retry_index=1),
            attempt("failed", "e", 500, outcome="failed"),
            attempt("cancelled", "f", 600, outcome="cancelled"),
            attempt("missing", "g", 700, first_actionable_output_us=None),
        ]
        analysis = kd4_model_attempt_analysis.analyze(records, {"malformed_json": 1})

        self.assertEqual(analysis["totalPhysicalAttempts"], 7)
        self.assertEqual(analysis["includedPhysicalAttempts"], 4)
        self.assertEqual(analysis["includedLogicalRequests"], 3)
        self.assertEqual(
            analysis["outcomeCounts"], {"success": 4, "failed": 2, "cancelled": 1}
        )
        self.assertEqual(analysis["exclusionCounts"]["no_terminal_success"], 2)
        self.assertEqual(
            analysis["exclusionCounts"]["missing_first_actionable_output_us"], 1
        )
        group = analysis["groups"][0]
        self.assertEqual(group["sampleCount"], 3)
        self.assertEqual(group["generationPurpose"], "implementation")
        self.assertEqual(group["generationDisposition"], "decision_bearing")
        self.assertEqual(group["decisionLatencyUs"]["p50"], 200.0)
        bins = group["predictors"]["cached_input_token_count"]["quantileBins"]
        self.assertEqual(sum(item["count"] for item in bins), 3)
        self.assertEqual(analysis["componentReconciliation"]["coveredCount"], 3)
        self.assertEqual(analysis["componentReconciliation"]["withinToleranceCount"], 3)
        self.assertEqual(
            analysis["componentReconciliation"]["suppliedResidualMismatchCount"], 0
        )
        self.assertEqual(len(analysis["rows"]), 3)
        retry_row = next(
            row for row in analysis["rows"] if row["sampling_request_id"] == "retry"
        )
        self.assertEqual(retry_row["retry_count"], 1)
        self.assertEqual(retry_row["retry_overhead_us"], 300.0)
        self.assertEqual(retry_row["decision_latency_us"], 700.0)
        self.assertIn("dispatch-to-first-actionable-output", analysis["interpretation"])
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
                + json.dumps(
                    {"fields": {"event.name": "codex.model_attempt", "attempt_id": "a"}}
                )
                + "\n",
                encoding="utf-8",
            )
            records, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
        self.assertEqual(len(records), 1)
        self.assertEqual(exclusions, {"malformed_json": 1, "not_model_attempt": 1})
        args = kd4_perf_snapshot.build_parser().parse_args(
            [
                "--model-attempt-jsonl",
                "attempts.jsonl",
                "--model-attempt-report",
                "report.txt",
            ]
        )
        self.assertEqual(args.model_attempt_jsonl, [Path("attempts.jsonl")])
        self.assertEqual(args.model_attempt_report, Path("report.txt"))

    def test_model_attempt_jsonl_loader_deduplicates_overlapping_files(self) -> None:
        attempt = {
            "event.name": "codex.model_attempt",
            "sampling_request_id": "request",
            "attempt_id": "attempt",
            "retry_index": 0,
            "outcome": "success",
        }
        conflicting = {**attempt, "outcome": "failed"}
        with tempfile.TemporaryDirectory() as tempdir:
            first = Path(tempdir) / "first.jsonl"
            second = Path(tempdir) / "second.jsonl"
            first.write_text(json.dumps(attempt) + "\n", encoding="utf-8")
            second.write_text(
                json.dumps(attempt) + "\n" + json.dumps(conflicting) + "\n",
                encoding="utf-8",
            )

            records, diagnostics = kd4_model_attempt_analysis.load_jsonl(
                [first, second]
            )

        self.assertEqual(records, [attempt, conflicting])
        self.assertEqual(diagnostics["duplicate_physical_attempt_collapsed"], 1)
        self.assertEqual(diagnostics["conflicting_physical_attempt_duplicate"], 1)

    def test_stable_context_components_join_to_every_physical_attempt(self) -> None:
        attempt = {
            "event.name": "codex.model_attempt",
            "sampling_request_id": "request",
            "attempt_id": "attempt",
            "retry_index": 0,
            "outcome": "success",
            "provider_baseline": "fresh_full_replay",
            "fresh_response_id_established": True,
            "wire_request_bytes": 1200,
            "input_token_count": 1000,
            "cached_input_token_count": 750,
        }
        component = {
            "event.name": "codex.model_context_component",
            "sampling_request_id": "request",
            "attempt_id": "attempt",
            "retry_index": 0,
            "component_kind": "repository",
            "contract_version": 1,
            "semantic_id": "repository:v1:opaque",
            "content_hash": "abcdef012345",
            "serialized_bytes": 4000,
            "approx_tokens": 1000,
            "active": True,
            "local_reused": True,
        }
        with tempfile.TemporaryDirectory() as tempdir:
            path = Path(tempdir) / "attempts.jsonl"
            path.write_text(
                json.dumps(attempt) + "\n" + json.dumps(component) + "\n",
                encoding="utf-8",
            )
            records, diagnostics = kd4_model_attempt_analysis.load_jsonl([path])

        self.assertEqual(diagnostics, {})
        stable = kd4_model_attempt_analysis.analyze(records)["stableContext"]
        self.assertEqual(stable["averageActiveContextTokens"], 1000.0)
        self.assertEqual(stable["peakActiveContextTokens"], 1000.0)
        self.assertEqual(stable["localReusedBytes"], 4000.0)
        self.assertEqual(stable["providerCachedShare"], 0.75)
        self.assertEqual(stable["successfulRebases"], 1)
        self.assertEqual(stable["componentVersions"][0]["requestAppearances"], 1)
        self.assertEqual(
            stable["componentVersions"][0]["cumulativeLogicalExposureTokens"],
            1000.0,
        )

    def test_stable_context_exposure_counts_retries_independent_of_provider_cache(
        self,
    ) -> None:
        records = []
        for index in range(10):
            records.append(
                {
                    "event.name": "codex.model_attempt",
                    "sampling_request_id": "request"
                    if index < 2
                    else f"request-{index}",
                    "attempt_id": f"attempt-{index}",
                    "retry_index": index if index < 2 else 0,
                    "outcome": "success",
                    "input_token_count": 1000,
                    "cached_input_token_count": 900,
                    "_stable_context_components": [
                        {
                            "component_kind": "repository",
                            "contract_version": 1,
                            "semantic_id": "repository:v1:opaque",
                            "content_hash": "abcdef012345",
                            "serialized_bytes": 4000,
                            "approx_tokens": 1000,
                            "active": True,
                            "local_reused": index > 0,
                        }
                    ],
                }
            )

        analysis = kd4_model_attempt_analysis.analyze(records)
        stable = analysis["stableContext"]

        self.assertEqual(analysis["totalPhysicalAttempts"], 10)
        self.assertEqual(analysis["totalLogicalRequests"], 9)
        self.assertEqual(stable["cumulativeLogicalContextTokens"], 10_000.0)
        self.assertEqual(
            stable["componentVersions"][0]["cumulativeLogicalExposureTokens"],
            10_000.0,
        )
        self.assertEqual(stable["providerCachedShare"], 0.9)
        self.assertEqual(stable["localConstructedBytes"], 4000.0)
        self.assertEqual(stable["localReusedBytes"], 36_000.0)
        self.assertEqual(stable["componentCacheHits"], 9.0)

    def test_stable_context_summary_tolerates_missing_provider_cache_fields(
        self,
    ) -> None:
        stable = kd4_model_attempt_analysis.analyze(
            [
                {
                    "event.name": "codex.model_attempt",
                    "sampling_request_id": "request",
                    "attempt_id": "attempt",
                    "retry_index": 0,
                    "outcome": "failed",
                    "input_token_count": None,
                    "cached_input_token_count": None,
                }
            ]
        )["stableContext"]

        self.assertIsNone(stable["providerCachedShare"])
        self.assertEqual(stable["wireRequestBytes"], 0.0)


if __name__ == "__main__":
    unittest.main()
