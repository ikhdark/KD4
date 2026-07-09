#!/usr/bin/env python3

"""
Utility script to verify (and optionally fix) the Table of Contents in a
Markdown file. By default, it checks that the ToC between `<!-- Begin ToC -->`
and `<!-- End ToC -->` matches the headings in the file. With --fix, it
rewrites the file to update the ToC.
"""

import argparse
import difflib
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence, TextIO

# Markers for the Table of Contents section
BEGIN_TOC: str = "<!-- Begin ToC -->"
END_TOC: str = "<!-- End ToC -->"
DEFAULT_DIFF_MAX_LINES = 200
HEADING_RE = re.compile(r"^(#{2,6})\s+(.*)$")
CODE_FENCE_RE = re.compile(r"^\s*(```|~~~)")
PUNCT_TRANSLATION = str.maketrans(
    {
        chr(value): None
        for value in range(128)
        if not chr(value).isalnum() and chr(value) not in " -"
    }
)
DASH_TRANSLATION = str.maketrans(
    {
        "\u00a0": " ",
        "\u2011": "-",
        "\u2013": "-",
        "\u2014": "-",
    }
)


@dataclass(frozen=True)
class TocParseResult:
    begin_idx: int
    end_idx: int
    current: list[str]
    expected: list[str]


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check and optionally fix the README.md Table of Contents."
    )
    parser.add_argument(
        "file", nargs="?", default="README.md", help="Markdown file to process"
    )
    parser.add_argument(
        "--fix", action="store_true", help="Rewrite file with updated ToC"
    )
    parser.add_argument(
        "--diff-max-lines",
        type=int,
        default=DEFAULT_DIFF_MAX_LINES,
        help="Maximum diff lines to print before truncating; use 0 for full diff.",
    )
    args = parser.parse_args()
    path = Path(args.file)
    return check_or_fix(path, args.fix, diff_max_lines=args.diff_max_lines)


def generate_toc_lines(lines: Iterable[str]) -> list[str]:
    """
    Generate markdown list lines for headings (## to ######) in content.
    """
    toc: list[str] = []
    in_code = False
    used_slugs: dict[str, int] = {}
    for line in lines:
        if CODE_FENCE_RE.match(line):
            in_code = not in_code
            continue
        if in_code:
            continue
        m = HEADING_RE.match(line)
        if not m:
            continue
        level = len(m.group(1))
        text = m.group(2).strip()
        indent = "  " * (level - 2)
        slug = disambiguate_slug(slugify_heading(text), used_slugs)
        toc.append(f"{indent}- [{text}](#{slug})")
    return toc


def disambiguate_slug(slug: str, used_slugs: dict[str, int]) -> str:
    count = used_slugs.get(slug, 0)
    used_slugs[slug] = count + 1
    if count == 0:
        return slug
    return f"{slug}-{count}"


def slugify_heading(text: str) -> str:
    slug = text.lower().translate(DASH_TRANSLATION)
    slug = slug.translate(PUNCT_TRANSLATION)
    return slug.strip().replace(" ", "-")


def parse_markdown_toc(lines: Sequence[str]) -> TocParseResult | None:
    begin_idx = -1
    end_idx = -1
    current: list[str] = []
    heading_lines: list[str] = []
    in_toc = False

    for idx, line in enumerate(lines):
        stripped = line.strip()
        if stripped == BEGIN_TOC and begin_idx == -1:
            begin_idx = idx
            in_toc = True
            continue
        if stripped == END_TOC and in_toc:
            end_idx = idx
            in_toc = False
            continue
        if in_toc:
            if line.lstrip().startswith("- ["):
                current.append(line)
            continue
        heading_lines.append(line)

    if begin_idx == -1 and end_idx == -1:
        return None
    if begin_idx == -1 or end_idx == -1 or end_idx < begin_idx:
        raise ValueError("malformed ToC markers")

    return TocParseResult(
        begin_idx=begin_idx,
        end_idx=end_idx,
        current=current,
        expected=generate_toc_lines(heading_lines),
    )


def print_toc_diff(
    current: Sequence[str],
    expected: Sequence[str],
    *,
    max_lines: int = DEFAULT_DIFF_MAX_LINES,
    stream: TextIO = sys.stdout,
) -> None:
    diff = difflib.unified_diff(
        current,
        expected,
        fromfile="existing ToC",
        tofile="generated ToC",
        lineterm="",
    )
    for index, line in enumerate(diff):
        if max_lines > 0 and index >= max_lines:
            print(
                f"... Diff truncated after {max_lines} lines; rerun with --diff-max-lines 0.",
                file=stream,
            )
            break
        print(line, file=stream)


def check_or_fix(
    readme_path: Path, fix: bool, diff_max_lines: int = DEFAULT_DIFF_MAX_LINES
) -> int:
    if not readme_path.is_file():
        print(f"Error: file not found: {readme_path}", file=sys.stderr)
        return 1
    content = readme_path.read_text(encoding="utf-8")
    lines = content.splitlines()
    try:
        parsed = parse_markdown_toc(lines)
    except ValueError as exc:
        print(f"Error: {exc} in {readme_path}.", file=sys.stderr)
        return 1
    if parsed is None:
        # No ToC markers found; treat as a no-op so repos without a ToC don't fail CI
        print(
            f"Note: Skipping ToC check; no markers found in {readme_path}.",
        )
        return 0
    if parsed.current == parsed.expected:
        return 0
    if not fix:
        print(
            "ERROR: README ToC is out of date. Diff between existing and generated ToC:"
        )
        print_toc_diff(parsed.current, parsed.expected, max_lines=diff_max_lines)
        return 1
    # rebuild file with updated ToC
    prefix = lines[: parsed.begin_idx + 1]
    suffix = lines[parsed.end_idx :]
    new_lines = prefix + [""] + parsed.expected + [""] + suffix
    # newline="" keeps LF as-is; the default translation re-expands \n to
    # os.linesep and rewrites the whole file as CRLF on Windows.
    readme_path.write_text("\n".join(new_lines) + "\n", encoding="utf-8", newline="")
    print(f"Updated ToC in {readme_path}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
