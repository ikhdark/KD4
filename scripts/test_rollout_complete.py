from __future__ import annotations

import copy
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import rollout_reports as dump


def encode(rows):
    return "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows).encode()


def expand(report, pointer):
    """Resolve the documented JSON pointers, including references within references."""
    if pointer in report["references"]:
        return expand(report, report["references"][pointer])
    value = report["records"]
    for part in pointer.split("/")[1:]:
        key = part.replace("~1", "/").replace("~0", "~")
        value = value[int(key)] if isinstance(value, list) else value[key]
    if isinstance(value, dict):
        return {
            key: expand(report, pointer + "/" + key.replace("~", "~0").replace("/", "~1"))
            for key in value
        }
    if isinstance(value, list):
        return [expand(report, f"{pointer}/{i}") for i in range(len(value))]
    return value


class CompleteRolloutTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.source = self.root / "session.jsonl"
        self.output = self.root / "report.txt"

    def report(self, rows):
        self.source.write_bytes(encode(rows))
        dump.dump_complete(self.source, self.output)
        return json.loads(self.output.read_text(encoding="utf-8"))

    def test_all_fields_round_trip_with_exact_references(self):
        text = "λ😀\r\n" * 2000 + "important-tail"
        payload = {"a/b~c": text, "literalNull": None, "unknownTiming": [1, {}, []]}
        rows = [
            {"payload": payload, "extra": text},
            {"payload": copy.deepcopy(payload), "extra": {"references": None}},
            {"payload": {"changed": text + "changed"}},
        ]
        report = self.report(rows)
        self.assertEqual([expand(report, f"/{i}") for i in range(3)], rows)
        self.assertEqual(report["references"]["/0/extra"], "/0/payload/a~1b~0c")
        self.assertEqual(report["references"]["/1/payload"], "/0/payload")
        self.assertEqual(report["opaqueEncryptedFields"], {})
        self.assertEqual(report["recordCount"], 3)
        self.assertEqual(
            report["snapshot"]["sha256"], hashlib.sha256(encode(rows)).hexdigest()
        )

    def test_opaque_reasoning_is_explicit_without_hiding_plaintext(self):
        encrypted = "opaque-ciphertext" * 100
        rows = [{"payload": {
            "encrypted_content": encrypted,
            "summary": "visible summary",
            "unknown": {"encrypted_content": "λ"},
        }}]
        report = self.report(rows)
        self.assertNotIn(encrypted, self.output.read_text(encoding="utf-8"))
        self.assertEqual(report["records"], [{"payload": {
            "encrypted_content": None,
            "summary": "visible summary",
            "unknown": {"encrypted_content": None},
        }}])
        self.assertEqual(report["opaqueEncryptedFields"]["/0/payload/encrypted_content"], {
            "utf8Bytes": len(encrypted.encode()),
            "sha256": hashlib.sha256(encrypted.encode()).hexdigest(),
        })
        self.assertEqual(
            report["opaqueEncryptedFields"]["/0/payload/unknown/encrypted_content"],
            {"utf8Bytes": 2, "sha256": hashlib.sha256("λ".encode()).hexdigest()},
        )
        self.assertEqual(len(report["opaqueEncryptedFields"]), 2)
        self.assertEqual(report["references"], {})

    def test_invalid_or_partial_records_do_not_replace_a_report(self):
        for invalid in [b'{"unfinished":', b'{"invalid":}\n', b'[]\n', b'\xff\n']:
            with self.subTest(invalid=invalid):
                self.source.write_bytes(encode([{"complete": True}]) + invalid)
                self.output.write_text("previous report", encoding="utf-8")
                with self.assertRaises(ValueError):
                    dump.dump_complete(self.source, self.output)
                self.assertEqual(self.output.read_text(encoding="utf-8"), "previous report")

    def test_live_append_does_not_change_snapshot_identity_or_coverage(self):
        initial = encode([{"payload": "first"}])
        self.source.write_bytes(initial)
        snapshot = dump.read_rollout_snapshot(self.source)
        self.source.write_bytes(initial + encode([{"payload": "later"}]))
        with mock.patch.object(dump, "read_rollout_snapshot", return_value=snapshot) as read:
            dump.dump_complete(self.source, self.output)
        read.assert_called_once_with(self.source)
        report = json.loads(self.output.read_text(encoding="utf-8"))
        self.assertEqual(report["recordCount"], 1)
        self.assertEqual(report["snapshot"]["sha256"], hashlib.sha256(initial).hexdigest())
        self.assertTrue(snapshot.stream.closed)

    def test_source_overwrite_is_rejected(self):
        initial = encode([{"payload": "retained"}])
        self.source.write_bytes(initial)
        with self.assertRaisesRegex(ValueError, "must not overwrite"):
            dump.dump_complete(self.source, self.source)
        self.assertEqual(self.source.read_bytes(), initial)

    def test_complete_cli_uses_the_complete_renderer(self):
        self.source.write_bytes(encode([{"unknown": "tail" * 2000}]))
        result = subprocess.run(
            [sys.executable, "-B", str(Path(dump.__file__)), "dump", "--complete",
             str(self.root / "reports"), str(self.source)],
            capture_output=True, timeout=30, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(
            (self.root / "reports" / "session.txt").read_text(encoding="utf-8")
        )
        self.assertEqual(expand(report, "/0"), {"unknown": "tail" * 2000})


if __name__ == "__main__":
    unittest.main()
