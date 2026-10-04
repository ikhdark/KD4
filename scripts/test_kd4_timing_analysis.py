"""Exercise canonical diagnostics through audit and native runner evidence."""

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_timing_analysis as analysis
import contextlib
import io
from scripts import kd4_turn_latency_audit as audit


def timing_profile() -> dict:
    return {
        "schemaVersion": 25,
        "profileValid": True,
        "classificationComplete": True,
        "inclusiveDurationNs": 1_000_000_000,
        "machineDurationNs": 900_000_000,
        "exclusive": {
            "modelOnlyNs": 600_000_000,
            "toolOnlyNs": 200_000_000,
            "orchestrationNs": 100_000_000,
            "interactiveOnlyWaitNs": 100_000_000,
        },
        "unions": {"modelStreamWaitUnionNs": 590_000_000},
        "counters": {"logicalGenerationCount": 2, "toolCallCount": 1},
        "milestones": {"firstUsefulActionMs": 12.5, "firstDomainActionMs": 12.5},
        "modelRequests": [
            {
                "generationIndex": 0,
                "modelStreamWaitNs": 300_000_000,
                "tokenUsage": {
                    "inputTokens": 100,
                    "cachedInputTokens": 80,
                    "visibleOutputTokens": 10,
                    "reasoningTokens": 5,
                },
            },
            {
                "generationIndex": 1,
                "isContinuation": True,
                "modelStreamWaitNs": 300_000_000,
                "physicalAttemptIds": ["retry-1", "retry-2", "retry-2"],
                "outputTokens": 10,
                "reasoningOutputTokens": 2,
            },
        ],
        "toolCalls": [
            {
                "callId": "tool-1",
                "generationIndex": 0,
                "acceptedAtMs": 20,
                "processSpawnedAtMs": 30,
                "processExitedAtMs": 80,
                "outputCollectedAtMs": 85,
                "deliveredAtMs": 90,
                "outputModelVisibleAtMs": 100,
                "modelResumedAtMs": 110,
            }
        ],
    }


class TimingEvidenceRegressionsTest(unittest.TestCase):
    def test_audit_conflicts_compare_full_profiles_not_only_milestones(self):
        first = timing_profile()
        second = copy.deepcopy(first)
        second["exclusive"]["modelOnlyNs"] += 1
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text("\n".join(json.dumps({
                "timestamp": "2026-09-27T00:00:00Z", "type": "event_msg",
                "payload": {"type": "task_complete", "turn_id": "turn", "timing": timing},
            }) for timing in (first, second)), encoding="utf-8")
            report = audit.analyze_session_path(path, Path(directory))
        self.assertEqual(report["coverage"]["conflictingTerminalProfiles"], 1)
        actions = report["firstUsefulActionAnalysis"]
        self.assertEqual(actions["exclusions"]["conflictingTerminalProfiles"], 1)
        self.assertEqual(actions["canonicalTurnCount"], 0)
        self.assertEqual(report["runnerDiagnostics"]["coverage"]["validCompleteTimingProfiles"], 0)

    def test_first_output_and_progress_require_assistant_provenance(self):
        events = [
            {"elapsedMs": index * 1000, "message": {"type": "response_item", "payload": {
                "type": "message", "role": role, "content": [],
            }}}
            for index, role in enumerate(("developer", "user", "system", "assistant", "user"))
        ]
        before = analysis.analyze_runner_evidence({"schemaVersion": 1, "events": events[:3]})
        self.assertIsNone(before["firstOutputMs"])
        self.assertIsNone(before["lastProgress"])
        report = analysis.analyze_runner_evidence({"schemaVersion": 1, "events": events})
        self.assertEqual(report["firstOutputMs"], 3000)
        self.assertEqual(report["lastProgress"]["elapsedMs"], 3000)
        for message in (
            {"method": "item/agentMessage/delta", "params": {"delta": "hello"}},
            {"method": "item/completed", "params": {"item": {"type": "agentMessage", "text": "hello"}}},
        ):
            report = analysis.analyze_runner_evidence({"schemaVersion": 1, "events": [
                {"elapsedMs": 42, "message": message},
            ]})
            self.assertEqual(report["firstOutputMs"], 42)

    def test_builtin_rollout_calls_keep_identity_completion_and_pending_work(self):
        items = [
            {"type": "web_search_call", "id": "web", "status": "in_progress"},
            {"type": "web_search_call", "id": "web", "status": "completed"},
            {"type": "local_shell_call", "call_id": "shell", "status": "completed"},
            {"type": "image_generation_call", "id": "image", "status": "in_progress"},
        ]
        report = analysis.analyze_runner_evidence({"schemaVersion": 1, "events": [
            {"elapsedMs": (index + 1) * 1000, "message": {"type": "response_item", "payload": item}}
            for index, item in enumerate(items)
        ]})
        self.assertEqual(report["directToolCount"], 3)
        self.assertEqual(report["firstToolMs"], 1000)
        self.assertEqual([call["id"] for call in report["pendingTools"]], ["image"])
        self.assertEqual({call["tool"] for call in report["tools"]}, {
            "web_search", "local_shell", "image_generation",
        })
        self.assertEqual(report["failures"], [])

    def test_resumed_history_does_not_invalidate_complete_request_usage(self):
        timing = timing_profile()
        timing["modelRequests"] = timing["modelRequests"][:1]
        timing["counters"]["modelRequestCount"] = 1
        report = analysis.analyze_runner_evidence({"schemaVersion": 1, "events": [
            {"message": {"method": "turn/completed", "params": {
                "threadId": "thread", "turn": {"id": "turn", "status": "completed", "timing": timing},
            }}},
            {"message": {"method": "thread/tokenUsage/updated", "params": {
                "threadId": "thread", "tokenUsage": {"total": {
                    "inputTokens": 1100, "cachedInputTokens": 880,
                    "outputTokens": 165, "reasoningOutputTokens": 55,
                }},
            }}},
        ]})
        self.assertTrue(report["tokens"]["complete"])
        self.assertEqual(report["tokens"]["inputTokens"], 100)
        self.assertEqual(report["cacheHitRate"], 0.8)
        self.assertEqual(report["nativeCumulativeTokens"]["inputTokens"], 1100)
        self.assertEqual(report["tokenReconciliation"]["residuals"]["inputTokens"], 1000)
        self.assertFalse(report["tokenReconciliation"]["addedToRequestTotals"])


