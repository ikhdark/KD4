from __future__ import annotations

import contextlib
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts import _rollout_analyze2 as analyzer
from scripts import _rollout_dump as full_dump
from scripts import _rollout_narrative as narrative
from scripts import rollout_snapshot


def record(kind, payload, second=0):
    return {
        "timestamp": f"2026-09-26T00:00:{second:02d}Z",
        "type": kind,
        "payload": payload,
    }


def encode(rows):
    return b"".join(
        (json.dumps(row, ensure_ascii=False) + "\n").encode("utf-8") for row in rows
    )


def native_timing():
    return {
        "profileValid": True,
        "inclusiveDurationNs": 20_000_000_000,
        "unions": {
            "modelActiveUnionNs": 5_000_000_000,
            "toolActiveUnionNs": 10_000_000_000,
            "interactiveWaitUnionNs": 0,
        },
        "counters": {"modelRequestCount": 2, "logicalGenerationCount": 2},
        "modelRequests": [
            {
                "samplingRequestId": "primary",
                "tokenUsage": {
                    "inputTokens": 100,
                    "cachedInputTokens": 80,
                    "visibleOutputTokens": 10,
                    "reasoningTokens": 2,
                },
            },
            {
                "samplingRequestId": "compaction",
                "generationReason": "compaction",
                "tokenUsage": {
                    "inputTokens": 50,
                    "cachedInputTokens": 20,
                    "visibleOutputTokens": 5,
                    "reasoningTokens": 1,
                },
            },
        ],
    }


class RolloutReportsTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.path = self.root / "rollout.jsonl"
        self.output = self.root / "report.txt"

    def analyze(self, rows):
        self.path.write_bytes(encode(rows))
        with contextlib.redirect_stdout(io.StringIO()) as output:
            analyzer.analyze(self.path)
        return output.getvalue()

    def test_native_timing_and_compaction_usage_replace_event_estimates(self):
        token_event = record(
            "event_msg",
            {
                "type": "token_count",
                "info": {"last_token_usage": {"input_tokens": 100}},
            },
        )
        output = self.analyze(
            [
                record("event_msg", {"type": "task_started"}),
                record("sampling_boundary", {}, 1),
                record("response_item", {"type": "message", "role": "assistant"}, 4),
                record(
                    "response_item",
                    {"type": "function_call", "name": "read", "call_id": "c"},
                    6,
                ),
                token_event,
                token_event,
                record(
                    "event_msg",
                    {"type": "task_complete", "timing": native_timing()},
                    20,
                ),
            ]
        )
        self.assertIn("wall=20.0s boundaries=1 generations=2", output)
        self.assertIn(
            "model_active=5.0s tool_active=10.0s interactive_wait=0.0s", output
        )
        self.assertIn("in=150 cached=100 uncached=50 out=18 usage=complete", output)

    def test_partial_missing_and_invalid_native_evidence_remain_explicit(self):
        partial = native_timing()
        partial["modelRequests"].pop()
        invalid = {**native_timing(), "profileValid": False}
        for timing in (partial, invalid, None):
            with self.subTest(timing=timing):
                output = self.analyze(
                    [record("event_msg", {"type": "turn_aborted", "timing": timing})]
                )
                self.assertIn("in=? cached=? uncached=? out=?", output)
                if timing is partial:
                    self.assertIn(
                        "usage=partial request_retention=False observed_in=100", output
                    )
                else:
                    self.assertIn("wall=?", output)
                    self.assertIn("model_active=?", output)
                    self.assertIn("usage=unavailable", output)

    def test_world_state_merges_nested_patches_and_detects_removals(self):
        initial = {
            "agents_md": "same",
            "environments": {"cwd": "a", "shell": "ps"},
            "skills": {"tool": "present"},
        }
        updated = {**initial, "environments": {"cwd": "b", "shell": "ps"}}
        deleted = {**updated, "skills": {}}
        output = self.analyze(
            [
                record("world_state", {"full": True, "state": initial}),
                record(
                    "world_state",
                    {"full": False, "state": {"environments": {"cwd": "b"}}},
                ),
                record("world_state", {"full": True, "state": updated}),
                record(
                    "world_state", {"full": False, "state": {"skills": {"tool": None}}}
                ),
                record("world_state", {"full": True, "state": deleted}),
                record(
                    "world_state",
                    {
                        "full": True,
                        "state": {k: v for k, v in deleted.items() if k != "agents_md"},
                    },
                ),
            ]
        )
        lines = [line for line in output.splitlines() if "idx=" in line]
        self.assertIn("changed=['environments']", lines[1])
        self.assertIn("changed=[]", lines[2])
        self.assertIn("changed=['skills']", lines[3])
        self.assertIn("changed=[]", lines[4])
        self.assertIn("changed=['agents_md']", lines[5])

    def test_byte_inventory_and_analysis_share_one_snapshot_during_append(self):
        initial = encode([record("session_meta", {"id": "日本語"})])
        appended = encode([record("newer_record_type", {})])
        self.path.write_bytes(initial)
        read_snapshot = rollout_snapshot.read_rollout_snapshot
        captured = []

        def append_after_capture(path):
            snapshot = read_snapshot(path)
            captured.append(snapshot)
            with path.open("ab") as writer:
                writer.write(appended)
            return snapshot

        with (
            mock.patch.object(
                rollout_snapshot,
                "read_rollout_snapshot",
                side_effect=append_after_capture,
            ) as reader,
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            analyzer.analyze(self.path)
        reader.assert_called_once_with(self.path)
        self.assertEqual(self.path.read_bytes(), initial + appended)
        self.assertTrue(captured[0].stream.closed)
        self.assertNotIn("newer_record_type", output.getvalue())
        self.assertIn(f"{len(initial):>10,} ('session_meta', None)", output.getvalue())

    def test_changed_manifests_and_long_delta_contracts_are_preserved(self):
        manifest = {
            "model_visible": [
                {
                    "name": "tool",
                    "description": "original-contract",
                    "parameters": {"schema": "s" * 4000 + "schema-end"},
                }
            ]
        }
        changed = copy.deepcopy(manifest)
        changed["model_visible"][0]["description"] = "changed-contract"
        delta = [{"name": "other", "value": {"description": "d" * 4000 + "delta-end"}}]
        self.path.write_bytes(
            encode(
                [
                    record("tool_manifest", {"hash": "a", "manifest": manifest}),
                    record("tool_manifest", {"hash": "a", "manifest": manifest}),
                    record("tool_manifest", {"hash": "b", "manifest": changed}),
                    record(
                        "tool_manifest", {"hash": "c", "base_hash": "b", "added": delta}
                    ),
                ]
            )
        )
        full_dump.dump(self.path, self.output)
        output = self.output.read_text(encoding="utf-8")
        self.assertEqual(output.count("original-contract"), 1)
        self.assertEqual(output.count("changed-contract"), 1)
        self.assertEqual(output.count("schema-end"), 2)
        self.assertIn("delta-end", output)
        self.assertIn("manifest already shown", output)

    def test_cache_fingerprints_are_aliased_without_losing_request_mapping(self):
        requests = [
            {"promptCacheKeyFingerprint": key} for key in ("a" * 64, "a" * 64, "b" * 64)
        ] + [{}]
        self.path.write_bytes(
            encode(
                [
                    record(
                        "event_msg",
                        {
                            "type": "task_complete",
                            "timing": {"modelRequests": requests},
                        },
                    )
                ]
            )
        )
        full_dump.dump(self.path, self.output)
        output = self.output.read_text(encoding="utf-8")
        self.assertEqual(output.count("a" * 64), 1)
        self.assertEqual(output.count("b" * 64), 1)
        self.assertEqual(output.count('"promptCacheKey": "cache-key-1"'), 2)
        self.assertEqual(output.count('"promptCacheKey": "cache-key-2"'), 1)
        self.assertIn('"promptCacheKey": null', output)

    def test_narrative_keeps_long_inputs_and_compacted_inputs_but_bounds_outputs(self):
        arguments = "a" * 7000 + "important-input" + "z" * 7000
        tool_output = (
            "o" * (narrative.OUT_LIMIT + 4000)
            + "omitted-output-middle"
            + "o" * (narrative.OUT_LIMIT + 4000)
        )
        call = {"type": "function_call", "name": "exec", "arguments": arguments}
        self.path.write_bytes(
            encode(
                [
                    record("response_item", call),
                    record(
                        "response_item",
                        {"type": "function_call_output", "output": tool_output},
                    ),
                    record("compacted", {"replacement_history": [call]}),
                ]
            )
        )
        narrative.dump(self.path, self.output)
        output = self.output.read_text(encoding="utf-8")
        self.assertEqual(output.count(arguments), 2)
        self.assertNotIn("omitted-output-middle", output)
        self.assertIn("chars omitted", output)

    def test_narrative_cli_help_and_argument_errors_do_not_write(self):
        self.path.write_bytes(encode([record('session_meta', {'id': 'retained'})]))
        before = {path.relative_to(self.root): path.read_bytes()
                  for path in self.root.rglob('*') if path.is_file()}
        cases = (
            (['--help'], 0),
            ([], 2),
            (['reports'], 2),
            (['reports', str(self.path), f'{self.path}@invalid'], 2),
        )
        for arguments, status in cases:
            with self.subTest(arguments=arguments):
                result = subprocess.run(
                    [sys.executable, '-B', str(Path(narrative.__file__)), *arguments],
                    cwd=self.root, capture_output=True, encoding='utf-8', timeout=30,
                    creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0),
                )
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertIn('usage:', result.stdout if status == 0 else result.stderr)
                self.assertEqual(set(self.root.iterdir()), {self.path})
                self.assertEqual(
                    {path.relative_to(self.root): path.read_bytes()
                     for path in self.root.rglob('*') if path.is_file()}, before,
                )

    def test_narrative_cli_preserves_multiple_inputs_and_start_index(self):
        self.path.write_bytes(encode([
            record('response_item', {'type': 'message', 'role': 'user',
                                     'content': [{'text': 'first record'}]}),
            record('response_item', {'type': 'message', 'role': 'assistant',
                                     'content': [{'text': 'second record'}]}, 1),
        ]))
        result = subprocess.run(
            [sys.executable, '-B', str(Path(narrative.__file__)), 'reports',
             str(self.path), f'{self.path}@1'],
            cwd=self.root, capture_output=True, encoding='utf-8', timeout=30,
            creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        full = (self.root / 'reports' / 'rollout.narr.txt').read_text(encoding='utf-8')
        suffix = (self.root / 'reports' / 'rollout.narr1.txt').read_text(encoding='utf-8')
        self.assertIn('first record', full)
        self.assertIn('second record', full)
        self.assertNotIn('first record', suffix)
        self.assertIn('second record', suffix)
        self.assertEqual(result.stdout.count('wrote '), 2)

    def test_reader_rejects_interior_corruption_and_nonobjects_before_output(self):
        prefix = encode([record("session_meta", {})])
        for bad in (b"broken\n", b"[]\n", b"[]", b"\xff\n"):
            for dump in (full_dump.dump, narrative.dump):
                with self.subTest(bad=bad, dump=dump):
                    self.path.write_bytes(prefix + bad)
                    self.output.write_text("existing report", encoding="utf-8")
                    with self.assertRaises(ValueError):
                        dump(self.path, self.output)
                    self.assertEqual(
                        self.output.read_text(encoding="utf-8"), "existing report"
                    )

    def test_reader_accepts_valid_unterminated_record_and_warns_on_partial_utf8(self):
        data = encode([record("session_meta", {"id": "last"})]).rstrip(b"\n")
        self.path.write_bytes(data)
        self.assertEqual(
            rollout_snapshot.read_rollout_records(self.path),
            [(json.loads(data), len(data))],
        )
        self.path.write_bytes(data + b"\n" + b'{"type":"\xe2\x82')
        with contextlib.redirect_stderr(io.StringIO()) as stderr:
            records = rollout_snapshot.read_rollout_records(self.path)
        self.assertEqual(records, [(json.loads(data), len(data) + 1)])
        self.assertIn("complete prefix only", stderr.getvalue())

    def test_all_report_clis_preserve_complete_prefix_after_partial_tail(self):
        self.path.write_bytes(
            encode(
                [
                    record("session_meta", {"id": "test"}),
                    record("event_msg", {"type": "task_started"}),
                    record(
                        "event_msg",
                        {"type": "task_complete", "timing": native_timing()},
                        20,
                    ),
                ]
            )
            + b'{"type":'
        )
        scripts = Path(__file__).parent
        for name in (
            "_rollout_analyze",
            "_rollout_analyze2",
            "_rollout_dump",
            "_rollout_narrative",
        ):
            with self.subTest(name=name):
                arguments = [sys.executable, "-B", str(scripts / f"{name}.py")]
                if name in ("_rollout_dump", "_rollout_narrative"):
                    arguments.append(str(self.root / name))
                elif name == "_rollout_analyze":
                    arguments.append("-v")
                arguments.append(str(self.path))
                result = subprocess.run(
                    arguments,
                    capture_output=True,
                    encoding="utf-8",
                    timeout=30,
                    env={**os.environ, "PYTHONIOENCODING": "utf-8"},
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("complete prefix only", result.stderr)
                if name in ("_rollout_dump", "_rollout_narrative"):
                    files = list((self.root / name).glob("*.txt"))
                    self.assertEqual(len(files), 1)
                    output = files[0].read_text(encoding="utf-8")
                else:
                    output = result.stdout
                self.assertIn("task_complete", output)


if __name__ == "__main__":
    unittest.main()
