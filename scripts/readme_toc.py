#!/usr/bin/env python3

"""
Utility script to verify (and optionally fix) the Table of Contents in a
Markdown file. By default, it checks that the ToC between `<!-- Begin ToC -->`
and `<!-- End ToC -->` matches the headings in the file. With --fix, it
rewrites the file to update the ToC.
"""

import argparse
import difflib
import html
import re
import sys
import unicodedata
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TextIO

# Markers for the Table of Contents section
BEGIN_TOC: str = "<!-- Begin ToC -->"
END_TOC: str = "<!-- End ToC -->"
DEFAULT_DIFF_MAX_LINES = 200
HEADING_RE = re.compile(r"^(#{2,6})\s+(.*)$")
CODE_FENCE_RE = re.compile(r"^\s*(`{3,}|~{3,})(.*)$")
LINE_ENDING_RE = re.compile(r"(\r\n|\r|\n)")
CLOSING_ATX_RE = re.compile(r"[ \t]+#+[ \t]*$")
INLINE_CODE_RE = re.compile(r"(`+)(.+?)\1")
INLINE_LINK_RE = re.compile(r"!?\[([^\]]*)\]\([^\n)]*\)")
REFERENCE_LINK_RE = re.compile(r"!?\[([^\]]*)\](?:\[[^\]]*\])")
AUTOLINK_RE = re.compile(r"<((?:https?://|mailto:)[^>]+)>", re.IGNORECASE)
HTML_TAG_RE = re.compile(r"</?[A-Za-z][^>]*>")
BACKSLASH_ESCAPE_RE = re.compile(r"\\([!\"#$%&'()*+,\-./:;<=>?@\[\\\]^_`{|}~])")
INLINE_MARKER_RE = re.compile(r"(?<!\\)[*_~]")


@dataclass(frozen=True)
class TocParseResult:
    begin_idx: int
    end_idx: int
    current: list[str]
    expected: list[str]


