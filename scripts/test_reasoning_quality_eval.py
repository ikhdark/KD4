from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import reasoning_quality_eval

evaluate_rollout = reasoning_quality_eval.evaluate_rollout


def write_jsonl(path, items):
    path.write_text("\n".join(json.dumps(item) for item in items), encoding="utf-8")


class ReasoningQualityEvalTests(unittest.TestCase):
    def test_repeated_tool_calls_and_missing_validation_after_write(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "shell_command",
                        "arguments": json.dumps({"command": "just fmt"}),
                    },
                    {
                        "type": "function_call",
                        "name": "shell_command",
                        "arguments": json.dumps({"command": "just fmt"}),
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["tool_calls"], 2)
        self.assertEqual(result["repeated_tool_calls"], 1)
        self.assertEqual(result["write_events"], 2)
        self.assertEqual(result["missing_validation_after_write"], 1)
        self.assertLess(result["score"], 100)

    def test_validation_clears_write_gap(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: src.rs\n",
                    },
                    {
                        "type": "function_call",
                        "call_id": "validation-1",
                        "name": "shell_command",
                        "arguments": json.dumps(
                            {"command": "just test-fast -p codex-core foo"}
                        ),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "validation-1",
                        "success": True,
                        "output": "ok",
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["write_events"], 1)
        self.assertEqual(result["validation_commands"], 1)
        self.assertEqual(result["successful_validation_commands"], 1)
        self.assertEqual(result["missing_validation_after_write"], 0)

    def test_failed_validation_does_not_clear_write_gap(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: src.rs\n",
                    },
                    {
                        "type": "function_call",
                        "call_id": "validation-1",
                        "name": "shell_command",
                        "arguments": json.dumps({"command": "cargo test -p codex-core"}),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "validation-1",
                        "success": False,
                        "output": "exit code: 101",
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["validation_commands"], 1)
        self.assertEqual(result["successful_validation_commands"], 0)
        self.assertEqual(result["failed_validation_commands"], 1)
        self.assertEqual(result["missing_validation_after_write"], 1)

    def test_broad_final_claim_without_validation_is_flagged(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: src.rs\n",
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "phase": "final_answer",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "The script is correct now. Tests passed.",
                            }
                        ],
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["broad_final_correctness_claims"], 1)
        self.assertEqual(result["unsupported_final_correctness_claims"], 1)
        self.assertEqual(result["validation_claims_without_receipts"], 1)
        self.assertLess(result["score"], 100)

    def test_python_unittest_counts_as_validation_evidence_for_final_claim(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: src.rs\n",
                    },
                    {
                        "type": "function_call",
                        "call_id": "validation-1",
                        "name": "shell_command",
                        "arguments": json.dumps(
                            {"command": "python -m unittest scripts.test_cargo_lane"}
                        ),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "validation-1",
                        "success": True,
                        "output": "Ran 1 test\n\nOK",
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "phase": "final_answer",
                        "content": "Validation passed for the focused unittest.",
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["validation_commands"], 1)
        self.assertEqual(result["successful_validation_commands"], 1)
        self.assertEqual(result["broad_final_correctness_claims"], 0)
        self.assertEqual(result["unsupported_final_correctness_claims"], 0)
        self.assertEqual(result["validation_claims_without_receipts"], 0)

    def test_pending_validation_does_not_support_final_claim(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: src.rs\n",
                    },
                    {
                        "type": "function_call",
                        "call_id": "validation-1",
                        "name": "shell_command",
                        "arguments": json.dumps({"command": "cargo test -p codex-core"}),
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "phase": "final_answer",
                        "content": "Done. Tests passed.",
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["validation_commands"], 1)
        self.assertEqual(result["successful_validation_commands"], 0)
        self.assertEqual(result["unsupported_final_correctness_claims"], 1)
        self.assertEqual(result["validation_claims_without_receipts"], 1)

    def test_program_args_payload_counts_as_validation_after_success(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: src.rs\n",
                    },
                    {
                        "type": "function_call",
                        "call_id": "validation-1",
                        "name": "shell_command",
                        "arguments": json.dumps(
                            {
                                "kind": "argv",
                                "program": "python",
                                "args": [
                                    "-m",
                                    "unittest",
                                    "scripts.test_reasoning_quality_eval",
                                ],
                            }
                        ),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "validation-1",
                        "output": "OK",
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["validation_commands"], 1)
        self.assertEqual(result["successful_validation_commands"], 1)
        self.assertEqual(result["missing_validation_after_write"], 0)

    def test_nested_final_assistant_message_is_scored(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "response.output_item.done",
                        "item": {
                            "type": "message",
                            "role": "assistant",
                            "phase": "final",
                            "content": "Complete. Validation passed.",
                        },
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["broad_final_correctness_claims"], 1)
        self.assertEqual(result["unsupported_final_correctness_claims"], 1)
        self.assertEqual(result["validation_claims_without_receipts"], 1)

    def test_evaluate_rollout_no_longer_uses_separate_call_and_failure_walks(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "shell_command",
                        "arguments": json.dumps({"command": "just fmt"}),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call-1",
                        "success": False,
                        "output": "exit code: 1",
                    },
                ],
            )

            with (
                mock.patch.object(
                    reasoning_quality_eval,
                    "find_tool_calls",
                    side_effect=AssertionError("old call walk used"),
                ),
                mock.patch.object(
                    reasoning_quality_eval,
                    "find_tool_failures",
                    side_effect=AssertionError("old failure walk used"),
                ),
            ):
                result = evaluate_rollout(rollout)

        self.assertEqual(result["tool_calls"], 1)
        self.assertEqual(result["failed_tool_outputs"], 1)

    def test_tool_command_is_parsed_once_per_call_during_evaluation(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "shell_command",
                        "arguments": json.dumps({"command": "just fmt"}),
                    },
                ],
            )
            original = reasoning_quality_eval.tool_command

            with mock.patch.object(
                reasoning_quality_eval,
                "tool_command",
                side_effect=original,
            ) as tool_command:
                result = evaluate_rollout(rollout)

        self.assertEqual(result["tool_calls"], 1)
        self.assertEqual(tool_command.call_count, 1)

    def test_nonzero_exit_code_output_counts_as_failure(self):
        self.assertTrue(reasoning_quality_eval.output_failed("Exit code: 2"))
        self.assertTrue(reasoning_quality_eval.output_failed("exit code: 101"))
        self.assertFalse(reasoning_quality_eval.output_failed("exit code: 0"))
        self.assertTrue(reasoning_quality_eval.output_failed({"exit_code": 1}))
        self.assertTrue(reasoning_quality_eval.output_failed({"exitCode": 2}))
        self.assertTrue(reasoning_quality_eval.output_failed({"status": "failed"}))
        self.assertFalse(reasoning_quality_eval.output_failed({"exit_code": 0}))

    def test_structured_exit_code_failure_does_not_clear_write_gap(self):
        with tempfile.TemporaryDirectory() as tmp:
            rollout = Path(tmp) / "rollout.jsonl"
            write_jsonl(
                rollout,
                [
                    {
                        "type": "function_call",
                        "name": "apply_patch",
                        "input": "*** Begin Patch\n*** Update File: src.rs\n",
                    },
                    {
                        "type": "function_call",
                        "call_id": "validation-1",
                        "name": "shell_command",
                        "arguments": json.dumps({"command": "cargo test -p codex-core"}),
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "validation-1",
                        "output": {"exit_code": 101, "stderr": "test failed"},
                    },
                ],
            )

            result = evaluate_rollout(rollout)

        self.assertEqual(result["successful_validation_commands"], 0)
        self.assertEqual(result["failed_validation_commands"], 1)
        self.assertEqual(result["missing_validation_after_write"], 1)


if __name__ == "__main__":
    unittest.main()
