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


class SubscriptionUsageTest(unittest.TestCase):
    @staticmethod
    def _limits(used=20, **overrides):
        return {
            "limit_id": "codex",
            "plan_type": "plus",
            "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 2_000_000_000},
            "secondary": {"used_percent": 40, "window_minutes": 10080, "resets_at": 2_000_100_000},
            **overrides,
        }

    def test_rollout_text_json_summary_cache_and_token_opt_out(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "rollout.jsonl"
            lines = [_meta(str(root)), _event({"type": "task_started", "turn_id": "open"})]
            for used in (20, 20, 25):
                lines.append(_event({"type": "token_count", "info": None,
                                     "rate_limits": self._limits(used)}))
            # A response/tool payload is not a provider limit observation.
            lines.append(_response({"type": "token_count", "rate_limits": self._limits(90)},
                                   "2026-08-17T00:00:00Z"))
            source.write_text("\n".join(lines) + "\n", encoding="utf-8")
            original = source.read_bytes()
            report = audit.analyze_session_path(source, root)
            subscription = report["sessionDiagnostics"]["subscriptionUsage"]
            self.assertTrue(subscription["available"])
            self.assertEqual(subscription["tokenCountEvents"], 3)
            self.assertEqual(subscription["windowCount"], 2)
            self.assertEqual(report["sessionDiagnostics"]["activeTurnsExcluded"], 1)
            primary, secondary = subscription["windows"]
            self.assertEqual(primary["firstUsedPercent"], 20)
            self.assertEqual(primary["lastUsedPercent"], 25)
            self.assertEqual(primary["peakUsedPercent"], 25)
            self.assertEqual(primary["lastRemainingPercent"], 75)
            self.assertEqual(primary["observedChangePercentagePoints"], 5)
            self.assertEqual(secondary["observedChangePercentagePoints"], 0)
            self.assertEqual(primary["samples"], 3)
            text = audit.render_report(report)
            self.assertIn("subscription usage:", text)
            self.assertIn("observed change=+5pp", text)
            self.assertIn("not usage attributable to this session", text)
            saved = copy.deepcopy(report)
            summary = audit.bounded_summary(report)
            self.assertEqual(summary["subscriptionUsage"]["windows"], subscription["windows"])
            self.assertIn("not usage attributable", summary["subscriptionUsage"]["measurementNote"])
            with mock.patch.object(audit, "_MAX_SUMMARY_BYTES", 1):
                tiny = audit.bounded_summary(report)
            self.assertEqual(tiny["subscriptionUsage"]["windows"], [])
            self.assertEqual(tiny["subscriptionUsage"]["omittedWindows"], 2)
            self.assertEqual(report, saved)
            for flags in (["--json"], ["--summary-json", "--tokens", "off"]):
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    self.assertEqual(audit.main([str(source), "--repo-root", str(root), *flags]), 0)
                emitted = json.loads(output.getvalue())
                actual = emitted.get("subscriptionUsage", emitted.get("sessionDiagnostics", {}).get("subscriptionUsage"))
                self.assertEqual(actual["windows"], subscription["windows"])
            for _ in range(2):
                cached = audit.analyze_session_path(source, root, cache_dir=root / "cache")
                self.assertEqual(cached["sessionDiagnostics"]["subscriptionUsage"], subscription)
            self.assertEqual(source.read_bytes(), original)

    def test_missing_invalid_and_zero_are_distinct(self):
        usage = diagnostics.SubscriptionUsage()
        self.assertFalse(usage.report()["available"])
        for value in (None, [], {"limit_id": []}, {}, {"primary": []}):
            usage.observe("file", 1_000_000_000, value)
        for value in (True, -1, 101, "20", float("nan"), float("inf")):
            usage.observe("file", 1_000_000_000, self._limits(
                primary={"used_percent": value}, secondary=None))
        result = usage.report()
        self.assertFalse(result["available"])
        self.assertEqual(result["missingRateLimitSnapshots"], 1)
        self.assertEqual(result["invalidRateLimitSnapshots"], 2)
        self.assertEqual(result["invalidWindows"], 7)
        for _ in range(2):
            usage.observe("file", 1_000_000_000, self._limits(0, secondary=None))
        [window] = usage.report()["windows"]
        self.assertEqual(window["observedChangePercentagePoints"], 0)
        self.assertEqual(window["lastRemainingPercent"], 100)

    def test_unreliable_changes_are_unavailable_not_zero(self):
        for override, times, values, reason in (
            ({}, [1], [10], "insufficient_snapshots"),
            ({"resets_at": None}, [1, 2], [10, 20], "window_identity_unavailable"),
            ({"window_minutes": None}, [1, 2], [10, 20], "window_identity_unavailable"),
            ({}, [2, 1], [10, 20], "invalid_out_of_order_or_expired_timestamp"),
            ({}, [None, 2], [10, 20], "invalid_out_of_order_or_expired_timestamp"),
            ({"resets_at": 2}, [1, 2], [10, 20], "invalid_out_of_order_or_expired_timestamp"),
            ({}, [1, 2, 3], [20, 10, 30], "usage_decreased_within_window"),
        ):
            with self.subTest(reason=reason, override=override, times=times):
                usage = diagnostics.SubscriptionUsage()
                for timestamp, value in zip(times, values):
                    snapshot = self._limits(value, secondary=None)
                    snapshot["primary"].update(override)
                    usage.observe("file", timestamp * 1_000_000_000 if timestamp is not None else None, snapshot)
                [window] = usage.report()["windows"]
                self.assertIsNone(window["observedChangePercentagePoints"])
                self.assertEqual(window["changeUnavailableReason"], reason)
        for key in ("window_minutes", "resets_at"):
            for value in (True, 0, -1, 1.5, "300", 2**63):
                usage = diagnostics.SubscriptionUsage()
                snapshot = self._limits(secondary=None)
                snapshot["primary"][key] = value
                usage.observe("file", 1_000_000_000, snapshot)
                self.assertEqual(usage.report()["invalidWindows"], 1)
                self.assertFalse(usage.report()["available"])

    def test_reset_plan_limit_duration_and_file_boundaries_are_not_merged(self):
        usage = diagnostics.SubscriptionUsage()
        snapshots = [self._limits(secondary=None) for _ in range(6)]
        snapshots[1]["primary"]["resets_at"] += 300
        snapshots[2]["plan_type"] = "pro"
        snapshots[3]["limit_id"] = "other"
        snapshots[4]["primary"]["window_minutes"] = 60
        for index, snapshot in enumerate(snapshots):
            usage.observe("other-file" if index == 5 else "file", 1_000_000_000, snapshot)
        result = usage.report()
        self.assertEqual(result["windowCount"], 6)
        self.assertTrue(all(row["observedChangePercentagePoints"] is None for row in result["windows"]))

    def test_parse_errors_and_legacy_rollouts_preserve_unavailable_coverage(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            report = _report(root, [100])
            subscription = report["sessionDiagnostics"]["subscriptionUsage"]
            self.assertFalse(subscription["available"])
            self.assertIn("subscription limits unavailable", audit.render_report(report))
            self.assertNotIn("subscriptionUsage", audit.bounded_summary(report))
            source = root / "rollout.jsonl"
            with source.open("a", encoding="utf-8") as handle:
                handle.write(_event({"type": "token_count", "rate_limits": None}) + "\n")
            missing = audit.bounded_summary(audit.analyze_session_path(source, root))["subscriptionUsage"]
            self.assertFalse(missing["available"])
            self.assertEqual(missing["missingRateLimitSnapshots"], 1)
            with source.open("a", encoding="utf-8") as handle:
                for used in (10, 20):
                    handle.write(_event({"type": "token_count", "rate_limits": self._limits(used)}) + "\n")
                handle.write("{broken\n")
            subscription = audit.analyze_session_path(source, root)["sessionDiagnostics"]["subscriptionUsage"]
            self.assertEqual(subscription["parseErrorCount"], 1)
            self.assertTrue(all(row["changeUnavailableReason"] == "incomplete_session_coverage"
                                for row in subscription["windows"]))


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
            report = _report(Path(temp), [400, 1000, 100, 300, 200])
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
                self.assertEqual(
                    {row["metric"] for row in comparison["metrics"]},
                    set(current["cohorts"][0]["metrics"]),
                )
                self.assertEqual(comparison["comparedStatistics"], 0)
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
        comparison = summary["baselineComparison"]
        full_comparison = report["baselineComparison"]
        self.assertEqual(
            comparison["metrics"],
            full_comparison["metrics"][:len(comparison["metrics"])],
        )
        self.assertEqual(
            len(comparison["metrics"]) + comparison.get("omittedMetrics", 0),
            len(full_comparison["metrics"]),
        )
        self.assertEqual(
            {key: value for key, value in comparison.items()
             if key not in ("metrics", "omittedMetrics")},
            {key: value for key, value in full_comparison.items() if key != "metrics"},
        )
        with mock.patch.object(audit, "_MAX_SUMMARY_BYTES", 1_000_000):
            untrimmed = audit.bounded_summary(report)
        self.assertEqual(untrimmed["baselineComparison"], full_comparison)
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
                "--json",
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
            # status, and unavailable evidence is distinct from passing. Gates
            # survive bounded summaries even when individual metrics are omitted.
            summary_args = command[2:-1] + ["--summary-json"]
            for flags, status, code in (
                ([], "regression", 1),
                (["--comparison-threshold", "2"], "passed", 0),
                (["--comparison-min-samples", "6"], "insufficient_evidence", 3),
            ):
                out = io.StringIO()
                with self.subTest(gate=flags), contextlib.redirect_stdout(out):
                    self.assertEqual(
                        audit.main(summary_args + flags + ["--gate-metric", "elapsedMs"]),
                        code,
                    )
                gate = json.loads(out.getvalue())["baselineComparison"]["gate"]
                self.assertEqual(gate["status"], status)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(
                    audit.main(summary_args + ["--gate-metric", "finalAnswerRecall"]), 3
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

            self.assertEqual(
                {path: path.read_bytes() for path in root.iterdir()}, original
            )

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


class UsageEfficiencyTest(unittest.TestCase):
    @staticmethod
    def _measured_report(root, turns=1, timing=None):
        timing = copy.deepcopy(timing or _timing())
        timing["counters"]["modelRequestCount"] = len(timing["modelRequests"])
        timing["counters"]["provenAvoidedModelRequests"] = 3
        return _report(root, [1000] * turns, **{
            key: value for key, value in timing.items() if key != "inclusiveDurationNs"
        })

    def test_provider_partition_purpose_costs_and_estimates(self):
        with tempfile.TemporaryDirectory() as temp:
            report = self._measured_report(Path(temp))
        efficiency = report["sessionDiagnostics"]["usageEfficiency"]
        self.assertEqual(efficiency["completeProviderTurns"], 1)
        self.assertEqual(efficiency["observedCompleteTurnTotalTokens"], 235)
        self.assertEqual({r["component"]: r["tokens"] for r in efficiency["rankedTokenComponents"]}, {
            "uncachedInputTokens": 30, "cachedInputTokens": 180,
            "visibleOutputTokens": 18, "reasoningTokens": 7,
        })
        self.assertAlmostEqual(sum(r["share"] for r in efficiency["rankedTokenComponents"]), 1)
        self.assertEqual(efficiency["purposePartitionTurns"], 1)
        self.assertEqual({r["purpose"]: r["totalTokens"] for r in efficiency["costByPurpose"]}, {
            "implementation": 120, "deterministic_tool_continuation": 115,
        })
        row = report["perTurn"][0]["diagnostics"]["usageEfficiency"]
        self.assertEqual(row["physicalRequests"], 2)
        self.assertEqual(row["inputTokensPerRequest"], 105)
        self.assertAlmostEqual(row["cacheHitRatio"], 180 / 210)
        self.assertAlmostEqual(row["reasoningOutputRatio"], 7 / 25)
        self.assertEqual(row["promptEstimates"]["repeatedUnchangedContextTokens"], 150)
        self.assertEqual(row["promptEstimates"]["logicalPromptTokens"], 200)
        self.assertEqual(efficiency["avoidedModelRequests"]["total"], 3)
        self.assertIsNone(row["savedTokens"])
        self.assertIsNone(row["savedSubscriptionPercentagePoints"])
        self.assertTrue(all(not r["provenSavings"] for r in efficiency["experiments"]))
        text = audit.render_report(report)
        self.assertIn("usage efficiency:", text)
        self.assertIn("improve_cache_reuse", text)
        self.assertIn("saved tokens/quota unknown", text)

    def test_opt_out_partial_unknown_retention_and_missing_usage(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self._measured_report(root)
            source = root / "rollout.jsonl"
            disabled = audit.analyze_session_path(source, root, include_tokens=False)
            unknown = _report(root, [1000])
            missing_timing = _timing()
            missing_timing["modelRequests"][0].pop("tokenUsage")
            missing = self._measured_report(root, timing=missing_timing)
        for report in (disabled, unknown, missing):
            efficiency = report["sessionDiagnostics"]["usageEfficiency"]
            self.assertEqual(efficiency["unmeasuredProviderTurns"], 1)
            self.assertIsNone(efficiency["observedCompleteTurnTotalTokens"])
            self.assertEqual(efficiency["rankedTokenComponents"], [])
            self.assertEqual(efficiency["costByPurpose"], [])
            self.assertEqual(efficiency["rankedTurns"], [])
        for name in ("totalTokens", "outputTokens", "reasoningTokens", "physicalRequests"):
            self.assertIsNone(disabled["perTurn"][0]["diagnostics"]["metrics"][name])

    def test_token_opt_out_disables_estimate_calibration_through_real_audit(self):
        timing = _timing()
        for request, (estimate, actual) in zip(timing["modelRequests"], ((12, 5), (4, 9))):
            request["requestTokenCategories"].update(
                localInputEstimate=estimate, providerInputTokens=actual,
            )
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            enabled = self._measured_report(root, timing=timing)
            disabled = audit.analyze_session_path(root / "rollout.jsonl", root, include_tokens=False)
        # Absolute errors add: |12 - 5| + |4 - 9| = 12; they do not cancel.
        self.assertEqual(enabled["perTurn"][0]["diagnostics"]["metrics"]["inputEstimateAbsoluteErrorTokens"], 12)
        diagnostic = disabled["perTurn"][0]["diagnostics"]
        self.assertIsNone(diagnostic["metrics"]["inputEstimateAbsoluteErrorTokens"])
        self.assertEqual(diagnostic["unavailableReasons"]["inputEstimateAbsoluteErrorTokens"], "token_computation_disabled")
        self.assertEqual(disabled["sessionDiagnostics"]["metrics"]["inputEstimateAbsoluteErrorTokens"], {
            "unit": "tokens", "samples": 0, "missing": 1,
        })
        self.assertEqual(diagnostic["metrics"]["modelActiveMs"], 600)

    def test_purpose_partition_rejects_cross_generation_duplicate_usage(self):
        timing = _timing()
        timing["modelRequests"][1]["tokenUsage"] = copy.deepcopy(timing["modelRequests"][0]["tokenUsage"])
        for request in timing["modelRequests"]:
            request["samplingRequestId"] = "same-request"
        with tempfile.TemporaryDirectory() as temp:
            report = self._measured_report(Path(temp), timing=timing)
        efficiency = report["sessionDiagnostics"]["usageEfficiency"]
        self.assertEqual(efficiency["observedCompleteTurnTotalTokens"], 115)
        self.assertEqual(efficiency["purposePartitionTurns"], 0)
        self.assertEqual(efficiency["costByPurpose"], [])

    def test_zero_token_duplicates_do_not_inflate_purpose_request_counts(self):
        timing = _timing()
        for request in timing["modelRequests"]:
            request["samplingRequestId"] = "same-request"
            request["tokenUsage"] = dict.fromkeys(request["tokenUsage"], 0)
        with tempfile.TemporaryDirectory() as temp:
            report = self._measured_report(Path(temp), timing=timing)
        efficiency = report["sessionDiagnostics"]["usageEfficiency"]
        self.assertEqual(efficiency["observedCompleteTurnTotalTokens"], 0)
        self.assertEqual(efficiency["purposePartitionTurns"], 0)
        self.assertEqual(efficiency["costByPurpose"], [])

    def test_mixed_purpose_generation_and_zero_usage(self):
        timing = _timing()
        timing["modelRequests"][1]["generationIndex"] = 0
        for request in timing["modelRequests"]:
            request["tokenUsage"] = dict.fromkeys(request["tokenUsage"], 0)
        with tempfile.TemporaryDirectory() as temp:
            report = self._measured_report(Path(temp), timing=timing)
        efficiency = report["sessionDiagnostics"]["usageEfficiency"]
        self.assertEqual(efficiency["observedCompleteTurnTotalTokens"], 0)
        self.assertEqual(efficiency["costByPurpose"][0]["purpose"], "mixed")
        self.assertIsNone(efficiency["rankedTurns"][0]["cacheHitRatio"])
        self.assertIsNone(efficiency["rankedTurns"][0]["reasoningOutputRatio"])
        self.assertTrue(all(row["share"] is None for row in efficiency["rankedTokenComponents"]))

    def test_baseline_detects_usage_decreases_without_claiming_savings(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            baseline = self._measured_report(root, turns=5)
            timing = _timing()
            for request in timing["modelRequests"]:
                usage = request["tokenUsage"]
                usage["reasoningTokens"] = 0
                usage["visibleOutputTokens"] = 0
                usage["totalTokens"] = usage["inputTokens"]
            current = self._measured_report(root, turns=5, timing=timing)
            comparison = diagnostics.compare_diagnostics(current["sessionDiagnostics"], baseline)
            assessment = comparison["usageSavingsAssessment"]
            self.assertFalse(assessment["subscriptionSavingsProven"])
            self.assertEqual(assessment["decreasedStatisticCounts"]["outputTokens"], 2)
            self.assertEqual(assessment["decreasedStatisticCounts"]["reasoningTokens"], 2)
            reverse = diagnostics.compare_diagnostics(baseline["sessionDiagnostics"], current)
            self.assertEqual(diagnostics.regression_gate(reverse, ["outputTokens"])["status"], "regression")
            old = copy.deepcopy(baseline)
            for cohort in old["sessionDiagnostics"]["cohorts"]:
                cohort["metrics"].pop("outputTokens")
            legacy = diagnostics.compare_diagnostics(current["sessionDiagnostics"], old)
            self.assertTrue(all(row.get("reason") == "no_matching_baseline_metric"
                                for row in legacy["metrics"] if row["metric"] == "outputTokens"))
            baseline_path = root / "before.json"
            baseline_path.write_text(json.dumps(baseline), encoding="utf-8")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(audit.main([str(root / "rollout.jsonl"), "--repo-root", str(root),
                                             "--baseline", str(baseline_path), "--summary-json"]), 0)
            self.assertEqual(json.loads(out.getvalue())["baselineComparison"]["usageSavingsAssessment"], assessment)

    def test_ranking_cache_and_full_json_are_consistent(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self._measured_report(root, turns=12)
            source = root / "rollout.jsonl"
            records = [json.loads(line) for line in source.read_text(encoding="utf-8").splitlines()]
            for record in records:
                payload = record.get("payload", {})
                if payload.get("type") == "task_complete":
                    factor = int(payload["turn_id"]) % 6 + 1
                    for request in payload["timing"]["modelRequests"]:
                        request["tokenUsage"] = {
                            name: value * factor for name, value in request["tokenUsage"].items()
                        }
            source.write_text("\n".join(map(json.dumps, records)) + "\n", encoding="utf-8")
            report = audit.analyze_session_path(source, root)
            efficiency = report["sessionDiagnostics"]["usageEfficiency"]
            self.assertEqual(
                [(row["turnId"], row["totalTokens"]) for row in efficiency["rankedTurns"]],
                [("11", 1410), ("5", 1410), ("10", 1175), ("4", 1175),
                 ("3", 940), ("9", 940), ("2", 705), ("8", 705), ("1", 470), ("7", 470)],
            )
            self.assertEqual(efficiency["omittedRankedTurns"], 2)
            for expected in ("miss", "hit"):
                cached = audit.analyze_session_path(root / "rollout.jsonl", root, cache_dir=root / "cache")
                self.assertEqual(cached["analysisCache"]["status"], expected)
                self.assertEqual(cached["sessionDiagnostics"]["usageEfficiency"], efficiency)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(audit.main([str(root / "rollout.jsonl"), "--repo-root", str(root), "--json"]), 0)
            self.assertEqual(json.loads(out.getvalue())["sessionDiagnostics"]["usageEfficiency"], efficiency)


if __name__ == "__main__":
    unittest.main()
