from __future__ import annotations

import contextlib
import io
import json
import os
import tempfile
import unittest
from unittest import mock
from pathlib import Path

from scripts import kd4_turn_latency_audit


def _event(payload: dict, timestamp: str = "2026-08-17T00:00:00Z") -> str:
    return json.dumps({"timestamp": timestamp, "type": "event_msg", "payload": payload})


def _response(payload: dict, timestamp: str) -> str:
    return json.dumps(
        {"timestamp": timestamp, "type": "response_item", "payload": payload}
    )


def _meta(cwd: str) -> str:
    return json.dumps({"type": "session_meta", "payload": {"cwd": cwd}})


def _timing(*, valid: bool = True, complete: bool = True) -> dict:
    return {
        "schemaVersion": 23,
        "profileValid": valid,
        "classificationComplete": complete,
        "startedAtUnixMs": 1_786_924_800_000,
        "completedAtUnixMs": 1_786_924_801_000,
        "inclusiveDurationNs": 1000,
        "milestones": {"firstUsefulActionMs": 12.5},
        "machineDurationNs": 900,
        "exclusive": {
            "modelOnlyNs": 600,
            "toolOnlyNs": 200,
            "modelPlusToolNs": 0,
            "orchestrationNs": 100,
            "interactiveOnlyWaitNs": 100,
        },
        "unions": {
            "modelRequestWaitUnionNs": 10,
            "modelStreamWaitUnionNs": 590,
            "interactiveWaitUnionNs": 150,
        },
        "counters": {
            "logicalGenerationCount": 2,
            "toolCallCount": 1,
            "samePurposeContinuationCount": 1,
            "suppressedDeterministicContinuationCount": 1,
            "exactRepeatedWaitCount": 1,
            "waitOnlyGenerationCount": 1,
            "internallyDrainedWaitCount": 2,
            "noProgressDirectiveCount": 1,
            "provenLoopActivationCount": 1,
            "userInputWaitCount": 1,
        },
        "modelRequests": [
            {
                "generationIndex": 0,
                "attemptKind": "primary",
                "generationPurpose": "deterministic_tool_continuation",
                "modelStreamWaitNs": 300,
                "decisionLatencyNs": 250,
                "toolCallCount": 1,
                "toolActiveUnionNs": 100,
                "unchangedRelevantState": True,
                "nextStructuredActionChanged": False,
                "outputTokens": 15,
                "reasoningOutputTokens": 5,
                "tokenUsage": {
                    "inputTokens": 100,
                    "cachedInputTokens": 80,
                    "visibleOutputTokens": 10,
                    "reasoningTokens": 5,
                    "totalTokens": 115,
                },
                "requestTokenCategories": {
                    "logicalTotal": 95,
                    "localInputEstimate": 100,
                    "repeatedUnchangedContext": 70,
                },
                "dispatchMs": 10,
                "completedMs": 20,
            },
            {
                "generationIndex": 1,
                "attemptKind": "primary",
                "generationPurpose": "implementation",
                "modelStreamWaitNs": 290,
                "decisionLatencyNs": None,
                "unchangedRelevantState": False,
                "nextStructuredActionChanged": True,
                "outputTokens": 10,
                "reasoningOutputTokens": 2,
                "tokenUsage": {
                    "inputTokens": 110,
                    "cachedInputTokens": 100,
                    "visibleOutputTokens": 8,
                    "reasoningTokens": 2,
                    "totalTokens": 120,
                },
                "requestTokenCategories": {
                    "logicalTotal": 105,
                    "localInputEstimate": 110,
                    "repeatedUnchangedContext": 80,
                },
                "dispatchMs": 30,
                "completedMs": 40,
            },
        ],
        "toolCalls": [
            {
                "callId": "relay-1",
                "toolName": "shell_command",
                "source": "direct",
                "generationIndex": 0,
                "acceptedAtMs": 1,
                "firstPollAtMs": 2,
                "parallelGateAdmittedAtMs": 3,
                "handlerEntryAtMs": 4,
                "handlerExitAtMs": 7,
                "processSpawnedAtMs": 5,
                "processExitedAtMs": 6,
                "outputCollectedAtMs": 8,
                "deliveredAtMs": 9,
                "outputModelVisibleAtMs": 9,
                "modelResumedAtMs": 10,
                "itemToFirstPollMs": 1,
                "parallelGateWaitMs": 1,
                "preToolHookMs": 0,
                "postToolHookMs": 0,
                "workspaceEvidenceBeforeMs": 0,
                "workspaceEvidenceAfterMs": 0,
                "authorizationStateCoordinationMs": 0,
                "handlerDurationMs": 3,
                "postHandlerMs": 2,
                "totalDurationMs": 7,
                "eager": True,
                "processAliveAtDelivery": False,
            }
        ],
        "toolCallTimingOverflow": 2,
        "observationalNonprogressTokens": {
            "logicalGenerations": 1,
            "inputTokens": 100,
            "cachedInputTokens": 80,
            "visibleOutputTokens": 10,
            "reasoningTokens": 5,
            "totalTokens": 115,
        },
        "observationalNonprogressLatency": {
            "logicalGenerations": 1,
            "physicalAttempts": 1,
            "modelStreamWaitNs": 300,
            "decisionReadyAttempts": 1,
            "decisionLatencyNs": 250,
            "toolCalls": 1,
            "toolActiveUnionNs": 100,
        },
    }


