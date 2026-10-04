from __future__ import annotations

import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import atomic_json


def windows_error(code: int) -> OSError:
    error = PermissionError("replacement blocked")
    error.winerror = code
    return error


class AtomicJsonTest(unittest.TestCase):
    def test_exclusive_stream_never_clobbers_and_cleans_failed_writes(self):
        class InterruptedSource(io.BytesIO):
            def read(self, size=-1):
                if self.tell():
                    raise OSError("producer interrupted")
                return super().read(2)

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report"
            with self.assertRaisesRegex(OSError, "producer interrupted"):
                atomic_json.write_stream_atomic(output, InterruptedSource(b"partial"), exclusive=True)
            self.assertEqual(list(Path(directory).iterdir()), [])
            atomic_json.write_stream_atomic(output, io.BytesIO(b"complete"), exclusive=True)
            with self.assertRaises(FileExistsError):
                atomic_json.write_stream_atomic(output, io.BytesIO(b"replacement"), exclusive=True)
            self.assertEqual(output.read_bytes(), b"complete")
            self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_exclusive_publication_preserves_concurrent_winner(self):
        link = os.link
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report"
            def race(source, destination):
                destination.write_bytes(b"winner")
                link(source, destination)
            with mock.patch.object(atomic_json.os, "link", side_effect=race), self.assertRaises(FileExistsError):
                atomic_json.write_stream_atomic(output, io.BytesIO(b"loser"), exclusive=True)
            self.assertEqual(output.read_bytes(), b"winner")
            self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_stream_output_uses_bounded_reads_and_keeps_source_open(self):
        payload = b"captured bytes\n" * 100_000

        class BoundedSource(io.BytesIO):
            def read(source, size=-1):
                self.assertGreater(size, 0)
                self.assertLessEqual(size, 1024 * 1024)
                return super().read(size)

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "nested" / "snapshot"
            with BoundedSource(payload) as source:
                atomic_json.write_stream_atomic(output, source)
                self.assertFalse(source.closed)
            self.assertEqual(output.read_bytes(), payload)
            self.assertEqual(list(output.parent.iterdir()), [output])

    def test_windows_retries_reuse_the_written_payload(self):
        replace = os.replace
        for code in (5, 32, 33):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "report"
                output.write_bytes(b"old")
                attempts = []

                def blocked_twice(source, destination, attempts=attempts, code=code):
                    attempts.append(source)
                    self.assertEqual(source.read_bytes(), b"new")
                    if len(attempts) <= 2:
                        raise windows_error(code)
                    replace(source, destination)

                with (
                    mock.patch.object(
                        atomic_json.os, "replace", side_effect=blocked_twice
                    ),
                    mock.patch.object(atomic_json.time, "sleep") as sleep,
                    mock.patch.object(
                        atomic_json.shutil,
                        "copyfileobj",
                        wraps=atomic_json.shutil.copyfileobj,
                    ) as copy,
                    mock.patch.object(atomic_json.os, "fsync", wraps=os.fsync) as fsync,
                ):
                    atomic_json.write_stream_atomic(output, io.BytesIO(b"new"))
                self.assertEqual(output.read_bytes(), b"new")
                self.assertEqual(len(attempts), 3)
                self.assertEqual(len(set(attempts)), 1)
                copy.assert_called_once()
                fsync.assert_called_once()
                self.assertEqual(sleep.call_count, 2)
                self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_persistent_windows_error_has_a_bounded_budget_and_preserves_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report"
            output.write_bytes(b"old")
            error = windows_error(5)
            with (
                mock.patch.object(
                    atomic_json.os, "replace", side_effect=error
                ) as replace,
                mock.patch.object(atomic_json.time, "sleep") as sleep,
                self.assertRaises(PermissionError) as raised,
            ):
                atomic_json.write_bytes_atomic(output, b"new")
            self.assertIs(raised.exception, error)
            self.assertGreater(replace.call_count, 1)
            self.assertLessEqual(replace.call_count, 6)
            self.assertLessEqual(
                sum(call.args[0] for call in sleep.call_args_list), 0.25
            )
            self.assertEqual(output.read_bytes(), b"old")
            self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_other_errors_fail_without_retry_and_preserve_output(self):
        for error in (PermissionError("permission denied"), OSError("disk full")):
            with self.subTest(error=error), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "report"
                output.write_bytes(b"old")
                with (
                    mock.patch.object(
                        atomic_json.os, "replace", side_effect=error
                    ) as replace,
                    mock.patch.object(atomic_json.time, "sleep") as sleep,
                    self.assertRaises(OSError) as raised,
                ):
                    atomic_json.write_bytes_atomic(output, b"new")
                self.assertIs(raised.exception, error)
                replace.assert_called_once()
                sleep.assert_not_called()
                self.assertEqual(output.read_bytes(), b"old")
                self.assertEqual(list(Path(directory).iterdir()), [output])

    @unittest.skipUnless(os.name == "nt", "requires Windows file sharing")
    def test_real_windows_reader_can_close_between_publication_attempts(self):
        replace = os.replace
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report"
            output.write_bytes(b"old")
            with output.open("rb") as reader:
                failures = []

                def finish_read_on_contention(source, destination):
                    try:
                        replace(source, destination)
                    except PermissionError as error:
                        failures.append(error.winerror)
                        self.assertEqual(reader.read(), b"old")
                        reader.close()
                        raise

                with mock.patch.object(
                    atomic_json.os, "replace", side_effect=finish_read_on_contention
                ):
                    atomic_json.write_bytes_atomic(output, b"new")
            self.assertEqual(len(failures), 1)
            self.assertIn(failures[0], (5, 32, 33))
            self.assertEqual(output.read_bytes(), b"new")
            self.assertEqual(list(Path(directory).iterdir()), [output])


if __name__ == "__main__":
    unittest.main()
