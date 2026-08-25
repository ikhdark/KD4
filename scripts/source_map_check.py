#!/usr/bin/env python3
"""Synchronize and validate SOURCEMAP.md repository inventories."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path, PurePosixPath

try:
    from scripts.generated_output_lock import GenerationLockError, source_map_lock
except ModuleNotFoundError:
    from generated_output_lock import GenerationLockError, source_map_lock

REPO_ROOT = Path(__file__).resolve().parents[1]
TOP_LEVEL_OWNERS_HEADING = "## Top-level ownership"
INSTRUCTION_SCOPES_HEADING = "## Instruction scopes"
RUST_PACKAGE_INVENTORY_HEADING = "## Rust package inventory"
NON_RUST_PROJECT_INVENTORY_HEADING = "## Non-Rust project inventory"
SECTION_HEADING_RE = re.compile(r"^##\s+")
CODE_SPAN_RE = re.compile(r"`([^`]+)`")
TABLE_SEPARATOR_RE = re.compile(r"^:?-{3,}:?$")
PROJECT_MANIFEST_NAMES = frozenset({"package.json", "pyproject.toml"})
TRACKED_PATH_SNAPSHOT_BEGIN = "<!-- BEGIN TRACKED PATH SNAPSHOT -->"
TRACKED_PATH_SNAPSHOT_END = "<!-- END TRACKED PATH SNAPSHOT -->"
TRACKED_PATH_SNAPSHOT_INSERT_AFTER = (
    "Update it in the same change whenever the repository materially changes."
)


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


def repository_source_inventory(repo_root: Path) -> tuple[set[str], set[str]]:
    args = [
        "git",
        "ls-files",
        "-t",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ]
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
    source_paths: set[str] = set()
    tracked_source_paths: set[str] = set()
    for record in result.stdout.split("\0"):
        if not record:
            continue
        if len(record) < 3 or record[1] != " ":
            raise ValueError("git ls-files returned an invalid tagged path record")
        path = PurePosixPath(record[2:]).as_posix()
        if not (repo_root / path).is_file():
            continue
        source_paths.add(path)
        if record[0] != "?":
            tracked_source_paths.add(path)
    return source_paths, tracked_source_paths


def repository_source_paths(repo_root: Path) -> set[str]:
    return repository_source_inventory(repo_root)[0]


def repository_tracked_source_paths(repo_root: Path) -> set[str]:
    return repository_source_inventory(repo_root)[1]


def tracked_path_snapshot(source_paths: set[str]) -> tuple[int, str]:
    payload = "".join(f"{path}\n" for path in sorted(source_paths)).encode("utf-8")
    return len(source_paths), hashlib.sha256(payload).hexdigest()


def render_tracked_path_snapshot(source_paths: set[str], *, newline: str) -> str:
    count, digest = tracked_path_snapshot(source_paths)
    return newline.join(
        (
            TRACKED_PATH_SNAPSHOT_BEGIN,
            f"Tracked repository path snapshot: `count={count} sha256={digest}`.",
            TRACKED_PATH_SNAPSHOT_END,
        )
    )


def sync_tracked_path_snapshot(
    source_map: Path,
    *,
    repo_root: Path | None = None,
    source_paths: set[str] | None = None,
) -> bool:
    root = repo_root if repo_root is not None else source_map.resolve().parent
    raw_markdown = source_map.read_bytes().decode("utf-8")
    newline = "\r\n" if "\r\n" in raw_markdown else "\n"
    sources = source_paths if source_paths is not None else repository_tracked_source_paths(root)
    snapshot = render_tracked_path_snapshot(sources, newline=newline)
    block_re = re.compile(
        rf"{re.escape(TRACKED_PATH_SNAPSHOT_BEGIN)}.*?"
        rf"{re.escape(TRACKED_PATH_SNAPSHOT_END)}",
        flags=re.DOTALL,
    )
    matches = list(block_re.finditer(raw_markdown))
    if len(matches) > 1:
        raise ValueError("duplicate tracked path snapshot blocks")
    if matches:
        updated = block_re.sub(snapshot, raw_markdown, count=1)
    else:
        anchor = TRACKED_PATH_SNAPSHOT_INSERT_AFTER
        if raw_markdown.count(anchor) != 1:
            raise ValueError(
                "cannot place tracked path snapshot: maintenance anchor must occur once"
            )
        updated = raw_markdown.replace(
            anchor,
            f"{anchor}{newline}{newline}{snapshot}",
            1,
        )
    if updated == raw_markdown:
        return False
    temporary = source_map.with_name(f".{source_map.name}.{os.getpid()}.tmp")
    try:
        temporary.write_bytes(updated.encode("utf-8"))
        os.replace(temporary, source_map)
    finally:
        temporary.unlink(missing_ok=True)
    return True


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
        if source_paths is None and tracked_source_paths is None:
            sources, tracked_sources = repository_source_inventory(root)
        else:
            sources = (
                source_paths
                if source_paths is not None
                else repository_source_paths(root)
            )
            tracked_sources = (
                tracked_source_paths
                if tracked_source_paths is not None
                else sources
            )
    except (OSError, UnicodeError, ValueError) as exc:
        print(f"{source_map}: {exc}", file=sys.stderr)
        return 1

    failed = False
    for line_number, owner in owners:
        if owner_has_source(owner, tracked_sources):
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
            "Synchronize SOURCEMAP.md's tracked path snapshot, then check its "
            "structure and material repository inventories."
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
    root = (
        args.repo_root
        if args.repo_root is not None
        else args.source_map.resolve().parent
    )
    try:
        with source_map_lock(root, f"source-map-check:{os.getpid()}"):
            source_paths, tracked_source_paths = repository_source_inventory(root)
            sync_tracked_path_snapshot(
                args.source_map,
                repo_root=root,
                source_paths=tracked_source_paths,
            )
            return check_source_map(
                args.source_map,
                repo_root=root,
                source_paths=source_paths,
                tracked_source_paths=tracked_source_paths,
            )
    except (GenerationLockError, OSError, UnicodeError, ValueError) as exc:
        print(f"{args.source_map}: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
