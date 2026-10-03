from __future__ import annotations

import contextlib
import copy
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_session_diagnostics as diagnostics
from scripts import kd4_turn_latency_audit as audit
from scripts.test_kd4_turn_latency_audit import _event, _meta, _response, _timing


def _report(root: Path, durations: list[int], **timing_overrides) -> dict:
    source = root / "rollout.jsonl"
    lines = [_meta(str(root))]
    for index, duration in enumerate(durations):
        timing = _timing()
        timing.update(inclusiveDurationNs=duration * 1_000_000, **timing_overrides)
        lines.extend(
            [
                _event({"type": "task_started", "turn_id": str(index)}),
                _event(
                    {"type": "task_complete", "turn_id": str(index), "timing": timing}
                ),
            ]
        )
    source.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return audit.analyze_session_path(source, root)


def _elapsed_rows(comparison: dict) -> list[dict]:
    return [row for row in comparison["metrics"] if row["metric"] == "elapsedMs"]


class SessionDiagnosticsTest(unittest.TestCase):
    def test_nested_calls_terminal_cost_and_calibration_require_complete_measurements(self):
        timing = {
            "counters": {"toolCallCount": 2, "toolOutputTruncationCount": 1,
                         "truncationInducedContinuationCount": 0},
            "toolClosure": {"complete": True},
            "toolCalls": [{"callId": "parent", "source": "direct"},
                          {"callId": "child", "source": "code_mode", "parentCallId": "parent"}],
            "modelRequests": [{"generationIndex": 0, "generationPurpose": "terminal",
                               "modelStreamWaitNs": 5_000_000,
                               "requestTokenCategories": {
                                   "localInputEstimate": 120, "providerInputTokens": 100,
                               }}],
        }
        turn = {"requestRetention": {"complete": True}}
        result = diagnostics._turn_metrics({"timing": timing}, turn, {}, {})
        metrics = result["metrics"]
        self.assertEqual((metrics["rootToolCalls"], metrics["nestedToolCalls"]), (1, 1))
        self.assertEqual(metrics["terminalGenerations"], 1)
        self.assertEqual(metrics["terminalModelMs"], 5)
        self.assertEqual(metrics["inputEstimateAbsoluteErrorTokens"], 20)
        self.assertEqual(result["truncationAttribution"]["unattributedOmissionDisposition"], "unknown")
        timing["toolCallTimingOverflow"] = 1
        turn["requestRetention"]["complete"] = False
        metrics = diagnostics._turn_metrics({"timing": timing}, turn, {}, {})["metrics"]
        for key in ("rootToolCalls", "nestedToolCalls", "terminalGenerations",
                    "terminalModelMs", "inputEstimateAbsoluteErrorTokens"):
            self.assertIsNone(metrics[key], key)

    def test_nonprogress_fallback_requires_complete_explicit_observations(self):
        timing = _timing()
        timing.pop("observationalNonprogressLatency")
        timing["counters"]["modelRequestCount"] = 2
        turn = audit._turn_report(
            {
                "turn_id": "t",
                "timing": timing,
                "status": "task_complete",
                "lifecycle": "completed",
                "timestamp": None,
                "file": "rollout.jsonl",
                "line": 1,
                "cwd": "",
            },
            Path.cwd(),
            [],
        )
        record = {"turn_id": "t", "timing": timing}
        metrics = diagnostics._turn_metrics(record, turn, {}, {})["metrics"]
        self.assertEqual(metrics["nonprogressGenerations"], 1)
        turn["requestRetention"]["complete"] = False
        self.assertIsNone(
            diagnostics._turn_metrics(record, turn, {}, {})["metrics"][
                "nonprogressGenerations"
            ]
        )
        turn["requestRetention"]["complete"] = True
        timing["modelRequests"][0].pop("unchangedRelevantState")
        self.assertIsNone(
            diagnostics._turn_metrics(record, turn, {}, {})["metrics"][
                "nonprogressGenerations"
            ]
        )

    def test_incomplete_tool_receipts_and_coverage_do_not_prove_zero(self):
        receipt = diagnostics.tool_observation(
            "context_checkpoint", "{}", '{"changed":true}'
        )
        record = {"timing": {}, "diagnosticToolObservations": [receipt]}
        result = diagnostics._turn_metrics(record, {}, {}, {})["metrics"]
        self.assertEqual(result["checkpointAttempts"], 1)
        self.assertIsNone(result["usefulCheckpoints"])
        for coverage in (
            {"parseErrorCount": 1},
            {"unpairedToolCalls": 1},
            {"terminalTurnsWithUnresolvedToolCalls": 1},
        ):
            result = diagnostics._turn_metrics(record, {}, coverage, {})["metrics"]
            self.assertIsNone(result["checkpointAttempts"])
            self.assertIsNone(result["duplicateToolRequests"])
        record["diagnosticToolObservations"] = [
            diagnostics.tool_observation("unknown", "", "")
        ]
        result = diagnostics._turn_metrics(record, {}, {}, {})["metrics"]
        self.assertIsNone(result["checkpointAttempts"])
        self.assertIsNone(result["failedCommands"])

    def test_requested_runtime_metrics_and_token_opt_out(self):
        timing = _timing()
        timing["unions"]["toolActiveUnionNs"] = 350_000_000
        timing["counters"].update(
            modelRequestCount=2,
            toolOutputModelTokenCount=321,
            toolOutputRecoveryCallCount=4,
            toolOutputArtifactRereadCount=2,
            toolOutputRecoveryRetruncationCount=1,
            toolOutputProjectionTruncationCount=3,
            toolOutputTruncationCount=2,
        )
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            report = _report(
                root,
                [1000],
                **{
                    key: value
                    for key, value in timing.items()
                    if key != "inclusiveDurationNs"
                },
            )
            metrics = report["perTurn"][0]["diagnostics"]["metrics"]
            self.assertEqual(
                {
                    key: metrics[key]
                    for key in (
                        "generations",
                        "modelActiveMs",
                        "toolActiveMs",
                        "orchestrationMs",
                        "inputTokens",
                        "cachedInputTokens",
                        "uncachedInputTokens",
                        "toolOutputTokensProjected",
                        "artifactRecoveryCalls",
                        "artifactRereads",
                        "recoveryRetruncations",
                        "nonprogressGenerations",
                    )
                },
                {
                    "generations": 2,
                    "modelActiveMs": 600,
                    "toolActiveMs": 350,
                    "orchestrationMs": 100,
                    "inputTokens": 210,
                    "cachedInputTokens": 180,
                    "uncachedInputTokens": 30,
                    "toolOutputTokensProjected": 321,
                    "artifactRecoveryCalls": 4,
                    "artifactRereads": 2,
                    "recoveryRetruncations": 1,
                    "nonprogressGenerations": 1,
                },
            )
            self.assertEqual(
                report["perTurn"][0]["diagnostics"]["truncationCountBySource"],
                {"commandOutput": 2, "runtimeProjection": 3, "recoverySections": 1},
            )
            disabled = audit.analyze_session_path(
                root / "rollout.jsonl", root, include_tokens=False
            )
            for key in ("inputTokens", "cachedInputTokens", "uncachedInputTokens"):
                self.assertIsNone(disabled["perTurn"][0]["diagnostics"]["metrics"][key])
            self.assertEqual(
                disabled["sessionDiagnostics"]["metrics"]["toolOutputTokensProjected"][
                    "total"
                ],
                321,
            )
            summary = audit.bounded_summary(report)
            self.assertNotIn("diagnostics", summary["perTurn"][0])
            self.assertNotIn("sessionDiagnostics", summary)
            rendered = audit.render_report(report)
            self.assertIn("artifactRereads: n=1", rendered)
            self.assertIn("finalAnswerRecall: n=0 missing=1", rendered)

    def test_tool_observations_and_unknown_outcomes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "rollout.jsonl"
            calls = [
                ("exec_command", '{"cmd":"fail", "a":1}', '{"exit_code":2}'),
                ("exec_command", '{"a":1,"cmd":"fail"}', "Process exited with code 0"),
                ("context_checkpoint", "{}", '{"changed":false}'),
                (
                    "context_checkpoint",
                    '{"summary":"useful"}',
                    '{"changed":true,"checkpoint_item_persisted":true}',
                ),
            ]
            lines = [_meta(str(root)), _event({"type": "task_started", "turn_id": "t"})]
            for index, (name, arguments, output) in enumerate(calls):
                lines.append(
                    _response(
                        {
                            "type": "function_call",
                            "call_id": str(index),
                            "name": name,
                            "arguments": arguments,
                        },
                        "2026-08-17T00:00:00Z",
                    )
                )
                lines.append(
                    _response(
                        {
                            "type": "function_call_output",
                            "call_id": str(index),
                            "output": output,
                        },
                        "2026-08-17T00:00:01Z",
                    )
                )
            lines.append(
                _event({"type": "task_complete", "turn_id": "t", "timing": _timing()})
            )
            source.write_text("\n".join(lines), encoding="utf-8")
            report = audit.analyze_session_path(source, root)
            metrics = report["perTurn"][0]["diagnostics"]["metrics"]
            self.assertEqual(metrics["failedCommands"], 1)
            self.assertEqual(metrics["duplicateToolRequests"], 1)
            self.assertIsNone(metrics["redundantToolRequests"])
            self.assertEqual(metrics["checkpointAttempts"], 2)
            self.assertEqual(metrics["usefulCheckpoints"], 1)
            # Unknown results cannot be interpreted as successful commands/checkpoints.
            source.write_text(
                source.read_text()
                .replace('{\\"exit_code\\":2}', "unavailable")
                .replace('{\\"changed\\":false}', "unavailable"),
                encoding="utf-8",
            )
            metrics = audit.analyze_session_path(source, root)["perTurn"][0][
                "diagnostics"
            ]["metrics"]
            self.assertIsNone(metrics["failedCommands"])
            self.assertIsNone(metrics["usefulCheckpoints"])
            self.assertEqual(metrics["checkpointAttempts"], 2)

    def test_successful_code_mode_wrapper_is_not_zero_failed_commands(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "rollout.jsonl"
            source.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event({"type": "task_started", "turn_id": "t"}),
                        _response(
                            {
                                "type": "function_call",
                                "call_id": "wrapped",
                                "name": "functions.exec",
                                "arguments": "text(await exec(...))",
                            },
                            "2026-08-17T00:00:00Z",
                        ),
                        _response(
                            {
                                "type": "function_call_output",
                                "call_id": "wrapped",
                                "output": '{"exit_code":0}',
                            },
                            "2026-08-17T00:00:01Z",
                        ),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "t",
                                "timing": _timing(),
                            }
                        ),
                    ]
                ),
                encoding="utf-8",
            )
            measured = audit.analyze_session_path(source, root)["perTurn"][0][
                "diagnostics"
            ]
            self.assertIsNone(measured["metrics"]["failedCommands"])
            self.assertEqual(
                measured["unavailableReasons"]["failedCommands"],
                "nested_command_outcomes_not_observed",
            )

    def test_evidence_truth_set_cli_and_snapshot_validation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            initial = _report(root, [100])
            annotation = {
                "turnId": "0",
                "rolloutSha256": initial["coverage"]["snapshots"][0]["sha256"],
                "discoveredEvidenceIds": ["a", "b", "c"],
                "finalEvidenceIds": ["a", "b"],
                "truthIds": ["a", "b", "c", "d"],
                "answerClaimIds": ["a", "b", "wrong"],
                "redundantToolRequestIds": ["call-1"],
            }
            evidence = {"schemaVersion": 1, "turns": [annotation]}
            evidence_path = root / "evidence.json"
            evidence_path.write_text(json.dumps(evidence), encoding="utf-8-sig")
            before = {path: path.read_bytes() for path in root.iterdir()}
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(
                    audit.main(
                        [
                            str(root / "rollout.jsonl"),
                            "--diagnostic-evidence",
                            str(evidence_path),
                            "--json",
                        ]
                    ),
                    0,
                )
            report = json.loads(out.getvalue())
            metrics = report["perTurn"][0]["diagnostics"]["metrics"]
            self.assertEqual(metrics["discoveredButNotFinalizedEvidence"], 1)
            self.assertEqual(metrics["evidenceSurvival"], 2 / 3)
            self.assertEqual(metrics["finalAnswerRecall"], 0.5)
            self.assertEqual(metrics["finalAnswerPrecision"], 2 / 3)
            self.assertEqual(metrics["redundantToolRequests"], 1)
            self.assertNotIn(
                "total", report["sessionDiagnostics"]["metrics"]["finalAnswerRecall"]
            )
            self.assertEqual(
                {path: path.read_bytes() for path in root.iterdir()}, before
            )
            for updates in (
                {"rolloutSha256": "stale"},
                {"turnId": "unknown"},
                {"truthIds": ["a", "a"]},
                {"truthIds": "a"},
                {"truthIds": [False]},
                {"unrecognized": []},
            ):
                with self.subTest(updates=updates), self.assertRaises(ValueError):
                    audit.analyze_session_path(
                        root / "rollout.jsonl",
                        root,
                        diagnostic_evidence={
                            "schemaVersion": 1,
                            "turns": [annotation | updates],
                        },
                    )
            evidence["turns"] = [annotation, annotation]
            with self.assertRaisesRegex(ValueError, "duplicate"):
                audit.analyze_session_path(
                    root / "rollout.jsonl", root, diagnostic_evidence=evidence
                )
            evidence["turns"] = [
                {
                    "turnId": "0",
                    "rolloutSha256": annotation["rolloutSha256"],
                    "truthIds": [],
                    "answerClaimIds": [],
                    "discoveredEvidenceIds": [],
                    "finalEvidenceIds": [],
                }
            ]
            empty = audit.analyze_session_path(
                root / "rollout.jsonl", root, diagnostic_evidence=evidence
            )["perTurn"][0]["diagnostics"]
            self.assertEqual(empty["metrics"]["discoveredButNotFinalizedEvidence"], 0)
            for key in (
                "evidenceSurvival",
                "finalAnswerRecall",
                "finalAnswerPrecision",
            ):
                self.assertIsNone(empty["metrics"][key])
                self.assertIn("empty", empty["unavailableReasons"][key])

    def test_runtime_counter_saturation_and_legacy_missingness(self):
        for value in (None, True, -1, 1.5, 2**32 - 1):
            with tempfile.TemporaryDirectory() as temp:
                report = _report(
                    Path(temp), [100], counters={"toolOutputArtifactRereadCount": value}
                )
            metric = report["sessionDiagnostics"]["metrics"]["artifactRereads"]
            self.assertEqual(metric, {"unit": "count", "samples": 0, "missing": 1})
        with tempfile.TemporaryDirectory() as temp:
            report = _report(Path(temp), [100], observationalNonprogressLatency={})
        metrics = report["perTurn"][0]["diagnostics"]["metrics"]
        self.assertIsNone(metrics["nonprogressGenerations"])
        self.assertIsNone(metrics["toolActiveMs"])
        self.assertIsNone(metrics["toolOutputTokensProjected"])

    def test_distributions_missing_fields_and_ranked_costs(self):
        with tempfile.TemporaryDirectory() as temp:
            report = _report(Path(temp), [100, 200, 300, 400, 1000])
        cohort = report["sessionDiagnostics"]["cohorts"][0]
        self.assertEqual(cohort["population"], "repository_root")
        self.assertEqual(cohort["turns"], 5)
        elapsed = cohort["metrics"]["elapsedMs"]
        self.assertEqual(
            elapsed,
            {
                "unit": "ms",
                "samples": 5,
                "missing": 0,
                "total": 2000,
                "mean": 400,
                "p50": 300,
                "p95": 1000,
                "max": 1000,
            },
        )
        self.assertEqual(
            cohort["metrics"]["retryOnlyMs"],
            {
                "unit": "ms",
                "samples": 0,
                "missing": 5,
            },
        )
        self.assertEqual(
            cohort["rankedTimeCosts"],
            [
                {"metric": "modelOnlyMs", "totalMs": 3000},
                {"metric": "toolOnlyMs", "totalMs": 1000},
                {"metric": "orchestrationMs", "totalMs": 500},
            ],
        )
        self.assertEqual(cohort["metrics"]["humanOnlyWaitMs"]["total"], 500)
        self.assertIn("p50=300.0 p95=1000.0", audit.render_report(report))

    def test_unmeasured_invalid_and_saturated_values_are_not_zero(self):
        for value in (
            None,
            True,
            -1,
            1.5,
            float("nan"),
            float("inf"),
            2**64 - 1,
            2**2048,
        ):
            with self.subTest(value=value):
                record = {
                    "turn_id": "t",
                    "status": "task_complete",
                    "lifecycle": "completed",
                    "timing": {"machineDurationNs": value},
                }
                result = diagnostics.build_diagnostics(
                    [record], {"t": "other"}, {"startedTurnsWithoutTerminal": 0}
                )
                self.assertEqual(
                    result["cohorts"][0]["metrics"]["agentActiveMs"],
                    {
                        "unit": "ms",
                        "samples": 0,
                        "missing": 1,
                    },
                )

    def test_comparison_direction_threshold_zero_and_sample_guard(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            baseline = _report(root, [100] * 5)
            for duration, status in (
                (100, "within_threshold"),
                (140, "within_threshold"),
                (150, "increased"),
                (50, "decreased"),
            ):
                with self.subTest(duration=duration):
                    current = _report(root, [duration] * 5)
                    rows = _elapsed_rows(
                        diagnostics.compare_diagnostics(
                            current["sessionDiagnostics"], baseline
                        )
                    )
                    self.assertEqual([row["status"] for row in rows], [status, status])
                    self.assertEqual(rows[0]["delta"], duration - 100)
                    self.assertEqual(rows[0]["terminalStatus"], "task_complete")
            zero = _report(root, [0] * 5)
            rows = _elapsed_rows(
                diagnostics.compare_diagnostics(baseline["sessionDiagnostics"], zero)
            )
            self.assertEqual(rows[0]["status"], "increased")
            self.assertNotIn("relativeChange", rows[0])
            single = _report(root, [1000])
            rows = _elapsed_rows(
                diagnostics.compare_diagnostics(single["sessionDiagnostics"], baseline)
            )
            self.assertEqual(rows[0]["reason"], "insufficient_samples")
            self.assertNotIn("delta", rows[0])
            large_baseline = _report(root, [1000] * 5)
            current = _report(root, [1150] * 5)
            rows = _elapsed_rows(
                diagnostics.compare_diagnostics(
                    current["sessionDiagnostics"], large_baseline
                )
            )
            self.assertEqual(rows[0]["status"], "within_threshold")
            rows = _elapsed_rows(
                diagnostics.compare_diagnostics(
                    current["sessionDiagnostics"],
                    large_baseline,
                    relative_threshold=0.10,
                )
            )
            self.assertEqual(rows[0]["status"], "increased")

    def test_cohorts_keep_aborts_populations_and_schemas_separate(self):
        records = []
        populations = {}
        for index, (population, status, lifecycle, schema) in enumerate(
            [
                ("repository_root", "task_complete", "completed", 25),
                ("eval", "task_complete", "completed", 25),
                ("repository_root", "turn_aborted", "canceled", 25),
                ("repository_root", "task_complete", "completed", 24),
            ]
        ):
            records.append(
                {
                    "turn_id": str(index),
                    "status": status,
                    "lifecycle": lifecycle,
                    "timing": {"schemaVersion": schema, "inclusiveDurationNs": 100},
                }
            )
            populations[str(index)] = population
        result = diagnostics.build_diagnostics(
            records, populations, {"startedTurnsWithoutTerminal": 2}
        )
        self.assertEqual(len(result["cohorts"]), 4)
        self.assertTrue(all(row["turns"] == 1 for row in result["cohorts"]))
        self.assertEqual(result["activeTurnsExcluded"], 2)
        baseline = copy.deepcopy(result)
        baseline["cohorts"] = baseline["cohorts"][:1]
        comparison = diagnostics.compare_diagnostics(
            result, {"sessionDiagnostics": baseline}, min_samples=1
        )
        self.assertEqual(
            sum(
                row.get("reason") == "no_matching_baseline_metric"
                for row in comparison["metrics"]
            ),
            3 * len(result["cohorts"][0]["metrics"]),
        )

    def test_cohorts_flag_sessions_from_different_builds(self):
        # Two dirty builds of one commit differ only in the executable hash.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name, executable in (("a", "1" * 64), ("b", "2" * 64)):
                build = {
                    "version": "0.0.0",
                    "commit": "2aa8319174f7",
                    "dirty": "true",
                    "profile": "release",
                    "built": "2026-09-30T00:00:00Z",
                    "executable_sha256": executable,
                }
                meta = {"cwd": str(root), "harness_build": build}
                lines = [
                    json.dumps({"type": "session_meta", "payload": meta}),
                    _event({"type": "task_started", "turn_id": name}),
                    _event(
                        {"type": "task_complete", "turn_id": name, "timing": _timing()}
                    ),
                ]
                (root / f"rollout-{name}.jsonl").write_text(
                    "\n".join(lines) + "\n", encoding="utf-8"
                )
            report = audit.analyze_session_path(root, root)
        [cohort] = report["sessionDiagnostics"]["cohorts"]
        self.assertEqual(
            cohort["builds"], {f"sha256:{'1' * 64}": 1, f"sha256:{'2' * 64}": 1}
        )
        self.assertIn(
            "mixed builds", "\n".join(diagnostics.render_diagnostics(report))
        )

    def test_coverage_blocks_comparisons_and_partial_metrics(self):
        with tempfile.TemporaryDirectory() as temp:
            baseline = _report(Path(temp), [100] * 5)
        for key in diagnostics._COVERAGE_BLOCKERS:
            with self.subTest(key=key):
                current = copy.deepcopy(baseline["sessionDiagnostics"])
                current["coverageBlockers"][key] = 1
                comparison = diagnostics.compare_diagnostics(current, baseline)
                self.assertTrue(
                    all(
                        row["reason"] == "incomplete_session_coverage"
                        for row in comparison["metrics"]
                    )
                )
        current = copy.deepcopy(baseline["sessionDiagnostics"])
        current["cohorts"][0]["metrics"]["elapsedMs"]["missing"] = 1
        row = _elapsed_rows(diagnostics.compare_diagnostics(current, baseline))[0]
        self.assertEqual(row["reason"], "incomplete_metric_coverage")

    def test_real_rollout_excludes_invalid_duplicate_conflicting_and_open_turns(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            _report(root, [100, 200])
            source = root / "rollout.jsonl"
            with source.open("a", encoding="utf-8") as handle:
                handle.write(
                    _event(
                        {
                            "type": "task_complete",
                            "turn_id": "0",
                            "timing": {**_timing(), "inclusiveDurationNs": 100_000_000},
                        }
                    )
                    + "\n"
                )
                handle.write(
                    _event(
                        {"type": "task_complete", "turn_id": "1", "timing": _timing()}
                    )
                    + "\n"
                )
                handle.write(
                    _event(
                        {
                            "type": "task_complete",
                            "turn_id": "invalid",
                            "timing": _timing(valid=False),
                        }
                    )
                    + "\n"
                )
                handle.write(_event({"type": "task_started", "turn_id": "open"}) + "\n")
            result = audit.analyze_session_path(source, root)["sessionDiagnostics"]
        self.assertEqual(result["cohorts"][0]["turns"], 1)
        self.assertEqual(result["cohorts"][0]["metrics"]["elapsedMs"]["total"], 100)
        self.assertEqual(result["coverageBlockers"]["conflictingTerminalProfiles"], 1)
        self.assertEqual(result["activeTurnsExcluded"], 1)

    def test_summary_retains_comparisons_without_mutating_full_report(self):
        with tempfile.TemporaryDirectory() as temp:
            report = _report(Path(temp), [100] * 5)
        report["baselineComparison"] = diagnostics.compare_diagnostics(
            report["sessionDiagnostics"], report
        )
        saved = copy.deepcopy(report)
        summary = audit.bounded_summary(report)
        self.assertNotIn("sessionDiagnostics", summary)
        self.assertEqual(
            summary["baselineComparison"],
            report["baselineComparison"],
        )
        with self.assertRaisesRegex(ValueError, "regenerate"):
            diagnostics.compare_diagnostics(report["sessionDiagnostics"], summary)
        with mock.patch.object(audit, "_MAX_SUMMARY_BYTES", 1):
            tiny = audit.bounded_summary(report)
        self.assertEqual(tiny["baselineComparison"]["metrics"], [])
        self.assertEqual(
            tiny["baselineComparison"]["omittedMetrics"],
            len(report["baselineComparison"]["metrics"]),
        )
        self.assertTrue(tiny["summaryBudget"]["limitExceeded"])
        self.assertEqual(report, saved)

    def test_cli_compares_saved_full_report_and_preserves_input_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            baseline = _report(root, [100] * 5)
            baseline_path = root / "baseline.json"
            baseline_path.write_text(json.dumps(baseline), encoding="utf-8-sig")
            _report(root, [200] * 5)
            original = {path: path.read_bytes() for path in root.iterdir()}
            command = [
                sys.executable,
                str(Path(audit.__file__)),
                str(root / "rollout.jsonl"),
                "--repo-root",
                str(root),
                "--baseline",
                str(baseline_path),
                "--summary-json",
            ]
            completed = subprocess.run(
                command, capture_output=True, text=True, check=False
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            self.assertEqual(
                _elapsed_rows(report["baselineComparison"])[0]["status"], "increased"
            )
            self.assertEqual(
                {path: path.read_bytes() for path in root.iterdir()}, original
            )
            for flags, expected in (
                (["--comparison-min-samples", "6"], "insufficient_samples"),
                (["--comparison-threshold", "2"], "within_threshold"),
            ):
                out = io.StringIO()
                with self.subTest(flags=flags), contextlib.redirect_stdout(out):
                    self.assertEqual(audit.main(command[2:] + flags), 0)
                row = _elapsed_rows(json.loads(out.getvalue())["baselineComparison"])[0]
                self.assertEqual(row.get("reason", row["status"]), expected)
            # An explicit gate turns the observational comparison into an exit
            # status, and unavailable evidence is distinct from passing.
            for flags, status, code in (
                ([], "regression", 1),
                (["--comparison-threshold", "2"], "passed", 0),
                (["--comparison-min-samples", "6"], "insufficient_evidence", 3),
            ):
                out = io.StringIO()
                with self.subTest(gate=flags), contextlib.redirect_stdout(out):
                    self.assertEqual(
                        audit.main(command[2:] + flags + ["--gate-metric", "elapsedMs"]),
                        code,
                    )
                gate = json.loads(out.getvalue())["baselineComparison"]["gate"]
                self.assertEqual(gate["status"], status)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(
                    audit.main(command[2:] + ["--gate-metric", "finalAnswerRecall"]), 3
                )
            for argv in (
                command[2:] + ["--gate-metric", "bogus"],
                [str(root / "rollout.jsonl"), "--gate-metric", "elapsedMs"],
            ):
                out, err = io.StringIO(), io.StringIO()
                with (
                    self.subTest(argv=argv[-2:]),
                    contextlib.redirect_stdout(out),
                    contextlib.redirect_stderr(err),
                    self.assertRaises(SystemExit) as raised,
                ):
                    audit.main(argv)
                self.assertEqual(raised.exception.code, 2)
                self.assertEqual(out.getvalue(), "")

    def test_regression_gate_uses_metric_direction_and_never_passes_missing_proof(self):
        def comparison(metric, *statuses):
            return {
                "metrics": [{"metric": metric, "status": status} for status in statuses],
                "minSamples": 5,
                "relativeThreshold": 0.1,
                "absoluteThresholds": {},
                "unmatchedBaselineCohorts": 0,
            }

        for metric, statuses, expected in (
            ("elapsedMs", ("increased", "within_threshold"), "regression"),
            ("elapsedMs", ("decreased", "within_threshold"), "passed"),
            ("finalAnswerRecall", ("decreased",), "regression"),
            ("finalAnswerRecall", ("increased",), "passed"),
            ("elapsedMs", ("within_threshold", "unavailable"), "insufficient_evidence"),
            ("elapsedMs", (), "insufficient_evidence"),
            ("elapsedMs", ("increased", "unavailable"), "regression"),
        ):
            with self.subTest(metric=metric, statuses=statuses):
                gate = diagnostics.regression_gate(
                    comparison(metric, *statuses), [metric]
                )
                self.assertEqual(gate["status"], expected)
        for metrics in ([], ["bogus"]):
            with self.subTest(metrics=metrics), self.assertRaises(ValueError):
                diagnostics.regression_gate(comparison("elapsedMs"), metrics)

    def test_empty_and_malformed_evidence_never_reports_an_improvement(self):
        with tempfile.TemporaryDirectory() as temp:
            report = _report(Path(temp), [100] * 5)
        current = report["sessionDiagnostics"]
        empty = diagnostics.build_diagnostics(
            [], {}, {"startedTurnsWithoutTerminal": 1}
        )
        comparison = diagnostics.compare_diagnostics(empty, report)
        self.assertEqual(comparison["comparedStatistics"], 0)
        self.assertEqual(comparison["unmatchedBaselineCohorts"], 1)
        self.assertEqual(comparison["metrics"], [])
        text = "\n".join(
            diagnostics.render_diagnostics(
                {
                    "sessionDiagnostics": empty,
                    "baselineComparison": comparison,
                }
            )
        )
        self.assertIn("0 compared statistics", text)
        for value in (None, [], float("nan"), -1):
            baseline = copy.deepcopy(report)
            baseline["sessionDiagnostics"]["cohorts"][0]["metrics"]["elapsedMs"][
                "p95"
            ] = value
            with self.subTest(value=value):
                row = _elapsed_rows(diagnostics.compare_diagnostics(current, baseline))[
                    0
                ]
                self.assertEqual(row["reason"], "invalid_baseline_distribution")
                self.assertNotIn("delta", row)
        for field, value in (("metrics", []), ("turns", True)):
            baseline = copy.deepcopy(report)
            baseline["sessionDiagnostics"]["cohorts"][0][field] = value
            with (
                self.subTest(field=field),
                self.assertRaisesRegex(ValueError, "malformed"),
            ):
                diagnostics.compare_diagnostics(current, baseline)
        baseline = copy.deepcopy(report)
        baseline["sessionDiagnostics"]["cohorts"] *= 2
        with self.assertRaisesRegex(ValueError, "duplicate"):
            diagnostics.compare_diagnostics(current, baseline)

    def test_bad_baseline_and_thresholds_rejected_without_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            report = _report(root, [100])
            for baseline in (
                [],
                {},
                {"sessionDiagnostics": {"schemaVersion": 0}},
                {"sessionDiagnostics": {"schemaVersion": 1}},
            ):
                with self.subTest(baseline=baseline), self.assertRaises(ValueError):
                    diagnostics.compare_diagnostics(
                        report["sessionDiagnostics"], baseline
                    )
            for options in (
                {"min_samples": 0},
                {"relative_threshold": float("nan")},
                {"relative_threshold": -1},
            ):
                with self.subTest(options=options), self.assertRaises(ValueError):
                    diagnostics.compare_diagnostics(
                        report["sessionDiagnostics"], report, **options
                    )
            baseline_path = root / "old.json"
            baseline_path.write_text("{}", encoding="utf-8")
            out, err = io.StringIO(), io.StringIO()
            with (
                contextlib.redirect_stdout(out),
                contextlib.redirect_stderr(err),
                self.assertRaises(SystemExit) as raised,
            ):
                audit.main(
                    [
                        str(root / "rollout.jsonl"),
                        "--baseline",
                        str(baseline_path),
                        "--json",
                    ]
                )
            self.assertEqual(raised.exception.code, 2)
            self.assertEqual(out.getvalue(), "")
            self.assertIn("regenerate it from rollouts", err.getvalue())


if __name__ == "__main__":
    unittest.main()
