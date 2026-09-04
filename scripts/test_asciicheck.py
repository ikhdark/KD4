#!/usr/bin/env python3

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class AsciiCheckTest(unittest.TestCase):
    def run_cli(
        self,
        *paths: Path,
        fix: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        script = Path(__file__).with_name("asciicheck.py").resolve()
        command = [sys.executable, str(script)]
        if fix:
            command.append("--fix")
        command.extend(str(path) for path in paths)
        return subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
        )

    def test_cli_accepts_ascii_and_allowed_unicode(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "allowed.md"
            path.write_text("plain ASCII\nallowed sparkle: ✨\n", encoding="utf-8")
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, "")

    def test_cli_reports_and_fixes_invalid_character(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "invalid.md"
            path.write_text("alpha\nem—dash\n", encoding="utf-8")

            checked = self.run_cli(path)
            fixed = self.run_cli(path, fix=True)
            rewritten = path.read_text(encoding="utf-8")

        self.assertEqual(checked.returncode, 1)
        self.assertIn("line 2, column 3", checked.stdout)
        self.assertIn("U+2014", checked.stdout)
        self.assertEqual(fixed.returncode, 1)
        self.assertIn("Fixed 1 of 1 errors", fixed.stdout)
        self.assertEqual(rewritten, "alpha\nem-dash\n")

    def test_cli_rejects_invalid_utf8(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "invalid.bin"
            path.write_bytes(b"ok\n\xff\n")
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 1)
        self.assertIn("UTF-8 decoding error", completed.stdout)

    def test_cli_reports_invalid_utf8_sequence_across_real_chunk_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "invalid.bin"
            boundary = 1024 * 1024
            path.write_bytes(b"a" * (boundary - 1) + b"\xe2(\xa1")
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 1)
        self.assertIn(f"byte offset: {boundary - 1}", completed.stdout)
        self.assertIn(f"location: line 1, column {boundary}", completed.stdout)

    def test_cli_reports_truncated_utf8_sequence_at_eof(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "truncated.bin"
            path.write_bytes(b"ab\xe2\x82")
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 1)
        self.assertIn("byte offset: 2", completed.stdout)
        self.assertIn("location: line 1, column 3", completed.stdout)

    def test_cli_fix_preserves_file_with_only_unfixable_content(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "control.txt"
            original = b"alpha\x01omega"
            path.write_bytes(original)
            completed = self.run_cli(path, fix=True)
            contents = path.read_bytes()

        self.assertEqual(completed.returncode, 1)
        self.assertEqual(contents, original)
        self.assertNotIn("Attempting to fix", completed.stdout)

    def test_cli_counts_classic_mac_line_endings(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "classic-mac.txt"
            path.write_text("alpha\rbeta—gamma", encoding="utf-8", newline="")
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 1)
        self.assertIn("line 2, column 5", completed.stdout)

    def test_cli_reports_missing_file_without_traceback(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "missing.txt"
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 1)
        self.assertIn("Could not read", completed.stderr)
        self.assertNotIn("Traceback", completed.stderr)


if __name__ == "__main__":
    unittest.main()
