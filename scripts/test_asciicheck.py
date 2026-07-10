#!/usr/bin/env python3

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import asciicheck


class AsciiCheckTest(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
