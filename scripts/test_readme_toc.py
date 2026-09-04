#!/usr/bin/env python3

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

BEGIN_TOC = "<!-- Begin ToC -->"
END_TOC = "<!-- End ToC -->"


class ReadmeTocTest(unittest.TestCase):
    def run_cli(
        self,
        path: Path,
        *options: str,
    ) -> subprocess.CompletedProcess[str]:
        script = Path(__file__).with_name("readme_toc.py").resolve()
        return subprocess.run(
            [sys.executable, str(script), *options, str(path)],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )

    def test_cli_fix_skips_code_blocks_and_normalizes_slugs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                "# Title\n"
                f"{BEGIN_TOC}\n- [Old](#old)\n{END_TOC}\n"
                "## Install & Setup\n"
                "```\n## Not Real\n```\n"
                "~~~markdown\n## Also Not Real\n~~~\n"
                "### API\u00a0Reference\n"
                "#### Fast\u2011Path \u2014 Notes!\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path, "--fix")
            updated = path.read_text(encoding="utf-8")

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("- [Install & Setup](#install--setup)", updated)
        self.assertIn("  - [API\u00a0Reference](#api-reference)", updated)
        self.assertIn("    - [Fast\u2011Path \u2014 Notes!](#fastpath--notes)", updated)
        self.assertNotIn("Not Real](", updated)

    def test_cli_fix_preserves_underscores_and_removes_unicode_dashes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                f"{BEGIN_TOC}\n{END_TOC}\n"
                "## run\\_tui\\_with\\_exec\\_server.sh\n"
                "## A \u2013 B\n"
                "## A \u2014 B\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path, "--fix")
            updated = path.read_text(encoding="utf-8")

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn(
            "- [run_tui_with_exec_server.sh](#run_tui_with_exec_serversh)", updated
        )
        self.assertIn("- [A \u2013 B](#a--b)", updated)
        self.assertIn("- [A \u2014 B](#a--b-1)", updated)

    def test_cli_fix_disambiguates_duplicate_slugs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                f"{BEGIN_TOC}\n{END_TOC}\n## Usage\n## Usage!\n### Usage\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path, "--fix")
            updated = path.read_text(encoding="utf-8")

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("- [Usage](#usage)\n", updated)
        self.assertIn("- [Usage!](#usage-1)\n", updated)
        self.assertIn("  - [Usage](#usage-2)\n", updated)

    def test_cli_does_not_close_fence_with_other_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                f"{BEGIN_TOC}\n- [Real](#real)\n{END_TOC}\n"
                "```text\n"
                "~~~\n"
                "## Still Code\n"
                "```\n"
                "## Real\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_cli_requires_closing_fence_at_least_as_long(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                f"{BEGIN_TOC}\n- [Real](#real)\n{END_TOC}\n"
                "````text\n"
                "```\n"
                "## Still Code\n"
                "````\n"
                "## Real\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_cli_fix_replaces_marker_contents_with_current_headings(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                "# Title\n"
                f"{BEGIN_TOC}\n\n- [Old](#old)\n\n{END_TOC}\n"
                "## Current\n### Child\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path, "--fix")
            updated = path.read_text(encoding="utf-8")

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("- [Current](#current)\n  - [Child](#child)", updated)
        self.assertNotIn("[Old]", updated)

    def test_cli_without_markers_is_noop(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            original = "# Title\n\n## Section\n"
            path.write_text(original, encoding="utf-8")
            completed = self.run_cli(path)
            unchanged = path.read_text(encoding="utf-8")

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("no markers found", completed.stdout)
        self.assertEqual(unchanged, original)

    def test_cli_can_require_markers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "SOURCEMAP.md"
            path.write_text("# Source Map\n\n## Inventory\n", encoding="utf-8")
            completed = self.run_cli(path, "--require-markers")

        self.assertEqual(completed.returncode, 1)
        self.assertIn("required ToC markers not found", completed.stderr)

    def test_cli_rejects_unexpected_content_inside_toc(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                f"{BEGIN_TOC}\nunexpected prose\n{END_TOC}\n## Current\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 1)
        self.assertIn("-unexpected prose", completed.stderr)
        self.assertIn("+-[Current](#current)", completed.stderr.replace(" ", ""))

    def test_cli_ignores_markers_inside_code_fences(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                "````markdown\n"
                f"{BEGIN_TOC}\n"
                "```\n"
                f"{END_TOC}\n"
                "````\n"
                f"{BEGIN_TOC}\n- [Current](#current)\n{END_TOC}\n"
                "## Current\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_cli_rejects_duplicate_and_unexpected_markers(self) -> None:
        malformed_documents = (
            f"{BEGIN_TOC}\n{BEGIN_TOC}\n{END_TOC}\n",
            f"{END_TOC}\n",
            f"{BEGIN_TOC}\n{END_TOC}\n{END_TOC}\n",
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            for index, document in enumerate(malformed_documents):
                with self.subTest(index=index):
                    path = Path(temp_dir) / f"malformed-{index}.md"
                    path.write_text(document, encoding="utf-8")
                    completed = self.run_cli(path)
                    self.assertEqual(completed.returncode, 1)
                    self.assertIn("Error:", completed.stderr)

    def test_cli_fix_updates_only_toc_block(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                f"# Title\n{BEGIN_TOC}\n- [Old](#old)\n{END_TOC}\n\n## New Section\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path, "--fix")
            updated = path.read_text(encoding="utf-8")

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            updated,
            f"# Title\n{BEGIN_TOC}\n\n"
            f"- [New Section](#new-section)\n\n{END_TOC}\n\n"
            "## New Section\n",
        )

    def test_cli_fix_preserves_crlf_and_nonstandard_separators_outside_toc(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            original = (
                "# Title\r\n"
                f"{BEGIN_TOC}\r\n"
                "- [Old](#old)\r\n"
                f"{END_TOC}\r\n"
                "Before\x85After\r\n"
                "## New Section\r\n"
            )
            path.write_text(original, encoding="utf-8", newline="")
            completed = self.run_cli(path, "--fix")
            with path.open("r", encoding="utf-8", newline="") as readme_file:
                updated = readme_file.read()

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("Before\x85After\r\n", updated)
        self.assertNotIn("\n", updated.replace("\r\n", ""))
        self.assertIn("- [New Section](#new-section)\r\n", updated)

    def test_cli_writes_out_of_date_error_to_stderr(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            path.write_text(
                f"{BEGIN_TOC}\n- [Old](#old)\n{END_TOC}\n## New\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path)

        self.assertEqual(completed.returncode, 1)
        self.assertEqual(completed.stdout, "")
        self.assertIn("out of date", completed.stderr)

    def test_cli_capped_diff_reports_truncation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "README.md"
            old_toc = "\n".join(f"- [Old {index}](#old-{index})" for index in range(12))
            headings = "\n".join(f"## New {index}" for index in range(12))
            path.write_text(
                f"{BEGIN_TOC}\n{old_toc}\n{END_TOC}\n{headings}\n",
                encoding="utf-8",
            )
            completed = self.run_cli(path, "--diff-max-lines", "6")

        self.assertEqual(completed.returncode, 1)
        self.assertIn("Diff truncated after 6 lines", completed.stderr)


if __name__ == "__main__":
    unittest.main()
