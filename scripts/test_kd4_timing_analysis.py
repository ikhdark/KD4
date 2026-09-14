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


class SharedTimingAnalysisTest(unittest.TestCase):
    def evidence(self, timing=None):
        return {"schemaVersion": 1, "attemptId": "task-1", "elapsedMs": 1000,
                "status": "completed", "events": [{"elapsedMs": 1000, "message": {
                    "method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed", "timing": timing or timing_profile()}}}}]}

    def test_audit_and_runner_share_independent_expected_metrics(self):
        timing = timing_profile()
        original = copy.deepcopy(timing)
        runner = analysis.analyze_runner_evidence(self.evidence(timing))
        runtime = runner["runtime"]
        self.assertEqual(runtime["tokens"]["inputTokens"], 100)
        self.assertEqual(runtime["tokens"]["outputTokens"], 25)
        self.assertEqual(runtime["tokens"]["physicalAttempts"], 3)
        self.assertEqual(runtime["tokens"]["coverage"], 1 / 3)
        self.assertIsNone(runtime["tokens"]["billableTokens"])
        self.assertEqual(runtime["toolRelay"]["phaseTotalsMs"]["endToEndDurationMs"], 80)
        self.assertEqual(runtime["toolRelay"]["phaseTotalsMs"]["processRuntimeMs"], 50)
        self.assertEqual(runtime["modelStreamWaitNs"], 590_000_000)
        self.assertEqual(runtime["modelOnlyNs"], 600_000_000)
        self.assertEqual(runner["logicalGenerations"], 2)
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "rollout.jsonl"
            source.write_text(json.dumps({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-1", "timing": timing}}), encoding="utf-8")
            report = audit.analyze_session_path(source, root)
            self.assertEqual(report["perTurn"][0]["tokens"], runtime["tokens"])
            self.assertEqual(report["perTurn"][0]["firstUsefulActionMs"], 12.5)
        self.assertEqual(timing, original)

    def test_no_token_function_runs_in_scripted_audit_or_runner(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "rollout.jsonl"
            source.write_text(json.dumps({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t", "timing": timing_profile()}}), encoding="utf-8")
            with mock.patch.object(analysis, "_token_report", side_effect=AssertionError("token calculation called")), mock.patch.object(analysis, "_diagnostic_token_report", side_effect=AssertionError("token calculation called")), mock.patch.object(audit, "_token_intervals", side_effect=AssertionError("token calculation called")):
                report = audit.analyze_session_path(source, root, include_tokens=False, runner_evidence=self.evidence())
            self.assertFalse(report["perTurn"][0]["tokens"]["enabled"])
            self.assertFalse(report["runnerDiagnostics"]["tokens"]["enabled"])
            self.assertEqual(report["runnerDiagnostics"]["logicalGenerations"], 2)
            self.assertEqual(report["perTurn"][0]["tokenIntervals"], [])

    def test_cli_startup_failure_without_rollout(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            evidence = root / "evidence.json"
            evidence.write_text(json.dumps({"schemaVersion": 1, "attemptId": "startup", "status": "setup_failed", "elapsedMs": 12, "events": [], "failure": {"kind": "authentication", "message": "credential missing"}}), encoding="utf-8")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                result = audit.main(["--runner-evidence", str(evidence), "--tokens", "off", "--json"])
            report = json.loads(stdout.getvalue())
            self.assertEqual(result, 0)
            self.assertIsNone(report["source"])
            self.assertEqual(report["runnerDiagnostics"]["failures"][0]["kind"], "authentication")
            self.assertEqual(report["runnerDiagnostics"]["status"], "setup_failed")
            self.assertIsNone(report["runnerDiagnostics"]["physicalRequests"])

    def test_reference_usage_snapshots_are_not_added_as_generations(self):
        events = [{"message": {"method": "thread/tokenUsage/updated", "params": {"threadId": "thread", "tokenUsage": {"total": {"inputTokens": count, "cachedInputTokens": 60, "outputTokens": 20, "reasoningOutputTokens": 7}}}}} for count in (100, 150, 150)]
        report = analysis.analyze_runner_evidence({"schemaVersion": 1, "events": events})
        self.assertEqual(report["tokens"]["inputTokens"], 150)
        self.assertEqual(report["tokens"]["totalTokens"], 170)
        self.assertEqual(report["tokens"]["visibleOutputTokens"], 13)
        self.assertIsNone(report["tokens"]["promptCategories"])
        self.assertIsNone(report["logicalGenerations"])
        self.assertIsNone(report["runtime"])

    def test_pending_tool_and_model_claim_are_distinct_from_tool_failure(self):
        events = [
            {"elapsedMs": 10, "message": {"method": "item/started", "params": {"turnId": "t", "item": {"id": "tool", "type": "commandExecution", "status": "inProgress"}}}},
            {"elapsedMs": 20, "message": {"method": "item/completed", "params": {"turnId": "t", "item": {"id": "msg", "type": "agentMessage", "text": "I cannot access the tool"}}}},
        ]
        report = analysis.analyze_runner_evidence({"schemaVersion": 1, "status": "timeout", "elapsedMs": 600000, "events": events})
        self.assertEqual(report["pendingTools"][0]["id"], "tool")
        self.assertEqual(report["lastProgress"]["elapsedMs"], 20)
        self.assertEqual(report["symptoms"][0]["source"], "model_claim")
        self.assertFalse(report["symptoms"][0]["causallyEstablished"])
        self.assertEqual([row["kind"] for row in report["failures"]], ["timeout", "missing_terminal_event"])
        events.append({"elapsedMs": 30, "message": {"method": "item/completed", "params": {"turnId": "t", "item": {"id": "tool", "type": "commandExecution", "status": "failed", "exitCode": 1, "aggregatedOutput": "unknown tool"}}}})
        report = analysis.analyze_runner_evidence({"schemaVersion": 1, "events": events})
        self.assertEqual(report["pendingTools"], [])
        self.assertEqual(report["directToolCount"], 1)
        self.assertEqual(report["failures"][0]["kind"], "tool_execution_failure")
        self.assertEqual(report["symptoms"][-1]["source"], "tool_output")

    def test_profile_duplicate_is_not_another_generation(self):
        evidence = self.evidence()
        evidence["events"] *= 2
        report = analysis.analyze_runner_evidence(evidence)
        self.assertEqual(report["logicalGenerations"], 2)
        self.assertEqual(report["tokens"]["inputTokens"], 100)
        self.assertEqual(report["coverage"]["nativeTimingProfiles"], 1)

    def test_retries_need_observed_reason_and_unchanged_state(self):
        request = {"isContinuation": True, "generationPurpose": "repair", "generationReason": "compaction", "unchangedRelevantState": True, "nextStructuredActionChanged": False}
        result = analysis.classify_model_request(request, classification_complete=True, prior_successful_test={"passed": True}, linked_commands=[{"requiredTest": True}])
        self.assertEqual(result["primary"], "recovery")
        self.assertIsNone(result["interpretation"])
        result = analysis.classify_model_request(request, classification_complete=True, prior_successful_test={"passed": True}, linked_commands=[{"requiredTest": True}], intervening_mutation=False)
        self.assertEqual(result["interpretation"], "redundant_verification")
        result = analysis.classify_model_request({"isContinuation": False, "attemptKind": "retry"}, classification_complete=True, prior_successful_test=None, linked_commands=[])
        self.assertEqual(result["primary"], "retry")

    def test_category_estimates_retain_signed_reconciliation(self):
        request = timing_profile()["modelRequests"][0]
        request["requestTokenCategories"] = {"accountingBasis": "logical_prompt", "baseInstructions": 10, "toolSchemas": 20, "conversationHistory": 50, "currentInput": 20, "logicalTotal": 100, "localInputEstimate": 97, "localReconciliationResidual": -3, "providerInputTokens": 100, "providerReconciliationResidual": 3}
        report = analysis._token_report([request])
        self.assertEqual(report["rankedPromptConsumers"][0]["category"], "conversationHistory")
        self.assertEqual(report["rankedPromptConsumers"][0]["share"], .5)
        self.assertEqual(report["promptCategoryEvidence"]["localReconciliationResidual"], -3)
        self.assertEqual(report["promptCategoryCoverage"], 1)
        self.assertIsNone(analysis._token_report([])["providerTotals"])
        self.assertIsNone(analysis._token_report([])["promptCategories"])

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

    def test_nested_lineage_and_cutoff_evidence_survive(self):
        timing = timing_profile()
        timing["toolCalls"].append({"callId": "nested", "parentCallId": "tool-1", "generationIndex": 0, "outputTruncated": True})
        report = analysis.analyze_runner_evidence(self.evidence(timing), include_tokens=False)
        self.assertEqual(report["directToolCount"], 1)
        self.assertEqual(report["nestedToolCount"], 1)
        self.assertEqual(report["generations"][0]["toolCallIds"], ["tool-1", "nested"])
        self.assertEqual(report["nativeToolCalls"][1]["parentCallId"], "tool-1")
        self.assertEqual(report["symptoms"][0]["kind"], "native_output_projection")
        self.assertFalse(report["symptoms"][0]["causallyEstablished"])


if __name__ == "__main__":
    unittest.main()


