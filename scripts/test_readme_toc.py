#!/usr/bin/env python3

import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import readme_toc


class ReadmeTocTest(unittest.TestCase):
    def test_generate_toc_lines_skips_code_blocks_and_normalizes_slugs(self) -> None:
        lines = [
            "# Title",
            "## Install & Setup",
            "```",
            "## Not Real",
            "```",
            "~~~markdown",
            "## Also Not Real",
            "~~~",
            "### API\u00a0Reference",
            "#### Fast\u2011Path \u2014 Notes!",
        ]

        self.assertEqual(
            readme_toc.generate_toc_lines(lines),
            [
                "- [Install & Setup](#install--setup)",
                "  - [API\u00a0Reference](#api-reference)",
                "    - [Fast\u2011Path \u2014 Notes!](#fast-path---notes)",
            ],
        )

    def test_generate_toc_lines_disambiguates_duplicate_slugs(self) -> None:
        lines = [
            "# Title",
            "## Usage",
            "## Usage!",
            "### Usage",
        ]

        self.assertEqual(
            readme_toc.generate_toc_lines(lines),
            [
                "- [Usage](#usage)",
                "- [Usage!](#usage-1)",
                "  - [Usage](#usage-2)",
            ],
        )

    def test_parse_markdown_toc_finds_markers_and_expected_without_joining(
        self,
    ) -> None:
        lines = [
            "# Title",
            readme_toc.BEGIN_TOC,
            "",
            "- [Old](#old)",
            "",
            readme_toc.END_TOC,
            "## Current",
            "### Child",
        ]

        parsed = readme_toc.parse_markdown_toc(lines)

        self.assertEqual(parsed.begin_idx, 1)
        self.assertEqual(parsed.end_idx, 5)
        self.assertEqual(parsed.current, ["- [Old](#old)"])
        self.assertEqual(
            parsed.expected,
            ["- [Current](#current)", "  - [Child](#child)"],
        )

    def test_check_without_markers_is_noop(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "README.md"
            path.write_text("# Title\n\n## Section\n", encoding="utf-8")
            output = io.StringIO()

            with contextlib.redirect_stdout(output):
                result = readme_toc.check_or_fix(path, fix=False)

            self.assertEqual(result, 0)
            self.assertIn("no markers found", output.getvalue())

    def test_fix_updates_only_toc_block(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "README.md"
            path.write_text(
                "\n".join(
                    [
                        "# Title",
                        readme_toc.BEGIN_TOC,
                        "- [Old](#old)",
                        readme_toc.END_TOC,
                        "",
                        "## New Section",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            result = readme_toc.check_or_fix(path, fix=True)

            self.assertEqual(result, 0)
            self.assertEqual(
                path.read_text(encoding="utf-8"),
                "\n".join(
                    [
                        "# Title",
                        readme_toc.BEGIN_TOC,
                        "",
                        "- [New Section](#new-section)",
                        "",
                        readme_toc.END_TOC,
                        "",
                        "## New Section",
                    ]
                )
                + "\n",
            )

    def test_capped_diff_reports_truncation(self) -> None:
        current = [f"- [Old {index}](#old-{index})" for index in range(12)]
        expected = [f"- [New {index}](#new-{index})" for index in range(12)]
        output = io.StringIO()

        readme_toc.print_toc_diff(current, expected, max_lines=6, stream=output)

        text = output.getvalue()
        self.assertIn("Diff truncated", text)
        self.assertLess(text.count("\n"), 12)


if __name__ == "__main__":
    unittest.main()
