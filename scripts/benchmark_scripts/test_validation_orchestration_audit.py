"""Contracts for the validation-orchestration audit.

The expectations here come from the recorded rollout format and from what the
audit claims, not from the implementation's current output: a generation's
cost is the request that produced its tool calls, `modelOnlyNs` is cumulative
within a turn, and chronological relaunch candidates remain unverified: call text does
not establish input identity or that execution was unnecessary.
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.benchmark_scripts.validation_orchestration_audit import (  # noqa: E402
    analyse,
    read_generations,
)


def _write(path: Path, records: list[dict]) -> Path:
    path.write_text(
        "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
    )
    return path


def _call(text: str) -> dict:
    return {
        "type": "response_item",
        "timestamp": "2026-10-14T00:00:01.000Z",
        "payload": {"type": "custom_tool_call", "name": "exec", "input": text},
    }


def _output(text: str) -> dict:
    return {
        "type": "response_item",
        "timestamp": "2026-10-14T00:00:03.000Z",
        "payload": {"type": "custom_tool_call_output", "output": text},
    }


def _usage(**kwargs: int) -> dict:
    usage = {
        "input_tokens": kwargs.get("input_tokens", 100),
        "cached_input_tokens": kwargs.get("cached_input_tokens", 0),
        "output_tokens": kwargs.get("output_tokens", 10),
        "reasoning_output_tokens": kwargs.get("reasoning_output_tokens", 0),
    }
    return {
        "type": "token_usage_record",
        "timestamp": "2026-10-14T00:00:02.000Z",
        "payload": {"response_id": "r", "turn_id": "t", "usage": usage},
    }


def _boundary(turn: str, model_ns: int) -> dict:
    return {
        "type": "sampling_boundary",
        "timestamp": "2026-10-14T00:00:02.000Z",
        "payload": {
            "turn_id": turn,
            "timing_checkpoint": {"timing": {"exclusive": {"modelOnlyNs": model_ns}}},
        },
    }


class AttributionTests(unittest.TestCase):
    def setUp(self) -> None:
        owned = tempfile.TemporaryDirectory()
        self.addCleanup(owned.cleanup)
        self.tmp = Path(owned.name)

    def test_calls_attach_to_the_request_that_produced_them(self) -> None:
        """A rollout records `call -> usage -> output`, so the call belongs to
        the usage record that follows it, and the output to the one before."""
        path = _write(
            self.tmp / "a.jsonl",
            [
                _call("just core-test-fast core_lib -E 'test(x)'"),
                _usage(),
                _output("test result: ok. 3 passed"),
            ],
        )
        generations = read_generations(path)
        self.assertEqual(len(generations), 1)
        self.assertEqual(generations[0].role, "launch")
        self.assertIn("3 passed", "\n".join(generations[0].outputs))

    def test_token_count_events_are_read_as_per_request_usage(self) -> None:
        """Older rollouts carry usage as `token_count.info.last_token_usage`.
        Ignoring that shape reported zero generations for 12 real rollouts."""
        path = _write(
            self.tmp / "b.jsonl",
            [
                _call("cargo test -p codex-core"),
                {
                    "type": "event_msg",
                    "timestamp": "2026-10-14T00:00:02.000Z",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 50,
                                "cached_input_tokens": 10,
                                "output_tokens": 5,
                                "reasoning_output_tokens": 2,
                            }
                        },
                    },
                },
            ],
        )
        generations = read_generations(path)
        self.assertEqual(len(generations), 1)
        self.assertEqual(generations[0].input_tokens, 50)
        self.assertEqual(generations[0].reasoning_tokens, 2)
        self.assertEqual(generations[0].uncached_input, 40)

    def test_model_seconds_are_the_rise_within_a_turn(self) -> None:
        """`modelOnlyNs` is cumulative per turn and resets at a new turn, so a
        request costs the rise, not the running total."""
        path = _write(
            self.tmp / "c.jsonl",
            [
                _boundary("t1", 2_000_000_000),
                _call("just core-test-fast core_lib -E 'test(a)'"),
                _usage(),
                _boundary("t1", 5_000_000_000),
                _call("just core-test-fast core_lib -E 'test(b)'"),
                _usage(),
                _boundary("t2", 1_000_000_000),
                _call("just core-test-fast core_lib -E 'test(c)'"),
                _usage(),
            ],
        )
        generations = read_generations(path)
        self.assertEqual(
            [round(g.model_seconds, 3) for g in generations], [2.0, 3.0, 1.0]
        )


class RedundancyTests(unittest.TestCase):
    def setUp(self) -> None:
        owned = tempfile.TemporaryDirectory()
        self.addCleanup(owned.cleanup)
        self.tmp = Path(owned.name)

    def test_relaunch_after_a_patch_in_a_template_literal_is_not_redundant(
        self,
    ) -> None:
        """Code-mode builds patches inside JS strings. Matching only the tool
        name missed 59 of 147 flagged cases and reported them as redundant."""
        path = _write(
            self.tmp / "d.jsonl",
            [
                _call("just core-test-fast core_lib -E 'test(a)'"),
                _usage(),
                _output("1 failed"),
                _call('const p=`*** Begin Patch\n*** Update File: x.rs\n`;'),
                _usage(),
                _output("ok"),
                _call("just core-test-fast core_lib -E 'test(a)'"),
                _usage(),
                _output("test result: ok"),
            ],
        )
        self.assertEqual(analyse(path)["unverified_relaunches"], 0)

    def test_relaunch_that_only_reruns_the_same_check_is_unverified(self) -> None:
        path = _write(
            self.tmp / "e.jsonl",
            [
                _call("just core-test-fast core_lib -E 'test(a)'"),
                _usage(),
                _output("test result: ok"),
                _call("just core-test-fast core_lib -E 'test(a)'"),
                _usage(),
                _output("test result: ok"),
            ],
        )
        self.assertEqual(analyse(path)["unverified_relaunches"], 1)

    def test_a_relaunch_that_inspects_is_not_counted_as_redundant(self) -> None:
        """Narrowing after reading the failure is new work, not a rerun."""
        path = _write(
            self.tmp / "f.jsonl",
            [
                _call("just core-test-fast core_lib -E 'test(a)'"),
                _usage(),
                _output("1 failed"),
                _call(
                    'text(await tools.read_file({path:"x.rs"}));'
                    " just core-test-fast core_lib -E 'test(a::inner)'"
                ),
                _usage(),
                _output("test result: ok"),
            ],
        )
        self.assertEqual(analyse(path)["unverified_relaunches"], 0)


if __name__ == "__main__":
    unittest.main()
