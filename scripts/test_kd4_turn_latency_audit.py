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
