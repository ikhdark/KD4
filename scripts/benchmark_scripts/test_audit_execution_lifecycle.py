"""Lifecycle projections preserve source identity and disclose damaged evidence."""
from __future__ import annotations

import contextlib
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest

from scripts.benchmark_scripts import audit_execution_lifecycle as lifecycle


def row(second, kind, **payload):
    timestamp = f"2026-10-01T00:00:{second:02d}Z" if isinstance(second, int) else second
    return {"timestamp": timestamp, "type": kind, "payload": payload}


def event(second, subtype, turn_id="a", **payload):
    return row(second, "event_msg", type=subtype, turn_id=turn_id, **payload)


def call(second, call_id="c", name="functions.exec_command", **extra):
    return row(second, "response_item", type="function_call", call_id=call_id, name=name, **extra)


class LifecycleEvidenceTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "rollout.jsonl"

    def write(self, rows):
        self.path.write_text("\n".join(r if isinstance(r, str) else json.dumps(r) for r in rows) + "\n",
                             encoding="utf-8")

    def test_compact_projection_references_exact_large_records(self):
        secret = "private tool/message body " * 50000
        self.write([event(0, "task_started"), call(1, arguments=secret),
                    row(2, "sampling_boundary", turn_id="a", sampling_request_id="r",
                        counters={"count": 2, "not_counter": secret}, history=secret),
                    event(3, "task_complete", timing={"schemaVersion": 25, "profileValid": True,
                        "milestones": {"firstUsefulActionMs": 1}, "toolCalls": [secret]})])
        compact = lifecycle.capture(self.path, compact=True)
        full = lifecycle.capture(self.path)
        self.assertNotIn("private tool/message body", json.dumps(compact))
        self.assertEqual(full["observations"][1]["payload"]["arguments"], secret)
        self.assertEqual(compact["observations"][2]["lifecycle"]["counters"], {"count": 2})
        data = self.path.read_bytes()
        offset = 0
        for observed, source in zip(compact["observations"], data.splitlines(keepends=True)):
            ref = observed["source_ref"]
            self.assertEqual(ref["sha256"], hashlib.sha256(data).hexdigest())
            self.assertEqual((ref["expanded_byte_offset"], ref["expanded_bytes"]), (offset, len(source)))
            offset += len(source)
        self.assertEqual(compact["counts"], full["counts"])
        self.assertLess(len(json.dumps(compact)), 10000)

    def test_error_flood_bounds_examples_preserves_counts_and_positions(self):
        self.write(["malformed"] * 100 + [event(0, "task_started")] + ["malformed"] * 50)
        report = lifecycle.capture(self.path, compact=True, error_example_limit=2)
        self.assertEqual(report["records"], 151)
        self.assertEqual(report["error_count"], 150)
        self.assertEqual(report["omitted_error_examples"], 148)
        self.assertEqual(report["error_line_ranges"], [[1, 100], [102, 151]])
        self.assertEqual([error["line"] for error in report["errors"]], [1, 2])
        output = self.path.with_suffix(".json")
        with contextlib.redirect_stdout(io.StringIO()):
            code = lifecycle.main([str(self.path), "--output", str(output), "--compact", "--error-example-limit", "0"])
        self.assertEqual(code, 1)
        self.assertFalse(json.loads(output.read_text())["complete"])

    def test_identical_replay_and_conflicting_owner_are_distinct(self):
        rows = [event(0, "task_started"), call(1), call(1),
                row(2, "response_item", type="function_call_output", call_id="c", output="done")]
        self.write(rows)
        report = lifecycle.capture(self.path)
        self.assertEqual(report["replayed_call_ids"], 1)
        self.assertEqual(report["observations"][-1]["turn_id"], "a")
        self.write(rows + [event(3, "task_started", "b"), call(4, turn_id="b"), rows[-1]])
        report = lifecycle.capture(self.path)
        self.assertEqual(report["conflicting_call_ids"], ["c"])
        self.assertIsNone(report["observations"][-1]["turn_id"])

    def test_overlap_records_displacement_and_unresolved_turn(self):
        self.write([event(0, "task_started"), call(1), event(2, "task_started", "b"),
                    row(3, "response_item", type="function_call_output", call_id="c", output="done"),
                    event(4, "task_complete", "b")])
        report = lifecycle.capture(self.path, compact=True)
        self.assertEqual(report["ambiguities"][0]["category"], "overlapping_turn_starts")
        self.assertEqual(report["ambiguities"][0]["displaced_turn"]["turn_id"], "a")
        self.assertEqual(report["turns"][0]["status"], "unresolved")
        self.assertEqual(report["turns"][1]["status"], "task_complete")
        self.assertEqual(report["observations"][3]["turn_id"], "a")

    def test_many_overlaps_retain_each_identity_without_quadratic_active_copies(self):
        self.write([event(0, "task_started", str(index)) for index in range(300)])
        report = lifecycle.capture(self.path, compact=True)
        self.assertEqual(len(report["turns"]), 300)
        self.assertTrue(all(turn["status"] == "unresolved" for turn in report["turns"]))
        self.assertEqual(report["ambiguities"][-1]["unresolved_turn_count"], 299)
        self.assertTrue(all("unresolved_start_lines" not in entry for entry in report["ambiguities"]))


if __name__ == "__main__":
    unittest.main()
