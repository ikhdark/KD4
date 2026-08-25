from __future__ import annotations

import contextlib
import hashlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_turn_latency_audit
from scripts import rollout_snapshot


class RolloutSnapshotTest(unittest.TestCase):
    def test_snapshot_stays_fixed_after_open_writer_appends(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "rollout.jsonl"
            initial = b'{"type":"session_meta","payload":{}}\n'
            appended = b'{"type":"event_msg","payload":{"type":"task_started"}}\n'

            with path.open("ab", buffering=0) as writer:
                writer.write(initial)
                snapshot = rollout_snapshot.read_rollout_snapshot(path)
                writer.write(appended)

            self.assertEqual(snapshot.data, initial)
            self.assertEqual(snapshot.byte_length, len(initial))
            self.assertEqual(snapshot.sha256, hashlib.sha256(initial).hexdigest())
            self.assertEqual(path.read_bytes(), initial + appended)

    def test_cli_writes_the_captured_bytes_and_reports_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "rollout.jsonl"
            output = Path(temp) / "snapshot.jsonl"
            data = b'{"type":"session_meta","payload":{}}\n'
            path.write_bytes(data)
            stdout = io.StringIO()

            with contextlib.redirect_stdout(stdout):
                exit_code = rollout_snapshot.main([str(path), "--output", str(output)])

            metadata = json.loads(stdout.getvalue())
            self.assertEqual(exit_code, 0)
            self.assertEqual(output.read_bytes(), data)
            self.assertEqual(metadata["path"], str(path.resolve()))
            self.assertEqual(metadata["output"], str(output.resolve()))
            self.assertEqual(metadata["byteLength"], len(data))
            self.assertEqual(metadata["sha256"], hashlib.sha256(data).hexdigest())

    def test_analyzers_report_the_exact_snapshot_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            root.mkdir()
            path = Path(temp) / "rollout.jsonl"
            data = (
                json.dumps(
                    {
                        "timestamp": "2026-08-18T00:00:00Z",
                        "type": "session_meta",
                        "payload": {"cwd": str(root)},
                    }
                )
                + "\n"
            ).encode()
            path.write_bytes(data)
            expected = {
                "path": str(path.resolve()),
                "byteLength": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            }

            with mock.patch.object(
                kd4_turn_latency_audit,
                "read_rollout_snapshot",
                wraps=rollout_snapshot.read_rollout_snapshot,
            ) as snapshot_reader:
                latency = kd4_turn_latency_audit.analyze_session_path(path, root)

            self.assertEqual(latency["coverage"]["snapshots"], [expected])
            self.assertEqual(
                latency["firstUsefulActionAnalysis"]["sourceSnapshots"],
                [expected],
            )
            self.assertEqual(latency["coverage"]["bytes"], len(data))
            snapshot_reader.assert_called_once_with(path)


if __name__ == "__main__":
    unittest.main()
