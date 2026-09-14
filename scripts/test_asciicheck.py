#!/usr/bin/env python3

import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import asciicheck


class AsciiCheckTest(unittest.TestCase):
    def test_crlf_chunk_boundaries_preserve_character_and_decode_locations(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "boundary.txt"
            for payload, location in (
                (b"abc\r\n\x01", "line 2, column 1"),
                ("abc\r\n\u2014".encode(), "line 2, column 1"),
                (b"abc\r\n\xff", "line 2, column 1"),
            ):
                path.write_bytes(payload)
                output = io.StringIO()
                with (
                    mock.patch.object(asciicheck, "_READ_CHUNK_SIZE", 4),
                    contextlib.redirect_stdout(output),
                ):
                    self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))
                self.assertIn(location, output.getvalue())
                self.assertIn(str(path), output.getvalue())
                self.assertNotIn("line 3", output.getvalue())

    def test_reports_bounded_sample_with_exact_total(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "many.txt"
            path.write_bytes(b"\x01" * 1000)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))
            self.assertEqual(output.getvalue().count("Invalid character"), 100)
            self.assertIn(
                "900 additional errors omitted; 1000 errors total", output.getvalue()
            )

    def test_failed_atomic_replace_preserves_original_and_cleans_temp(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fix.txt"
            original = "unchanged\u2014text\r\n".encode()
            path.write_bytes(original)
            with (
                mock.patch.object(
                    asciicheck.os, "replace", side_effect=OSError("replace failed")
                ),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=True))
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(list(Path(directory).iterdir()), [path])

    def test_ascii_and_allowed_unicode_pass(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "allowed.md"
            path.write_text("plain ASCII\nallowed sparkle: ✨\n", encoding="utf-8")

            self.assertFalse(asciicheck.lint_utf8_ascii(path, fix=False))

    def test_invalid_character_reports_location_and_fix_rewrites_it(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "invalid.md"
            path.write_text("alpha\nem—dash\n", encoding="utf-8")
            output = io.StringIO()

            with contextlib.redirect_stdout(output):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))

            self.assertIn("line 2, column 3", output.getvalue())
            self.assertIn("U+2014", output.getvalue())

            with contextlib.redirect_stdout(io.StringIO()):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=True))

            self.assertEqual(path.read_text(encoding="utf-8"), "alpha\nem-dash\n")

    def test_invalid_utf8_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "invalid.bin"
            path.write_bytes(b"ok\n\xff\n")
            output = io.StringIO()

            with contextlib.redirect_stdout(output):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))

            self.assertIn("UTF-8 decoding error", output.getvalue())

    def test_invalid_utf8_reports_chunk_boundary_sequence_start(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "invalid.bin"
            path.write_bytes(b"abc\xe2(\xa1")
            output = io.StringIO()

            with (
                mock.patch.object(asciicheck, "_READ_CHUNK_SIZE", 4),
                contextlib.redirect_stdout(output),
            ):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))

            self.assertIn("byte offset: 3", output.getvalue())
            self.assertIn("location: line 1, column 4", output.getvalue())

    def test_truncated_utf8_reports_sequence_start_at_eof(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "truncated.bin"
            path.write_bytes(b"ab\xe2\x82")
            output = io.StringIO()

            with contextlib.redirect_stdout(output):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))

            self.assertIn("byte offset: 2", output.getvalue())
            self.assertIn("location: line 1, column 3", output.getvalue())

    def test_fix_does_not_rewrite_unfixable_only_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "control.txt"
            path.write_bytes(b"alpha\x01omega")

            with (
                mock.patch("builtins.open", wraps=open) as open_file,
                contextlib.redirect_stdout(io.StringIO()),
            ):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=True))

            self.assertEqual(open_file.call_count, 1)
            self.assertEqual(path.read_bytes(), b"alpha\x01omega")

    def test_classic_mac_line_endings_advance_line_number(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "classic-mac.txt"
            path.write_text("alpha\rbeta—gamma", encoding="utf-8", newline="")
            output = io.StringIO()

            with contextlib.redirect_stdout(output):
                self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))

            self.assertIn("line 2, column 5", output.getvalue())

    def test_missing_file_is_reported_without_traceback(self) -> None:
        path = Path("definitely-missing-asciicheck-input")
        stderr = io.StringIO()

        with contextlib.redirect_stderr(stderr):
            self.assertTrue(asciicheck.lint_utf8_ascii(path, fix=False))

        self.assertIn("Could not read", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
