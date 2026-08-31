from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_perf_snapshot
from scripts import kd4_model_attempt_analysis


class Kd4PerfSnapshotTest(unittest.TestCase):
    def test_rollout_analysis_is_owned_by_turn_latency_audit(self) -> None:
        help_text = kd4_perf_snapshot.build_parser().format_help()

        self.assertNotIn("--rollout-jsonl", help_text)
        self.assertNotIn("--first-useful-action-report", help_text)

    def test_percentile_interpolates_sorted_values(self) -> None:
        self.assertEqual(kd4_perf_snapshot.percentile([4.0, 1.0, 3.0, 2.0], 0.5), 2.5)
        self.assertAlmostEqual(
            kd4_perf_snapshot.percentile([1.0, 2.0, 3.0, 4.0], 0.95),
            3.85,
        )

    def test_sample_statistics_share_one_ordering(self) -> None:
        builtin_sorted = sorted
        for values, expected in (
            ([4.0], (4.0, 4.0, 4.0, 4.0)),
            ([2.0, 1.0], (1.5, 1.95, 1.0, 2.0)),
            ([3.0, 1.0, 2.0], (2.0, 2.9, 1.0, 3.0)),
            ([4.0, 1.0, 3.0, 2.0], (2.5, 3.85, 1.0, 4.0)),
            ([2.0, 1.0, 2.0, 1.0], (1.5, 2.0, 1.0, 2.0)),
        ):
            with (
                self.subTest(values=values),
                mock.patch("builtins.sorted", wraps=builtin_sorted) as ordering,
            ):
                actual = kd4_perf_snapshot._ordered_sample_statistics(values)

            self.assertEqual(ordering.call_count, 1)
            for actual_value, expected_value in zip(actual, expected, strict=True):
                self.assertAlmostEqual(actual_value, expected_value)

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

    def test_scenario_streams_output_to_files_and_bounds_failure_diagnostics(
        self,
    ) -> None:
        scenario = kd4_perf_snapshot.Scenario(
            name="fixture",
            command=(sys.executable, "fixture.py"),
            cwd=Path.cwd(),
            default_iterations=1,
            category="test",
        )
        stdout = b"a" * (kd4_perf_snapshot.FAILURE_OUTPUT_TAIL_BYTES + 17)
        stderr = b"prefix" + b"\xff" * (
            kd4_perf_snapshot.FAILURE_OUTPUT_TAIL_BYTES + 23
        )

        def run(
            command: tuple[str, ...], **kwargs: object
        ) -> subprocess.CompletedProcess:
            self.assertNotIn("capture_output", kwargs)
            kwargs["stdout"].write(stdout)  # type: ignore[union-attr]
            kwargs["stderr"].write(stderr)  # type: ignore[union-attr]
            return subprocess.CompletedProcess(command, 7)

        with mock.patch.object(kd4_perf_snapshot.subprocess, "run", side_effect=run):
            result = kd4_perf_snapshot.measure_scenario(scenario)

        self.assertEqual(result.samples[0].stdout_bytes, len(stdout))
        self.assertEqual(result.samples[0].stderr_bytes, len(stderr))
        self.assertEqual(result.status, "failed")
        self.assertIn("command exited 7", result.reason or "")
        self.assertNotIn("prefix", result.reason or "")
        self.assertLessEqual(
            len((result.reason or "").encode("utf-8")),
            2 * kd4_perf_snapshot.FAILURE_OUTPUT_TAIL_BYTES * 3 + 128,
        )

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

    def test_focused_core_scenario_uses_the_named_core_target(self) -> None:
        scenario = kd4_perf_snapshot.scenario_catalog()["focused-core-test"]

        self.assertEqual(
            scenario.command,
            (
                "just",
                "core-test-fast",
                "core_lib",
                "-E",
                "test(typed_agents_inherit_every_non_root_tool_class)",
            ),
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
            kd4_perf_snapshot,
            "_git_text",
            return_value="# branch.oid head\n# branch.head main",
        ) as git_text:
            metadata = kd4_perf_snapshot.environment_metadata(
                Path.cwd(), hash_binary=False
            )

        self.assertEqual(metadata["dirtyPaths"], 0)
        self.assertEqual(metadata["head"], "head")
        self.assertEqual(metadata["branch"], "main")
        git_text.assert_called_once_with(
            Path.cwd(),
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        )

    def test_git_metadata_parses_dirty_detached_and_unborn_status(self) -> None:
        for status, expected in (
            (
                "# branch.oid abc\n# branch.head (detached)\n? untracked file",
                ("abc", None, 1),
            ),
            (
                "# branch.oid (initial)\n# branch.head feature/unborn",
                (None, "feature/unborn", 0),
            ),
        ):
            with (
                self.subTest(status=status),
                mock.patch.object(
                    kd4_perf_snapshot, "_git_text", return_value=status
                ) as git_text,
            ):
                metadata = kd4_perf_snapshot._git_repository_metadata(Path.cwd())

            self.assertEqual(metadata, expected)
            self.assertEqual(git_text.call_count, 1)

    def test_git_metadata_uses_compatibility_fallback_for_legacy_git(self) -> None:
        with mock.patch.object(
            kd4_perf_snapshot,
            "_git_text",
            side_effect=[None, " M tracked\n?? new", "head", "main"],
        ) as git_text:
            metadata = kd4_perf_snapshot._git_repository_metadata(Path.cwd())

        self.assertEqual(metadata, ("head", "main", 2))
        self.assertEqual(git_text.call_count, 4)

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