class SharedTimingAnalysisTest(unittest.TestCase):
    def test_rollout_response_usage_reconciles_turns_and_rejects_partial_evidence(self):
        def usage(input_tokens, cached, output, reasoning):
            return {
                "input_tokens": input_tokens,
                "cached_input_tokens": cached,
                "output_tokens": output,
                "reasoning_output_tokens": reasoning,
            }

        rows = []
        for turn_index, turn in enumerate(("a", "b")):
            rows.append({"type": "event_msg", "payload": {
                "type": "task_started", "turn_id": turn,
            }})
            for index, counts in enumerate((usage(100, 80, 15, 5), usage(50, 40, 8, 3))):
                cumulative = counts if index == 0 else usage(150, 120, 23, 8)
                record = {"type": "token_usage_record", "payload": {
                    "thread_id": "embedded-session-uuid", "turn_id": turn,
                    "response_id": f"response-{index}", "usage": counts,
                    "turn_token_usage": cumulative,
                    "thread_token_usage": {
                        key: value + turn_index * usage(150, 120, 23, 8)[key]
                        for key, value in cumulative.items()
                    },
                }}
                rows.append(record)
                if index == 0:
                    rows.append(copy.deepcopy(record))  # Replayed provider receipt.
            rows.append({"type": "event_msg", "payload": {
                "type": "token_count", "info": {"total_token_usage": {
                    key: value * (turn_index + 1)
                    for key, value in usage(150, 120, 23, 8).items()
                }},
            }})
            rows.append({"type": "event_msg", "payload": {
                "type": "task_complete", "turn_id": turn,
            }})

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "official.jsonl"

            def analyze(records, *, tokens=True):
                source.write_text("\n".join(map(json.dumps, records)) + "\n", encoding="utf-8")
                return audit.analyze_session_path(source, root, include_tokens=tokens)["runnerDiagnostics"]

            report = analyze(rows)
            self.assertTrue(report["tokens"]["complete"])
            self.assertEqual(report["tokens"]["source"], "rollout_response_usage")
            self.assertEqual(report["tokens"]["inputTokens"], 300)
            self.assertEqual(report["tokens"]["outputTokens"], 46)
            self.assertEqual(report["tokens"]["reasoningTokens"], 16)
            self.assertEqual(report["tokens"]["deduplicatedRequestRecords"], 2)
            self.assertEqual(report["physicalRequests"], 4)
            self.assertEqual(report["cacheHitRate"], 0.8)
            self.assertEqual(report["nativeCumulativeTokens"]["inputTokens"], 300)
            self.assertEqual(len(report["nativeProviderUsage"]), 1)
            self.assertIsNone(report["runtime"])
            self.assertIsNone(report["logicalGenerations"])

            for defect in (
                "missing_request", "conflicting_replay", "missing_identity",
                "invalid_total", "unfinished",
            ):
                with self.subTest(defect=defect):
                    damaged = copy.deepcopy(rows)
                    if defect == "missing_request":
                        damaged = [row for row in damaged if not (
                            row["payload"].get("turn_id") == "a"
                            and row["payload"].get("response_id") == "response-0"
                        )]
                    elif defect == "conflicting_replay":
                        damaged[2]["payload"]["usage"]["input_tokens"] += 1
                    elif defect == "missing_identity":
                        del damaged[1]["payload"]["response_id"]
                    elif defect == "invalid_total":
                        damaged[1]["payload"]["usage"]["total_tokens"] = 1
                    else:
                        damaged.pop()
                    partial = analyze(damaged)
                    self.assertFalse(partial["tokens"]["complete"])
                    self.assertIsNone(partial["tokens"]["inputTokens"])
                    self.assertIsNone(partial["cacheHitRate"])

            mixed = copy.deepcopy(rows)
            profile = timing_profile()
            profile["counters"]["modelRequestCount"] = 2
            for request, counts in zip(profile["modelRequests"], (usage(100, 80, 15, 5), usage(50, 40, 8, 3))):
                request.pop("physicalAttemptIds", None)
                request["tokenUsage"] = analysis._native_usage(counts)
            mixed[5]["payload"]["timing"] = profile
            report = analyze(mixed)
            self.assertEqual(report["tokens"]["source"], "mixed_request_usage")
            self.assertEqual(report["tokens"]["inputTokens"], 300)
            self.assertTrue(report["tokens"]["complete"])
            self.assertEqual(report["physicalRequests"], 4)
            self.assertFalse(analyze(rows, tokens=False)["tokens"]["enabled"])

    def test_response_outputs_pair_by_call_id_without_crossing_turns_or_threads(self):
        events = []
        for call_type in ("function_call", "custom_tool_call"):
            for thread, turn in (("a", "1"), ("b", "1"), ("a", "2")):
                events.append(
                    {
                        "type": "response_item",
                        "payload": {
                            "thread_id": thread,
                            "turn_id": turn,
                            "type": call_type,
                            "id": "item-" + call_type,
                            "call_id": call_type,
                            "name": "tool",
                        },
                    }
                )
            events.append(
                {
                    "type": "response_item",
                    "payload": {
                        "thread_id": "a",
                        "turn_id": "1",
                        "type": call_type + "_output",
                        "id": "output-" + call_type,
                        "call_id": call_type,
                        "output": "done",
                    },
                }
            )
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events}
        )
        self.assertEqual(len(report["tools"]), 6)
        self.assertEqual(len(report["pendingTools"]), 4)
        self.assertEqual(
            {
                (call["threadId"], call["turnId"], call["id"])
                for call in report["pendingTools"]
            },
            {
                (thread, turn, call_type)
                for thread, turn in (("b", "1"), ("a", "2"))
                for call_type in ("function_call", "custom_tool_call")
            },
        )
        completed = [call for call in report["tools"] if "completionEventIndex" in call]
        self.assertEqual(len(completed), 2)
        self.assertTrue(all(call["status"] == "output_observed" for call in completed))

    def test_provider_usage_replay_is_deduplicated_and_conflicts_are_visible(self):
        profile = timing_profile()
        request = profile["modelRequests"][0]
        request["samplingRequestId"] = "sampling-1"
        profile["modelRequests"] = [request, copy.deepcopy(request)]
        profile["counters"]["logicalGenerationCount"] = 1
        report = analysis.analyze_runner_evidence(self.evidence(profile))
        self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 100)
        self.assertEqual(report["tokens"]["observedTotals"]["outputTokens"], 15)
        self.assertEqual(report["tokens"]["deduplicatedRequestRecords"], 1)
        self.assertEqual(report["tokens"]["conflictingUsageRequestIds"], [])
        self.assertTrue(report["tokens"]["complete"])
        profile["modelRequests"][1]["tokenUsage"]["inputTokens"] = 120
        report = analysis.analyze_runner_evidence(self.evidence(profile))
        self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 120)
        self.assertEqual(report["tokens"]["conflictingUsageRequestIds"], ["sampling-1"])
        self.assertFalse(report["tokens"]["complete"])
        self.assertIsNone(report["tokens"]["providerTotals"])

    def test_tool_dispatch_counters_flow_through_audit_and_require_complete_evidence(
        self,
    ):
        timing = timing_profile()
        timing["toolCalls"][0].update(retryCount=2, reentryCount=3)
        timing["toolCalls"].append(
            {
                "callId": "nested",
                "parentCallId": "tool-1",
                "retryCount": 5,
                "reentryCount": 7,
            }
        )
        evidence = self.evidence(timing)
        evidence["events"].append(copy.deepcopy(evidence["events"][0]))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "evidence.json"
            path.write_text(json.dumps(evidence), encoding="utf-8")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                result = audit.main(
                    ["--runner-evidence", str(path), "--tokens", "off", "--json"]
                )
            self.assertEqual(result, 0)
            report = json.loads(stdout.getvalue())
            dispatch = report["runnerDiagnostics"]["toolDispatch"]
            self.assertTrue(dispatch["complete"])
            self.assertEqual(
                (
                    dispatch["retryCount"],
                    dispatch["reentryCount"],
                    dispatch["retainedCalls"],
                ),
                (7, 10, 2),
            )
            self.assertEqual(
                audit.bounded_summary(report)["runnerDiagnostics"]["toolDispatch"],
                dispatch,
            )
            self.assertFalse(report["runnerDiagnostics"]["tokens"]["enabled"])
            for mutation in (
                "missing",
                "negative",
                "boolean",
                "saturated",
                "overflow",
                "missing_turn",
            ):
                with self.subTest(mutation=mutation):
                    changed = copy.deepcopy(timing)
                    call = changed["toolCalls"][1]
                    if mutation == "missing":
                        del call["retryCount"]
                    elif mutation in ("negative", "boolean", "saturated"):
                        call["retryCount"] = {
                            "negative": -1,
                            "boolean": True,
                            "saturated": 2**32 - 1,
                        }[mutation]
                    elif mutation == "overflow":
                        changed["toolCallTimingOverflow"] = 1
                    native = self.evidence(changed)
                    if mutation == "missing_turn":
                        native["events"].append(
                            {
                                "message": {
                                    "method": "turn/completed",
                                    "params": {
                                        "turn": {"id": "missing", "status": "completed"}
                                    },
                                }
                            }
                        )
                    partial = audit.analyze_session_path(
                        None,
                        Path(directory),
                        runner_evidence=native,
                        include_tokens=False,
                    )["runnerDiagnostics"]["toolDispatch"]
                    self.assertFalse(partial["complete"])
                    self.assertIsNone(partial["retryCount"])
                    self.assertIsNone(partial["reentryCount"])
            empty = analysis.analyze_runner_evidence(
                {"schemaVersion": 1, "events": []}, include_tokens=False
            )["toolDispatch"]
            self.assertFalse(empty["available"])
            self.assertIsNone(empty["retryCount"])
            timing["toolCalls"] = []
            zero = analysis.analyze_runner_evidence(
                self.evidence(timing), include_tokens=False
            )["toolDispatch"]
            self.assertTrue(zero["complete"])
            self.assertEqual(zero["retryCount"], 0)

    def test_wait_diagnostics_reach_runner_report_without_claiming_cpu_cost(self):
        profile = timing_profile()
        profile["toolCalls"][0].update(
            toolName="exec_command", retryCount=0, reentryCount=14000, totalDurationMs=1000,
            timerWaits=[{"waitKind": "owner_output_wait", "wakeReason": "completed"}] * 3,
        )
        report = analysis.analyze_runner_evidence(self.evidence(profile))
        waits = report["toolDispatch"]["waitDiagnostics"]
        self.assertEqual(waits["observedWaits"], [
            {"waitKind": "owner_output_wait", "wakeReason": "completed", "count": 3}
        ])
        self.assertEqual(waits["highReentryCallCount"], 1)
        self.assertEqual(waits["highReentryCalls"][0]["callId"], "tool-1")
        self.assertEqual(waits["highReentryCalls"][0]["reentriesPerSecond"], 14000)
        self.assertIn("not proof", waits["note"])
        # A folded record's sequence counts every wake it covers.
        profile["toolCalls"][0]["timerWaits"] = [
            {"waitKind": "owner_output_wait", "wakeReason": "completed", "sequence": 14000},
            {"waitKind": "output_drain", "wakeReason": "completed", "sequence": 14001},
        ]
        folded = analysis.analyze_runner_evidence(self.evidence(profile))
        self.assertEqual(folded["toolDispatch"]["waitDiagnostics"]["observedWaits"], [
            {"waitKind": "output_drain", "wakeReason": "completed", "count": 1},
            {"waitKind": "owner_output_wait", "wakeReason": "completed", "count": 14000},
        ])
        profile["toolCalls"][0].pop("timerWaits")
        profile["toolCalls"][0]["reentryCount"] = 1
        dispatch = analysis.analyze_runner_evidence(self.evidence(profile))["toolDispatch"]
        self.assertNotIn("waitDiagnostics", dispatch)

    def test_request_history_cap_cannot_produce_complete_usage_or_continuation_totals(
        self,
    ):
        timing = timing_profile()
        timing["modelRequests"] = [
            dict(
                copy.deepcopy(timing["modelRequests"][0]),
                generationIndex=index,
                isContinuation=index > 0,
            )
            for index in range(1024)
        ]
        timing["counters"]["modelRequestCount"] = 1025
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "rollout.jsonl"
            source.write_text(
                "\n".join(
                    json.dumps({"type": "event_msg", "payload": payload})
                    for payload in (
                        {"type": "task_started", "turn_id": "turn-1"},
                        {
                            "type": "task_complete",
                            "turn_id": "turn-1",
                            "timing": timing,
                        },
                    )
                ),
                encoding="utf-8",
            )
            report = audit.analyze_session_path(
                source, root, runner_evidence=self.evidence(timing)
            )
            runner = report["runnerDiagnostics"]
            self.assertFalse(runner["requestRetention"]["complete"])
            self.assertEqual(runner["requestRetention"]["recordedRequests"], 1025)
            self.assertEqual(runner["requestRetention"]["retainedRequests"], 1024)
            self.assertIsNone(runner["physicalRequests"])
            self.assertIsNone(analysis.continuation_count(timing))
            self.assertIsNone(
                report["latencyBreakdown"]["modelInference"]["retryAttempts"]
            )
            for tokens in (
                runner["tokens"],
                runner["runtime"]["tokens"],
                report["populations"]["all"]["tokens"],
            ):
                self.assertFalse(tokens["complete"])
                self.assertIsNone(tokens["inputTokens"])
                self.assertEqual(tokens["observedTotals"]["inputTokens"], 102400)
            self.assertEqual(
                audit.bounded_summary(report)["runnerDiagnostics"]["requestRetention"],
                runner["requestRetention"],
            )
            timing["counters"]["modelRequestCount"] = 1024
            complete = analysis.analyze_runner_evidence(self.evidence(timing))
            self.assertTrue(complete["requestRetention"]["complete"])
            self.assertTrue(complete["tokens"]["complete"])
            self.assertEqual(complete["tokens"]["inputTokens"], 102400)
            self.assertEqual(analysis.continuation_count(timing), 1023)

    def test_retry_and_fallback_attempts_are_not_continuations(self):
        # The runtime flags every row after the turn's first as a continuation,
        # retries included. Three logical generations have two continuations.
        timing = timing_profile()
        timing["modelRequests"] = [
            {"generationIndex": index, "attemptKind": kind, "isContinuation": flag}
            for index, kind, flag in (
                (0, "primary", False),
                (0, "retry", True),
                (1, "primary", True),
                (1, "fallback", True),
                (2, "primary", True),
            )
        ]
        timing["counters"]["modelRequestCount"] = 5
        self.assertEqual(analysis.continuation_count(timing), 2)
        self.assertEqual(analysis.analyze_timing(timing)["continuationCount"], 2)

    def test_contradictory_provider_usage_is_unavailable_through_audit(self):
        timing = timing_profile()
        timing["modelRequests"] = timing["modelRequests"][:1]
        with tempfile.TemporaryDirectory() as directory:
            for fields in (
                {"cachedInputTokens": 101},
                {"totalTokens": 114},
                {"totalTokens": "invalid"},
                {"totalTokens": None},
                {"totalTokens": True},
                {"outputTokens": 16},
                {"nonCachedInputTokens": 19},
            ):
                with self.subTest(fields=fields):
                    changed = copy.deepcopy(timing)
                    changed["modelRequests"][0]["tokenUsage"].update(fields)
                    report = audit.analyze_session_path(
                        None, Path(directory), runner_evidence=self.evidence(changed)
                    )
                    for tokens in (
                        report["runnerDiagnostics"]["tokens"],
                        report["runnerDiagnostics"]["runtime"]["tokens"],
                    ):
                        self.assertFalse(tokens["complete"])
                        self.assertEqual(tokens["invalidUsageAttempts"], 1)
                        self.assertIsNone(tokens["inputTokens"])
                        self.assertIsNone(tokens["providerTotals"])
                    self.assertIsNone(report["runnerDiagnostics"]["cacheHitRate"])
            timing["modelRequests"][0]["tokenUsage"].update(
                totalTokens=115, outputTokens=15, nonCachedInputTokens=20
            )
            valid = analysis.analyze_runner_evidence(self.evidence(timing))["tokens"]
            self.assertTrue(valid["complete"])
            self.assertEqual(valid["invalidUsageAttempts"], 0)
            self.assertEqual(valid["totalTokens"], 115)

    def test_nonprogress_requires_explicit_booleans_in_audit_and_runner(self):
        for facts, expected in (
            ({"unchangedRelevantState": True}, 0),
            ({"unchangedRelevantState": True, "nextStructuredActionChanged": None}, 0),
            ({"unchangedRelevantState": True, "nextStructuredActionChanged": 0}, 0),
            (
                {
                    "unchangedRelevantState": "false",
                    "nextStructuredActionChanged": False,
                },
                0,
            ),
            ({"unchangedRelevantState": True, "nextStructuredActionChanged": True}, 0),
            ({"unchangedRelevantState": True, "nextStructuredActionChanged": False}, 1),
        ):
            with self.subTest(facts=facts), tempfile.TemporaryDirectory() as temp:
                timing = timing_profile()
                timing["modelRequests"] = [dict(timing["modelRequests"][0], **facts)]
                source = Path(temp) / "rollout.jsonl"
                source.write_text(
                    "\n".join(
                        json.dumps({"type": "event_msg", "payload": payload})
                        for payload in (
                            {"type": "task_started", "turn_id": "t"},
                            {"type": "task_complete", "turn_id": "t", "timing": timing},
                        )
                    ),
                    encoding="utf-8",
                )
                report = audit.analyze_session_path(
                    source, Path(temp), include_tokens=False
                )
                for metric in (
                    report["perTurn"][0]["observationalNonprogressLatency"],
                    report["populations"]["all"]["observationalNonprogressLatency"],
                    report["runnerDiagnostics"]["runtime"][
                        "observationalNonprogressLatency"
                    ],
                ):
                    self.assertEqual(metric["logicalGenerations"], expected)
                    self.assertEqual(
                        metric["modelStreamWaitNs"], expected * 300_000_000
                    )
                self.assertEqual(
                    report["behaviorSignals"]["turnsWithObservationalNonprogress"],
                    expected,
                )

    def test_source_only_cost_is_visible_without_claiming_nonprogress_or_savings(self):
        timing = timing_profile()
        timing["observationalNonprogressLatency"] = {"logicalGenerations": 0}
        timing["modelRequests"] = [
            {
                "generationIndex": 0,
                "attemptKind": "primary",
                "progressKinds": ["new_source_evidence"],
                "unchangedRelevantState": False,
                "nextStructuredActionChanged": True,
                "modelStreamWaitNs": 300_000_000,
            },
            {
                "generationIndex": 0,
                "attemptKind": "retry",
                "modelStreamWaitNs": 100_000_000,
            },
            {
                "generationIndex": 1,
                "attemptKind": "primary",
                "progressKinds": ["workspace_mutation", "new_source_evidence"],
                "modelStreamWaitNs": 900_000_000,
            },
            {"generationIndex": 2, "attemptKind": "primary"},
        ]
        with tempfile.TemporaryDirectory() as directory:
            for recorded, complete in ((4, True), (5, False), (None, None)):
                with self.subTest(recorded=recorded):
                    timing["counters"]["modelRequestCount"] = recorded
                    source = Path(directory) / "rollout.jsonl"
                    source.write_text(
                        json.dumps({
                            "type": "event_msg",
                            "payload": {
                                "type": "task_complete",
                                "turn_id": "turn-1",
                                "timing": timing,
                            },
                        }),
                        encoding="utf-8",
                    )
                    report = audit.analyze_session_path(
                        source, Path(directory), include_tokens=False,
                        runner_evidence=self.evidence(timing),
                    )
                    bounded = audit.bounded_summary(report)
                    for owner in (
                        report["perTurn"][0],
                        report["populations"]["all"],
                        report["runnerDiagnostics"]["runtime"],
                        bounded["perTurn"][0],
                        bounded["populations"]["all"],
                    ):
                        metric = owner["sourceEvidenceOnlyLatency"]
                        self.assertEqual(metric["logicalGenerations"], 1)
                        self.assertEqual(metric["physicalAttempts"], 2)
                        self.assertEqual(metric["modelStreamWaitNs"], 400_000_000)
                        self.assertIs(metric["retentionComplete"], complete)
                        self.assertIn("not proven waste or savings", metric["definition"])
                    self.assertIn("source_evidence_only", report["perTurn"][0]["signals"])
                    self.assertEqual(
                        report["populations"]["all"]["observationalNonprogressLatency"][
                            "logicalGenerations"
                        ],
                        0,
                    )

    def test_captured_request_volume_flows_through_audit_without_tokens(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "requests.jsonl"
            path.write_text(
                '{"request":{"z":"é","a":[1]}}\n\n'
                '{"turnIndex":2,"request":{"input":"ok"}}\n',
                encoding="utf-8",
            )
            evidence = self.evidence()
            evidence["providerRequestsPath"] = str(path)
            report = audit.analyze_session_path(
                None, root, include_tokens=False, runner_evidence=evidence
            )
            captured = report["runnerDiagnostics"]["capturedRequests"]
            self.assertTrue(captured["available"])
            self.assertEqual(captured["requestCount"], 2)
            self.assertEqual(captured["serializedRequestBytes"], 32)
            self.assertIsNone(captured["error"])
            self.assertIn("not tokens", captured["measurementNote"])
            self.assertFalse(report["runnerDiagnostics"]["tokens"]["enabled"])
            self.assertEqual(
                audit.bounded_summary(report)["runnerDiagnostics"]["capturedRequests"],
                {
                    "available": True,
                    "requestCount": 2,
                    "serializedRequestBytes": 32,
                    "error": None,
                },
            )

    def test_missing_or_malformed_request_capture_is_unavailable_not_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "requests.jsonl"
            for contents in [
                None,
                '{"request":{"input":"ok"}}\ninvalid\n',
                '{"request":[]}\n',
            ]:
                with self.subTest(contents=contents):
                    if contents is not None:
                        path.write_text(contents, encoding="utf-8")
                    evidence = self.evidence()
                    evidence["providerRequestsPath"] = str(path)
                    report = audit.analyze_session_path(
                        None, root, include_tokens=False, runner_evidence=evidence
                    )
                    captured = report["runnerDiagnostics"]["capturedRequests"]
                    self.assertFalse(captured["available"])
                    self.assertIsNone(captured["requestCount"])
                    self.assertIsNone(captured["serializedRequestBytes"])
                    self.assertIsNotNone(captured["error"])
            path.write_text("", encoding="utf-8")
            empty = audit.analyze_session_path(
                None, root, include_tokens=False, runner_evidence=evidence
            )
            self.assertEqual(
                empty["runnerDiagnostics"]["capturedRequests"]["requestCount"], 0
            )
            self.assertEqual(
                empty["runnerDiagnostics"]["capturedRequests"][
                    "serializedRequestBytes"
                ],
                0,
            )

    def test_native_tool_activity_replays_thread_scoped_lifecycles_through_audit_cli(
        self,
    ):
        def event(ms, method, thread, turn, call, kind="commandExecution"):
            return {
                "elapsedMs": ms,
                "message": {
                    "method": method,
                    "params": {
                        "threadId": thread,
                        "turnId": turn,
                        "item": {"id": call, "type": kind, "command": "rg needle src"},
                    },
                },
            }

        events = [
            event(10, "item/started", "root", "t", "same"),
            event(12, "item/started", "child", "t", "same"),
            event(24, "item/completed", "child", "t", "same"),
            event(30, "item/completed", "root", "t", "same"),
            event(35, "item/completed", "root", "t", "same"),
            event(40, "item/started", "root", "t", "same"),
            event(45, "item/started", "root", "t2", "same"),
            event(60, "item/completed", "root", "t2", "same"),
            event(61, "item/started", "root", "t2", "generic", "toolCall"),
            event(71, "item/completed", "root", "t2", "generic", "toolCall"),
            event(72, "item/completed", "root", "t2", "edit", "fileChange"),
            event(73, "item/started", "root", "t2", "agent", "collabAgentToolCall"),
            event(80, "item/completed", "root", "t2", "agent", "collabAgentToolCall"),
            event(82, "item/started", "root", "t2", "cancelled"),
            {
                "elapsedMs": 90,
                "message": {
                    "method": "turn/completed",
                    "params": {
                        "threadId": "root",
                        "turn": {"id": "t2", "status": "interrupted"},
                    },
                },
            },
        ]
        evidence = {"schemaVersion": 1, "events": events, "status": "completed"}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "native-evidence.json"
            path.write_text(json.dumps(evidence), encoding="utf-8")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(
                    audit.main(
                        ["--runner-evidence", str(path), "--tokens", "off", "--json"]
                    ),
                    0,
                )
            full = json.loads(output.getvalue())
        report = full["runnerDiagnostics"]
        activity = report["toolActivity"]
        self.assertTrue(activity["available"])
        self.assertEqual(
            (
                activity["observedCount"],
                activity["startedCount"],
                activity["completedCount"],
                activity["pendingCount"],
            ),
            (7, 6, 6, 1),
        )
        self.assertEqual(
            activity["byKind"],
            {
                "commandExecution": 4,
                "mcpToolCall": 0,
                "dynamicToolCall": 0,
                "fileChange": 1,
                "webSearch": 0,
                "toolCall": 1,
                "collabAgentToolCall": 1,
            },
        )
        self.assertEqual(
            [
                (row["threadId"], row["turnId"], row["observedCount"])
                for row in activity["turns"]
            ],
            [("root", "t", 1), ("child", "t", 1), ("root", "t2", 5)],
        )
        self.assertEqual(
            [(row["threadId"], row["id"]) for row in report["pendingTools"]],
            [("root", "cancelled")],
        )
        self.assertEqual(
            [row["durationMs"] for row in report["tools"]],
            [20, 12, 15, 10, None, 7, None],
        )
        self.assertEqual(report["directToolCount"], 7)
        self.assertEqual(
            activity["durations"]["rgSearch"],
            {
                "observedCount": 4,
                "measuredCount": 3,
                "missingOrInvalidCount": 1,
                "observedTotalMs": 47,
                "totalMs": None,
                "minMs": 12,
                "maxMs": 20,
            },
        )
        self.assertEqual(
            activity["durations"]["byKind"]["fileChange"]["measuredCount"], 0
        )
        self.assertIsNone(
            activity["durations"]["byKind"]["fileChange"]["observedTotalMs"]
        )
        self.assertEqual(
            activity["durations"]["byKind"]["collabAgentToolCall"]["totalMs"], 7
        )
        self.assertEqual(
            audit.bounded_summary(full)["runnerDiagnostics"]["toolActivity"][
                "durations"
            ],
            activity["durations"],
        )
        self.assertIn('"observedTotalMs": 47', audit.render_report(full))
        # Replayed notifications (including starts after completions) cannot
        # duplicate work or turn a completed item back into pending work.
        evidence["events"] = events + events
        replayed = analysis.analyze_runner_evidence(evidence, include_tokens=False)
        self.assertEqual(replayed["toolActivity"], activity)
        self.assertEqual(
            audit.bounded_summary(full)["runnerDiagnostics"]["toolActivity"]["byKind"],
            activity["byKind"],
        )

    def test_tool_durations_reject_bad_clocks_and_keep_search_failures(self):
        for start, end, expected in [
            (10, 10, 0),
            (10, 35, 25),
            (None, 35, None),
            (40, 35, None),
            (-1, 35, None),
            (True, 35, None),
            (10, float("inf"), None),
            (float("nan"), 35, None),
        ]:
            with self.subTest(start=start, end=end):
                events = []
                for call_id, kind, command in [
                    ("search", "commandExecution", "rg needle src"),
                    ("compound", "commandExecution", "rg needle src; python slow.py"),
                    ("mcp", "mcpToolCall", None),
                ]:
                    for ms, method in [
                        (start, "item/started"),
                        (end, "item/completed"),
                    ]:
                        events.append(
                            {
                                "elapsedMs": ms,
                                "message": {
                                    "method": method,
                                    "params": {
                                        "turnId": "t",
                                        "item": {
                                            "id": call_id,
                                            "type": kind,
                                            "command": command,
                                            "exitCode": 2,
                                            "status": "failed",
                                        },
                                    },
                                },
                            }
                        )
                report = analysis.analyze_runner_evidence(
                    {"schemaVersion": 1, "events": events}
                )
                durations = report["toolActivity"]["durations"]
                self.assertEqual(durations["rgSearch"]["observedCount"], 1)
                self.assertEqual(durations["rgSearch"]["totalMs"], expected)
                self.assertEqual(
                    durations["byKind"]["mcpToolCall"]["totalMs"], expected
                )
                self.assertEqual(
                    durations["rgSearch"]["measuredCount"], int(expected is not None)
                )
                self.assertEqual(
                    [row["durationMs"] for row in report["tools"]], [expected] * 3
                )
                self.assertGreater(len(report["failures"]), 0)

    def test_native_tool_activity_distinguishes_missing_evidence_from_observed_zero(
        self,
    ):
        empty = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": []}, include_tokens=False
        )
        self.assertFalse(empty["toolActivity"]["available"])
        completed = analysis.analyze_runner_evidence(
            self.evidence(), include_tokens=False
        )
        self.assertTrue(completed["toolActivity"]["available"])
        self.assertEqual(completed["toolActivity"]["observedCount"], 0)
        self.assertEqual(completed["toolActivity"]["completedCount"], 0)

    def test_cache_hit_rate_requires_complete_nonzero_provider_usage(self):
        timing = timing_profile()
        timing["modelRequests"] = timing["modelRequests"][:1]
        timing["modelRequests"][0]["tokenUsage"]["inputTokens"] = 200
        evidence = self.evidence(timing)
        self.assertEqual(
            analysis.analyze_runner_evidence(evidence)["cacheHitRate"], 0.4
        )
        self.assertIsNone(
            analysis.analyze_runner_evidence(evidence, include_tokens=False)[
                "cacheHitRate"
            ]
        )
        for input_tokens, cached_tokens in [(0, 0), (10, 20)]:
            timing["modelRequests"][0]["tokenUsage"].update(
                inputTokens=input_tokens, cachedInputTokens=cached_tokens
            )
            self.assertIsNone(
                analysis.analyze_runner_evidence(evidence)["cacheHitRate"]
            )
        timing["modelRequests"][0]["tokenUsage"].update(
            inputTokens=200, cachedInputTokens=80
        )
        evidence["events"].append(
            {
                "message": {
                    "method": "turn/completed",
                    "params": {"turn": {"id": "missing", "status": "completed"}},
                }
            }
        )
        self.assertIsNone(analysis.analyze_runner_evidence(evidence)["cacheHitRate"])

    def test_replayed_requests_can_gain_usage_without_breaking_cache_reporting(self):
        profile = timing_profile()
        complete = dict(profile["modelRequests"][0], samplingRequestId="sampling-1")
        pending = {key: value for key, value in complete.items() if key != "tokenUsage"}
        for requests in ([pending, complete], [complete, pending]):
            with self.subTest(requests=requests):
                profile["modelRequests"] = requests
                report = audit.analyze_session_path(
                    None, Path.cwd(), runner_evidence=self.evidence(profile)
                )["runnerDiagnostics"]
                self.assertTrue(report["tokens"]["complete"])
                self.assertEqual(report["tokens"]["inputTokens"], 100)
                self.assertEqual(report["tokens"]["deduplicatedRequestRecords"], 1)
                self.assertEqual(report["cacheHitRate"], 0.8)

    def test_zero_request_turn_is_not_missing_usage_but_unknown_counts_are(self):
        for count in (0, None, False, 1):
            with self.subTest(count=count):
                profile = timing_profile()
                profile["modelRequests"] = profile["modelRequests"][:1]
                evidence = self.evidence(profile)
                zero = timing_profile()
                zero["modelRequests"] = []
                if count is not None:
                    zero["counters"]["modelRequestCount"] = count
                event = copy.deepcopy(evidence["events"][0])
                event["message"]["params"]["turn"].update(
                    id="zero", status="interrupted", timing=zero
                )
                evidence["events"].append(event)
                report = analysis.analyze_runner_evidence(evidence)
                known_zero = type(count) is int and count == 0
                self.assertEqual(report["tokens"]["complete"], known_zero)
                self.assertEqual(
                    report["tokens"]["inputTokens"], 100 if known_zero else None
                )
                self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 100)
                self.assertEqual(
                    report["tokenCoverage"]["missingTerminalTurnIds"],
                    [] if known_zero else ["zero"],
                )

    def test_cumulative_reconciliation_requires_a_matching_pre_turn_baseline(self):
        for baseline, extra, late in (
            (1000, 0, False),
            (0, 10, False),
            (None, 10, False),
            (1000, 0, True),
        ):
            with self.subTest(baseline=baseline, extra=extra, late=late):
                profile = timing_profile()
                profile["modelRequests"] = profile["modelRequests"][:1]
                terminal = self.evidence(profile)["events"][0]
                terminal["message"]["params"]["threadId"] = "thread"
                base = baseline or 0

                def usage(input_tokens, output, cached, reasoning):
                    return {
                        "message": {
                            "method": "thread/tokenUsage/updated",
                            "params": {
                                "threadId": "thread",
                                "tokenUsage": {
                                    "total": {
                                        "inputTokens": input_tokens,
                                        "outputTokens": output,
                                        "cachedInputTokens": cached,
                                        "reasoningOutputTokens": reasoning,
                                    }
                                },
                            },
                        }
                    }

                start = {
                    "message": {
                        "method": "turn/started",
                        "params": {
                            "threadId": "thread",
                            "turn": {"id": "turn-1"},
                        },
                    }
                }
                initial = usage(base, 0, 0, 0)
                events = (
                    [start]
                    if baseline is None
                    else [start, initial]
                    if late
                    else [initial, start]
                )
                events.extend([terminal, usage(base + 100 + extra, 15, 80, 5)])
                report = analysis.analyze_runner_evidence(
                    {"schemaVersion": 1, "events": events}
                )
                comparable = baseline is not None and not late
                complete = not (comparable and extra)
                self.assertEqual(
                    report["tokenReconciliation"]["populationComparable"], comparable
                )
                self.assertEqual(report["tokens"]["complete"], complete)
                self.assertEqual(
                    report["tokens"]["inputTokens"], 100 if complete else None
                )
                self.assertEqual(report["cacheHitRate"], 0.8 if complete else None)
                if comparable:
                    self.assertEqual(
                        report["tokenReconciliation"]["residuals"]["inputTokens"], extra
                    )

    def test_failure_replays_are_scoped_and_counted_once(self):
        events = []
        for thread in ("parent", "child"):
            events.extend(
                [
                    {
                        "message": {
                            "method": "item/completed",
                            "params": {
                                "threadId": thread,
                                "turnId": "turn",
                                "item": {
                                    "id": "call",
                                    "type": "commandExecution",
                                    "command": "python fail.py",
                                    "status": "failed",
                                    "exitCode": 2,
                                },
                            },
                        }
                    },
                    {
                        "message": {
                            "method": "turn/completed",
                            "params": {
                                "threadId": thread,
                                "turn": {"id": "turn", "status": "failed"},
                            },
                        }
                    },
                ]
            )
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events + events}
        )
        self.assertEqual(len(report["tools"]), 2)
        self.assertEqual(
            [row["kind"] for row in report["failures"]],
            ["tool_execution_failure", "turn_failed"] * 2,
        )
        self.assertEqual(report["pendingTools"], [])

    def test_rollout_status_prefers_structured_execution_results(self):
        cases = (
            (
                "python fail.py",
                {"execution_state": "exited", "exit_code": 2, "output": ""},
                "failed",
            ),
            (
                "python ok.py",
                {"execution_state": "exited", "exit_code": 0, "output": "Exit code: 2"},
                "completed",
            ),
            (
                "python slow.py",
                {"execution_state": "running", "session_id": 7},
                "running",
            ),
            (
                'rg "foo|bar" src',
                {"execution_state": "exited", "exit_code": 1, "output": ""},
                "completed",
            ),
            (
                'rg "foo|bar" src; python fail.py',
                {"execution_state": "exited", "exit_code": 1, "output": ""},
                "failed",
            ),
            ("python fail.py", "Exit code: 2", "failed"),
        )
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "rollout.jsonl"
            for command, result, status in cases:
                with self.subTest(command=command, status=status):
                    path.write_text(
                        "\n".join(
                            json.dumps(row)
                            for row in (
                                {
                                    "timestamp": "2026-09-26T00:00:00Z",
                                    "type": "response_item",
                                    "payload": {
                                        "type": "function_call",
                                        "call_id": "c",
                                        "name": "exec_command",
                                        "arguments": json.dumps({"cmd": command}),
                                    },
                                },
                                {
                                    "timestamp": "2026-09-26T00:00:01Z",
                                    "type": "response_item",
                                    "payload": {
                                        "type": "function_call_output",
                                        "call_id": "c",
                                        "output": result
                                        if isinstance(result, str)
                                        else json.dumps(result),
                                    },
                                },
                            )
                        ),
                        encoding="utf-8",
                    )
                    report = audit.analyze_session_path(path, Path(temp))
                    self.assertEqual(
                        report["behaviorSignals"]["failedToolCalls"],
                        int(status == "failed"),
                    )
                    self.assertEqual(
                        report["behaviorSignals"]["runningToolCalls"],
                        int(status == "running"),
                    )

    def evidence(self, timing=None):
        return {
            "schemaVersion": 1,
            "attemptId": "task-1",
            "elapsedMs": 1000,
            "status": "completed",
            "events": [
                {
                    "elapsedMs": 1000,
                    "message": {
                        "method": "turn/completed",
                        "params": {
                            "turn": {
                                "id": "turn-1",
                                "status": "completed",
                                "timing": timing or timing_profile(),
                            }
                        },
                    },
                }
            ],
        }

    def test_audit_and_runner_share_independent_expected_metrics(self):
        timing = timing_profile()
        original = copy.deepcopy(timing)
        runner = analysis.analyze_runner_evidence(self.evidence(timing))
        runtime = runner["runtime"]
        self.assertIsNone(runtime["tokens"]["inputTokens"])
        self.assertIsNone(runtime["tokens"]["outputTokens"])
        self.assertEqual(runtime["tokens"]["observedTotals"]["inputTokens"], 100)
        self.assertEqual(runtime["tokens"]["observedTotals"]["outputTokens"], 25)
        self.assertEqual(runtime["tokens"]["physicalAttempts"], 3)
        self.assertEqual(runtime["tokens"]["coverage"], 1 / 3)
        self.assertIsNone(runtime["tokens"]["billableTokens"])
        self.assertEqual(
            runtime["toolRelay"]["phaseTotalsMs"]["endToEndDurationMs"], 80
        )
        self.assertEqual(runtime["toolRelay"]["phaseTotalsMs"]["processRuntimeMs"], 50)
        self.assertEqual(runtime["modelStreamWaitNs"], 590_000_000)
        self.assertEqual(runtime["modelOnlyNs"], 600_000_000)
        self.assertEqual(runner["logicalGenerations"], 2)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "rollout.jsonl"
            source.write_text(
                json.dumps(
                    {
                        "type": "event_msg",
                        "payload": {
                            "type": "task_complete",
                            "turn_id": "turn-1",
                            "timing": timing,
                        },
                    }
                ),
                encoding="utf-8",
            )
            report = audit.analyze_session_path(source, root)
            self.assertEqual(report["perTurn"][0]["tokens"], runtime["tokens"])
            self.assertEqual(report["perTurn"][0]["firstUsefulActionMs"], 12.5)
        self.assertEqual(timing, original)

    def test_no_token_function_runs_in_scripted_audit_or_runner(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "rollout.jsonl"
            source.write_text(
                json.dumps(
                    {
                        "type": "event_msg",
                        "payload": {
                            "type": "task_complete",
                            "turn_id": "t",
                            "timing": timing_profile(),
                        },
                    }
                ),
                encoding="utf-8",
            )
            with (
                mock.patch.object(
                    analysis,
                    "_token_report",
                    side_effect=AssertionError("token calculation called"),
                ),
                mock.patch.object(
                    analysis,
                    "_diagnostic_token_report",
                    side_effect=AssertionError("token calculation called"),
                ),
                mock.patch.object(
                    audit,
                    "_token_intervals",
                    side_effect=AssertionError("token calculation called"),
                ),
            ):
                report = audit.analyze_session_path(
                    source, root, include_tokens=False, runner_evidence=self.evidence()
                )
            self.assertFalse(report["perTurn"][0]["tokens"]["enabled"])
            self.assertFalse(report["runnerDiagnostics"]["tokens"]["enabled"])
            self.assertEqual(report["runnerDiagnostics"]["logicalGenerations"], 2)
            self.assertEqual(report["perTurn"][0]["tokenIntervals"], [])

    def test_cli_startup_failure_without_rollout(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            evidence = root / "evidence.json"
            evidence.write_text(
                json.dumps(
                    {
                        "schemaVersion": 1,
                        "attemptId": "startup",
                        "status": "setup_failed",
                        "elapsedMs": 12,
                        "events": [],
                        "failure": {
                            "kind": "authentication",
                            "message": "credential missing",
                        },
                    }
                ),
                encoding="utf-8",
            )
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                result = audit.main(
                    ["--runner-evidence", str(evidence), "--tokens", "off", "--json"]
                )
            report = json.loads(stdout.getvalue())
            self.assertEqual(result, 0)
            self.assertIsNone(report["source"])
            self.assertEqual(
                report["runnerDiagnostics"]["failures"][0]["kind"], "authentication"
            )
            self.assertEqual(report["runnerDiagnostics"]["status"], "setup_failed")
            self.assertIsNone(report["runnerDiagnostics"]["physicalRequests"])

    def test_reference_usage_snapshots_are_not_added_as_generations(self):
        events = [
            {
                "message": {
                    "method": "thread/tokenUsage/updated",
                    "params": {
                        "threadId": "thread",
                        "tokenUsage": {
                            "total": {
                                "inputTokens": count,
                                "cachedInputTokens": 60,
                                "outputTokens": 20,
                                "reasoningOutputTokens": 7,
                            }
                        },
                    },
                }
            }
            for count in (100, 150, 150)
        ]
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events}
        )
        self.assertIsNone(report["tokens"]["inputTokens"])
        self.assertIsNone(report["tokens"]["totalTokens"])
        self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 150)
        self.assertEqual(report["tokens"]["observedTotals"]["totalTokens"], 170)
        self.assertEqual(report["tokens"]["observedTotals"]["visibleOutputTokens"], 13)
        self.assertIsNone(report["tokens"]["promptCategories"])
        self.assertIsNone(report["logicalGenerations"])
        self.assertIsNone(report["runtime"])

    def test_pending_tool_and_model_claim_are_distinct_from_tool_failure(self):
        events = [
            {
                "elapsedMs": 10,
                "message": {
                    "method": "item/started",
                    "params": {
                        "turnId": "t",
                        "item": {
                            "id": "tool",
                            "type": "commandExecution",
                            "status": "inProgress",
                        },
                    },
                },
            },
            {
                "elapsedMs": 20,
                "message": {
                    "method": "item/completed",
                    "params": {
                        "turnId": "t",
                        "item": {
                            "id": "msg",
                            "type": "agentMessage",
                            "text": "I cannot access the tool",
                        },
                    },
                },
            },
        ]
        report = analysis.analyze_runner_evidence(
            {
                "schemaVersion": 1,
                "status": "timeout",
                "elapsedMs": 600000,
                "events": events,
            }
        )
        self.assertEqual(report["pendingTools"][0]["id"], "tool")
        self.assertEqual(report["lastProgress"]["elapsedMs"], 20)
        self.assertEqual(report["symptoms"][0]["source"], "model_claim")
        self.assertFalse(report["symptoms"][0]["causallyEstablished"])
        self.assertEqual(
            [row["kind"] for row in report["failures"]],
            ["timeout", "missing_terminal_event"],
        )
        events.append(
            {
                "elapsedMs": 30,
                "message": {
                    "method": "item/completed",
                    "params": {
                        "turnId": "t",
                        "item": {
                            "id": "tool",
                            "type": "commandExecution",
                            "status": "failed",
                            "exitCode": 1,
                            "aggregatedOutput": "unknown tool",
                        },
                    },
                },
            }
        )
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events}
        )
        self.assertEqual(report["pendingTools"], [])
        self.assertEqual(report["directToolCount"], 1)
        self.assertEqual(report["failures"][0]["kind"], "tool_execution_failure")
        self.assertEqual(report["symptoms"][-1]["source"], "tool_output")

    def test_terminal_error_without_status_is_reported_as_failed(self):
        error = {"message": "provider rejected request"}
        events = [
            {
                "message": {
                    "method": "turn/completed",
                    "params": {"turn": {"id": "t", "error": error}},
                }
            }
        ]
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events}
        )
        self.assertEqual(report["status"], "failed")
        self.assertEqual(report["terminalTurns"], {"t": "failed"})
        self.assertEqual(report["failures"][0]["kind"], "turn_failed")
        self.assertEqual(report["failures"][0]["evidence"], error)
        events[0]["message"]["params"]["turn"]["error"] = None
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events}
        )
        self.assertEqual(report["status"], "completed")
        self.assertEqual(report["failures"], [])

    def test_rg_no_match_is_not_a_tool_failure(self):
        for command, exit_code, output, failed in [
            ('rg -n "needle" src', 1, "", False),
            ('"C:\\tools\\rg.exe" needle src', 1, "", False),
            ('rg "needle|other" src', 1, "", False),
            ('rg "fn\\(" src', 1, "", False),
            ("rg 'foo$|bar' src", 1, "", False),
            ('rg "$(python fail.py)" src', 1, "", True),
            ("rg needle missing-file", 2, "file not found", True),
            ("rg needle src; python fail.py", 1, "", True),
            ("python fail.py", 1, "", True),
            ("rg needle src", 1, "unexpected error", True),
        ]:
            with self.subTest(command=command, exit_code=exit_code):
                events = [
                    {
                        "message": {
                            "method": "item/started",
                            "params": {
                                "turnId": "t",
                                "item": {
                                    "id": "tool",
                                    "type": "commandExecution",
                                    "command": command,
                                },
                            },
                        }
                    },
                    {
                        "message": {
                            "method": "item/completed",
                            "params": {
                                "turnId": "t",
                                "item": {
                                    "id": "tool",
                                    "type": "commandExecution",
                                    "status": "failed",
                                    "exitCode": exit_code,
                                    "aggregatedOutput": output,
                                },
                            },
                        }
                    },
                ]
                report = analysis.analyze_runner_evidence(
                    {"schemaVersion": 1, "events": events}
                )
                self.assertEqual(len(report["failures"]), int(failed))
                self.assertEqual(report["pendingTools"], [])
                self.assertEqual(
                    report["tools"][0].get("outcome"), None if failed else "no_match"
                )

    def test_profile_duplicate_is_not_another_generation(self):
        evidence = self.evidence()
        evidence["events"] *= 2
        report = analysis.analyze_runner_evidence(evidence)
        self.assertEqual(report["logicalGenerations"], 2)
        self.assertIsNone(report["tokens"]["inputTokens"])
        self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 100)
        self.assertEqual(report["coverage"]["nativeTimingProfiles"], 1)

    def test_retries_need_observed_reason_and_unchanged_state(self):
        request = {
            "isContinuation": True,
            "generationPurpose": "repair",
            "generationReason": "compaction",
            "unchangedRelevantState": True,
            "nextStructuredActionChanged": False,
        }
        result = analysis.classify_model_request(
            request,
            classification_complete=True,
            prior_successful_test={"passed": True},
            linked_commands=[{"requiredTest": True}],
        )
        self.assertEqual(result["primary"], "recovery")
        self.assertIsNone(result["interpretation"])
        result = analysis.classify_model_request(
            request,
            classification_complete=True,
            prior_successful_test={"passed": True},
            linked_commands=[{"requiredTest": True}],
            intervening_mutation=False,
        )
        self.assertEqual(result["interpretation"], "redundant_verification")
        result = analysis.classify_model_request(
            {"isContinuation": False, "attemptKind": "retry"},
            classification_complete=True,
            prior_successful_test=None,
            linked_commands=[],
        )
        self.assertEqual(result["primary"], "retry")

    def test_category_estimates_retain_signed_reconciliation(self):
        request = timing_profile()["modelRequests"][0]
        request["requestTokenCategories"] = {
            "accountingBasis": "logical_prompt",
            "baseInstructions": 10,
            "toolSchemas": 20,
            "conversationHistory": 50,
            "currentInput": 20,
            "logicalTotal": 100,
            "localInputEstimate": 97,
            "localReconciliationResidual": -3,
            "providerInputTokens": 100,
            "providerReconciliationResidual": 3,
        }
        report = analysis._token_report([request])
        self.assertEqual(
            report["rankedPromptConsumers"][0]["category"], "conversationHistory"
        )
        self.assertEqual(report["rankedPromptConsumers"][0]["share"], 0.5)
        self.assertEqual(
            report["promptCategoryEvidence"]["localReconciliationResidual"], -3
        )
        self.assertEqual(report["promptCategoryCoverage"], 1)
        self.assertIsNone(analysis._token_report([])["providerTotals"])
        self.assertIsNone(analysis._token_report([])["promptCategories"])

    def test_missing_continuation_is_unknown_but_explicit_false_and_retry_are_observed(
        self,
    ):
        def classify(request):
            return analysis.classify_model_request(
                request,
                classification_complete=True,
                prior_successful_test=None,
                linked_commands=[],
            )

        missing = classify({})
        self.assertEqual(
            (missing["primary"], missing["confidence"]), ("unknown", "unknown")
        )
        self.assertNotIn("isContinuation=false", missing["basis"])
        self.assertEqual(classify({"isContinuation": False})["primary"], "initial")
        self.assertEqual(classify({"attemptKind": "retry"})["primary"], "retry")
        self.assertEqual(
            classify({"generationPurpose": "repair"})["primary"], "recovery"
        )
        self.assertEqual(classify({"isContinuation": "false"})["primary"], "unknown")
        runner = analysis.analyze_runner_evidence(self.evidence())
        self.assertEqual(
            runner["generations"][0]["classification"]["primary"], "unknown"
        )

    def test_invalid_timing_does_not_remove_valid_provider_usage(self):
        valid = timing_profile()
        valid["modelRequests"] = valid["modelRequests"][:1]
        invalid = copy.deepcopy(valid)
        invalid["profileValid"] = False
        invalid["modelRequests"][0]["tokenUsage"]["inputTokens"] = 200
        evidence = self.evidence(valid)
        evidence["events"].append(
            {
                "message": {
                    "method": "turn/completed",
                    "params": {
                        "turn": {
                            "id": "turn-2",
                            "status": "completed",
                            "timing": invalid,
                        }
                    },
                }
            }
        )
        evidence["events"].append(
            {
                "message": {
                    "method": "thread/tokenUsage/updated",
                    "params": {
                        "threadId": "thread",
                        "tokenUsage": {
                            "total": {
                                "inputTokens": 300,
                                "cachedInputTokens": 160,
                                "outputTokens": 30,
                                "reasoningOutputTokens": 10,
                            }
                        },
                    },
                }
            }
        )
        report = analysis.analyze_runner_evidence(evidence)
        self.assertEqual(report["coverage"]["validCompleteTimingProfiles"], 1)
        self.assertEqual(report["runtime"]["population"]["turnIds"], ["turn-1"])
        self.assertEqual(report["tokens"]["inputTokens"], 300)
        self.assertTrue(report["tokens"]["complete"])
        self.assertEqual(report["tokenCoverage"]["requestProfileTurns"], 2)
        self.assertEqual(report["nativeCumulativeTokens"]["inputTokens"], 300)
        self.assertEqual(report["tokenReconciliation"]["residuals"]["inputTokens"], 0)
        self.assertFalse(report["tokenReconciliation"]["addedToRequestTotals"])
        native_usage = evidence["events"][-1]["message"]["params"]["tokenUsage"][
            "total"
        ]
        native_usage["inputTokens"] = 400
        report = analysis.analyze_runner_evidence(evidence)
        self.assertEqual(report["tokens"]["coverage"], 1)
        self.assertTrue(report["tokens"]["complete"])
        self.assertEqual(report["tokens"]["inputTokens"], 300)
        self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 300)
        self.assertEqual(report["tokenReconciliation"]["residuals"]["inputTokens"], 100)
        self.assertEqual(report["tokens"]["providerTotals"]["inputTokens"], 300)
        native_usage["inputTokens"] = 300
        # A missing usage record in the rejected timing profile must remain
        # partial, even though every accepted timing profile has usage.
        invalid["modelRequests"][0].pop("tokenUsage")
        report = analysis.analyze_runner_evidence(evidence)
        self.assertIsNone(report["tokens"]["inputTokens"])
        self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 100)
        self.assertFalse(report["tokens"]["complete"])
        self.assertIsNone(report["tokens"]["providerTotals"])
        self.assertEqual(report["tokens"]["coverage"], 0.5)
        self.assertEqual(report["tokenReconciliation"]["residuals"]["inputTokens"], 200)
        self.assertEqual(report["nativeCumulativeTokens"]["inputTokens"], 300)

    def test_terminal_turn_without_profile_prevents_complete_attempt_usage(self):
        timing = timing_profile()
        timing["modelRequests"] = timing["modelRequests"][:1]
        evidence = self.evidence(timing)
        evidence["events"].append(
            {
                "message": {
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-2", "status": "completed"}},
                }
            }
        )
        report = analysis.analyze_runner_evidence(evidence)
        self.assertEqual(report["tokens"]["coverage"], 1)
        self.assertFalse(report["tokens"]["complete"])
        self.assertEqual(report["tokenCoverage"]["missingTerminalTurnIds"], ["turn-2"])
        self.assertIsNone(report["tokens"]["providerTotals"])
        self.assertIsNone(report["tokens"]["inputTokens"])
        self.assertIsNone(report["tokens"]["cacheShare"])
        self.assertEqual(report["tokens"]["observedTotals"]["inputTokens"], 100)

    def test_output_only_fallback_cannot_claim_zero_input_or_complete_cost(self):
        timing = timing_profile()
        timing["modelRequests"] = [{"outputTokens": 8, "reasoningOutputTokens": 3}]
        report = analysis.analyze_runner_evidence(self.evidence(timing))
        tokens = report["tokens"]
        for key in (
            "inputTokens",
            "outputTokens",
            "totalTokens",
            "billableTokens",
            "blendedTokens",
            "providerTotals",
            "cacheShare",
        ):
            self.assertIsNone(tokens[key], key)
        self.assertEqual(tokens["coverage"], 0)
        self.assertFalse(tokens["complete"])
        self.assertEqual(tokens["observedTotals"]["outputTokens"], 8)
        self.assertEqual(tokens["observedTotals"]["visibleOutputTokens"], 5)
        self.assertEqual(tokens["observedTotals"]["reasoningTokens"], 3)

    def test_request_classification_summary_counts_all_records_before_display_limits(
        self,
    ):
        timing = timing_profile()
        timing["modelRequests"] = (
            [{"isContinuation": False}] * 2
            + [{"attemptKind": "retry"}] * 3
            + [
                {
                    "isContinuation": True,
                    "generationPurpose": "repair",
                    "unchangedRelevantState": True,
                    "nextStructuredActionChanged": False,
                }
            ]
            * 6
            + [{}]
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "evidence.json"
            path.write_text(json.dumps(self.evidence(timing)), encoding="utf-8")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(
                    audit.main(
                        ["--runner-evidence", str(path), "--tokens", "off", "--json"]
                    ),
                    0,
                )
            summary = json.loads(output.getvalue())["runnerDiagnostics"][
                "requestClassification"
            ]
        self.assertEqual(summary["requestRecords"], 12)
        self.assertEqual(
            summary["primaryCounts"],
            {"initial": 2, "retry": 3, "recovery": 6, "unknown": 1},
        )
        self.assertEqual(
            summary["tagCounts"], {"retry": 3, "recovery": 6, "non_progress": 6}
        )
        self.assertEqual(summary["confidenceCounts"], {"observed": 11, "unknown": 1})

    def test_missing_milestones_remain_unknown(self):
        timing = timing_profile()
        timing["schemaVersion"] = 24
        self.assertIsNone(analysis.analyze_timing(timing)["firstUsefulActionMs"])

    def test_invalid_native_profile_cannot_claim_runtime_measurements(self):
        timing = timing_profile()
        timing["profileValid"] = False
        report = analysis.analyze_runner_evidence(self.evidence(timing))
        self.assertIsNone(report["runtime"])
        self.assertIsNone(report["physicalRequests"])
        self.assertEqual(report["coverage"]["nativeTimingProfiles"], 1)
        self.assertEqual(report["coverage"]["validCompleteTimingProfiles"], 0)
        self.assertEqual(report["generations"][0]["eventIndex"], 0)

    def test_native_command_evidence_preserves_separate_pending_calls(self):
        events = [
            {
                "message": {
                    "method": "item/started",
                    "params": {
                        "turnId": "t",
                        "item": {
                            "id": tool,
                            "type": "commandExecution",
                            "command": command,
                        },
                    },
                }
            }
            for tool, command in (
                ("search", "rg duration"),
                ("tests", "cargo test --lib duration"),
            )
        ]
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events}, include_tokens=False
        )
        self.assertEqual(
            {row["id"] for row in report["pendingTools"]}, {"search", "tests"}
        )
        events.append(
            {
                "message": {
                    "method": "item/completed",
                    "params": {
                        "turnId": "t",
                        "item": {
                            "id": "tests",
                            "type": "commandExecution",
                            "exitCode": 0,
                        },
                    },
                }
            }
        )
        report = analysis.analyze_runner_evidence(
            {"schemaVersion": 1, "events": events}, include_tokens=False
        )
        self.assertEqual([row["id"] for row in report["pendingTools"]], ["search"])
        test_call = next(row for row in report["tools"] if row["id"] == "tests")
        self.assertEqual(
            (test_call["command"], test_call["exitCode"]),
            ("cargo test --lib duration", 0),
        )

    def test_nested_lineage_and_cutoff_evidence_survive(self):
        timing = timing_profile()
        timing["toolCalls"].append(
            {
                "callId": "nested",
                "parentCallId": "tool-1",
                "generationIndex": 0,
                "outputTruncated": True,
            }
        )
        report = analysis.analyze_runner_evidence(
            self.evidence(timing), include_tokens=False
        )
        self.assertEqual(report["directToolCount"], 1)
        self.assertEqual(report["nestedToolCount"], 1)
        self.assertEqual(report["generations"][0]["toolCallIds"], ["tool-1", "nested"])
        self.assertEqual(report["nativeToolCalls"][1]["parentCallId"], "tool-1")
        self.assertEqual(report["symptoms"][0]["kind"], "native_output_projection")
        self.assertFalse(report["symptoms"][0]["causallyEstablished"])

    def test_same_turn_and_call_ids_in_distinct_threads_keep_timing_and_usage(self):
        evidence = self.evidence()
        events = []
        for thread, tokens, nested in (("parent", 100, False), ("child", 200, True)):
            event = copy.deepcopy(evidence["events"][0])
            params = event["message"]["params"]
            params["threadId"] = thread
            timing = params["turn"]["timing"]
            timing["modelRequests"] = [
                {
                    "generationIndex": 0,
                    "tokenUsage": {
                        "inputTokens": tokens,
                        "cachedInputTokens": 0,
                        "visibleOutputTokens": 10,
                        "reasoningTokens": 5,
                    },
                }
            ]
            timing["counters"]["modelRequestCount"] = 1
            timing["toolCalls"][0].update(retryCount=1, reentryCount=0)
            if nested:
                timing["toolCalls"][0]["parentCallId"] = "cell"
            events.append(event)
        evidence["events"] = [events[0], events[1], copy.deepcopy(events[1])]
        # Replay through the audit entry point, including bounded report output.
        report = audit.analyze_session_path(None, Path.cwd(), runner_evidence=evidence)
        runner = report["runnerDiagnostics"]
        self.assertEqual(runner["coverage"]["nativeTimingProfiles"], 2)
        self.assertEqual(runner["coverage"]["terminalTurns"], 2)
        self.assertEqual(runner["tokens"]["totalTokens"], 330)
        self.assertTrue(runner["tokens"]["complete"])
        self.assertEqual(runner["toolDispatch"]["retryCount"], 2)
        self.assertEqual(runner["runtime"]["toolRelay"]["generationGroups"], 2)
        self.assertEqual(runner["runtime"]["toolRelay"]["batchGroups"], 0)
        self.assertEqual((runner["directToolCount"], runner["nestedToolCount"]), (1, 1))
        self.assertEqual(
            [row["threadId"] for row in runner["generations"]], ["parent", "child"]
        )
        self.assertEqual(
            [row["threadId"] for row in runner["nativeToolCalls"]], ["parent", "child"]
        )
        self.assertEqual(len(runner["terminalTurns"]), 2)
        self.assertEqual(
            audit.bounded_summary(report)["runnerDiagnostics"]["coverage"][
                "nativeTimingProfiles"
            ],
            2,
        )
        del evidence["events"][1]["message"]["params"]["turn"]["timing"]
        evidence["events"].pop()
        partial = analysis.analyze_runner_evidence(evidence)
        self.assertFalse(partial["tokens"]["complete"])
        self.assertIsNone(partial["tokens"]["totalTokens"])
        self.assertEqual(
            partial["tokenCoverage"]["missingTerminalTurnIds"], ['["child","turn-1"]']
        )

    def test_configuration_hash_uses_captured_values_not_layer_locations(self):
        evidence = self.evidence()
        evidence["effectiveConfig"] = {
            "config": {"model": "test", "example_settings": {"inspect": "low"}},
            "layers": ["first-home"],
        }
        first = audit.analyze_session_path(None, Path.cwd(), runner_evidence=evidence)
        config = first["behaviorMetrics"]["configuration"]
        self.assertRegex(config["sha256"], r"^[0-9a-f]{64}$")
        self.assertEqual(
            audit.bounded_summary(first)["runnerDiagnostics"]["configuration"], config
        )
        evidence["effectiveConfig"] = {
            "layers": ["second-home"],
            "config": {"example_settings": {"inspect": "low"}, "model": "test"},
        }
        self.assertEqual(
            analysis.analyze_runner_evidence(evidence)["configuration"], config
        )
        evidence["effectiveConfig"]["config"]["example_settings"]["inspect"] = (
            "high"
        )
        self.assertNotEqual(
            analysis.analyze_runner_evidence(evidence)["configuration"]["sha256"],
            config["sha256"],
        )
        for missing in (None, {}, {"config": None}):
            evidence["effectiveConfig"] = missing
            self.assertIsNone(
                analysis.analyze_runner_evidence(evidence)["configuration"]["sha256"]
            )


if __name__ == "__main__":
    unittest.main()