@dataclass(frozen=True)
class MarkdownLine:
    text: str
    ending: str


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check and optionally fix a Markdown Table of Contents."
    )
    parser.add_argument(
        "file", nargs="?", default="SOURCEMAP.md", help="Markdown file to process"
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
    parser.add_argument(
        "--require-markers",
        action="store_true",
        help="Fail when the Markdown file has no ToC marker block.",
    )
    args = parser.parse_args()
    path = Path(args.file)
    return check_or_fix(
        path,
        args.fix,
        diff_max_lines=args.diff_max_lines,
        require_markers=args.require_markers,
    )


def generate_toc_lines(lines: Iterable[str]) -> list[str]:
    """
    Generate markdown list lines for headings (## to ######) in content.
    """
    toc: list[str] = []
    code_fence: tuple[str, int] | None = None
    used_slugs: dict[str, int] = {}
    for line in lines:
        code_fence, is_fence_line = advance_code_fence(line, code_fence)
        if is_fence_line:
            continue
        if code_fence is not None:
            continue
        m = HEADING_RE.match(line)
        if not m:
            continue
        level = len(m.group(1))
        text = heading_plain_text(m.group(2))
        indent = "  " * (level - 2)
        slug = disambiguate_slug(slugify_heading(text), used_slugs)
        label = text.replace("\\", "\\\\").replace("[", "\\[").replace("]", "\\]")
        toc.append(f"{indent}- [{label}](#{slug})")
    return toc


def advance_code_fence(
    line: str, code_fence: tuple[str, int] | None
) -> tuple[tuple[str, int] | None, bool]:
    match = CODE_FENCE_RE.match(line)
    if match is None:
        return code_fence, False
    marker = match.group(1)
    if code_fence is None:
        return (marker[0], len(marker)), True
    marker_char, marker_length = code_fence
    if (
        marker[0] == marker_char
        and len(marker) >= marker_length
        and not match.group(2).strip()
    ):
        return None, True
    return code_fence, True


def disambiguate_slug(slug: str, used_slugs: dict[str, int]) -> str:
    count = used_slugs.get(slug, 0)
    used_slugs[slug] = count + 1
    if count == 0:
        return slug
    return f"{slug}-{count}"


def heading_plain_text(markdown: str) -> str:
    """Return the rendered text used by a GFM ATX heading."""
    text = CLOSING_ATX_RE.sub("", markdown.strip())
    text = INLINE_CODE_RE.sub(lambda match: match.group(2).strip(), text)
    text = INLINE_LINK_RE.sub(lambda match: match.group(1), text)
    text = REFERENCE_LINK_RE.sub(lambda match: match.group(1), text)
    text = AUTOLINK_RE.sub(lambda match: match.group(1), text)
    text = HTML_TAG_RE.sub("", text)
    text = INLINE_MARKER_RE.sub("", text)
    text = BACKSLASH_ESCAPE_RE.sub(lambda match: match.group(1), text)
    return html.unescape(text).strip()


def slugify_heading(text: str) -> str:
    """Approximate GitHub's rendered-heading slugger for Unicode text."""
    slug: list[str] = []
    for character in text.lower():
        if character.isspace():
            slug.append("-")
        elif character == "_" or not unicodedata.category(character).startswith(
            ("C", "P")
        ):
            slug.append(character)
    return "".join(slug)


def parse_markdown_toc(lines: Sequence[str]) -> TocParseResult | None:
    begin_idx = -1
    end_idx = -1
    current: list[str] = []
    heading_lines: list[str] = []
    in_toc = False
    code_fence: tuple[str, int] | None = None

    for idx, line in enumerate(lines):
        stripped = line.strip()
        previous_fence = code_fence
        code_fence, is_fence_line = advance_code_fence(line, code_fence)
        markers_allowed = previous_fence is None and not is_fence_line
        if markers_allowed and stripped == BEGIN_TOC:
            if begin_idx != -1:
                raise ValueError("duplicate ToC begin marker")
            begin_idx = idx
            in_toc = True
            continue
        if markers_allowed and stripped == END_TOC:
            if not in_toc or end_idx != -1:
                raise ValueError("unexpected ToC end marker")
            end_idx = idx
            in_toc = False
            continue
        if in_toc:
            if stripped:
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


def split_markdown_lines(content: str) -> list[MarkdownLine]:
    parts = LINE_ENDING_RE.split(content)
    lines = [
        MarkdownLine(parts[index], parts[index + 1])
        for index in range(0, len(parts) - 1, 2)
    ]
    if parts[-1]:
        lines.append(MarkdownLine(parts[-1], ""))
    return lines


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
    readme_path: Path,
    fix: bool,
    diff_max_lines: int = DEFAULT_DIFF_MAX_LINES,
    *,
    require_markers: bool = False,
) -> int:
    if not readme_path.is_file():
        print(f"Error: file not found: {readme_path}", file=sys.stderr)
        return 1
    with readme_path.open("r", encoding="utf-8", newline="") as readme_file:
        content = readme_file.read()
    markdown_lines = split_markdown_lines(content)
    lines = [line.text for line in markdown_lines]
    try:
        parsed = parse_markdown_toc(lines)
    except ValueError as exc:
        print(f"Error: {exc} in {readme_path}.", file=sys.stderr)
        return 1
    if parsed is None:
        if require_markers:
            print(
                f"Error: required ToC markers not found in {readme_path}.",
                file=sys.stderr,
            )
            return 1
        # No ToC markers found; treat as a no-op so repos without a ToC don't fail CI
        print(
            f"Note: Skipping ToC check; no markers found in {readme_path}.",
        )
        return 0
    if parsed.current == parsed.expected:
        return 0
    if not fix:
        print(
            "ERROR: README ToC is out of date. Diff between existing and generated ToC:",
            file=sys.stderr,
        )
        print_toc_diff(
            parsed.current,
            parsed.expected,
            max_lines=diff_max_lines,
            stream=sys.stderr,
        )
        return 1
    newline = markdown_lines[parsed.begin_idx].ending
    if not newline:
        newline = next((line.ending for line in markdown_lines if line.ending), "\n")
    prefix = "".join(
        line.text + line.ending for line in markdown_lines[: parsed.begin_idx + 1]
    )
    suffix = "".join(
        line.text + line.ending for line in markdown_lines[parsed.end_idx :]
    )
    generated_toc = "".join(f"{line}{newline}" for line in parsed.expected)
    new_content = prefix + newline + generated_toc + newline + suffix
    with readme_path.open("w", encoding="utf-8", newline="") as readme_file:
        readme_file.write(new_content)
    print(f"Updated ToC in {readme_path}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
