from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts import kd4_first_useful_action_analysis as analysis
from scripts.rollout_snapshot import read_rollout_snapshot


def record(timestamp: str, record_type: str, payload: dict[str, object]) -> str:
    return json.dumps({"timestamp": timestamp, "type": record_type, "payload": payload})


class FirstUsefulActionAnalysisTest(unittest.TestCase):
    def test_module_has_no_standalone_rollout_lookup_cli(self) -> None:
        self.assertFalse(hasattr(analysis, "main"))

    def test_legacy_reconstruction_excludes_control_only_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "rollout.jsonl"
            path.write_text(
                "\n".join(
                    [
                        record(
                            "2026-08-17T00:00:00Z",
                            "event_msg",
                            {"type": "task_started"},
                        ),
                        record(
                            "2026-08-17T00:00:04Z",
                            "event_msg",
                            {"type": "user_message"},
                        ),
                        record(
                            "2026-08-17T00:00:05Z",
                            "response_item",
                            {"type": "function_call", "name": "update_plan"},
                        ),
                        record(
                            "2026-08-17T00:00:07Z",
                            "response_item",
                            {"type": "tool_search_call", "execution": "client"},
                        ),
                        record(
                            "2026-08-17T00:00:08Z",
                            "response_item",
                            {"type": "function_call", "name": "exec_command"},
                        ),
                        record(
                            "2026-08-17T00:00:09Z",
                            "event_msg",
                            {"type": "task_complete"},
                        ),
                    ]
                ),
                encoding="utf-8",
            )

            result = analysis.analyze_snapshots([read_rollout_snapshot(path)])

        self.assertEqual(result["legacyReconstructedTurnCount"], 1)
        self.assertEqual(
            result["legacyReconstructed"]["startToUserInputEventMs"]["p50"],
            4000.0,
        )
        self.assertEqual(
            result["legacyReconstructed"]["userInputEventToUsefulToolEmittedMs"]["p50"],
            4000.0,
        )

    def test_schema_25_uses_separate_action_class_boundaries(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "rollout.jsonl"
            path.write_text(
                "\n".join(
                    [
                        record(
                            "2026-08-17T00:00:00Z",
                            "event_msg",
                            {"type": "task_started"},
                        ),
                        record(
                            "2026-08-17T00:00:01Z",
                            "event_msg",
                            {
                                "type": "task_complete",
                                "timing": {
                                    "schemaVersion": 25,
                                    "milestones": {
                                        "userInputRecordedMs": 100,
                                        "firstUsefulToolAcceptedMs": 250,
                                        "firstUsefulToolGateAdmittedMs": 280,
                                        "firstUsefulActionMs": 320,
                                        "firstSuccessfulUsefulActionMs": 500,
                                        "firstInfrastructureActionMs": 120,
                                        "firstToolDiscoveryActionMs": 220,
                                        "firstDomainActionMs": 320,
                                        "firstSuccessfulDomainActionMs": 500,
                                    },
                                },
                            },
                        ),
                    ]
                ),
                encoding="utf-8",
            )

            result = analysis.analyze_snapshots([read_rollout_snapshot(path)])

        self.assertEqual(result["canonicalTurnCount"], 1)
        self.assertEqual(result["legacyReconstructedTurnCount"], 0)
        self.assertEqual(
            result["canonical"]["userInputToUsefulAcceptedMs"]["p50"], 150.0
        )
        self.assertEqual(result["canonical"]["usefulParallelGateWaitMs"]["p50"], 30.0)
        self.assertEqual(
            result["canonical"]["usefulAuthorizationAndDispatchMs"]["p50"], 40.0
        )
        self.assertEqual(
            result["canonical"]["usefulExecutionToSuccessMs"]["p50"], 180.0
        )
        self.assertEqual(
            result["canonical"]["startToFirstInfrastructureActionMs"]["p50"],
            120.0,
        )
        self.assertEqual(
            result["canonical"]["startToFirstToolDiscoveryActionMs"]["p50"],
            220.0,
        )
        self.assertEqual(
            result["canonical"]["startToFirstDomainActionMs"]["p50"], 320.0
        )
        self.assertEqual(
            result["canonical"]["startToFirstSuccessfulDomainActionMs"]["p50"],
            500.0,
        )

    def test_schema_19_field_is_not_treated_as_canonical(self) -> None:
        self.assertFalse(analysis.is_useful_tool("functions.wait_agent"))
        self.assertFalse(analysis.is_useful_tool("functions.exec"))
        self.assertFalse(analysis.is_useful_tool("functions.tool_search"))
        self.assertTrue(analysis.is_useful_tool("functions.exec_command"))


if __name__ == "__main__":
    unittest.main()
