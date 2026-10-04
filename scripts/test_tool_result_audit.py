from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from scripts.tool_result_audit import audit


class ToolResultAuditTest(unittest.TestCase):
    def test_exact_bytes_references_and_mentions_are_not_conflated(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            path = home / "sessions" / "sample.jsonl"
            path.parent.mkdir()
            artifact_id = "00000000-0000-0000-0000-000000000001"
            directory = home / "tool-output" / "session"
            directory.mkdir(parents=True)
            evidence = "λ exact\r\n".encode()
            digest = hashlib.sha256(evidence).hexdigest()
            (directory / f"{artifact_id}.log").write_bytes(evidence)
            (directory / f"{artifact_id}.meta.json").write_text(
                json.dumps(
                    {
                        "canonical_bytes": len(evidence),
                        "canonical_sha256": digest,
                    }
                ),
                encoding="utf-8",
            )
            output = json.dumps(
                {"output": "λ", "exit_code": 0, "artifact_id": artifact_id},
                ensure_ascii=False,
            )
            records = [{"type": "session_meta", "payload": {"id": "session"}}]
            for index in range(2):
                records.extend(
                    [
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "custom_tool_call",
                                "name": "exec",
                                "call_id": str(index),
                                "input": "const r=await tools.exec_command({});text(r.output);text(r.exit_code);",
                            },
                        },
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "custom_tool_call_output",
                                "call_id": str(index),
                                "output": output,
                            },
                        },
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "reasoning",
                                "summary": [
                                    {"text": "exit_code establishes completion"}
                                ],
                            },
                        },
                        {
                            "type": "tool_manifest",
                            "payload": {"manifest": ["same schema"]},
                        },
                    ]
                )
            data = b"".join(
                json.dumps(row, ensure_ascii=False).encode() + b"\n" for row in records
            )
            path.write_bytes(data)
            report = audit(path)
            self.assertEqual(report["records"], len(records))
            self.assertEqual(
                report["snapshot"]["sha256"], hashlib.sha256(data).hexdigest()
            )
            self.assertEqual(report["snapshot"]["byteLength"], len(data))
            self.assertEqual(report["manifest_records"], 2)
            self.assertEqual(report["distinct_manifest_hashes"], 1)
            first, second = report["results"]
            self.assertEqual(first["visible_bytes"], len(output.encode()))
            self.assertEqual(
                first["visible_estimated_tokens"], (len(output.encode()) + 3) // 4
            )
            self.assertEqual(first["producer_field_mentions"], ["output", "exit_code"])
            self.assertEqual(first["later_reasoning_field_mentions"], ["exit_code"])
            self.assertIsNone(first["exact_duplicate_of"])
            self.assertEqual(second["exact_duplicate_of"], "0")
            self.assertEqual(len(report["artifacts"]), 1)
            self.assertTrue(report["artifacts"][0]["canonical_verified"])
            self.assertEqual(
                report["artifacts"][0]["retained_file_bytes"], len(evidence)
            )
            self.assertNotEqual(len(evidence), first["visible_bytes"])

    def test_missing_evidence_and_incomplete_tail_remain_explicit(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "sample.jsonl"
            row = {
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "unknown",
                    "output": '{"artifact_id":"00000000-0000-0000-0000-000000000001"}',
                },
            }
            prefix = json.dumps(row).encode() + b"\n"
            path.write_bytes(prefix + b'{"partial":')
            report = audit(path)
            self.assertTrue(report["incomplete_tail"])
            self.assertEqual(report["records"], 1)
            self.assertFalse(report["artifacts"][0]["available"])
            self.assertNotIn("retained_file_bytes", report["artifacts"][0])
            path.write_bytes(prefix + b'{"partial":\n')
            with self.assertRaisesRegex(ValueError, "invalid complete record"):
                audit(path)


if __name__ == "__main__":
    unittest.main()
