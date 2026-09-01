from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from scripts import kd4_live_agent_benchmark as benchmark


def _run(
    *, outcome_correct: bool, task_contract_compliant: bool
) -> dict[str, object]:
    return {
        "success": outcome_correct,
        "outcomeCorrect": outcome_correct,
        "taskContractCompliant": task_contract_compliant,
        "completionMs": 100.0,
        "ttfoMs": 10.0,
        "diagnostics": [],
        "modelWaitMs": 75.0,
        "continuationCount": 2,
        "actualCommandCount": 4,
        "taskContract": {"successfulTestObserved": task_contract_compliant},
    }


class Kd4LiveAgentBenchmarkTest(unittest.TestCase):
    def test_turn_measurements_use_union_wait_and_continuation_flags(self) -> None:
        event = {
            "type": "turn.completed",
            "timing": {
                "unions": {"modelStreamWaitUnionNs": 12_345_678},
                "modelRequests": [
                    {"isContinuation": False},
                    {"isContinuation": True},
                    {"isContinuation": True},
                ],
            },
        }

        self.assertEqual(benchmark.turn_measurements(event), (12.346, 2))
        self.assertEqual(
            benchmark.turn_measurements({"type": "turn.completed"}),
            (None, None),
        )

    def test_required_test_command_requires_quiet_unittest_execution(self) -> None:
        self.assertTrue(
            benchmark.is_required_test_command("python -m unittest -q")
        )
        self.assertTrue(
            benchmark.is_required_test_command(
                "$env:PYTHONDONTWRITEBYTECODE=1; python.exe -m unittest -q"
            )
        )
        self.assertFalse(
            benchmark.is_required_test_command("python -m unittest")
        )
        self.assertFalse(
            benchmark.is_required_test_command("python -m pytest -q")
        )

    def test_summary_reports_outcome_and_task_contract_separately(self) -> None:
        runs = [
            _run(outcome_correct=True, task_contract_compliant=True),
            _run(outcome_correct=True, task_contract_compliant=False),
            _run(outcome_correct=False, task_contract_compliant=False),
            _run(outcome_correct=False, task_contract_compliant=False),
            _run(outcome_correct=False, task_contract_compliant=False),
        ]

        summary = benchmark.summarize(runs)

        self.assertEqual(summary["successRatePercent"], 40.0)
        self.assertEqual(summary["outcomeCorrectness"]["ratePercent"], 40.0)
        self.assertEqual(
            summary["taskContractCompliance"]["ratePercent"], 20.0
        )
        self.assertEqual(summary["successfulCompletionTime"]["count"], 2)
        self.assertEqual(summary["modelWait"]["averageMs"], 75.0)
        self.assertEqual(summary["continuationCount"]["average"], 2.0)
        self.assertEqual(summary["actualCommandCount"]["average"], 4.0)
        self.assertEqual(summary["testsRan"]["runs"], 1)

    def test_diagnostics_use_structured_categories(self) -> None:
        observed = "\n".join(
            (
                "argument preflight failed: not valid under any of the given schemas",
                "stale_workspace_evidence force_fresh reuse suppression negative cache",
                "request_user_input is not supported in exec mode",
                "apply_patch verification failed: failed to find expected lines",
            )
        )

        diagnostics = benchmark.classify_diagnostics(
            observed_text=observed,
            timed_out=False,
            exit_code=1,
            terminal_event="turn.failed",
            invalid_json_lines=1,
            verifier_passed=False,
            required_test_passed=False,
            command_execution_failures=1,
        )

        categories = {diagnostic["category"] for diagnostic in diagnostics}
        self.assertEqual(
            categories,
            {
                "schema_rejection",
                "freshness_invalidation",
                "reuse_suppression",
                "unsupported_interactive_tool",
                "patch_mismatch",
                "execution_failure",
                "verifier_failure",
                "task_contract_violation",
                "invalid_event_stream",
            },
        )

    def test_exact_source_state_rejects_dirty_contents(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-source-test-") as temp:
            root = Path(temp)
            revision = benchmark.create_fixture(root)

            state = benchmark.exact_source_state(root, revision, "candidate")

            self.assertEqual(state["commit"], revision)
            self.assertTrue(state["clean"])
            self.assertTrue(state["exactContentsReconstructable"])

            (root / "duration.py").write_text(
                benchmark.CORRECT_IMPLEMENTATION,
                encoding="utf-8",
                newline="\n",
            )
            with self.assertRaisesRegex(RuntimeError, "1 dirty paths"):
                benchmark.exact_source_state(root, revision, "candidate")


if __name__ == "__main__":
    unittest.main()