class Kd4TurnLatencyAuditTest(unittest.TestCase):
    def test_empty_token_report_keeps_the_per_turn_schema_complete(self) -> None:
        tokens = kd4_turn_latency_audit._token_report([])

        self.assertTrue(tokens["complete"])
        self.assertEqual(tokens["inputTokens"], 0)
        self.assertEqual(tokens["cachedInputTokens"], 0)
        self.assertEqual(tokens["outputTokens"], 0)
        self.assertEqual(tokens["billableTokens"], 0)

    def test_token_report_marks_internal_provider_retries_as_partial(self) -> None:
        request = dict(_timing()["modelRequests"][0])
        request["physicalAttemptIds"] = ["attempt-1", "attempt-2", "attempt-2"]

        tokens = kd4_turn_latency_audit._token_report([request])

        self.assertEqual(tokens["physicalAttempts"], 2)
        self.assertEqual(tokens["providerUsageAttempts"], 1)
        self.assertEqual(tokens["coverage"], 0.5)
        self.assertFalse(tokens["complete"])
        self.assertIsNone(tokens["billableTokens"])
        self.assertEqual(tokens["observedBillableTokens"], 115)
        self.assertEqual(tokens["observedBlendedTokens"], 35)

    def test_token_intervals_group_retry_attempts_without_duplicating_tool_batches(
        self,
    ) -> None:
        timing = _timing()
        retry = dict(timing["modelRequests"][0])
        retry["attemptKind"] = "retry"
        retry["dispatchMs"] = 15
        retry["completedMs"] = 19
        retry["tokenUsage"] = {
            "inputTokens": 40,
            "cachedInputTokens": 30,
            "visibleOutputTokens": 2,
            "reasoningTokens": 1,
            "totalTokens": 43,
        }
        requests = [timing["modelRequests"][0], retry, timing["modelRequests"][1]]

        intervals = kd4_turn_latency_audit._token_intervals(
            requests, timing["toolCalls"]
        )

        self.assertEqual(len(intervals), 2)
        self.assertEqual(intervals[0]["requestIndexes"], [0, 1])
        self.assertEqual(intervals[0]["attemptKinds"], ["primary", "retry"])
        self.assertEqual(intervals[0]["physicalAttempts"], 2)
        self.assertEqual(intervals[0]["emittedToolCallIds"], ["relay-1"])
        self.assertEqual(intervals[0]["tokens"]["inputTokens"], 140)
        self.assertEqual(intervals[1]["precedingToolCallIds"], ["relay-1"])

    def test_tool_relay_slow_call_includes_pre_poll_queue_time(self) -> None:
        call = {
            "callId": "queued",
            "toolName": "shell_command",
            "acceptedAtMs": 0,
            "firstPollAtMs": 6_000,
            "outputCollectedAtMs": 10_000,
            "deliveredAtMs": 10_001,
            "outputModelVisibleAtMs": 10_501,
            "itemToFirstPollMs": 6_000,
            "totalDurationMs": 4_001,
        }

        relay = kd4_turn_latency_audit._tool_relay_report([call])

        self.assertEqual(relay["slowCallCount"], 1)
        self.assertEqual(relay["topSlowCalls"][0]["totalDurationMs"], 4_001)
        self.assertEqual(relay["topSlowCalls"][0]["endToEndDurationMs"], 10_501)
        self.assertTrue(relay["topSlowCalls"][0]["outputModelVisibilityRecorded"])

    def test_tool_relay_reports_process_exit_after_live_handle_delivery(self) -> None:
        call = {
            "callId": "background-relay",
            "acceptedAtMs": 1,
            "processSpawnedAtMs": 5,
            "outputCollectedAtMs": 8,
            "deliveredAtMs": 9,
            "processExitedAtMs": 20,
            "processAliveAtDelivery": True,
        }

        relay = kd4_turn_latency_audit._tool_relay_report([call])

        self.assertEqual(relay["processAliveAtDeliveryCalls"], 1)
        self.assertEqual(relay["phaseTotalsMs"]["modelVisibleToProcessExitMs"], 11)

    def test_reports_exclusive_gate_convoy_between_parallel_nested_reads(self) -> None:
        calls = [
            {
                "_turnId": "convoy",
                "callId": "read-a",
                "toolName": "exec_command",
                "source": "code_mode",
                "generationIndex": 4,
                "acceptedAtMs": 0,
                "firstPollAtMs": 1,
                "parallelGateAdmittedAtMs": 1,
                "handlerEntryAtMs": 2,
                "processSpawnedAtMs": 3,
                "processExitedAtMs": 113_900,
                "outputCollectedAtMs": 113_950,
                "outputModelVisibleAtMs": 114_000,
                "parallelGateWaitMs": 0,
                "totalDurationMs": 114_000,
            },
            {
                "_turnId": "convoy",
                "callId": "read-b",
                "toolName": "exec_command",
                "source": "code_mode",
                "generationIndex": 4,
                "acceptedAtMs": 1,
                "firstPollAtMs": 1,
                "parallelGateAdmittedAtMs": 114_001,
                "handlerEntryAtMs": 114_002,
                "processSpawnedAtMs": 114_003,
                "processExitedAtMs": 114_045,
                "outputCollectedAtMs": 114_050,
                "outputModelVisibleAtMs": 114_051,
                "parallelGateWaitMs": 114_000,
                "totalDurationMs": 114_050,
            },
        ]

        relay = kd4_turn_latency_audit._tool_relay_report(calls)

        self.assertEqual(relay["batchGroups"], 1)
        self.assertEqual(relay["batchedCalls"], 2)
        self.assertEqual(relay["phaseTotalsMs"]["parallelGateWaitMs"], 114_000)
        self.assertEqual(relay["dominantPhase"], "parallelGateWaitMs")
        self.assertEqual(relay["dominantPhaseOwner"], "ExclusiveGate")
        self.assertEqual(relay["exclusiveGateConvoyCount"], 1)
        self.assertEqual(
            relay["topExclusiveGateConvoys"][0],
            {
                "turnId": "convoy",
                "generationIndex": 4,
                "callIds": ["read-a", "read-b"],
                "waitingCallIds": ["read-b"],
                "parallelGateWaitMs": 114_000,
            },
        )

    def test_reports_post_tool_hook_as_owner_of_post_process_stall(self) -> None:
        call = {
            "_turnId": "post-hook",
            "callId": "exec-a",
            "toolName": "exec_command",
            "source": "direct",
            "generationIndex": 0,
            "acceptedAtMs": 0,
            "firstPollAtMs": 1,
            "parallelGateAdmittedAtMs": 1,
            "handlerEntryAtMs": 2,
            "processSpawnedAtMs": 3,
            "processExitedAtMs": 48,
            "outputCollectedAtMs": 99_049,
            "outputModelVisibleAtMs": 99_050,
            "modelResumedAtMs": 99_051,
            "parallelGateWaitMs": 0,
            "postToolHookMs": 99_000,
            "totalDurationMs": 99_050,
        }

        relay = kd4_turn_latency_audit._tool_relay_report([call])

        self.assertEqual(relay["phaseTotalsMs"]["processRuntimeMs"], 45)
        self.assertEqual(relay["phaseTotalsMs"]["postToolHookMs"], 99_000)
        self.assertEqual(relay["dominantPhase"], "postToolHookMs")
        self.assertEqual(relay["dominantPhaseOwner"], "PostToolUse")
        self.assertEqual(relay["dominantPhaseMs"], 99_000)

    def test_uuid_cli_resolves_snapshot_and_emits_bounded_execution_loop(self) -> None:
        session_id = "01a018c7-a357-7c11-a7ca-9248dd075f22"
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            codex_home = Path(temp) / "codex-home"
            sessions = codex_home / "sessions" / "2026" / "08" / "19"
            root.mkdir()
            sessions.mkdir(parents=True)
            rollout = sessions / f"rollout-2026-08-19T01-48-56-{session_id}.jsonl"
            rollout.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event(
                            {"type": "task_started", "turn_id": "turn"},
                            "2026-08-17T00:00:00Z",
                        ),
                        json.dumps(
                            {
                                "timestamp": "2026-08-17T00:00:01Z",
                                "type": "sampling_boundary",
                                "payload": {"turn_id": "turn"},
                            }
                        ),
                        _response(
                            {"type": "custom_tool_call", "call_id": "call-1"},
                            "2026-08-17T00:00:03Z",
                        ),
                        _response(
                            {
                                "type": "custom_tool_call_output",
                                "call_id": "call-1",
                                "output": "done",
                            },
                            "2026-08-17T00:00:05Z",
                        ),
                        json.dumps(
                            {
                                "timestamp": "2026-08-17T00:00:06Z",
                                "type": "sampling_boundary",
                                "payload": {"turn_id": "turn"},
                            }
                        ),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "turn",
                                "timing": _timing(),
                            },
                            "2026-08-17T00:00:10Z",
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            stdout = io.StringIO()
            with (
                mock.patch.dict(os.environ, {"CODEX_HOME": str(codex_home)}),
                contextlib.redirect_stdout(stdout),
            ):
                exit_code = kd4_turn_latency_audit.main(
                    [session_id, "--repo-root", str(root), "--summary-json"]
                )

        output = stdout.getvalue()
        report = json.loads(output)
        self.assertEqual(exit_code, 0)
        self.assertLess(len(output.encode("utf-8")), 16 * 1024)
        self.assertEqual(report["source"], str(rollout.resolve()))
        self.assertEqual(
            report["coverage"]["snapshots"][0]["path"], str(rollout.resolve())
        )
        self.assertEqual(report["executionLoop"]["samplingPasses"], 2)
        self.assertEqual(report["executionLoop"]["toolCalls"], 1)
        self.assertEqual(report["executionLoop"]["pairedToolCalls"], 1)
        self.assertEqual(
            report["executionLoop"]["samplingToFirstToolCallNs"], 2_000_000_000
        )
        self.assertEqual(
            report["executionLoop"]["pairedToolRoundTripNs"], 2_000_000_000
        )
        self.assertEqual(report["executionLoop"]["postToolHandoffNs"], 1_000_000_000)
        self.assertEqual(report["executionLoop"]["taskElapsedNs"], 10_000_000_000)
        self.assertEqual(report["toolRelay"]["calls"], 1)
        self.assertEqual(report["toolRelay"]["eagerCalls"], 1)
        self.assertEqual(report["toolRelay"]["timingOverflowCalls"], 2)
        self.assertEqual(
            report["toolRelay"]["phaseTotalsMs"]["requestToProcessSpawnMs"], 4
        )
        self.assertEqual(report["toolRelay"]["phaseTotalsMs"]["processRuntimeMs"], 1)
        self.assertEqual(
            report["toolRelay"]["phaseTotalsMs"]["processExitToModelVisibleMs"], 3
        )
        self.assertEqual(report["toolRelay"]["phaseTotalsMs"]["endToEndDurationMs"], 8)
        self.assertEqual(report["toolRelay"]["topSlowCalls"], [])
        self.assertEqual(report["perTurn"][0]["agentActiveDurationNs"], 900)
        self.assertEqual(report["perTurn"][0]["firstUsefulActionMs"], 12.5)
        self.assertEqual(report["perTurn"][0]["humanWaitNs"], 100)
        self.assertEqual(report["perTurn"][0]["humanOnlyWaitNs"], 100)
        self.assertEqual(report["perTurn"][0]["humanWaitUnionNs"], 150)
        self.assertEqual(
            report["perTurn"][0]["humanWaitCounts"]["userInputWaitCount"], 1
        )
        self.assertEqual(report["perTurn"][0]["tokens"]["inputTokens"], 210)
        self.assertEqual(report["perTurn"][0]["tokens"]["cachedInputTokens"], 180)
        self.assertEqual(report["perTurn"][0]["tokens"]["outputTokens"], 25)
        self.assertEqual(report["perTurn"][0]["tokens"]["reasoningTokens"], 7)
        self.assertEqual(report["perTurn"][0]["tokens"]["billableTokens"], 235)
        self.assertEqual(report["perTurn"][0]["tokens"]["blendedTokens"], 55)
        self.assertEqual(
            report["perTurn"][0]["tokens"]["promptCategories"][
                "repeatedUnchangedContext"
            ],
            150,
        )
        self.assertEqual(
            report["perTurn"][0]["observationalNonprogressTokens"]["totalTokens"],
            115,
        )
        self.assertEqual(report["populations"]["all"]["modelShare"], 2 / 3)
        self.assertEqual(
            report["coverage"]["terminalLifecycleStateCounts"], {"completed": 1}
        )
        self.assertEqual(report["perTurn"][0]["samplingPasses"], 2)
        self.assertEqual(report["perTurn"][0]["samplingPassTarget"], 8)
        self.assertEqual(report["perTurn"][0]["startedAt"], "2026-08-17T00:00:00+00:00")
        self.assertEqual(report["perTurn"][0]["boundarySource"], "timing")
        intervals = report["perTurn"][0]["tokenIntervals"]
        self.assertEqual(len(intervals), 2)
        self.assertEqual(intervals[0]["emittedToolCallIds"], ["relay-1"])
        self.assertEqual(intervals[1]["precedingToolCallIds"], ["relay-1"])
        self.assertEqual(intervals[1]["tokens"]["inputTokens"], 110)
        summary = kd4_turn_latency_audit.bounded_summary(report)
        self.assertEqual(len(summary["perTurn"][0]["tokenIntervals"]), 2)
        self.assertEqual(report["firstUsefulActionAnalysis"]["canonicalTurnCount"], 1)
        self.assertIn("firstUsefulActionAnalysis", summary)
        self.assertNotIn("measurementContract", summary["firstUsefulActionAnalysis"])
        self.assertNotIn("sourceSnapshots", summary["firstUsefulActionAnalysis"])
        self.assertIn(
            "boundary=2026-08-17", kd4_turn_latency_audit.render_report(report)
        )
        self.assertEqual(report["behaviorSignals"]["turnsOverSamplingPassTarget"], 0)

    def test_waiting_input_tail_is_classified_without_blocking_completed_audit(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            timing = _timing()
            second_relay = dict(timing["toolCalls"][0])
            second_relay["callId"] = "relay-2"
            timing["toolCalls"].append(second_relay)
            session.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event({"type": "task_started", "turn_id": "complete"}),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "complete",
                                "timing": timing,
                            }
                        ),
                        _event({"type": "task_started", "turn_id": "active"}),
                        _response(
                            {
                                "type": "custom_tool_call",
                                "call_id": "wait-for-user",
                                "name": "exec",
                                "input": "await tools.request_user_input({questions: []})",
                            },
                            "2026-08-17T00:00:01Z",
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(session, root)

        self.assertEqual(report["coverage"]["startedTurnsWithoutTerminal"], 1)
        self.assertEqual(report["behaviorSignals"]["activeTurnsExcluded"], 1)
        self.assertEqual(report["coverage"]["openTurnStateCounts"], {"user_waiting": 1})
        self.assertEqual(report["coverage"]["openTurns"][0]["state"], "user_waiting")
        self.assertEqual(report["toolRelay"]["batchGroups"], 1)
        self.assertEqual(report["toolRelay"]["batchedCalls"], 2)
        self.assertTrue(report["auditDecision"]["readyToFinalize"])
        self.assertIn("active_tail_excluded", report["auditDecision"]["reasonCodes"])
        self.assertIn("open_turn_user_waiting", report["auditDecision"]["reasonCodes"])

    def test_terminal_and_open_turn_states_are_explicit(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            session.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event({"type": "task_started", "turn_id": "failed"}),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "failed",
                                "error": {"message": "failed"},
                                "timing": _timing(),
                            }
                        ),
                        _event({"type": "task_started", "turn_id": "canceled"}),
                        _event(
                            {
                                "type": "turn_aborted",
                                "turn_id": "canceled",
                                "reason": "interrupted",
                                "timing": _timing(),
                            }
                        ),
                        _event({"type": "task_started", "turn_id": "abandoned"}),
                        _event(
                            {
                                "type": "turn_aborted",
                                "turn_id": "abandoned",
                                "reason": "replaced",
                                "timing": _timing(),
                            }
                        ),
                        _event({"type": "task_started", "turn_id": "running"}),
                        _response(
                            {
                                "type": "function_call",
                                "call_id": "running-process",
                                "name": "shell_command",
                                "arguments": "{}",
                            },
                            "2026-08-17T00:00:01Z",
                        ),
                        _event({"type": "task_started", "turn_id": "leaked"}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(session, root)

        self.assertEqual(
            report["coverage"]["terminalLifecycleStateCounts"],
            {"abandoned": 1, "canceled": 1, "failed": 1},
        )
        self.assertEqual(
            report["coverage"]["openTurnStateCounts"],
            {"active_without_pending_tool": 1, "unresolved_tool_call": 1},
        )
        self.assertEqual(report["behaviorSignals"]["canceledTurns"], 1)
        self.assertEqual(report["behaviorSignals"]["failedTurns"], 1)
        self.assertEqual(report["behaviorSignals"]["abandonedTurns"], 1)
        self.assertEqual(report["behaviorSignals"]["unresolvedToolCallTurns"], 1)
        self.assertEqual(report["behaviorSignals"]["activeWithoutPendingToolTurns"], 1)

    def test_terminal_turn_with_unresolved_tool_call_is_inconsistent(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            session.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event(
                            {"type": "task_started", "turn_id": "inconsistent"},
                            "2026-08-17T00:00:00Z",
                        ),
                        _response(
                            {
                                "type": "function_call",
                                "call_id": "wait-1",
                                "name": "wait",
                                "arguments": "{}",
                            },
                            "2026-08-17T00:00:01Z",
                        ),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "inconsistent",
                                "timing": _timing(),
                            },
                            "2026-08-17T00:00:02Z",
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(session, root)

        self.assertEqual(report["executionLoop"]["unpairedToolCalls"], 1)
        self.assertEqual(report["coverage"]["terminalTurnsWithUnresolvedToolCalls"], 1)
        violation = report["coverage"]["terminalTurnInvariantViolations"][0]
        self.assertEqual(violation["turnId"], "inconsistent")
        self.assertEqual(violation["pendingTools"], ["wait"])
        self.assertIn(
            "terminal_with_unresolved_tool_call", report["perTurn"][0]["signals"]
        )
        self.assertEqual(
            report["behaviorSignals"]["terminalTurnsWithUnresolvedToolCalls"], 1
        )
        self.assertIn(
            "terminal_turn_with_unresolved_tool_call",
            report["auditDecision"]["blockerCodes"],
        )
        self.assertFalse(report["auditDecision"]["readyToFinalize"])

    def test_blocked_task_complete_is_not_successful_completion(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            session.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event({"type": "task_started", "turn_id": "blocked"}),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "blocked",
                                "completion": {
                                    "status": "blocked",
                                    "reasons": ["unresolved tool call"],
                                },
                                "timing": _timing(),
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(session, root)

        self.assertEqual(
            report["coverage"]["terminalLifecycleStateCounts"], {"blocked": 1}
        )
        self.assertEqual(report["perTurn"][0]["lifecycle"], "blocked")
        self.assertEqual(report["behaviorSignals"]["blockedTurns"], 1)
        self.assertEqual(report["behaviorSignals"]["failedTurns"], 0)

    def test_emits_bounded_slow_calls_and_finalize_decision(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            lines = [
                _meta(str(root)),
                _event(
                    {"type": "task_started", "turn_id": "slow-audit"},
                    "2026-08-17T00:00:00Z",
                ),
            ]
            for ordinal in range(10):
                started_second = ordinal * 5
                completed_second = started_second + 6
                lines.extend(
                    [
                        _response(
                            {
                                "type": "custom_tool_call",
                                "call_id": f"call-{ordinal}",
                                "name": "exec",
                                "input": (
                                    "const result = await tools.exec_command({"
                                    'command: "private command"});'
                                ),
                            },
                            f"2026-08-17T00:00:{started_second:02d}Z",
                        ),
                        _response(
                            {
                                "type": "custom_tool_call_output",
                                "call_id": f"call-{ordinal}",
                                "output": [
                                    {
                                        "type": "input_text",
                                        "text": (
                                            "Script completed\n"
                                            "Wall time 6.0 seconds\n"
                                            "Output:\nprivate output"
                                        ),
                                    }
                                ],
                            },
                            f"2026-08-17T00:00:{completed_second:02d}Z",
                        ),
                    ]
                )
            timing = _timing()
            timing["inclusiveDurationNs"] = 100_000_000_000
            timing["machineDurationNs"] = 100_000_000_000
            timing["exclusive"] = {
                "orchestrationNs": 70_000_000_000,
                "modelOnlyNs": 20_000_000_000,
                "toolOnlyNs": 10_000_000_000,
                "modelPlusToolNs": 0,
            }
            lines.append(
                _event(
                    {
                        "type": "task_complete",
                        "turn_id": "slow-audit",
                        "timing": timing,
                    },
                    "2026-08-17T00:00:59Z",
                )
            )
            session.write_text("\n".join(lines) + "\n", encoding="utf-8")

            report = kd4_turn_latency_audit.analyze_session_path(session, root)

        orchestration = report["commandOrchestration"]
        self.assertEqual(orchestration["slowToolCallCount"], 10)
        self.assertEqual(len(orchestration["topSlowToolCalls"]), 8)
        self.assertEqual(orchestration["omittedSlowToolCalls"], 2)
        self.assertEqual(
            orchestration["topSlowToolCalls"][0]["tool"], "exec>exec_command"
        )
        self.assertEqual(
            orchestration["topSlowToolCalls"][0]["reportedExecWallNs"],
            6_000_000_000,
        )
        self.assertNotIn("private command", json.dumps(report))
        self.assertNotIn("private output", json.dumps(report))
        self.assertEqual(report["auditDecision"]["dominantPhase"], "orchestration")
        self.assertTrue(report["auditDecision"]["readyToFinalize"])
        rendered = kd4_turn_latency_audit.render_report(report)
        self.assertIn("audit decision: finalize", rendered)
        self.assertIn("Stop rollout inspection and answer from this report.", rendered)

    def test_separates_reported_child_runtime_from_tool_orchestration_gap(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            session.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _response(
                            {"type": "custom_tool_call", "call_id": "call-1"},
                            "2026-08-17T00:00:00Z",
                        ),
                        _response(
                            {
                                "type": "custom_tool_call_output",
                                "call_id": "call-1",
                                "output": [
                                    {"type": "input_text", "text": "Script completed"},
                                    {
                                        "type": "input_text",
                                        "text": (
                                            '{"wall_time_seconds":0.25}'
                                            '{"wall_time_seconds":0.75}'
                                        ),
                                    },
                                ],
                            },
                            "2026-08-17T00:00:03Z",
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(session, root)

        orchestration = report["commandOrchestration"]
        self.assertEqual(orchestration["pairedToolCalls"], 1)
        self.assertEqual(orchestration["reportedChildRuntimeCalls"], 1)
        self.assertEqual(orchestration["reportedChildCalls"], 2)
        self.assertEqual(orchestration["parallelBatches"], 1)
        self.assertEqual(orchestration["roundTripNs"], 3_000_000_000)
        self.assertEqual(orchestration["reportedChildWorkNs"], 1_000_000_000)
        self.assertEqual(orchestration["orchestrationGapLowerBoundNs"], 2_000_000_000)
        self.assertEqual(orchestration["orchestrationGapUpperBoundNs"], 2_250_000_000)
        self.assertEqual(orchestration["orchestrationShareLowerBound"], 2 / 3)
        self.assertEqual(orchestration["orchestrationShareUpperBound"], 3 / 4)
        self.assertEqual(
            orchestration["evidenceSource"], "responseOutputWallTimeFallback"
        )
        rendered = kd4_turn_latency_audit.render_report(report)
        self.assertIn("round-trip=3.0s child-work=1.0s gap=2.0-2.2s", rendered)

    def test_prefers_detailed_tool_call_timing_over_output_wall_time(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "call-1",
                "toolName": "exec_command",
                "source": "direct",
                "generationIndex": 0,
                "acceptedAtMs": 0,
                "firstPollAtMs": 1,
                "parallelGateAdmittedAtMs": 2,
                "handlerEntryAtMs": 3,
                "processSpawnedAtMs": 50,
                "processExitedAtMs": 95,
                "outputCollectedAtMs": 2_999,
                "outputModelVisibleAtMs": 3_000,
                "postToolHookMs": 2_900,
                "totalDurationMs": 3_000,
            }
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            session.write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event({"type": "task_started", "turn_id": "timed"}),
                        _response(
                            {"type": "custom_tool_call", "call_id": "call-1"},
                            "2026-08-17T00:00:00Z",
                        ),
                        _response(
                            {
                                "type": "custom_tool_call_output",
                                "call_id": "call-1",
                                "output": [
                                    {
                                        "type": "input_text",
                                        "text": '{"wall_time_seconds":1.0}',
                                    }
                                ],
                            },
                            "2026-08-17T00:00:10Z",
                        ),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "timed",
                                "timing": timing,
                            },
                            "2026-08-17T00:00:11Z",
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(session, root)

        orchestration = report["commandOrchestration"]
        self.assertEqual(orchestration["evidenceSource"], "toolCalls")
        self.assertEqual(orchestration["roundTripNs"], 3_000_000_000)
        self.assertEqual(orchestration["reportedChildWorkNs"], 45_000_000)
        self.assertEqual(orchestration["orchestrationGapLowerBoundNs"], 2_955_000_000)
        self.assertEqual(orchestration["orchestrationGapUpperBoundNs"], 2_955_000_000)

    def test_reports_coverage_and_segments_eval_from_repository_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            sessions = Path(temp) / "sessions"
            sessions.mkdir()
            root.mkdir()
            (sessions / "root.jsonl").write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event({"type": "task_started", "turn_id": "root"}),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "root",
                                "timing": _timing(),
                            }
                        ),
                        _event({"type": "task_started", "turn_id": "pending"}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            eval_cwd = root / ".codex" / "evals" / "run" / "work"
            eval_lines = [
                _meta(str(eval_cwd)),
                _event({"type": "task_started", "turn_id": "eval"}),
                _event(
                    {"type": "task_complete", "turn_id": "eval", "timing": _timing()}
                ),
            ]
            eval_lines.extend(["not-json"] * 101)
            (sessions / "eval.jsonl").write_text(
                "\n".join(eval_lines) + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(sessions, root)

        coverage = report["coverage"]
        self.assertEqual(coverage["files"], 2)
        self.assertEqual(coverage["parseErrorCount"], 101)
        self.assertEqual(len(coverage["parseErrors"]), 100)
        self.assertEqual(coverage["uniqueTaskStarts"], 3)
        self.assertEqual(coverage["validCompleteProfiles"], 2)
        self.assertEqual(coverage["startedTurnsWithoutTerminal"], 1)
        self.assertEqual(report["populations"]["eval"]["turns"], 1)
        self.assertEqual(report["populations"]["repository_root"]["turns"], 1)
        all_population = report["populations"]["all"]
        self.assertEqual(all_population["modelOnlyNs"], 1200)
        self.assertEqual(all_population["toolOnlyNs"], 400)
        self.assertEqual(all_population["decisionLatency"]["decisionReadyAttempts"], 2)
        self.assertEqual(
            all_population["observationalNonprogressLatency"]["modelStreamWaitNs"],
            600,
        )

    def test_excludes_invalid_profiles_and_falls_back_for_schema_14(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            sessions = Path(temp) / "sessions"
            sessions.mkdir()
            root.mkdir()
            historical = _timing()
            historical["schemaVersion"] = 14
            historical.pop("observationalNonprogressLatency")
            historical["modelRequests"].append(
                {
                    "generationIndex": 0,
                    "attemptKind": "retry",
                    "modelStreamWaitNs": 50,
                    "decisionLatencyNs": 40,
                }
            )
            (sessions / "rollout.jsonl").write_text(
                "\n".join(
                    [
                        _meta(str(root)),
                        _event({"type": "task_started", "turn_id": "historical"}),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": "historical",
                                "timing": historical,
                            }
                        ),
                        _event({"type": "task_started", "turn_id": "invalid"}),
                        _event(
                            {
                                "type": "turn_aborted",
                                "turn_id": "invalid",
                                "timing": _timing(valid=False),
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = kd4_turn_latency_audit.analyze_session_path(sessions, root)

        coverage = report["coverage"]
        self.assertEqual(coverage["uniqueTimedTerminalTurns"], 2)
        self.assertEqual(coverage["validCompleteProfiles"], 1)
        self.assertEqual(coverage["invalidProfiles"], 1)
        population = report["populations"]["all"]
        self.assertEqual(population["turns"], 1)
        self.assertEqual(
            population["observationalNonprogressLatency"]["logicalGenerations"],
            1,
        )
        self.assertEqual(
            population["observationalNonprogressLatency"]["decisionLatencyNs"],
            290,
        )
        self.assertEqual(
            population["observationalNonprogressLatency"]["physicalAttempts"],
            2,
        )
        self.assertFalse(population["tokens"]["complete"])
        self.assertIsNone(population["tokens"]["billableTokens"])
        self.assertEqual(population["tokens"]["providerUsageAttempts"], 2)
        self.assertEqual(population["tokens"]["physicalAttempts"], 3)


if __name__ == "__main__":
    unittest.main()
