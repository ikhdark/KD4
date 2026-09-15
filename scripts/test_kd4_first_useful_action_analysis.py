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
    def test_mixed_schema_coverage_quantiles_and_exclusions_from_snapshot(self):
        rows = []
        for version, latency in ((25, 10), (26, 20), (27, 30), (27, 40)):
            milestones = {"firstUsefulActionMs": latency, "firstDomainActionMs": latency}
            if version >= 26:
                milestones["firstModelOutputMs"] = latency / 2
            if version >= 27:
                milestones["firstVisibleOutputMs"] = latency / 2 + 1
            rows.extend([
                record("2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"}),
                record("2026-08-17T00:00:01Z", "event_msg", {"type": "task_complete", "timing": {"schemaVersion": version, "milestones": milestones}}),
            ])
        rows.extend([
            "invalid json",
            record("bad timestamp", "event_msg", {"type": "task_started"}),
            record("2026-08-17T00:00:02Z", "event_msg", {"type": "task_started"}),
            record("2026-08-17T00:00:03Z", "event_msg", {"type": "task_started"}),
        ])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text("\n".join(rows), encoding="utf-8")
            result = analysis.analyze_snapshots([read_rollout_snapshot(path)])
        self.assertEqual(result["recordCount"], 12)
        self.assertEqual(result["startedTurnCount"], 6)
        self.assertEqual(result["completedTurnCount"], 4)
        self.assertEqual(result["timingSchemaVersions"], {"25": 1, "26": 1, "27": 2})
        self.assertEqual(result["canonical"]["startToFirstDomainActionMs"], {
            "count": 4, "p50": 25, "p95": 38.5, "min": 10, "mean": 25,
            "max": 40, "populationStdDev": 11.18, "eligibleTurnCount": 4, "coverage": 1,
        })
        model_output = result["canonical"]["startToFirstModelOutputMs"]
        self.assertEqual((model_output["count"], model_output["coverage"], model_output["p50"]), (3, .75, 15))
        visible = result["canonical"]["startToFirstVisibleOutputMs"]
        self.assertEqual((visible["count"], visible["coverage"], visible["p50"]), (2, .5, 18.5))
        self.assertEqual(result["canonical"]["startToFirstActionableOutputMs"]["coverage"], 0)
        self.assertIsNone(result["canonical"]["startToFirstActionableOutputMs"]["p50"])
        self.assertEqual(result["exclusionRates"], {
            "invalidJsonLines": {"count": 1, "denominator": 12, "rate": 1 / 12},
            "invalidTimestamps": {"count": 1, "denominator": 11, "rate": 1 / 11},
            "incompleteTurns": {"count": 2, "denominator": 6, "rate": 1 / 3},
            "supersededTurns": {"count": 1, "denominator": 6, "rate": 1 / 6},
            "unterminatedTurns": {"count": 1, "denominator": 6, "rate": 1 / 6},
            "incompleteCanonicalMilestones": {"count": 0, "denominator": 4, "rate": 0},
        })

    def test_partial_canonical_milestone_is_not_admitted(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "rollout.jsonl"
            path.write_text(
                record("2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"})
                + "\n"
                + record(
                    "2026-08-17T00:00:01Z",
                    "event_msg",
                    {
                        "type": "task_complete",
                        "timing": {
                            "schemaVersion": 25,
                            "milestones": {"firstDomainActionMs": 10},
                        },
                    },
                )
            )
            report = analysis.analyze_snapshots([read_rollout_snapshot(path)])
            self.assertEqual(report["canonicalTurnCount"], 0)
        self.assertEqual(report["exclusions"]["incompleteCanonicalMilestones"], 1)

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
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text(
                "\n".join(
                    [
                        record(
                            "2026-08-17T00:00:00Z",
                            "event_msg",
                            {"type": "task_started"},
                        ),
                        record(
                            "2026-08-17T00:00:00.100Z",
                            "event_msg",
                            {"type": "user_message"},
                        ),
                        record(
                            "2026-08-17T00:00:00.500Z",
                            "response_item",
                            {"type": "function_call", "name": "exec_command"},
                        ),
                        record(
                            "2026-08-17T00:00:01Z",
                            "event_msg",
                            {
                                "type": "task_complete",
                                "timing": {
                                    "schemaVersion": 19,
                                    "milestones": {
                                        "firstUsefulActionMs": 320,
                                        "firstDomainActionMs": 320,
                                    },
                                },
                            },
                        ),
                    ]
                ),
                encoding="utf-8",
            )
            result = analysis.analyze_snapshots([read_rollout_snapshot(path)])
        self.assertEqual(result["canonicalTurnCount"], 0)
        self.assertEqual(result["legacyReconstructedTurnCount"], 1)

    def test_useful_tool_classification_excludes_control_and_discovery(self) -> None:
        self.assertFalse(analysis.is_useful_tool("functions.wait_agent"))
        self.assertFalse(analysis.is_useful_tool("functions.exec"))
        self.assertFalse(analysis.is_useful_tool("functions.tool_search"))
        self.assertTrue(analysis.is_useful_tool("functions.exec_command"))


if __name__ == "__main__":
    unittest.main()
