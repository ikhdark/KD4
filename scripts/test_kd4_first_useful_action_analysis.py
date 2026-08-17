from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts import kd4_first_useful_action_analysis as analysis


def record(timestamp: str, record_type: str, payload: dict[str, object]) -> str:
    return json.dumps({"timestamp": timestamp, "type": record_type, "payload": payload})


class FirstUsefulActionAnalysisTest(unittest.TestCase):
    def test_legacy_reconstruction_excludes_control_only_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "rollout.jsonl"
            path.write_text(
                "\n".join(
                    [
                        record("2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"}),
                        record("2026-08-17T00:00:04Z", "event_msg", {"type": "user_message"}),
                        record("2026-08-17T00:00:05Z", "response_item", {"type": "function_call", "name": "update_plan"}),
                        record("2026-08-17T00:00:07Z", "response_item", {"type": "function_call", "name": "exec_command"}),
                        record("2026-08-17T00:00:09Z", "event_msg", {"type": "task_complete"}),
                    ]
                ),
                encoding="utf-8",
            )

            result = analysis.analyze([Path(temp_dir)])

        self.assertEqual(result["legacyReconstructedTurnCount"], 1)
        self.assertEqual(
            result["legacyReconstructed"]["startToUserInputEventMs"]["p50"],
            4000.0,
        )
        self.assertEqual(
            result["legacyReconstructed"]["userInputEventToUsefulToolEmittedMs"]["p50"],
            3000.0,
        )

    def test_schema_20_uses_canonical_phase_boundaries(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "rollout.jsonl"
            path.write_text(
                "\n".join(
                    [
                        record("2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"}),
                        record(
                            "2026-08-17T00:00:01Z",
                            "event_msg",
                            {
                                "type": "task_complete",
                                "timing": {
                                    "schemaVersion": 20,
                                    "milestones": {
                                        "userInputRecordedMs": 100,
                                        "firstUsefulToolAcceptedMs": 250,
                                        "firstUsefulToolGateAdmittedMs": 280,
                                        "firstUsefulActionMs": 320,
                                        "firstSuccessfulUsefulActionMs": 500,
                                    },
                                },
                            },
                        ),
                    ]
                ),
                encoding="utf-8",
            )

            result = analysis.analyze([path])

        self.assertEqual(result["canonicalTurnCount"], 1)
        self.assertEqual(result["legacyReconstructedTurnCount"], 0)
        self.assertEqual(result["canonical"]["userInputToUsefulAcceptedMs"]["p50"], 150.0)
        self.assertEqual(result["canonical"]["usefulParallelGateWaitMs"]["p50"], 30.0)
        self.assertEqual(result["canonical"]["usefulAuthorizationAndDispatchMs"]["p50"], 40.0)
        self.assertEqual(result["canonical"]["usefulExecutionToSuccessMs"]["p50"], 180.0)

    def test_schema_19_field_is_not_treated_as_canonical(self) -> None:
        self.assertFalse(analysis.is_useful_tool("functions.wait_agent"))
        self.assertTrue(analysis.is_useful_tool("functions.exec_command"))


if __name__ == "__main__":
    unittest.main()
