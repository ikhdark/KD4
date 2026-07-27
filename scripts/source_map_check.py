#!/usr/bin/env python3
"""Validate that repository owners declared in SOURCEMAP.md exist."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parents[1]
TOP_LEVEL_OWNERS_HEADING = "## Top-level ownership"
SECTION_HEADING_RE = re.compile(r"^##\s+")
CODE_SPAN_RE = re.compile(r"`([^`]+)`")
TABLE_SEPARATOR_RE = re.compile(r"^:?-{3,}:?$")


def normalize_owner_path(owner: str, *, line_number: int) -> str:
    owner = owner.rstrip("/")
    path = PurePosixPath(owner)
    if (
        not owner
        or owner == "."
        or "\\" in owner
        or path.is_absolute()
        or ".." in path.parts
        or re.match(r"^[A-Za-z]:", owner)
    ):
        raise ValueError(
            f"line {line_number}: top-level owner must be a repository-relative path: "
            f"{owner!r}"
        )
    return path.as_posix()


def declared_top_level_owners(markdown: str) -> list[tuple[int, str]]:
    owners: list[tuple[int, str]] = []
    lines = markdown.splitlines()
    section_lines = [
        line_number
        for line_number, line in enumerate(lines, start=1)
        if line.strip() == TOP_LEVEL_OWNERS_HEADING
    ]
    if not section_lines:
        raise ValueError(f"missing {TOP_LEVEL_OWNERS_HEADING!r} section")
    if len(section_lines) > 1:
        raise ValueError(f"duplicate {TOP_LEVEL_OWNERS_HEADING!r} sections")

    in_section = False
    saw_header = False
    saw_separator = False

    for line_number, line in enumerate(lines, start=1):
        stripped = line.strip()
        if stripped == TOP_LEVEL_OWNERS_HEADING:
            in_section = True
            continue
        if in_section and SECTION_HEADING_RE.match(stripped):
            break
        if not in_section or not stripped:
            continue
        if not stripped.startswith("|") or not stripped.endswith("|"):
            raise ValueError(
                f"line {line_number}: top-level ownership section must contain "
                "only a Markdown table"
            )

        cells = [cell.strip() for cell in stripped[1:-1].split("|")]
        if len(cells) != 2:
            raise ValueError(
                f"line {line_number}: top-level ownership table must have "
                "exactly two columns"
            )
        path_cell, owns_cell = cells
        if not saw_header:
            if cells != ["Path", "Owns"]:
                raise ValueError(
                    f"line {line_number}: expected top-level ownership table header"
                )
            saw_header = True
            continue
        if not saw_separator:
            if not all(TABLE_SEPARATOR_RE.fullmatch(cell) for cell in cells):
                raise ValueError(
                    f"line {line_number}: expected top-level ownership table separator"
                )
            saw_separator = True
            continue
        if not path_cell or not owns_cell:
            raise ValueError(
                f"line {line_number}: top-level ownership rows require both columns"
            )

        paths = CODE_SPAN_RE.findall(path_cell)
        if not paths:
            raise ValueError(
                f"line {line_number}: top-level owner paths must use code spans"
            )
        owners.extend(
            (
                line_number,
                normalize_owner_path(path, line_number=line_number),
            )
            for path in paths
        )

    if not owners:
        raise ValueError("top-level ownership table declares no paths")
    return owners


def repository_source_paths(repo_root: Path) -> set[str]:
    result = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or f"git ls-files exited {result.returncode}"
        raise ValueError(f"failed to enumerate repository sources: {detail}")
    return {
        PurePosixPath(path).as_posix()
        for path in result.stdout.split("\0")
        if path and (repo_root / path).is_file()
    }


def owner_has_source(owner: str, source_paths: set[str]) -> bool:
    prefix = f"{owner}/"
    return owner in source_paths or any(
        path.startswith(prefix) for path in source_paths
    )


def check_source_map(
    source_map: Path,
    *,
    repo_root: Path | None = None,
    source_paths: set[str] | None = None,
) -> int:
    root = repo_root if repo_root is not None else source_map.resolve().parent
    try:
        markdown = source_map.read_text(encoding="utf-8")
        owners = declared_top_level_owners(markdown)
        sources = (
            source_paths if source_paths is not None else repository_source_paths(root)
        )
    except (OSError, UnicodeError, ValueError) as exc:
        print(f"{source_map}: {exc}", file=sys.stderr)
        return 1

    missing = [
        (line_number, owner)
        for line_number, owner in owners
        if not owner_has_source(owner, sources)
    ]
    for line_number, owner in missing:
        print(
            f"{source_map}:{line_number}: declared owner has no repository source: {owner}",
            file=sys.stderr,
        )
    return 1 if missing else 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check that top-level owners declared in SOURCEMAP.md exist."
    )
    parser.add_argument(
        "source_map",
        nargs="?",
        type=Path,
        default=REPO_ROOT / "SOURCEMAP.md",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        help="Repository root used to resolve declared owner paths.",
    )
    args = parser.parse_args(argv)
    return check_source_map(args.source_map, repo_root=args.repo_root)


if __name__ == "__main__":
    raise SystemExit(main())
