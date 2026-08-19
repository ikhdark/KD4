from __future__ import annotations

import json
import tempfile
import unittest
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
        "schemaVersion": 19,
        "profileValid": valid,
        "classificationComplete": complete,
        "inclusiveDurationNs": 1000,
        "exclusive": {"modelOnlyNs": 600, "toolOnlyNs": 200, "modelPlusToolNs": 0},
        "unions": {"modelRequestWaitUnionNs": 10, "modelStreamWaitUnionNs": 590},
        "counters": {
            "logicalGenerationCount": 2,
            "toolCallCount": 1,
            "samePurposeContinuationCount": 1,
            "suppressedDeterministicContinuationCount": 1,
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
            },
            {
                "generationIndex": 1,
                "attemptKind": "primary",
                "generationPurpose": "implementation",
                "modelStreamWaitNs": 290,
                "decisionLatencyNs": None,
                "unchangedRelevantState": False,
                "nextStructuredActionChanged": True,
            },
        ],
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
        self.assertEqual(
            orchestration["orchestrationGapLowerBoundNs"], 2_000_000_000
        )
        self.assertEqual(
            orchestration["orchestrationGapUpperBoundNs"], 2_250_000_000
        )
        self.assertEqual(orchestration["orchestrationShareLowerBound"], 2 / 3)
        self.assertEqual(orchestration["orchestrationShareUpperBound"], 3 / 4)
        rendered = kd4_turn_latency_audit.render_report(report)
        self.assertIn("round-trip=3.0s child-work=1.0s gap=2.0-2.2s", rendered)

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


if __name__ == "__main__":
    unittest.main()
