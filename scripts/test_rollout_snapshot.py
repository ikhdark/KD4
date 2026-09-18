from __future__ import annotations

import contextlib
import hashlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

try:
    from compression import zstd
except ImportError:
    from backports import zstd

from scripts import kd4_first_useful_action_analysis
from scripts import kd4_turn_latency_audit
from scripts import rollout_snapshot


class RolloutSnapshotTest(unittest.TestCase):
    def test_compressed_rollout_cli_accepts_path_directory_and_uuid(self):
        session_id = "01234567-89ab-cdef-0123-456789abcdef"
        rows = [
            {
                "timestamp": "2026-08-17T00:00:00Z",
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"},
            },
            {
                "timestamp": "2026-08-17T00:00:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "turn_id": "turn-1",
                    "timing": {
                        "schemaVersion": 25,
                        "milestones": {
                            "firstDomainActionMs": 12.5,
                            "firstUsefulActionMs": 12.5,
                        },
                    },
                },
            },
        ]
        data = ("\n".join(json.dumps(row) for row in rows) + "\n").encode()
        # Rust's streaming encoder can emit frames without a content size.
        compressed = zstd.compress(
            data, options={zstd.CompressionParameter.content_size_flag: 0}
        )
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            plain = root / f"rollout-{session_id}.jsonl"
            path = plain.with_name(plain.name + ".zst")
            path.write_bytes(compressed)
            metadata = {
                "path": str(path.resolve()),
                "byteLength": len(compressed),
                "sha256": hashlib.sha256(compressed).hexdigest(),
            }
            for source in (str(path), str(plain), str(root), session_id):
                with (
                    self.subTest(source=source),
                    contextlib.redirect_stdout(io.StringIO()) as stdout,
                ):
                    self.assertEqual(
                        kd4_turn_latency_audit.main(
                            [
                                source,
                                "--sessions-root",
                                str(root),
                                "--repo-root",
                                str(root),
                                "--json",
                            ]
                        ),
                        0,
                    )
                report = json.loads(stdout.getvalue())
                self.assertEqual(report["coverage"]["parseErrorCount"], 0)
                self.assertEqual(report["coverage"]["snapshots"], [metadata])
                self.assertEqual(report["coverage"]["bytes"], len(compressed))
                action = report["firstUsefulActionAnalysis"]
                self.assertEqual(action["completedTurnCount"], 1)
                self.assertEqual(
                    action["canonical"]["startToFirstDomainActionMs"]["p50"], 12.5
                )
            snapshot = rollout_snapshot.read_rollout_snapshot(plain)
            self.assertEqual(snapshot.text_lines(), data.decode().splitlines())
            standalone = kd4_first_useful_action_analysis.analyze_snapshots([snapshot])
            self.assertEqual(standalone["completedTurnCount"], 1)
            self.assertEqual(
                standalone["canonical"]["startToFirstDomainActionMs"]["p50"], 12.5
            )
            output = root / "captured.jsonl.zst"
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(
                    rollout_snapshot.main([str(path), "--output", str(output)]), 0
                )
            self.assertEqual(output.read_bytes(), compressed)

    def test_plain_rollout_wins_over_compressed_sibling_without_double_counting(self):
        session_id = "01234567-89ab-cdef-0123-456789abcdef"
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            plain = root / f"rollout-{session_id}.jsonl"
            plain.write_bytes(b'{"type":"session_meta","payload":{}}\n')
            plain.with_name(plain.name + ".zst").write_bytes(
                b"stale compressed sibling"
            )
            report = kd4_turn_latency_audit.analyze_session_path(root, root)
            self.assertEqual(
                report["coverage"]["snapshots"],
                [rollout_snapshot.read_rollout_snapshot(plain).metadata()],
            )
            self.assertEqual(report["coverage"]["parseErrorCount"], 0)
            self.assertEqual(
                kd4_turn_latency_audit.resolve_rollout_source(session_id, root),
                plain.resolve(),
            )

    def test_corrupt_or_truncated_compressed_rollout_fails_without_report(self):
        complete = zstd.compress(b'{"type":"session_meta","payload":{}}\n')
        for data in (b"not zstd", complete[:-1]):
            with self.subTest(data=data), tempfile.TemporaryDirectory() as temp:
                path = Path(temp) / "rollout.jsonl.zst"
                path.write_bytes(data)
                with (
                    contextlib.redirect_stdout(io.StringIO()) as stdout,
                    contextlib.redirect_stderr(io.StringIO()) as stderr,
                ):
                    with self.assertRaises(SystemExit) as raised:
                        kd4_turn_latency_audit.main([str(path), "--json"])
                self.assertEqual(raised.exception.code, 2)
                self.assertIn("cannot decompress rollout", stderr.getvalue())
                self.assertEqual(stdout.getvalue(), "")

    def test_cli_rejects_hardlink_to_live_rollout(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "live.jsonl"
            alias = Path(temp) / "alias.jsonl"
            path.write_bytes(b"original")
            alias.hardlink_to(path)
            snapshot = rollout_snapshot.read_rollout_snapshot(path)
            path.write_bytes(b"original appended")
            with (
                mock.patch.object(
                    rollout_snapshot, "read_rollout_snapshot", return_value=snapshot
                ),
                self.assertRaisesRegex(ValueError, "must not overwrite"),
            ):
                rollout_snapshot.main([str(path), "--output", str(alias)])
            self.assertEqual(path.read_bytes(), b"original appended")

    def test_audit_excludes_nonobjects_and_partial_utf8(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "rollout.jsonl"
            path.write_bytes(b'[]\n{"payload": {}}\n\xe2\x82')
            report = kd4_turn_latency_audit.analyze_session_path(path, Path(temp))
            self.assertEqual(report["coverage"]["parseErrorCount"], 2)
            self.assertEqual(
                report["coverage"]["snapshots"][0]["sha256"],
                hashlib.sha256(path.read_bytes()).hexdigest(),
            )

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
