from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
AUDIT_SCRIPT = REPO_ROOT / "scripts" / "kd4_turn_latency_audit.py"


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
        "schemaVersion": 25,
        "profileValid": valid,
        "classificationComplete": complete,
        "startedAtUnixMs": 1_786_924_800_000,
        "completedAtUnixMs": 1_786_924_801_000,
        "inclusiveDurationNs": 1000,
        "milestones": {
            "firstUsefulActionMs": 12.5,
            "firstInfrastructureActionMs": 2.5,
            "firstToolDiscoveryActionMs": 7.5,
            "firstDomainActionMs": 12.5,
            "firstSuccessfulDomainActionMs": 15.0,
        },
        "machineDurationNs": 900,
        "exclusive": {
            "modelOnlyNs": 600,
            "toolOnlyNs": 200,
            "modelPlusToolNs": 0,
            "orchestrationNs": 100,
            "interactiveOnlyWaitNs": 100,
        },
        "unions": {
            "modelActiveUnionNs": 600,
            "modelRequestWaitUnionNs": 10,
            "modelStreamWaitUnionNs": 590,
            "modelStreamProcessingUnionNs": 20,
            "interactiveWaitUnionNs": 150,
        },
        "local": {
            "preparationUnionNs": 11,
            "planningUnionNs": 23,
            "planningExclusiveUnionNs": 17,
            "planningCompactionOverlapUnionNs": 6,
            "compactionUnionNs": 9,
            "persistenceUnionNs": 13,
            "serializationUnionNs": 7,
            "routerBuildUnionNs": 5,
            "startupPrewarmWaitUnionNs": 3,
            "executorReadinessWaitUnionNs": 2,
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
        "toolCallTimingOverflow": 0,
        "preFirstModelOutput": {
            "clientCriticalPathNs": 80,
            "attributedClientUnionNs": 65,
            "unattributedPreOutputNs": 15,
            "historySnapshotNs": 10,
            "normalizationNs": 8,
            "promptConstructionNs": 20,
            "requestTransformationNs": 7,
            "serializationNs": 5,
            "transportReadinessNs": 15,
        },
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


def _write_rollout(path: Path, lines: list[str]) -> None:
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _run_cli(
    source: str | Path,
    repo_root: Path,
    output_flag: str = "--json",
    *,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    command = [
        sys.executable,
        str(AUDIT_SCRIPT),
        str(source),
        "--repo-root",
        str(repo_root),
    ]
    if output_flag:
        command.append(output_flag)
    completed = subprocess.run(
        command,
        cwd=REPO_ROOT,
        env={**os.environ, **(env or {})},
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0:
        raise AssertionError(
            f"audit CLI failed ({completed.returncode})\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed


def _json_report(source: str | Path, repo_root: Path) -> dict:
    return json.loads(_run_cli(source, repo_root).stdout)


def _single_turn_lines(root: Path, timing: dict, turn_id: str = "turn") -> list[str]:
    return [
        _meta(str(root)),
        _event({"type": "task_started", "turn_id": turn_id}),
        _event({"type": "task_complete", "turn_id": turn_id, "timing": timing}),
    ]


class Kd4TurnLatencyAuditCliIntegrationTest(unittest.TestCase):
    def test_cli_population_omits_retired_validation_counters(self) -> None:
        timing = _timing()
        timing["counters"].update(
            {
                "executedValidationCount": 2,
                "reusedValidationCount": 3,
                "duplicateValidationCount": 4,
                "forcedFreshValidationCount": 5,
            }
        )
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        population = report["populations"]["all"]
        self.assertEqual(population["executedValidationCount"], 2)
        for retired in (
            "reusedValidationCount",
            "duplicateValidationCount",
            "forcedFreshValidationCount",
        ):
            self.assertNotIn(retired, population)

    def test_cli_orchestration_evidence_counts_turns_not_slow_calls(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            lines = [_meta(str(root))]
            for turn_id, orchestration_ns in (("first", 700), ("second", 800)):
                timing = _timing()
                timing["inclusiveDurationNs"] = 1000
                timing["machineDurationNs"] = 1000
                timing["exclusive"] = {
                    "orchestrationNs": orchestration_ns,
                    "modelOnlyNs": 1000 - orchestration_ns,
                }
                lines.extend(
                    [
                        _event({"type": "task_started", "turn_id": turn_id}),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": turn_id,
                                "timing": timing,
                            }
                        ),
                    ]
                )
            _write_rollout(session, lines)
            report = _json_report(session, root)

        population = report["populations"]["all"]
        decision = report["auditDecision"]
        self.assertEqual(population["orchestrationMajorityTurns"], 2)
        self.assertEqual(decision["dominantPhase"], "orchestration")
        self.assertIn("repeated_orchestration_majority_turns", decision["reasonCodes"])
        self.assertNotIn("limited_representative_evidence", decision["reasonCodes"])

    def test_cli_empty_requests_keep_per_turn_token_schema_complete(self) -> None:
        timing = _timing()
        timing["modelRequests"] = []
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        tokens = report["perTurn"][0]["tokens"]
        self.assertTrue(tokens["complete"])
        self.assertEqual(tokens["inputTokens"], 0)
        self.assertEqual(tokens["cachedInputTokens"], 0)
        self.assertEqual(tokens["outputTokens"], 0)
        self.assertEqual(tokens["billableTokens"], 0)

    def test_cli_internal_provider_retries_make_token_coverage_partial(self) -> None:
        timing = _timing()
        request = dict(timing["modelRequests"][0])
        request["physicalAttemptIds"] = ["attempt-1", "attempt-2", "attempt-2"]
        timing["modelRequests"] = [request]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        tokens = report["perTurn"][0]["tokens"]
        self.assertEqual(tokens["physicalAttempts"], 2)
        self.assertEqual(tokens["providerUsageAttempts"], 1)
        self.assertEqual(tokens["coverage"], 0.5)
        self.assertFalse(tokens["complete"])
        self.assertIsNone(tokens["billableTokens"])
        self.assertEqual(tokens["observedBillableTokens"], 115)
        self.assertEqual(tokens["observedBlendedTokens"], 35)

    def test_cli_token_intervals_group_retries_without_duplicate_tool_batches(
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
        timing["modelRequests"] = [
            timing["modelRequests"][0],
            retry,
            timing["modelRequests"][1],
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        intervals = report["perTurn"][0]["tokenIntervals"]
        self.assertEqual(len(intervals), 2)
        self.assertEqual(intervals[0]["requestIndexes"], [0, 1])
        self.assertEqual(intervals[0]["attemptKinds"], ["primary", "retry"])
        self.assertEqual(intervals[0]["physicalAttempts"], 2)
        self.assertEqual(intervals[0]["emittedToolCallIds"], ["relay-1"])
        self.assertEqual(intervals[0]["tokens"]["inputTokens"], 140)
        self.assertEqual(intervals[1]["precedingToolCallIds"], ["relay-1"])

    def test_cli_slow_relay_includes_pre_poll_queue_time(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "queued",
                "toolName": "shell_command",
                "source": "direct",
                "acceptedAtMs": 0,
                "firstPollAtMs": 6_000,
                "outputCollectedAtMs": 10_000,
                "deliveredAtMs": 10_001,
                "outputModelVisibleAtMs": 10_501,
                "itemToFirstPollMs": 6_000,
                "totalDurationMs": 4_001,
            }
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        relay = report["toolRelay"]
        self.assertEqual(relay["slowCallCount"], 1)
        self.assertEqual(relay["topSlowCalls"][0]["totalDurationMs"], 4_001)
        self.assertEqual(relay["topSlowCalls"][0]["endToEndDurationMs"], 10_501)
        self.assertTrue(relay["topSlowCalls"][0]["outputModelVisibilityRecorded"])

    def test_cli_relay_reports_process_exit_after_live_handle_delivery(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "background-relay",
                "source": "direct",
                "acceptedAtMs": 1,
                "processSpawnedAtMs": 5,
                "outputCollectedAtMs": 8,
                "deliveredAtMs": 9,
                "processExitedAtMs": 20,
                "processAliveAtDelivery": True,
            }
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        relay = report["toolRelay"]
        self.assertEqual(relay["processAliveAtDeliveryCalls"], 1)
        self.assertEqual(relay["phaseTotalsMs"]["modelVisibleToProcessExitMs"], 11)

    def test_cli_tool_lifecycle_completeness_is_source_aware(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "direct-incomplete",
                "source": "direct",
                "acceptedAtMs": 1,
                "outputCollectedAtMs": 2,
            },
            {
                "callId": "nested-complete",
                "source": "code_mode",
                "acceptedAtMs": 3,
                "outputCollectedAtMs": 4,
                "parentCallId": "outer-call",
                "parentCellId": "cell-1",
                "runtimeToolCallId": "runtime-call-1",
            },
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        relay = report["toolRelay"]
        self.assertEqual(relay["incompleteLifecycleCalls"], 1)
        self.assertEqual(relay["incompleteDirectLifecycleCalls"], 1)
        self.assertEqual(relay["incompleteNestedLifecycleCalls"], 0)
        self.assertEqual(
            relay["incompleteLifecycleReasonCounts"],
            {"deliveredAtMs": 1, "outputModelVisibleAtMs": 1},
        )

    def test_cli_aborted_direct_lifecycle_allows_model_visibility_truncation(
        self,
    ) -> None:
        cases = [
            (
                "aborted-after-delivery",
                "turn_aborted",
                {
                    "callId": "aborted-after-delivery",
                    "source": "direct",
                    "acceptedAtMs": 1,
                    "outputCollectedAtMs": 2,
                    "deliveredAtMs": 3,
                },
            ),
            (
                "completed-before-model-visibility",
                "task_complete",
                {
                    "callId": "completed-before-model-visibility",
                    "source": "direct",
                    "acceptedAtMs": 4,
                    "outputCollectedAtMs": 5,
                    "deliveredAtMs": 6,
                },
            ),
            (
                "aborted-before-delivery",
                "turn_aborted",
                {
                    "callId": "aborted-before-delivery",
                    "source": "direct",
                    "acceptedAtMs": 7,
                    "outputCollectedAtMs": 8,
                },
            ),
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            lines = [_meta(str(root))]
            for turn_id, status, call in cases:
                timing = _timing()
                timing["toolCalls"] = [call]
                lines.extend(
                    [
                        _event({"type": "task_started", "turn_id": turn_id}),
                        _event({"type": status, "turn_id": turn_id, "timing": timing}),
                    ]
                )
            _write_rollout(session, lines)
            report = _json_report(session, root)

        relay = report["toolRelay"]
        self.assertEqual(relay["incompleteLifecycleCalls"], 2)
        self.assertEqual(relay["incompleteDirectLifecycleCalls"], 2)
        self.assertEqual(
            relay["incompleteLifecycleReasonCounts"],
            {"deliveredAtMs": 1, "outputModelVisibleAtMs": 1},
        )
        self.assertEqual(relay["expectedTerminalAbortModelVisibilityTruncations"], 1)

    def test_cli_nested_output_collection_requirement_is_schema_aware(self) -> None:
        legacy = _timing()
        legacy["schemaVersion"] = 24
        legacy["toolCalls"] = [
            {"callId": "legacy-nested", "source": "code_mode", "acceptedAtMs": 1}
        ]
        current = _timing()
        current["toolCalls"] = [
            {
                "callId": "current-nested",
                "source": "code_mode",
                "acceptedAtMs": 2,
                "parentCallId": "outer-call",
                "parentCellId": "cell-1",
                "runtimeToolCallId": "runtime-call-1",
            }
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            lines = [_meta(str(root))]
            for turn_id, timing in (("legacy", legacy), ("current", current)):
                lines.extend(
                    [
                        _event({"type": "task_started", "turn_id": turn_id}),
                        _event(
                            {
                                "type": "task_complete",
                                "turn_id": turn_id,
                                "timing": timing,
                            }
                        ),
                    ]
                )
            _write_rollout(session, lines)
            report = _json_report(session, root)

        relay = report["toolRelay"]
        self.assertEqual(relay["incompleteLifecycleCalls"], 1)
        self.assertEqual(relay["incompleteNestedLifecycleCalls"], 1)
        self.assertEqual(
            relay["incompleteLifecycleReasonCounts"], {"outputCollectedAtMs": 1}
        )

    def test_cli_current_nested_lifecycle_requires_runtime_correlation(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "nested-missing-correlation",
                "source": "code_mode",
                "acceptedAtMs": 1,
                "outputCollectedAtMs": 2,
            },
            {
                "callId": "nested-correlated",
                "source": "code_mode",
                "acceptedAtMs": 3,
                "outputCollectedAtMs": 4,
                "parentCallId": "outer-call",
                "parentCellId": "cell-1",
                "runtimeToolCallId": "runtime-call-1",
            },
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        relay = report["toolRelay"]
        self.assertEqual(relay["incompleteLifecycleCalls"], 1)
        self.assertEqual(relay["incompleteNestedLifecycleCalls"], 1)
        self.assertEqual(
            relay["incompleteLifecycleReasonCounts"],
            {"parentCallId": 1, "parentCellId": 1, "runtimeToolCallId": 1},
        )

    def test_cli_lifecycle_overflow_and_incomplete_attribution_block_finalize(
        self,
    ) -> None:
        timing = _timing()
        timing["toolCallTimingOverflow"] = 1
        timing["toolCalls"] = [
            {"callId": "incomplete", "source": "direct", "acceptedAtMs": 1}
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing))
            report = _json_report(session, root)

        decision = report["auditDecision"]
        self.assertFalse(decision["readyToFinalize"])
        self.assertIn("tool_lifecycle_timing_overflow", decision["blockerCodes"])
        self.assertIn("incomplete_tool_lifecycle_attribution", decision["blockerCodes"])

    def test_cli_detailed_timing_matches_duplicate_ids_once_in_order(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "duplicate",
                "source": "direct",
                "acceptedAtMs": 0,
                "outputCollectedAtMs": 9,
                "deliveredAtMs": 10,
                "outputModelVisibleAtMs": 10,
                "processSpawnedAtMs": 1,
                "processExitedAtMs": 2,
            },
            {
                "callId": "duplicate",
                "source": "direct",
                "acceptedAtMs": 100,
                "outputCollectedAtMs": 119,
                "deliveredAtMs": 120,
                "outputModelVisibleAtMs": 120,
                "processSpawnedAtMs": 101,
                "processExitedAtMs": 103,
            },
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            lines = [
                _meta(str(root)),
                _event({"type": "task_started", "turn_id": "turn"}),
                _response(
                    {"type": "custom_tool_call", "call_id": "duplicate"},
                    "2026-08-17T00:00:01Z",
                ),
                _response(
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "duplicate",
                        "output": "done",
                    },
                    "2026-08-17T00:00:02Z",
                ),
                _response(
                    {"type": "custom_tool_call", "call_id": "duplicate"},
                    "2026-08-17T00:00:03Z",
                ),
                _response(
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "duplicate",
                        "output": "done",
                    },
                    "2026-08-17T00:00:04Z",
                ),
                _event(
                    {"type": "task_complete", "turn_id": "turn", "timing": timing},
                    "2026-08-17T00:00:05Z",
                ),
            ]
            _write_rollout(session, lines)
            report = _json_report(session, root)

        orchestration = report["commandOrchestration"]
        self.assertEqual(orchestration["detailedTimingMatchCounts"], {"ordered": 2})
        self.assertEqual(orchestration["roundTripNs"], 30_000_000)
        self.assertEqual(orchestration["reportedChildWorkNs"], 3_000_000)

    def test_cli_detailed_timing_marks_unequal_duplicate_groups_ambiguous(
        self,
    ) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "duplicate",
                "executionId": "one",
                "source": "direct",
                "acceptedAtMs": 1,
                "outputCollectedAtMs": 2,
                "deliveredAtMs": 3,
                "outputModelVisibleAtMs": 3,
            },
            {
                "callId": "duplicate",
                "executionId": "two",
                "source": "direct",
                "acceptedAtMs": 4,
                "outputCollectedAtMs": 5,
                "deliveredAtMs": 6,
                "outputModelVisibleAtMs": 6,
            },
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            lines = [
                _meta(str(root)),
                _event({"type": "task_started", "turn_id": "turn"}),
                _response(
                    {"type": "custom_tool_call", "call_id": "duplicate"},
                    "2026-08-17T00:00:01Z",
                ),
                _response(
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "duplicate",
                        "output": "done",
                    },
                    "2026-08-17T00:00:02Z",
                ),
                _event(
                    {"type": "task_complete", "turn_id": "turn", "timing": timing},
                    "2026-08-17T00:00:03Z",
                ),
            ]
            _write_rollout(session, lines)
            report = _json_report(session, root)

        orchestration = report["commandOrchestration"]
        self.assertEqual(orchestration["detailedTimingAmbiguousRecords"], 1)
        self.assertEqual(orchestration["detailedTimingMatchCounts"], {"unmatched": 1})
        self.assertEqual(orchestration["reportedChildRuntimeCalls"], 0)

    def test_cli_reports_exclusive_gate_convoy_for_parallel_nested_reads(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
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
                "parentCallId": "outer",
                "parentCellId": "cell",
                "runtimeToolCallId": "runtime-a",
            },
            {
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
                "parentCallId": "outer",
                "parentCellId": "cell",
                "runtimeToolCallId": "runtime-b",
            },
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing, "convoy"))
            report = _json_report(session, root)

        relay = report["toolRelay"]
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

    def test_cli_reports_post_tool_hook_as_post_process_stall_owner(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
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
                "deliveredAtMs": 99_050,
                "outputModelVisibleAtMs": 99_050,
                "modelResumedAtMs": 99_051,
                "parallelGateWaitMs": 0,
                "postToolHookMs": 99_000,
                "totalDurationMs": 99_050,
            }
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(session, _single_turn_lines(root, timing, "post-hook"))
            report = _json_report(session, root)

        relay = report["toolRelay"]
        self.assertEqual(relay["phaseTotalsMs"]["processRuntimeMs"], 45)
        self.assertEqual(relay["phaseTotalsMs"]["postToolHookMs"], 99_000)
        self.assertEqual(relay["dominantPhase"], "postToolHookMs")
        self.assertEqual(relay["dominantPhaseOwner"], "PostToolUse")
        self.assertEqual(relay["dominantPhaseMs"], 99_000)

    def test_cli_uuid_resolves_snapshot_and_emits_bounded_execution_loop(self) -> None:
        session_id = "01a018c7-a357-7c11-a7ca-9248dd075f22"
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            codex_home = Path(temp) / "codex-home"
            sessions = codex_home / "sessions" / "2026" / "08" / "19"
            root.mkdir()
            sessions.mkdir(parents=True)
            rollout = sessions / f"rollout-2026-08-19T01-48-56-{session_id}.jsonl"
            _write_rollout(
                rollout,
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
                ],
            )
            env = {"CODEX_HOME": str(codex_home)}
            summary_result = _run_cli(session_id, root, "--summary-json", env=env)
            human_result = _run_cli(session_id, root, "", env=env)

        report = json.loads(summary_result.stdout)
        self.assertLess(len(summary_result.stdout.encode("utf-8")), 16 * 1024)
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
        self.assertEqual(report["perTurn"][0]["agentActiveDurationNs"], 900)
        self.assertEqual(report["perTurn"][0]["tokens"]["billableTokens"], 235)
        self.assertEqual(report["populations"]["all"]["modelShare"], 2 / 3)
        self.assertEqual(report["schemaVersion"], 16)
        self.assertEqual(
            report["latencyBreakdown"]["orchestration"]["exclusiveTotalNs"], 100
        )
        self.assertEqual(
            report["latencyBreakdown"]["modelInference"]["logicalGenerations"], 2
        )
        self.assertEqual(
            report["coverage"]["terminalLifecycleStateCounts"], {"completed": 1}
        )
        self.assertEqual(len(report["perTurn"][0]["tokenIntervals"]), 2)
        self.assertEqual(report["firstUsefulActionAnalysis"]["canonicalTurnCount"], 1)
        self.assertNotIn("measurementContract", report["firstUsefulActionAnalysis"])
        self.assertNotIn("sourceSnapshots", report["firstUsefulActionAnalysis"])
        self.assertIn("boundary=2026-08-17", human_result.stdout)
        self.assertIn(
            "orchestration breakdown (overlapping diagnostics", human_result.stdout
        )
        self.assertIn(
            "model inference breakdown (overlapping diagnostics", human_result.stdout
        )

    def test_cli_source_discovery_is_ordered_bounded_and_redacted(self) -> None:
        commands = [
            ("Get-Content -Raw AGENTS.md", "# repository instructions"),
            ("rg -n Widget", "scripts/widget.py:10:class Widget"),
            ("rg -n Widget", "scripts/widget.py:10:class Widget"),
            ("Get-Content scripts/widget.py", "class Widget: pass"),
            ("Get-Content -Raw SOURCEMAP.md", "scripts/source_owners.py"),
            (
                "python scripts/source_owners.py slice --owner audit --focus Widget",
                "scripts/test_widget.py",
            ),
            (
                "rg -n callers_contract codex-rs/core/src",
                "codex-rs/core/src/widget_tests.rs:12:fn callers_contract()",
            ),
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            lines = [
                _meta(str(root)),
                _event(
                    {"type": "task_started", "turn_id": "discovery"},
                    "2026-08-17T00:00:00Z",
                ),
            ]
            for ordinal, (command, output) in enumerate(commands, 1):
                lines.extend(
                    [
                        _response(
                            {
                                "type": "custom_tool_call",
                                "call_id": f"discovery-{ordinal}",
                                "name": "exec",
                                "input": (
                                    "await tools.exec_command({cmd: "
                                    + json.dumps(command)
                                    + "});"
                                ),
                            },
                            f"2026-08-17T00:00:{ordinal * 2 - 1:02d}Z",
                        ),
                        _response(
                            {
                                "type": "custom_tool_call_output",
                                "call_id": f"discovery-{ordinal}",
                                "output": output,
                            },
                            f"2026-08-17T00:00:{ordinal * 2:02d}Z",
                        ),
                    ]
                )
            lines.extend(
                [
                    _response(
                        {
                            "type": "custom_tool_call",
                            "call_id": "private",
                            "name": "exec",
                            "input": "await tools.exec_command({cmd: 'private command'});",
                        },
                        "2026-08-17T00:00:20Z",
                    ),
                    _response(
                        {
                            "type": "custom_tool_call_output",
                            "call_id": "private",
                            "output": "private output",
                        },
                        "2026-08-17T00:00:21Z",
                    ),
                    _event(
                        {
                            "type": "task_complete",
                            "turn_id": "discovery",
                            "timing": _timing(),
                        },
                        "2026-08-17T00:00:22Z",
                    ),
                ]
            )
            _write_rollout(session, lines)
            full_result = _run_cli(session, root)
            summary_result = _run_cli(session, root, "--summary-json")
            human_result = _run_cli(session, root, "")

        report = json.loads(full_result.stdout)
        discovery = report["sourceDiscovery"]
        self.assertEqual(discovery["eventCount"], 7)
        self.assertEqual(discovery["searchCount"], 3)
        self.assertEqual(discovery["readCount"], 3)
        self.assertEqual(discovery["broadSearchCount"], 2)
        self.assertEqual(discovery["repeatedSearchSignatureCount"], 1)
        self.assertEqual(
            [event["ordinal"] for event in discovery["events"]], list(range(1, 8))
        )
        self.assertEqual(discovery["events"][0]["requestedPaths"], ["AGENTS.md"])
        self.assertEqual(discovery["events"][1]["queries"], ["Widget"])
        signal_counts = discovery["candidateSignalCounts"]
        self.assertEqual(signal_counts["broad_search_without_path_scope"], 2)
        self.assertEqual(signal_counts["repeated_discovery"], 2)
        self.assertEqual(signal_counts["broad_source_map_before_owner_slice"], 1)
        self.assertEqual(signal_counts["ownership_evidence_late"], 1)
        self.assertEqual(signal_counts["callers_evidence_late"], 1)
        self.assertEqual(signal_counts["tests_evidence_late"], 1)
        self.assertEqual(signal_counts["contracts_evidence_late"], 1)
        self.assertNotIn("private command", full_result.stdout)
        self.assertNotIn("private output", full_result.stdout)
        summary = json.loads(summary_result.stdout)
        self.assertNotIn("signature", summary["sourceDiscovery"]["events"][0])
        self.assertIn("source discovery: events=7", human_result.stdout)
        self.assertIn("discovery 1: turn=discovery", human_result.stdout)

    def test_cli_waiting_input_tail_does_not_block_completed_audit(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            timing = _timing()
            second_relay = dict(timing["toolCalls"][0])
            second_relay["callId"] = "relay-2"
            timing["toolCalls"].append(second_relay)
            _write_rollout(
                session,
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
                ],
            )
            report = _json_report(session, root)

        self.assertEqual(report["coverage"]["startedTurnsWithoutTerminal"], 1)
        self.assertEqual(report["behaviorSignals"]["activeTurnsExcluded"], 1)
        self.assertEqual(report["coverage"]["openTurnStateCounts"], {"user_waiting": 1})
        self.assertEqual(report["coverage"]["openTurns"][0]["state"], "user_waiting")
        self.assertEqual(report["toolRelay"]["batchGroups"], 1)
        self.assertEqual(report["toolRelay"]["batchedCalls"], 2)
        self.assertTrue(report["auditDecision"]["readyToFinalize"])
        self.assertIn("active_tail_excluded", report["auditDecision"]["reasonCodes"])
        self.assertIn("open_turn_user_waiting", report["auditDecision"]["reasonCodes"])

    def test_cli_terminal_and_open_turn_states_are_explicit(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(
                session,
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
                ],
            )
            report = _json_report(session, root)

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

    def test_cli_terminal_turn_with_unresolved_tool_is_inconsistent(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(
                session,
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
                ],
            )
            report = _json_report(session, root)

        self.assertEqual(report["executionLoop"]["unpairedToolCalls"], 1)
        self.assertEqual(report["coverage"]["terminalTurnsWithUnresolvedToolCalls"], 1)
        violation = report["coverage"]["terminalTurnInvariantViolations"][0]
        self.assertEqual(violation["turnId"], "inconsistent")
        self.assertEqual(violation["pendingTools"], ["wait"])
        self.assertIn(
            "terminal_with_unresolved_tool_call", report["perTurn"][0]["signals"]
        )
        self.assertIn(
            "terminal_turn_with_unresolved_tool_call",
            report["auditDecision"]["blockerCodes"],
        )
        self.assertFalse(report["auditDecision"]["readyToFinalize"])

    def test_cli_bounds_slow_calls_and_emits_finalize_decision(self) -> None:
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
            _write_rollout(session, lines)
            full_result = _run_cli(session, root)
            human_result = _run_cli(session, root, "")

        report = json.loads(full_result.stdout)
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
        self.assertNotIn("private command", full_result.stdout)
        self.assertNotIn("private output", full_result.stdout)
        self.assertEqual(report["auditDecision"]["dominantPhase"], "orchestration")
        self.assertTrue(report["auditDecision"]["readyToFinalize"])
        self.assertIn("audit decision: finalize", human_result.stdout)
        self.assertIn(
            "Stop rollout inspection and answer from this report.", human_result.stdout
        )

    def test_cli_output_wall_time_fallback_is_low_confidence_and_unattributed(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(
                session,
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
                ],
            )
            full_result = _run_cli(session, root)
            human_result = _run_cli(session, root, "")

        orchestration = json.loads(full_result.stdout)["commandOrchestration"]
        self.assertEqual(orchestration["pairedToolCalls"], 1)
        self.assertEqual(orchestration["reportedChildRuntimeCalls"], 1)
        self.assertEqual(orchestration["reportedChildCalls"], 2)
        self.assertEqual(orchestration["parallelBatches"], 1)
        self.assertEqual(orchestration["roundTripNs"], 3_000_000_000)
        self.assertEqual(orchestration["reportedChildWorkNs"], 1_000_000_000)
        self.assertEqual(orchestration["orchestrationGapLowerBoundNs"], 0)
        self.assertEqual(
            orchestration["unattributedRemainderLowerBoundNs"], 2_000_000_000
        )
        self.assertEqual(
            orchestration["unattributedRemainderUpperBoundNs"], 2_250_000_000
        )
        self.assertEqual(orchestration["lowConfidenceRecords"], 1)
        self.assertEqual(
            orchestration["evidenceSource"], "responseOutputWallTimeFallback"
        )
        self.assertIn(
            "persisted-gap=0.0-0.0s unattributed-remainder=2.0-2.2s low-confidence=1",
            human_result.stdout,
        )

    def test_cli_prefers_detailed_timing_over_output_wall_time(self) -> None:
        timing = _timing()
        timing["toolCalls"] = [
            {
                "callId": "call-1",
                "toolName": "functions.exec",
                "source": "direct",
                "generationIndex": 0,
                "acceptedAtMs": 0,
                "firstPollAtMs": 1,
                "parallelGateAdmittedAtMs": 2,
                "handlerEntryAtMs": 3,
                "outputCollectedAtMs": 2_999,
                "deliveredAtMs": 3_000,
                "outputModelVisibleAtMs": 3_000,
                "postToolHookMs": 2_900,
                "totalDurationMs": 3_000,
            },
            {
                "callId": "nested-call-1",
                "parentCallId": "call-1",
                "parentCellId": "cell-1",
                "runtimeToolCallId": "nested-runtime-1",
                "toolName": "exec_command",
                "source": "code_mode",
                "generationIndex": 0,
                "acceptedAtMs": 49,
                "outputCollectedAtMs": 96,
                "processSpawnedAtMs": 50,
                "processExitedAtMs": 95,
            },
        ]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            session = Path(temp) / "rollout.jsonl"
            root.mkdir()
            _write_rollout(
                session,
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
                        {"type": "task_complete", "turn_id": "timed", "timing": timing},
                        "2026-08-17T00:00:11Z",
                    ),
                ],
            )
            report = _json_report(session, root)

        orchestration = report["commandOrchestration"]
        self.assertEqual(orchestration["evidenceSource"], "toolCalls")
        self.assertEqual(orchestration["roundTripNs"], 3_000_000_000)
        self.assertEqual(orchestration["reportedChildCalls"], 1)
        self.assertEqual(orchestration["reportedChildWorkNs"], 45_000_000)
        self.assertEqual(orchestration["orchestrationGapLowerBoundNs"], 2_955_000_000)
        self.assertEqual(orchestration["orchestrationGapUpperBoundNs"], 2_955_000_000)
        self.assertEqual(orchestration["persistedNestedLifecycleRecords"], 1)
        self.assertEqual(orchestration["lowConfidenceRecords"], 0)
        self.assertEqual(orchestration["unattributedRemainderLowerBoundNs"], 0)
        self.assertEqual(orchestration["unattributedRemainderUpperBoundNs"], 0)

    def test_cli_reports_coverage_and_segments_eval_from_repository_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            sessions = Path(temp) / "sessions"
            sessions.mkdir()
            root.mkdir()
            _write_rollout(
                sessions / "root.jsonl",
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
                ],
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
            _write_rollout(sessions / "eval.jsonl", eval_lines)
            report = _json_report(sessions, root)

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
            all_population["observationalNonprogressLatency"]["modelStreamWaitNs"], 600
        )

    def test_cli_excludes_invalid_profiles_and_falls_back_for_schema_14(self) -> None:
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
            _write_rollout(
                sessions / "rollout.jsonl",
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
                ],
            )
            report = _json_report(sessions, root)

        coverage = report["coverage"]
        self.assertEqual(coverage["uniqueTimedTerminalTurns"], 2)
        self.assertEqual(coverage["validCompleteProfiles"], 1)
        self.assertEqual(coverage["invalidProfiles"], 1)
        population = report["populations"]["all"]
        self.assertEqual(population["turns"], 1)
        self.assertEqual(
            population["observationalNonprogressLatency"]["logicalGenerations"], 1
        )
        self.assertEqual(
            population["observationalNonprogressLatency"]["decisionLatencyNs"], 290
        )
        self.assertEqual(
            population["observationalNonprogressLatency"]["physicalAttempts"], 2
        )
        self.assertFalse(population["tokens"]["complete"])
        self.assertIsNone(population["tokens"]["billableTokens"])
        self.assertEqual(population["tokens"]["providerUsageAttempts"], 2)
        self.assertEqual(population["tokens"]["physicalAttempts"], 3)


if __name__ == "__main__":
    unittest.main()
