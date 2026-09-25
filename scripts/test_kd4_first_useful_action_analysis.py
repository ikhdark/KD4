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
            milestones = {
                "firstUsefulActionMs": latency,
                "firstDomainActionMs": latency,
            }
            if version >= 26:
                milestones["firstModelOutputMs"] = latency / 2
            if version >= 27:
                milestones["firstVisibleOutputMs"] = latency / 2 + 1
            rows.extend(
                [
                    record(
                        "2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"}
                    ),
                    record(
                        "2026-08-17T00:00:01Z",
                        "event_msg",
                        {
                            "type": "task_complete",
                            "timing": {
                                "schemaVersion": version,
                                "profileValid": True,
                                "milestones": milestones,
                            },
                        },
                    ),
                ]
            )
        rows.extend(
            [
                "invalid json",
                record("bad timestamp", "event_msg", {"type": "task_started"}),
                record("2026-08-17T00:00:02Z", "event_msg", {"type": "task_started"}),
                record("2026-08-17T00:00:03Z", "event_msg", {"type": "task_started"}),
            ]
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text("\n".join(rows), encoding="utf-8")
            result = analysis.analyze_snapshots([read_rollout_snapshot(path)])
        self.assertEqual(result["recordCount"], 12)
        self.assertEqual(result["startedTurnCount"], 6)
        self.assertEqual(result["completedTurnCount"], 4)
        self.assertEqual(result["timingSchemaVersions"], {"25": 1, "26": 1, "27": 2})
        self.assertEqual(
            result["canonical"]["startToFirstDomainActionMs"],
            {
                "count": 4,
                "p50": 25,
                "p95": 38.5,
                "min": 10,
                "mean": 25,
                "max": 40,
                "populationStdDev": 11.18,
                "eligibleTurnCount": 4,
                "coverage": 1,
            },
        )
        model_output = result["canonical"]["startToFirstModelOutputMs"]
        self.assertEqual(
            (model_output["count"], model_output["coverage"], model_output["p50"]),
            (3, 0.75, 15),
        )
        visible = result["canonical"]["startToFirstVisibleOutputMs"]
        self.assertEqual(
            (visible["count"], visible["coverage"], visible["p50"]), (2, 0.5, 18.5)
        )
        self.assertEqual(
            result["canonical"]["startToFirstActionableOutputMs"]["coverage"], 0
        )
        self.assertIsNone(result["canonical"]["startToFirstActionableOutputMs"]["p50"])
        self.assertEqual(
            result["exclusionRates"],
            {
                "invalidJsonLines": {"count": 1, "denominator": 12, "rate": 1 / 12},
                "invalidTimestamps": {"count": 1, "denominator": 11, "rate": 1 / 11},
                "incompleteTurns": {"count": 2, "denominator": 6, "rate": 1 / 3},
                "supersededTurns": {"count": 1, "denominator": 6, "rate": 1 / 6},
                "unterminatedTurns": {"count": 1, "denominator": 6, "rate": 1 / 6},
                "abortedTurns": {"count": 0, "denominator": 4, "rate": 0},
                "terminalWithoutStart": {"count": 0, "denominator": 4, "rate": 0},
                "duplicateTerminalEvents": {"count": 0, "denominator": 4, "rate": 0},
                "invalidTimingProfiles": {"count": 0, "denominator": 4, "rate": 0},
                "incompleteCanonicalMilestones": {
                    "count": 0,
                    "denominator": 4,
                    "rate": 0,
                },
            },
        )

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
                            "profileValid": True,
                            "milestones": {"firstDomainActionMs": 10},
                        },
                    },
                )
            )
            report = analysis.analyze_snapshots([read_rollout_snapshot(path)])
            self.assertEqual(report["canonicalTurnCount"], 0)
        self.assertEqual(report["exclusions"]["incompleteCanonicalMilestones"], 1)

    def analyze_rows(self, rows: list[str]) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text("\n".join(rows), encoding="utf-8")
            return analysis.analyze_snapshots([read_rollout_snapshot(path)])

    def canonical_turn(self, milestones: dict, *, valid: bool = True) -> list[str]:
        return [
            record("2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"}),
            record(
                "2026-08-17T00:00:01Z",
                "event_msg",
                {
                    "type": "task_complete",
                    "timing": {
                        "schemaVersion": 25,
                        "profileValid": valid,
                        "milestones": milestones,
                    },
                },
            ),
        ]

    def test_invalid_timing_profile_milestones_are_excluded_not_averaged(self):
        # A clock-regressed profile claiming a 1 ms first action must not pull
        # the median below the one trustworthy 100 ms observation.
        result = self.analyze_rows(
            self.canonical_turn(
                {"firstUsefulActionMs": 100, "firstDomainActionMs": 100,
                 "firstModelOutputMs": 40}
            )
            + self.canonical_turn(
                {"firstUsefulActionMs": 1, "firstDomainActionMs": 1,
                 "firstModelOutputMs": 1},
                valid=False,
            )
        )
        useful = result["canonical"]["startToFirstUsefulActionMs"]
        self.assertEqual((useful["count"], useful["p50"]), (1, 100.0))
        self.assertEqual(useful["eligibleTurnCount"], 2)
        self.assertEqual(useful["coverage"], 0.5)
        model_output = result["canonical"]["startToFirstModelOutputMs"]
        self.assertEqual((model_output["count"], model_output["p50"]), (1, 40.0))
        self.assertEqual(result["exclusions"]["invalidTimingProfiles"], 1)
        self.assertEqual(result["exclusions"]["incompleteCanonicalMilestones"], 0)
        self.assertEqual(
            result["exclusionRates"]["invalidTimingProfiles"]["rate"], 0.5
        )

    def test_out_of_order_milestone_interval_is_not_zero_latency(self):
        # Input recorded after the first useful tool was accepted (mid-turn
        # steering) is an ordering inconsistency, not a 0 ms sample.
        base = {"firstUsefulActionMs": 400, "firstDomainActionMs": 400}
        result = self.analyze_rows(
            self.canonical_turn(
                {**base, "userInputRecordedMs": 500, "firstUsefulToolAcceptedMs": 200}
            )
            + self.canonical_turn(
                {**base, "userInputRecordedMs": 100, "firstUsefulToolAcceptedMs": 250}
            )
        )
        interval = result["canonical"]["userInputToUsefulAcceptedMs"]
        self.assertEqual(
            (interval["count"], interval["p50"], interval["min"]), (1, 150.0, 150.0)
        )
        self.assertEqual(interval["outOfOrderCount"], 1)
        self.assertEqual(interval["coverage"], 0.5)
        success = result["canonical"]["usefulExecutionToSuccessMs"]
        self.assertEqual((success["count"], success["outOfOrderCount"]), (0, 0))

    def test_aborted_turn_is_terminal_and_never_pairs_with_a_later_completion(self):
        result = self.analyze_rows(
            [
                record("2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"}),
                record(
                    "2026-08-17T00:00:05Z",
                    "response_item",
                    {"type": "function_call", "name": "exec_command"},
                ),
                record("2026-08-17T00:00:06Z", "event_msg", {"type": "turn_aborted"}),
                # Next turn's start is missing; its completion must not inherit
                # the aborted turn's start and tool.
                record("2026-08-17T00:01:00Z", "event_msg", {"type": "task_complete"}),
                record("2026-08-17T00:02:00Z", "event_msg", {"type": "task_started"}),
                record("2026-08-17T00:02:00Z", "event_msg", {"type": "turn_aborted"}),
                record("2026-08-17T00:03:00Z", "event_msg", {"type": "task_started"}),
                record(
                    "2026-08-17T00:03:02Z",
                    "response_item",
                    {"type": "function_call", "name": "exec_command"},
                ),
                record("2026-08-17T00:03:03Z", "event_msg", {"type": "task_complete"}),
            ]
        )
        self.assertEqual(result["exclusions"]["abortedTurns"], 2)
        self.assertEqual(result["exclusions"]["supersededTurns"], 0)
        self.assertEqual(result["exclusions"]["incompleteTurns"], 0)
        # The start-less completion is still a completed turn; it is disclosed
        # and lowers legacy coverage instead of disappearing.
        self.assertEqual(result["exclusions"]["terminalWithoutStart"], 1)
        self.assertEqual(result["completedTurnCount"], 2)
        emitted = result["legacyReconstructed"]["startToUsefulToolEmittedMs"]
        self.assertEqual(
            (emitted["count"], emitted["p50"], emitted["coverage"]), (1, 2000.0, 0.5)
        )

    def test_start_less_duplicate_and_mismatched_terminals_are_counted(self):
        def event(timestamp: str, payload: dict) -> str:
            return record(timestamp, "event_msg", payload)

        def complete(turn_id: str, latency: int) -> dict:
            return {
                "type": "task_complete",
                "turn_id": turn_id,
                "timing": {
                    "schemaVersion": 25,
                    "profileValid": True,
                    "milestones": {
                        "firstUsefulActionMs": latency,
                        "firstDomainActionMs": latency,
                    },
                },
            }

        result = self.analyze_rows(
            [
                # Truncated head: t0's start was not captured, and its terminal
                # was persisted twice.
                event("2026-08-17T00:00:01Z", complete("t0", 50)),
                event("2026-08-17T00:00:02Z", complete("t0", 50)),
                # t1 never terminates; the next terminal belongs to t2, whose
                # start was lost.
                event("2026-08-17T00:00:10Z", {"type": "task_started", "turn_id": "t1"}),
                event("2026-08-17T00:00:11Z", complete("t2", 70)),
                event("2026-08-17T00:00:20Z", {"type": "task_started", "turn_id": "t3"}),
                event("2026-08-17T00:00:21Z", complete("t3", 90)),
            ]
        )
        self.assertEqual(result["completedTurnCount"], 3)
        self.assertEqual(result["startedTurnCount"], 2)
        useful = result["canonical"]["startToFirstUsefulActionMs"]
        self.assertEqual((useful["count"], useful["p50"]), (3, 70.0))
        self.assertEqual(useful["coverage"], 1)
        exclusions = result["exclusions"]
        self.assertEqual(exclusions["terminalWithoutStart"], 2)
        self.assertEqual(exclusions["duplicateTerminalEvents"], 1)
        self.assertEqual(exclusions["unterminatedTurns"], 1)
        self.assertEqual(exclusions["incompleteTurns"], 1)
        self.assertEqual(
            result["exclusionRates"]["duplicateTerminalEvents"]["denominator"], 4
        )

    def test_legacy_out_of_order_user_input_is_not_negative_latency(self):
        tool = {"type": "function_call", "name": "exec_command"}
        result = self.analyze_rows(
            [
                record("2026-08-17T00:00:00Z", "event_msg", {"type": "task_started"}),
                record("2026-08-17T00:00:02Z", "response_item", tool),
                # User input recorded one second after the useful tool was emitted.
                record("2026-08-17T00:00:03Z", "event_msg", {"type": "user_message"}),
                record("2026-08-17T00:00:05Z", "event_msg", {"type": "task_complete"}),
                record("2026-08-17T00:10:00Z", "event_msg", {"type": "task_started"}),
                record("2026-08-17T00:10:01Z", "event_msg", {"type": "user_message"}),
                record("2026-08-17T00:10:04Z", "response_item", tool),
                record("2026-08-17T00:10:05Z", "event_msg", {"type": "task_complete"}),
            ]
        )
        interval = result["legacyReconstructed"]["userInputEventToUsefulToolEmittedMs"]
        self.assertEqual(
            (interval["count"], interval["p50"], interval["min"]), (1, 3000.0, 3000.0)
        )
        self.assertEqual(interval["outOfOrderCount"], 1)
        emitted = result["legacyReconstructed"]["startToUsefulToolEmittedMs"]
        self.assertEqual((emitted["count"], emitted["outOfOrderCount"]), (2, 0))

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
                                    "profileValid": True,
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
