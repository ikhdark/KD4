#!/usr/bin/env python3
"""Validate SOURCEMAP.md structure and material repository inventories."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parents[1]
TOP_LEVEL_OWNERS_HEADING = "## Top-level ownership"
INSTRUCTION_SCOPES_HEADING = "## Instruction scopes"
RUST_PACKAGE_INVENTORY_HEADING = "## Rust package inventory"
NON_RUST_PROJECT_INVENTORY_HEADING = "## Non-Rust project inventory"
SECTION_HEADING_RE = re.compile(r"^##\s+")
CODE_SPAN_RE = re.compile(r"`([^`]+)`")
TABLE_SEPARATOR_RE = re.compile(r"^:?-{3,}:?$")
PROJECT_MANIFEST_NAMES = frozenset({"package.json", "pyproject.toml"})


def normalize_repo_path(path_text: str, *, line_number: int) -> str:
    normalized = path_text.rstrip("/")
    path = PurePosixPath(normalized)
    if (
        not normalized
        or normalized == "."
        or "\\" in normalized
        or path.is_absolute()
        or ".." in path.parts
        or re.match(r"^[A-Za-z]:", normalized)
    ):
        raise ValueError(
            f"line {line_number}: inventory path must be a repository-relative path: "
            f"{normalized!r}"
        )
    return path.as_posix()


def normalize_owner_path(owner: str, *, line_number: int) -> str:
    normalized = normalize_repo_path(owner, line_number=line_number)
    if len(PurePosixPath(normalized).parts) != 1:
        raise ValueError(
            f"line {line_number}: top-level owner must name one top-level entry: "
            f"{owner!r}"
        )
    return normalized


def table_rows(
    markdown: str,
    *,
    heading: str,
    headers: tuple[str, ...],
) -> list[tuple[int, list[str]]]:
    lines = markdown.splitlines()
    section_lines = [
        line_number
        for line_number, line in enumerate(lines, start=1)
        if line.strip() == heading
    ]
    if not section_lines:
        raise ValueError(f"missing {heading!r} section")
    if len(section_lines) > 1:
        raise ValueError(f"duplicate {heading!r} sections")

    label = heading.removeprefix("## ").lower()
    in_section = False
    saw_header = False
    saw_separator = False
    rows: list[tuple[int, list[str]]] = []

    for line_number, line in enumerate(lines, start=1):
        stripped = line.strip()
        if stripped == heading:
            in_section = True
            continue
        if in_section and SECTION_HEADING_RE.match(stripped):
            break
        if not in_section or not stripped:
            continue
        if not stripped.startswith("|") or not stripped.endswith("|"):
            raise ValueError(
                f"line {line_number}: {label} section must contain only a "
                "Markdown table"
            )

        cells = [cell.strip() for cell in stripped[1:-1].split("|")]
        if len(cells) != len(headers):
            raise ValueError(
                f"line {line_number}: {label} table must have exactly "
                f"{len(headers)} columns"
            )
        if not saw_header:
            if tuple(cells) != headers:
                raise ValueError(f"line {line_number}: expected {label} table header")
            saw_header = True
            continue
        if not saw_separator:
            if not all(TABLE_SEPARATOR_RE.fullmatch(cell) for cell in cells):
                raise ValueError(
                    f"line {line_number}: expected {label} table separator"
                )
            saw_separator = True
            continue
        if not all(cells):
            raise ValueError(f"line {line_number}: {label} rows require every column")
        rows.append((line_number, cells))

    if not saw_header:
        raise ValueError(f"{heading!r} section has no table header")
    if not saw_separator:
        raise ValueError(f"{heading!r} section has no table separator")
    return rows


def declared_paths(
    markdown: str,
    *,
    heading: str,
    headers: tuple[str, ...],
    path_column: int,
    top_level: bool = False,
) -> list[tuple[int, str]]:
    paths: list[tuple[int, str]] = []
    seen: dict[str, int] = {}
    label = heading.removeprefix("## ").lower()

    for line_number, cells in table_rows(
        markdown,
        heading=heading,
        headers=headers,
    ):
        raw_paths = CODE_SPAN_RE.findall(cells[path_column])
        if not raw_paths:
            raise ValueError(f"line {line_number}: {label} paths must use code spans")
        for raw_path in raw_paths:
            path = (
                normalize_owner_path(raw_path, line_number=line_number)
                if top_level
                else normalize_repo_path(raw_path, line_number=line_number)
            )
            if previous_line := seen.get(path):
                raise ValueError(
                    f"line {line_number}: duplicate {label} path {path!r}; "
                    f"first declared on line {previous_line}"
                )
            seen[path] = line_number
            paths.append((line_number, path))
    return paths


def declared_top_level_owners(markdown: str) -> list[tuple[int, str]]:
    owners = declared_paths(
        markdown,
        heading=TOP_LEVEL_OWNERS_HEADING,
        headers=("Path", "Owns"),
        path_column=0,
        top_level=True,
    )
    if not owners:
        raise ValueError("top-level ownership table declares no paths")
    return owners


def declared_instruction_scopes(markdown: str) -> list[tuple[int, str]]:
    return declared_paths(
        markdown,
        heading=INSTRUCTION_SCOPES_HEADING,
        headers=("Path", "Applies to"),
        path_column=0,
    )


def declared_rust_package_roots(markdown: str) -> list[tuple[int, str]]:
    return declared_paths(
        markdown,
        heading=RUST_PACKAGE_INVENTORY_HEADING,
        headers=("Domain", "Package roots"),
        path_column=1,
    )


def declared_non_rust_manifests(markdown: str) -> list[tuple[int, str]]:
    return declared_paths(
        markdown,
        heading=NON_RUST_PROJECT_INVENTORY_HEADING,
        headers=("Manifest", "Owns"),
        path_column=0,
    )


def git_source_paths(repo_root: Path, *, tracked_only: bool) -> set[str]:
    args = ["git", "ls-files", "--cached"]
    if not tracked_only:
        args.extend(["--others", "--exclude-standard"])
    args.append("-z")
    result = subprocess.run(
        args,
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


def repository_source_paths(repo_root: Path) -> set[str]:
    return git_source_paths(repo_root, tracked_only=False)


def repository_tracked_source_paths(repo_root: Path) -> set[str]:
    return git_source_paths(repo_root, tracked_only=True)


def owner_has_source(owner: str, source_paths: set[str]) -> bool:
    prefix = f"{owner}/"
    return owner in source_paths or any(
        path.startswith(prefix) for path in source_paths
    )


def top_level_entries(source_paths: set[str]) -> set[str]:
    return {PurePosixPath(path).parts[0] for path in source_paths}


def instruction_scope_paths(source_paths: set[str]) -> set[str]:
    return {
        path
        for path in source_paths
        if PurePosixPath(path).name.casefold() == "agents.md"
    }


def rust_package_roots(source_paths: set[str]) -> set[str]:
    return {
        PurePosixPath(path).parent.as_posix()
        for path in source_paths
        if PurePosixPath(path).name == "Cargo.toml"
    }


def non_rust_project_manifests(source_paths: set[str]) -> set[str]:
    return {
        path
        for path in source_paths
        if PurePosixPath(path).name in PROJECT_MANIFEST_NAMES
    }


def report_inventory_drift(
    source_map: Path,
    *,
    label: str,
    declared: list[tuple[int, str]],
    expected: set[str],
) -> bool:
    failed = False
    declared_by_path = {path: line_number for line_number, path in declared}
    for path in sorted(expected - declared_by_path.keys()):
        print(
            f"{source_map}: missing {label} entry for tracked path: {path}",
            file=sys.stderr,
        )
        failed = True
    for path in sorted(declared_by_path.keys() - expected):
        print(
            f"{source_map}:{declared_by_path[path]}: {label} entry is not backed "
            f"by a tracked path: {path}",
            file=sys.stderr,
        )
        failed = True
    return failed


def check_source_map(
    source_map: Path,
    *,
    repo_root: Path | None = None,
    source_paths: set[str] | None = None,
    tracked_source_paths: set[str] | None = None,
) -> int:
    root = repo_root if repo_root is not None else source_map.resolve().parent
    try:
        markdown = source_map.read_text(encoding="utf-8")
        owners = declared_top_level_owners(markdown)
        instruction_scopes = declared_instruction_scopes(markdown)
        rust_packages = declared_rust_package_roots(markdown)
        non_rust_manifests = declared_non_rust_manifests(markdown)
        sources = (
            source_paths if source_paths is not None else repository_source_paths(root)
        )
        tracked_sources = (
            tracked_source_paths
            if tracked_source_paths is not None
            else (
                source_paths
                if source_paths is not None
                else repository_tracked_source_paths(root)
            )
        )
    except (OSError, UnicodeError, ValueError) as exc:
        print(f"{source_map}: {exc}", file=sys.stderr)
        return 1

    failed = False
    for line_number, owner in owners:
        if owner_has_source(owner, sources):
            continue
        print(
            f"{source_map}:{line_number}: declared owner has no repository source: "
            f"{owner}",
            file=sys.stderr,
        )
        failed = True

    inventories = (
        (
            "top-level ownership",
            owners,
            top_level_entries(tracked_sources),
        ),
        (
            "instruction scope",
            instruction_scopes,
            instruction_scope_paths(tracked_sources),
        ),
        (
            "Rust package",
            rust_packages,
            rust_package_roots(tracked_sources),
        ),
        (
            "non-Rust project manifest",
            non_rust_manifests,
            non_rust_project_manifests(tracked_sources),
        ),
    )
    for label, declared, expected in inventories:
        failed |= report_inventory_drift(
            source_map,
            label=label,
            declared=declared,
            expected=expected,
        )
    return 1 if failed else 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Check SOURCEMAP.md structure and its material repository inventories."
        )
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
        help="Repository root used to resolve declared inventory paths.",
    )
    args = parser.parse_args(argv)
    return check_source_map(args.source_map, repo_root=args.repo_root)


if __name__ == "__main__":
    raise SystemExit(main())
