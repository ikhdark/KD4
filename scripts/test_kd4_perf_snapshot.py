from __future__ import annotations

import json
import io
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_perf_snapshot
from scripts import kd4_model_attempt_analysis


class Kd4PerfSnapshotTest(unittest.TestCase):
    def setUp(self):
        capture_dir = tempfile.TemporaryDirectory()
        self.addCleanup(capture_dir.cleanup)
        patch = mock.patch.object(tempfile, "gettempdir", return_value=capture_dir.name)
        patch.start()
        self.addCleanup(patch.stop)

    def test_profiles_select_available_scenarios_and_quick_stays_lightweight(self):
        catalog = kd4_perf_snapshot.scenario_catalog()
        quick = kd4_perf_snapshot.PROFILE_SCENARIOS["quick"]
        self.assertEqual(quick, ("python-startup", "git-status"))
        for profile, names in kd4_perf_snapshot.PROFILE_SCENARIOS.items():
            with self.subTest(profile=profile):
                self.assertTrue(set(names) <= catalog.keys())

    def test_preflight_errors_launch_nothing_and_preserve_reports(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            output = root / "existing.json"
            output.write_text("previous evidence")
            cases = [
                ["--model-attempt-jsonl", str(root / "missing")],
                ["--model-attempt-jsonl", str(root)],
                ["--model-attempt-report", str(root / "report")],
                ["--iterations", "0"],
                ["--timeout-seconds", "0"],
                ["--output", str(root)],
                ["--model-attempt-jsonl", str(output)],
                [
                    "--model-attempt-jsonl",
                    str(root / "missing"),
                    "--model-attempt-report",
                    str(output),
                ],
            ]
            for flags in cases:
                with (
                    self.subTest(flags=flags),
                    mock.patch.object(kd4_perf_snapshot, "measure_scenario") as measure,
                    mock.patch.object(sys, "stderr", io.StringIO()),
                    self.assertRaises(SystemExit) as raised,
                ):
                    kd4_perf_snapshot.main(["--output", str(output), *flags])
                self.assertEqual(raised.exception.code, 2)
                measure.assert_not_called()
                self.assertEqual(output.read_text(), "previous evidence")

    def test_preflight_rejects_hardlinked_report_aliases_without_overwriting_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "attempts.jsonl"
            original = b'{"event.name":"codex.model_attempt","sampling_request_id":"request"}\n'
            source.write_bytes(original)
            alias = root / "report.txt"
            alias.hardlink_to(source)
            with (
                mock.patch.object(kd4_perf_snapshot, "measure_scenario") as measure,
                mock.patch.object(kd4_perf_snapshot, "environment_metadata") as environment,
                mock.patch.object(sys, "stdout", io.StringIO()),
                mock.patch.object(sys, "stderr", io.StringIO()),
                self.assertRaises(SystemExit) as raised,
            ):
                kd4_perf_snapshot.main([
                    "--analysis-only", "--model-attempt-jsonl", str(source),
                    "--model-attempt-report", str(alias),
                ])
            self.assertEqual(raised.exception.code, 2)
            measure.assert_not_called()
            environment.assert_not_called()
            self.assertEqual(source.read_bytes(), original)
            self.assertEqual(alias.read_bytes(), original)

    def test_cleanup_abort_saves_completed_evidence_and_stops_even_if_allowed(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "snapshot.json"
            first = kd4_perf_snapshot.Scenario(
                "python-startup",
                (sys.executable, "-c", "print('done')"),
                Path(temp),
                1,
                "test",
            )
            catalog = {
                "python-startup": first,
                "git-status": first,
                "installed-codex-version": first,
            }
            original = kd4_perf_snapshot._run_scenario
            calls = []

            def run(command, **kwargs):
                calls.append(command)
                if len(calls) == 2:
                    raise RuntimeError(
                        "cleanup unconfirmed; remaining measurements aborted"
                    )
                return original(command, **kwargs)

            with (
                mock.patch.object(
                    kd4_perf_snapshot, "scenario_catalog", return_value=catalog
                ),
                mock.patch.object(
                    kd4_perf_snapshot, "environment_metadata", return_value={}
                ),
                mock.patch.object(kd4_perf_snapshot, "_run_scenario", side_effect=run),
                mock.patch.object(sys, "stdout", io.StringIO()),
            ):
                status = kd4_perf_snapshot.main(
                    [
                        "--scenario",
                        "python-startup",
                        "--scenario",
                        "git-status",
                        "--scenario",
                        "installed-codex-version",
                        "--output",
                        str(output),
                        "--json",
                        "--allow-failures",
                        "--allow-incomplete",
                    ]
                )
            payload = json.loads(output.read_text())
            self.assertEqual(status, 1)
            self.assertEqual(len(calls), 2)
            self.assertEqual(payload["results"][0]["status"], "passed")
            self.assertEqual(payload["results"][1]["samples"][0]["outcome"], "aborted")
            self.assertEqual(payload["pendingScenarios"], ["installed-codex-version"])
            self.assertFalse(payload["complete"])
            self.assertFalse(payload["ok"])
            self.assertIn("cleanup unconfirmed", payload["abortReason"])

    def test_human_abort_exposes_full_log_paths(self):
        console = io.StringIO()
        with (
            mock.patch.object(
                kd4_perf_snapshot, "environment_metadata", return_value={}
            ),
            mock.patch.object(
                kd4_perf_snapshot,
                "_run_scenario",
                side_effect=RuntimeError("cleanup unconfirmed"),
            ),
            mock.patch.object(sys, "stdout", console),
            mock.patch.object(sys, "stderr", io.StringIO()),
        ):
            status = kd4_perf_snapshot.main(["--scenario", "python-startup"])
        self.assertEqual(status, 1)
        paths = list(Path(tempfile.gettempdir()).glob("kd4-perf-*.log"))
        self.assertEqual(len(paths), 2)
        for path in paths:
            self.assertIn(str(path), console.getvalue())

    @unittest.skipUnless(os.name == "nt", "Windows tree termination")
    def test_tree_cleanup_failure_aborts_remaining_measurements(self) -> None:
        scenario = kd4_perf_snapshot.Scenario(
            "timeout", (sys.executable,), Path.cwd(), 2, "test"
        )
        with mock.patch.object(
            kd4_perf_snapshot,
            "owned_process",
            side_effect=kd4_perf_snapshot.CleanupFailed("cleanup failed"),
        ) as launch:
            with self.assertRaisesRegex(RuntimeError, "remaining measurements aborted"):
                kd4_perf_snapshot.measure_scenario(scenario, timeout_seconds=1)
        self.assertEqual(launch.call_count, 1)

    def test_timeout_stops_descendants_before_another_measurement(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            marker = Path(temp) / "late-write"
            child = (
                "import pathlib, sys, time; "
                "print('descendant-ready', flush=True); time.sleep(3); "
                "pathlib.Path(sys.argv[1]).write_text('leaked')"
            )
            parent = (
                "import subprocess, sys, time; "
                "subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]]); "
                "time.sleep(30)"
            )
            scenario = kd4_perf_snapshot.Scenario(
                "timeout",
                (sys.executable, "-c", parent, child, str(marker)),
                Path.cwd(),
                2,
                "test",
            )
            result = kd4_perf_snapshot.measure_scenario(scenario, timeout_seconds=1)
            # The descendant exits on its own even if cleanup is broken, so the
            # regression cannot leave a persistent process behind.
            time.sleep(3)
            self.assertEqual(result.status, "failed")
            self.assertEqual(len(result.samples), 1)
            self.assertEqual(result.samples[0].outcome, "timeout")
            self.assertIsNone(result.samples[0].exit_code)
            self.assertGreaterEqual(result.samples[0].elapsed_ms, 900)
            self.assertIsNone(result.p50_ms)
            self.assertIn(
                "descendant-ready", Path(result.samples[0].stdout_path).read_text()
            )
            self.assertIn("descendant-ready", result.reason or "")
            self.assertFalse(marker.exists(), "timed-out descendant kept running")

    def test_default_installed_scenario_matches_publisher_bin_directory(self) -> None:
        with (
            mock.patch.dict(kd4_perf_snapshot.os.environ, {}, clear=True),
            mock.patch.object(Path, "home", return_value=Path("C:/fixture")),
        ):
            catalog = kd4_perf_snapshot.scenario_catalog()
        self.assertEqual(
            Path(catalog["installed-codex-version"].command[0]),
            Path("C:/fixture/Desktop/LOCAL-KD/bin/codex.exe"),
        )

    def test_elapsed_time_excludes_capture_setup_and_report_preparation(self) -> None:
        clock_ns = 0
        original_capture = tempfile.TemporaryFile
        original_tail = kd4_perf_snapshot._output_size_and_tail

        def capture():
            nonlocal clock_ns
            clock_ns += 1_000_000_000
            return original_capture()

        def run(command, **kwargs):
            nonlocal clock_ns
            clock_ns += 25_000_000
            return subprocess.CompletedProcess(command, 0)

        def tail(handle):
            nonlocal clock_ns
            clock_ns += 500_000_000
            return original_tail(handle)

        scenario = kd4_perf_snapshot.Scenario(
            "fixture", (sys.executable,), Path.cwd(), 1, "test"
        )
        with (
            mock.patch.object(tempfile, "TemporaryFile", side_effect=capture),
            mock.patch.object(kd4_perf_snapshot, "_run_scenario", side_effect=run),
            mock.patch.object(
                kd4_perf_snapshot, "_output_size_and_tail", side_effect=tail
            ),
            mock.patch.object(
                kd4_perf_snapshot.time, "perf_counter_ns", side_effect=lambda: clock_ns
            ),
        ):
            result = kd4_perf_snapshot.measure_scenario(scenario)
        self.assertEqual(result.status, "passed")
        self.assertEqual(result.samples[0].elapsed_ms, 25)

    def test_failed_sample_leaves_statistics_and_warm_percentiles_exclude_cold(
        self,
    ) -> None:
        # Cold 100 ms, warm 10 and 20 ms, then a 1 ms failed invocation.
        runs = iter(
            [(100_000_000, 0), (10_000_000, 0), (20_000_000, 0), (1_000_000, 1)]
        )
        clock_ns = 0

        def run(command, **kwargs):
            nonlocal clock_ns
            elapsed_ns, returncode = next(runs)
            clock_ns += elapsed_ns
            return subprocess.CompletedProcess(command, returncode)

        scenario = kd4_perf_snapshot.Scenario(
            "fixture", (sys.executable,), Path.cwd(), 4, "test"
        )
        with (
            mock.patch.object(kd4_perf_snapshot, "_run_scenario", side_effect=run),
            mock.patch.object(
                kd4_perf_snapshot.time, "perf_counter_ns", side_effect=lambda: clock_ns
            ),
        ):
            result = kd4_perf_snapshot.measure_scenario(scenario)

        self.assertEqual(result.status, "failed")
        self.assertEqual([sample.elapsed_ms for sample in result.samples], [100, 10, 20, 1])
        self.assertEqual(result.cold_ms, 100)
        self.assertAlmostEqual(result.warm_p50_ms, 15)
        self.assertAlmostEqual(result.warm_p95_ms, 19.5)
        self.assertAlmostEqual(result.p50_ms, 20)
        self.assertAlmostEqual(result.p95_ms, 92)
        self.assertEqual((result.min_ms, result.max_ms), (10, 100))

    def test_model_attempt_latency_discloses_unmeasured_inter_attempt_gaps(
        self,
    ) -> None:
        def attempt(request_id, attempt_id, retry_index, outcome):
            return {
                "sampling_request_id": request_id,
                "attempt_id": attempt_id,
                "retry_index": retry_index,
                "outcome": outcome,
                "dispatch_ready_us": 0,
                "first_actionable_output_us": 100,
                "completed_us": 200,
                "input_token_count": 10,
                "cached_input_token_count": 0,
                "uncached_input_token_count": 10,
            }

        analysis = kd4_model_attempt_analysis.analyze(
            [
                attempt("retried", "a", 0, "failed"),
                attempt("retried", "b", 1, "failed"),
                attempt("retried", "c", 2, "success"),
                attempt("clean", "d", 0, "success"),
            ]
        )

        self.assertEqual(analysis["includedLogicalRequests"], 2)
        self.assertEqual(analysis["retriedLogicalRequests"], 1)
        self.assertEqual(analysis["unmeasuredInterAttemptGaps"], 2)
        retried = next(
            row for row in analysis["rows"] if row["sampling_request_id"] == "retried"
        )
        # Two nonterminal attempts (200 us each) plus terminal 100 us; the
        # backoff between attempts is absent because it cannot be observed.
        self.assertEqual(retried["decision_latency_us"], 500.0)
        self.assertIn("not observable", analysis["interpretation"])
        self.assertIn(
            "excluded from decision latency): 2 across 1 retried requests",
            kd4_model_attempt_analysis.render(analysis),
        )

    def test_conflicting_attempts_are_quarantined_and_missing_context_is_not_zero(self):
        attempt = {
            "event.name": "codex.model_attempt",
            "sampling_request_id": "request",
            "attempt_id": "failed",
            "retry_index": 0,
            "duration_ms": 10,
        }
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "attempts.jsonl"
            path.write_text(
                "\n".join(
                    json.dumps(row)
                    for row in [
                        attempt,
                        {**attempt, "duration_ms": 20},
                        {**attempt, "attempt_id": "success", "retry_index": 1},
                    ]
                )
            )
            records, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
            self.assertEqual(records, [])
            self.assertEqual(exclusions["conflicted_logical_request_attempts"], 3)
        summary = kd4_model_attempt_analysis._stable_context_summary(
            [{}, {"logical_context_tokens": 0}, {"logical_context_tokens": 100}]
        )
        self.assertEqual(summary["averageActiveContextTokens"], 50)
        self.assertEqual(summary["measuredContextAttempts"], 2)
        self.assertEqual(summary["missingContextAttempts"], 1)

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
        self.assertEqual(kd4_perf_snapshot.percentile([4.0, 1.0], 0), 1.0)
        self.assertEqual(kd4_perf_snapshot.percentile([4.0, 1.0], 1), 4.0)
        for values, fraction in (([], 0.5), ([1.0], -0.1), ([1.0], 1.1)):
            with self.subTest(values=values, fraction=fraction), self.assertRaises(ValueError):
                kd4_perf_snapshot.percentile(values, fraction)

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
        self.assertTrue(all(sample.stdout_path is None for sample in result.samples))
        self.assertEqual(list(Path(tempfile.gettempdir()).glob("kd4-perf-*.log")), [])

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

        with mock.patch.object(kd4_perf_snapshot, "_run_scenario", side_effect=run):
            result = kd4_perf_snapshot.measure_scenario(scenario)

        self.assertEqual(result.samples[0].stdout_bytes, len(stdout))
        self.assertEqual(result.samples[0].stderr_bytes, len(stderr))
        self.assertEqual(Path(result.samples[0].stdout_path).read_bytes(), stdout)
        self.assertEqual(Path(result.samples[0].stderr_path).read_bytes(), stderr)
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

        with mock.patch.object(kd4_perf_snapshot, "_run_scenario") as launch:
            result = kd4_perf_snapshot.measure_scenario(scenario)
        launch.assert_not_called()

        self.assertEqual(result.status, "skipped")
        self.assertFalse(result.passed)
        self.assertTrue(result.required)
        self.assertEqual(result.samples, ())
        for field in ("cold_ms", "warm_p50_ms", "warm_p95_ms", "p50_ms", "p95_ms", "min_ms", "max_ms"):
            self.assertIsNone(getattr(result, field), field)
        self.assertIn("definitely-not-a-kd4-command", result.reason)

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
                "test(=agent::task_capabilities::tests::typed_agents_inherit_every_non_root_tool_class)",
            ),
        )

    def test_atomic_json_writer_replaces_target(self) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            target = Path(tempdir) / "snapshot.json"
            target.write_bytes(b'{"old": true}\n')
            kd4_perf_snapshot.write_json_atomic(target, {"ok": True})

            self.assertEqual(
                json.loads(target.read_text(encoding="utf-8")), {"ok": True}
            )

    def test_atomic_json_writer_removes_temporary_file_on_serialization_error(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            target = Path(tempdir) / "snapshot.json"
            target.write_bytes(b'{"old": true}\n')

            with self.assertRaises(TypeError):
                kd4_perf_snapshot.write_json_atomic(target, {"bad": object()})

            self.assertEqual(list(Path(tempdir).glob("*.tmp")), [])
            self.assertEqual(target.read_bytes(), b'{"old": true}\n')
            with mock.patch.object(
                kd4_perf_snapshot.os, "replace", side_effect=OSError("publish failed")
            ):
                with self.assertRaisesRegex(OSError, "publish failed"):
                    kd4_perf_snapshot.write_json_atomic(target, {"new": True})
            self.assertEqual(target.read_bytes(), b'{"old": true}\n')
            self.assertEqual(list(Path(tempdir).glob("*.tmp")), [])

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
        self.assertNotIn("memory_bytes", group["predictors"])
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
        self.assertEqual(
            analysis["quantileMethod"],
            "linear interpolation at (n - 1) * p on sorted samples",
        )
        self.assertIn(analysis["quantileMethod"], human)
        self.assertIn(analysis["sampleLimitations"], human)
        self.assertIn("tokens p50/p95", human)
        self.assertIn("spearman=", human)
        self.assertIn("reconciliation:", human)

    def test_model_attempt_spearman_handles_ties(self) -> None:
        self.assertEqual(
            kd4_model_attempt_analysis.spearman([1.0, 1.0, 2.0], [1.0, 1.0, 3.0]),
            1.0,
        )
        # Average ranks are [1.5, 1.5, 3, 4] and [1, 2, 3, 4] below;
        # asymmetric ties distinguish average ranks from minimum/dense ranks.
        self.assertEqual(
            kd4_model_attempt_analysis.spearman([1, 1, 2, 3], [1, 2, 3, 4]),
            0.948683,
        )
        self.assertIsNone(kd4_model_attempt_analysis.spearman([1.0, 1.0], [2.0, 3.0]))

    def test_model_attempt_percentiles_describe_observed_samples(self) -> None:
        self.assertEqual(kd4_model_attempt_analysis.percentile([10, 30], 0.95), 29)
        self.assertEqual(
            kd4_model_attempt_analysis._distribution([7]),
            {"count": 1, "p50": 7, "p95": 7},
        )
        self.assertEqual(
            kd4_model_attempt_analysis._distribution([]),
            {"count": 0, "p50": None, "p95": None},
        )

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

    def test_model_attempt_jsonl_loader_preserves_outer_event_fields(self) -> None:
        for alias in ("event.name", "event_name"):
            for nested_key in ("fields", "attributes", "body"):
                with self.subTest(alias=alias, nested_key=nested_key):
                    values = [
                        {
                            alias: "codex.model_attempt",
                            "attempt_id": "outer",
                            "outcome": "success",
                            nested_key: {
                                "event.name": "codex.model_context_component",
                                "attempt_id": "nested",
                                "outcome": "failed",
                                "duration_ms": 123,
                            },
                        },
                        {
                            alias: "unrelated.event",
                            nested_key: {"event.name": "codex.model_attempt"},
                        },
                    ]
                    with tempfile.TemporaryDirectory() as tempdir:
                        path = Path(tempdir) / "attempts.jsonl"
                        path.write_text(
                            "\n".join(json.dumps(value) for value in values),
                            encoding="utf-8",
                        )
                        records, exclusions = kd4_model_attempt_analysis.load_jsonl(
                            [path]
                        )
                    self.assertEqual(len(records), 1)
                    self.assertEqual(records[0]["attempt_id"], "outer")
                    self.assertEqual(records[0]["outcome"], "success")
                    self.assertEqual(records[0]["duration_ms"], 123)
                    self.assertEqual(exclusions, {"not_model_attempt": 1})

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

        self.assertEqual(records, [])
        self.assertEqual(diagnostics["conflicted_logical_request_attempts"], 2)
        self.assertEqual(diagnostics["duplicate_physical_attempt_collapsed"], 1)
        self.assertEqual(diagnostics["conflicting_physical_attempt_duplicate"], 1)

    def test_attempt_duplicates_ignore_export_envelopes_not_measurements(self) -> None:
        attempt = {
            "event.name": "codex.model_attempt",
            "sampling_request_id": "request",
            "attempt_id": "attempt",
            "retry_index": 0,
            "outcome": "success",
            "first_actionable_output_us": 5_000_000,
        }
        for wrapper in ("fields", "attributes", "body"):
            with self.subTest(wrapper=wrapper), tempfile.TemporaryDirectory() as temp:
                path = Path(temp) / "events.jsonl"
                wrapped = {
                    "timestamp": "2026-09-27T00:00:00Z",
                    "level": "INFO",
                    "target": "telemetry",
                    wrapper: {**attempt, "event_name": attempt["event.name"]},
                }
                del wrapped[wrapper]["event.name"]
                path.write_text(
                    "\n".join(map(json.dumps, [attempt, wrapped])), encoding="utf-8"
                )
                records, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
                self.assertEqual(len(records), 1)
                self.assertEqual(
                    exclusions, {"duplicate_physical_attempt_collapsed": 1}
                )
                wrapped[wrapper]["first_actionable_output_us"] += 1
                path.write_text(
                    "\n".join(map(json.dumps, [attempt, wrapped])), encoding="utf-8"
                )
                records, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
                self.assertEqual(records, [])
                self.assertEqual(
                    exclusions["conflicting_physical_attempt_duplicate"], 1
                )

    def test_partial_retry_sequences_keep_evidence_out_of_clean_latency_groups(
        self,
    ) -> None:
        for indexes in ([3], [0, 3], [None], [0, 0]):
            with self.subTest(indexes=indexes), tempfile.TemporaryDirectory() as temp:
                records = [
                    {
                        "event.name": "codex.model_attempt",
                        "sampling_request_id": "request",
                        "attempt_id": str(i),
                        "retry_index": index,
                        "outcome": "success" if i == len(indexes) - 1 else "failed",
                        "dispatch_ready_us": 0,
                        "first_actionable_output_us": 5_000_000,
                        "completed_us": 6_000_000,
                        "input_token_count": 100_000,
                        "cached_input_token_count": 90_000,
                        "uncached_input_token_count": 10_000,
                    }
                    for i, index in enumerate(indexes)
                ]
                path = Path(temp) / "events.jsonl"
                path.write_text("\n".join(map(json.dumps, records)), encoding="utf-8")
                loaded, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
                report = kd4_model_attempt_analysis.analyze(loaded, exclusions)
                self.assertEqual(report["partialRetryCoverageRequests"], 1)
                self.assertEqual(report["cleanIncludedLogicalRequests"], 0)
                self.assertEqual(report["groups"], [])
                self.assertEqual(report["rows"][0]["input_token_count"], 100_000)
                self.assertFalse(report["rows"][0]["retry_coverage_complete"])
                self.assertEqual(report["stableContext"]["providerCachedShare"], 0.9)
                if indexes[-1] == 3:
                    self.assertEqual(report["rows"][0]["retry_count"], 3)
                    self.assertEqual(report["unmeasuredInterAttemptGaps"], 3)
                self.assertIn(
                    "partial retry coverage", kd4_model_attempt_analysis.render(report)
                )

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
        retry = {**attempt, "attempt_id": "retry", "retry_index": 1,
                 "fresh_response_id_established": False}
        retry_component = {**component, "attempt_id": "retry", "retry_index": 1,
                           "local_reused": False, "approx_tokens": 500,
                           "serialized_bytes": 2000}
        orphan = {**component, "retry_index": 9}
        with tempfile.TemporaryDirectory() as tempdir:
            path = Path(tempdir) / "attempts.jsonl"
            path.write_text(
                "\n".join(map(json.dumps, [component, retry, orphan, attempt, retry_component, component])) + "\n",
                encoding="utf-8",
            )
            records, diagnostics = kd4_model_attempt_analysis.load_jsonl([path])

        self.assertEqual(diagnostics, {
            "duplicate_context_component_collapsed": 1, "orphan_context_component": 1,
        })
        self.assertEqual(
            {row["attempt_id"]: row["_stable_context_components"] for row in records},
            {"attempt": [component], "retry": [retry_component]},
        )
        stable = kd4_model_attempt_analysis.analyze(records)["stableContext"]
        self.assertEqual(stable["measuredContextAttempts"], 2)
        self.assertEqual(stable["missingContextAttempts"], 0)
        self.assertEqual(stable["averageActiveContextTokens"], 750.0)
        self.assertEqual(stable["peakActiveContextTokens"], 1000.0)
        self.assertEqual(stable["localReusedBytes"], 4000.0)
        self.assertEqual(stable["localConstructedBytes"], 2000.0)
        self.assertEqual(stable["providerCachedShare"], 0.75)
        self.assertEqual(stable["successfulRebases"], 1)
        self.assertEqual(stable["componentVersions"][0]["requestAppearances"], 2)
        self.assertEqual(
            stable["componentVersions"][0]["cumulativeLogicalExposureTokens"],
            1500.0,
        )

    def test_conflicting_component_measurements_never_choose_an_export_order(self):
        component = {
            "event.name": "codex.model_context_component",
            "sampling_request_id": "request", "attempt_id": "a", "retry_index": 0,
            "component_kind": "repository", "contract_version": 1,
            "semantic_id": "repo", "content_hash": "same-content",
            "serialized_bytes": 400, "approx_tokens": 100,
            "active": True, "local_reused": False,
        }
        request = {
            "event.name": "codex.model_attempt", "sampling_request_id": "request",
            "attempt_id": "a", "retry_index": 0,
        }
        unaffected = {**request, "sampling_request_id": "other", "attempt_id": "b"}
        # A duplicate is the same observation, not merely the same join key.
        # Conflicting versions must quarantine the request just like conflicting
        # physical attempts, rather than selecting whichever export came first.
        for field, value in (("approx_tokens", 200), ("serialized_bytes", 800),
                             ("active", False), ("local_reused", True), ("contract_version", 2)):
            conflict = {**component, field: value}
            for versions in ((component, conflict), (conflict, component)):
                with self.subTest(field=field, versions=versions), tempfile.TemporaryDirectory() as temp:
                    path = Path(temp) / "attempts.jsonl"
                    path.write_text("\n".join(map(json.dumps, [request, *versions, unaffected])), encoding="utf-8")
                    records, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
                    self.assertEqual(records, [unaffected])
                    self.assertEqual(exclusions["conflicting_context_component_duplicate"], 1)
                    self.assertEqual(exclusions["conflicted_logical_request_attempts"], 1)
                    self.assertNotIn("duplicate_context_component_collapsed", exclusions)
                    self.assertIsNone(kd4_model_attempt_analysis.analyze(records)["stableContext"]["averageActiveContextTokens"])

    def test_incomplete_component_identities_preserve_legacy_attempts(self):
        # Absent/empty identity is not evidence of a shared logical request.
        # Reject unjoinable components, never quarantine unrelated legacy rows.
        missing = object()
        invalid_fields = [
            (field, value)
            for field in ("sampling_request_id", "attempt_id")
            for value in (missing, None, "")
        ] + [("retry_index", value) for value in (missing, None, True, -1)]
        for field, value in invalid_fields:
            attempt = {
                "event.name": "codex.model_attempt", "sampling_request_id": "request",
                "attempt_id": "a", "retry_index": 0,
            }
            if value is missing:
                attempt.pop(field)
            else:
                attempt[field] = value
            unrelated = {**attempt, "attempt_id": "unrelated"}
            component = {
                **attempt, "event.name": "codex.model_context_component",
                "component_kind": "repository", "contract_version": 1,
                "semantic_id": "repo", "content_hash": "same-content",
                "serialized_bytes": 400, "approx_tokens": 100, "active": True,
            }
            conflict = {**component, "approx_tokens": 200}
            for versions in ((component, conflict), (conflict, component)):
                with self.subTest(field=field, value=value, versions=versions), tempfile.TemporaryDirectory() as temp:
                    path = Path(temp) / "attempts.jsonl"
                    path.write_text("\n".join(map(json.dumps, [attempt, unrelated, *versions])), encoding="utf-8")
                    records, exclusions = kd4_model_attempt_analysis.load_jsonl([path])
                    self.assertEqual(records, [attempt, unrelated])
                    self.assertEqual(exclusions, {"invalid_context_component_identity": 2})

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

    def test_provider_cached_share_pairs_cached_and_input_from_one_attempt(
        self,
    ) -> None:
        # Unreported cached input is unknown, not a cache miss; cached input
        # without its input total cannot enter the numerator.
        stable = kd4_model_attempt_analysis._stable_context_summary(
            [
                {"input_token_count": 1000, "cached_input_token_count": None},
                {"input_token_count": 1000, "cached_input_token_count": 900},
                {"input_token_count": None, "cached_input_token_count": 500},
                {"input_token_count": 100, "cached_input_token_count": 101},
            ]
        )
        self.assertEqual(stable["providerCachedShare"], 0.9)
        self.assertEqual(stable["providerCacheCoveredAttempts"], 1)
        self.assertEqual(stable["providerCacheUncoveredAttempts"], 3)

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
