from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
AUDIT_SCRIPT = REPO_ROOT / "scripts" / "kd4_turn_latency_audit.py"
ANALYSIS_SCRIPT = REPO_ROOT / "scripts" / "kd4_first_useful_action_analysis.py"


def record(timestamp: str, record_type: str, payload: dict[str, object]) -> str:
    return json.dumps({"timestamp": timestamp, "type": record_type, "payload": payload})


class FirstUsefulActionAnalysisTest(unittest.TestCase):
    def run_audit_cli(self, records: list[str]) -> dict[str, object]:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "rollout.jsonl"
            path.write_text("\n".join(records) + "\n", encoding="utf-8")
            result = subprocess.run(
                [
                    sys.executable,
                    str(AUDIT_SCRIPT),
                    str(path),
                    "--repo-root",
                    temp_dir,
                    "--json",
                ],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        parsed = json.loads(result.stdout)
        self.assertIsInstance(parsed, dict)
        return parsed

    def test_audit_cli_is_the_only_rollout_lookup_boundary(self) -> None:
        records = [
            record(
                "2026-08-17T00:00:00Z",
                "event_msg",
                {"type": "task_started", "turn_id": "turn-boundary"},
            ),
            record(
                "2026-08-17T00:00:01Z",
                "response_item",
                {"type": "function_call", "name": "exec_command"},
            ),
            record(
                "2026-08-17T00:00:02Z",
                "event_msg",
                {"type": "task_complete", "turn_id": "turn-boundary"},
            ),
        ]

        report = self.run_audit_cli(records)

        first_useful = report["firstUsefulActionAnalysis"]
        self.assertEqual(first_useful["legacyReconstructedTurnCount"], 1)
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "rollout.jsonl"
            path.write_text("\n".join(records) + "\n", encoding="utf-8")
            direct = subprocess.run(
                [sys.executable, str(ANALYSIS_SCRIPT), str(path)],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
        self.assertEqual(direct.returncode, 0, direct.stderr)
        self.assertEqual(direct.stdout, "")
        self.assertEqual(direct.stderr, "")

    def test_audit_cli_reconstructs_legacy_domain_action_after_control_tools(
        self,
    ) -> None:
        report = self.run_audit_cli(
            [
                record(
                    "2026-08-17T00:00:00Z",
                    "event_msg",
                    {"type": "task_started", "turn_id": "turn-legacy"},
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
                    {"type": "task_complete", "turn_id": "turn-legacy"},
                ),
            ]
        )

        first_useful = report["firstUsefulActionAnalysis"]
        self.assertEqual(first_useful["legacyReconstructedTurnCount"], 1)
        self.assertEqual(
            first_useful["legacyReconstructed"]["startToUserInputEventMs"]["p50"],
            4000.0,
        )
        self.assertEqual(
            first_useful["legacyReconstructed"]["userInputEventToUsefulToolEmittedMs"][
                "p50"
            ],
            4000.0,
        )

    def test_audit_cli_reports_separate_schema_25_action_boundaries(self) -> None:
        report = self.run_audit_cli(
            [
                record(
                    "2026-08-17T00:00:00Z",
                    "event_msg",
                    {"type": "task_started", "turn_id": "turn-schema-25"},
                ),
                record(
                    "2026-08-17T00:00:01Z",
                    "event_msg",
                    {
                        "type": "task_complete",
                        "turn_id": "turn-schema-25",
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
        )

        first_useful = report["firstUsefulActionAnalysis"]
        self.assertEqual(first_useful["canonicalTurnCount"], 1)
        self.assertEqual(first_useful["legacyReconstructedTurnCount"], 0)
        canonical = first_useful["canonical"]
        self.assertEqual(canonical["userInputToUsefulAcceptedMs"]["p50"], 150.0)
        self.assertEqual(canonical["usefulParallelGateWaitMs"]["p50"], 30.0)
        self.assertEqual(canonical["usefulAuthorizationAndDispatchMs"]["p50"], 40.0)
        self.assertEqual(canonical["usefulExecutionToSuccessMs"]["p50"], 180.0)
        self.assertEqual(canonical["startToFirstInfrastructureActionMs"]["p50"], 120.0)
        self.assertEqual(canonical["startToFirstToolDiscoveryActionMs"]["p50"], 220.0)
        self.assertEqual(canonical["startToFirstDomainActionMs"]["p50"], 320.0)
        self.assertEqual(
            canonical["startToFirstSuccessfulDomainActionMs"]["p50"], 500.0
        )

    def test_audit_cli_treats_schema_19_domain_field_as_legacy_and_filters_control_tools(
        self,
    ) -> None:
        report = self.run_audit_cli(
            [
                record(
                    "2026-08-17T00:00:00Z",
                    "event_msg",
                    {"type": "task_started", "turn_id": "turn-schema-19"},
                ),
                record(
                    "2026-08-17T00:00:01Z",
                    "event_msg",
                    {"type": "user_message"},
                ),
                record(
                    "2026-08-17T00:00:02Z",
                    "response_item",
                    {"type": "function_call", "name": "functions.wait_agent"},
                ),
                record(
                    "2026-08-17T00:00:03Z",
                    "response_item",
                    {"type": "custom_tool_call", "name": "functions.exec"},
                ),
                record(
                    "2026-08-17T00:00:04Z",
                    "response_item",
                    {"type": "function_call", "name": "functions.tool_search"},
                ),
                record(
                    "2026-08-17T00:00:05Z",
                    "response_item",
                    {"type": "function_call", "name": "functions.exec_command"},
                ),
                record(
                    "2026-08-17T00:00:06Z",
                    "event_msg",
                    {
                        "type": "task_complete",
                        "turn_id": "turn-schema-19",
                        "timing": {
                            "schemaVersion": 19,
                            "milestones": {"firstDomainActionMs": 2},
                        },
                    },
                ),
            ]
        )

        first_useful = report["firstUsefulActionAnalysis"]
        self.assertEqual(first_useful["canonicalTurnCount"], 0)
        self.assertEqual(first_useful["legacyReconstructedTurnCount"], 1)
        self.assertEqual(
            first_useful["legacyReconstructed"]["startToUsefulToolEmittedMs"]["p50"],
            5000.0,
        )
        self.assertEqual(
            first_useful["legacyReconstructed"]["userInputEventToUsefulToolEmittedMs"][
                "p50"
            ],
            4000.0,
        )


if __name__ == "__main__":
    unittest.main()
