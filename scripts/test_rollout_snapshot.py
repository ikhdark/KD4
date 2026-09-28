from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
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
    def test_compression_handoff_recovers_at_resolution_and_open_boundaries(self):
        data = b'{"type":"session_meta","payload":{}}\n'
        compressed = zstd.compress(data)
        existing = rollout_snapshot.existing_rollout_path
        resolve = Path.resolve
        for boundary in ("resolve", "open"):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as temp:
                plain = Path(temp) / "rollout.jsonl"
                plain.write_bytes(data)
                cold = plain.with_name(plain.name + ".zst")
                cold.write_bytes(compressed)

                def retire_plain(plain=plain):
                    plain.rename(plain.with_name("retired.jsonl"))

                def retire_before_resolve(path, retire=retire_plain):
                    selected = existing(path)
                    retire()
                    return selected

                def retire_before_open(
                    path, *args, plain=plain, retire=retire_plain, **kwargs
                ):
                    resolved = resolve(path, *args, **kwargs)
                    if path == plain:
                        retire()
                    return resolved

                transition = (
                    mock.patch.object(
                        rollout_snapshot,
                        "existing_rollout_path",
                        side_effect=retire_before_resolve,
                    )
                    if boundary == "resolve"
                    else mock.patch.object(
                        Path, "resolve", autospec=True, side_effect=retire_before_open
                    )
                )
                with transition:
                    snapshot = rollout_snapshot.read_rollout_snapshot(plain)
                with contextlib.closing(snapshot.stream):
                    self.assertEqual(snapshot.path, cold.resolve())
                    self.assertEqual(snapshot.text_lines(), data.decode().splitlines())
                    self.assertEqual(snapshot.byte_length, len(compressed))
                    self.assertEqual(
                        snapshot.sha256, hashlib.sha256(compressed).hexdigest()
                    )

    def test_handoff_retry_is_bounded_and_does_not_retry_permission_errors(self):
        cases = (
            (PermissionError("denied"), True, 1),
            (FileNotFoundError("gone"), False, 1),
            (FileNotFoundError("both gone"), True, 2),
        )
        for error, has_compressed, expected_opens in cases:
            with self.subTest(error=error), tempfile.TemporaryDirectory() as temp:
                plain = Path(temp) / "rollout.jsonl"
                plain.write_bytes(b"source")
                if has_compressed:
                    plain.with_name(plain.name + ".zst").write_bytes(b"compressed")
                opener = (
                    mock.patch.object(
                        rollout_snapshot, "_open_shared_binary", side_effect=error
                    )
                    if os.name == "nt"
                    else mock.patch.object(Path, "open", side_effect=error)
                )
                with opener as opened, self.assertRaises(type(error)):
                    rollout_snapshot.read_rollout_snapshot(plain)
                self.assertEqual(opened.call_count, expected_opens)

    def test_failed_export_preserves_evidence_and_never_publishes_partial_output(self):
        for existing_output in (False, True):
            with (
                self.subTest(existing_output=existing_output),
                tempfile.TemporaryDirectory() as temp,
            ):
                source = Path(temp) / "live.jsonl"
                output = Path(temp) / "snapshot.jsonl"
                data = b'{"type":"session_meta","payload":{}}\n'
                source.write_bytes(data)
                if existing_output:
                    output.write_bytes(b"previous verified evidence")
                snapshot = rollout_snapshot.read_rollout_snapshot(source)

                def interrupted_copy(captured, destination):
                    destination.write(captured.read(8))
                    raise OSError("disk full")

                with (
                    mock.patch.object(
                        rollout_snapshot, "read_rollout_snapshot", return_value=snapshot
                    ),
                    mock.patch("shutil.copyfileobj", side_effect=interrupted_copy),
                    contextlib.redirect_stdout(io.StringIO()) as stdout,
                    self.assertRaisesRegex(OSError, "disk full"),
                ):
                    rollout_snapshot.main([str(source), "--output", str(output)])
                self.assertEqual(stdout.getvalue(), "")
                self.assertTrue(snapshot.stream.closed)
                self.assertEqual(source.read_bytes(), data)
                if existing_output:
                    self.assertEqual(output.read_bytes(), b"previous verified evidence")
                else:
                    self.assertFalse(output.exists())
                self.assertEqual(list(Path(temp).glob(".*.tmp")), [])

    def test_export_rejects_mismatched_format_before_atomic_publication(self):
        with tempfile.TemporaryDirectory() as temp:
            source = Path(temp) / "live.jsonl.zst"
            source.write_bytes(zstd.compress(b'{"type":"session_meta"}\n'))
            output = Path(temp) / "snapshot.jsonl"
            output.write_bytes(b"previous evidence")
            with (
                mock.patch.object(rollout_snapshot, "write_stream_atomic") as publish,
                self.assertRaisesRegex(ValueError, "suffix must match"),
            ):
                rollout_snapshot.main([str(source), "--output", str(output)])
            publish.assert_not_called()
            self.assertEqual(output.read_bytes(), b"previous evidence")

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
                        "profileValid": True,
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
