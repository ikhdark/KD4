#!/usr/bin/env python3

from __future__ import annotations

import argparse
import html
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


DEFAULT_MAX_BYTES = 500 * 1024


@dataclass(frozen=True, slots=True)
class ChangedBlob:
    path: str
    size_bytes: int
    is_allowlisted: bool
    is_binary: bool


@dataclass(frozen=True, slots=True)
class ChangedPath:
    path: str
    is_binary: bool


def run_git(*args: str, input_text: str | None = None) -> str:
    result = subprocess.run(
        ["git", *args],
        check=True,
        capture_output=True,
        text=True,
        # git emits UTF-8 paths; the Windows cp1252 default either crashes the
        # decode or mojibakes names so allowlist matching silently fails.
        encoding="utf-8",
        errors="replace",
        input=input_text,
    )
    return result.stdout


def load_allowlist(path: Path) -> set[str]:
    allowlist: set[str] = set()
    with path.open(encoding="utf-8") as handle:
        for raw_line in handle:
            line = raw_line.split("#", 1)[0].strip()
            if line:
                allowlist.add(line)
    return allowlist


def parse_paths(text: str) -> list[str]:
    delimiter = "\0" if "\0" in text else "\n"
    return [path for path in text.split(delimiter) if path]


def load_paths_file(path: Path) -> list[str]:
    return parse_paths(path.read_text(encoding="utf-8"))


def parse_numstat_z(output: str) -> list[ChangedPath]:
    changed: list[ChangedPath] = []
    for record in output.split("\0"):
        if not record:
            continue
        added, deleted, path = record.split("\t", 2)
        changed.append(
            ChangedPath(path=path, is_binary=added == "-" and deleted == "-")
        )
    return changed


def get_changed_paths(
    base: str,
    head: str,
    *,
    include_kind: bool,
    run_git_func=run_git,
    paths: list[str] | None = None,
) -> list[ChangedPath]:
    args = [
        "diff",
        "--numstat",
        "--diff-filter=AM",
        "--no-renames",
        "-z",
        base,
        head,
    ]
    if paths is not None:
        if not include_kind:
            return [ChangedPath(path=path, is_binary=False) for path in paths]
        args.extend(["--", *paths])
        output = run_git_func(*args)
        binary_by_path = {
            changed.path: changed.is_binary for changed in parse_numstat_z(output)
        }
        return [
            ChangedPath(path=path, is_binary=binary_by_path.get(path, False))
            for path in paths
        ]
    output = run_git_func(*args)
    return parse_numstat_z(output)


def batch_blob_sizes(
    commit: str, paths: list[str], *, run_git_func=run_git
) -> dict[str, int]:
    if not paths:
        return {}
    input_text = "".join(f"{commit}:{path}\0" for path in paths)
    output = run_git_func(
        "cat-file", "-Z", "--batch-check=%(objectsize)", input_text=input_text
    )
    sizes = [int(entry) for entry in output.split("\0") if entry]
    if len(sizes) != len(paths):
        raise RuntimeError(
            f"git cat-file returned {len(sizes)} size(s) for {len(paths)} path(s)"
        )
    return dict(zip(paths, sizes, strict=True))


def collect_changed_blobs(
    base: str,
    head: str,
    allowlist: set[str],
    *,
    paths: list[str] | None = None,
    include_kind: bool = False,
    run_git_func=run_git,
) -> list[ChangedBlob]:
    blobs: list[ChangedBlob] = []
    changed_paths = get_changed_paths(
        base,
        head,
        include_kind=include_kind,
        paths=paths,
        run_git_func=run_git_func,
    )
    sizes = batch_blob_sizes(
        head, [changed.path for changed in changed_paths], run_git_func=run_git_func
    )
    for changed in changed_paths:
        blobs.append(
            ChangedBlob(
                path=changed.path,
                size_bytes=sizes[changed.path],
                is_allowlisted=changed.path in allowlist,
                is_binary=changed.is_binary,
            )
        )
    return blobs


def format_kib(size_bytes: int) -> str:
    return f"{size_bytes / 1024:.1f} KiB"


def write_step_summary(
    max_bytes: int,
    blobs: list[ChangedBlob],
    violations: list[ChangedBlob],
    *,
    include_kind: bool,
) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return

    lines = [
        "## Blob Size Policy",
        "",
        f"Default max: `{max_bytes}` bytes ({format_kib(max_bytes)})",
        f"Changed files checked: `{len(blobs)}`",
        f"Violations: `{len(violations)}`",
        "",
    ]

    violation_paths = {blob.path for blob in violations}
    if blobs:
        if include_kind:
            lines.extend(
                [
                    "| Path | Kind | Size | Status |",
                    "| --- | --- | ---: | --- |",
                ]
            )
        else:
            lines.extend(
                [
                    "| Path | Size | Status |",
                    "| --- | ---: | --- |",
                ]
            )
        for blob in blobs:
            status = blob_status(blob, violation_paths)
            kind = "binary" if blob.is_binary else "non-binary"
            size = f"{blob.size_bytes} bytes ({format_kib(blob.size_bytes)})"
            if include_kind:
                lines.append(
                    f"| {markdown_code(blob.path)} | {kind} | {markdown_code(size)} | {status} |"
                )
            else:
                lines.append(
                    f"| {markdown_code(blob.path)} | {markdown_code(size)} | {status} |"
                )
    else:
        lines.append("No changed files were detected.")

    lines.append("")
    Path(summary_path).write_text("\n".join(lines), encoding="utf-8")


def markdown_code(value: str) -> str:
    return f"<code>{html.escape(value).replace('|', '&#124;')}</code>"


def blob_status(blob: ChangedBlob, violation_paths: set[str]) -> str:
    if blob.path in violation_paths:
        return "blocked"
    if blob.is_allowlisted:
        return "allowlisted"
    return "ok"


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Fail if changed blobs exceed the configured size budget."
    )
    parser.add_argument(
        "--base", required=True, help="Base git revision to diff against."
    )
    parser.add_argument("--head", required=True, help="Head git revision to inspect.")
    parser.add_argument(
        "--max-bytes",
        type=int,
        default=DEFAULT_MAX_BYTES,
        help=f"Maximum allowed blob size in bytes. Default: {DEFAULT_MAX_BYTES}.",
    )
    parser.add_argument(
        "--allowlist",
        type=Path,
        required=True,
        help="Path to the newline-delimited allowlist file.",
    )
    parser.add_argument(
        "--include-kind",
        action="store_true",
        help="Include binary/non-binary kind in console and summary output.",
    )
    path_group = parser.add_mutually_exclusive_group()
    path_group.add_argument(
        "--stdin-paths",
        action="store_true",
        help="Read changed repo-relative paths from stdin instead of running git diff.",
    )
    path_group.add_argument(
        "--paths-file",
        type=Path,
        help="Read changed repo-relative paths from a newline- or NUL-delimited file.",
    )
    args = parser.parse_args()

    allowlist = load_allowlist(args.allowlist)
    paths = None
    if args.stdin_paths:
        paths = parse_paths(sys.stdin.read())
    elif args.paths_file is not None:
        paths = load_paths_file(args.paths_file)

    blobs = collect_changed_blobs(
        args.base,
        args.head,
        allowlist,
        paths=paths,
        include_kind=args.include_kind,
    )
    violations = [
        blob
        for blob in blobs
        if blob.size_bytes > args.max_bytes and not blob.is_allowlisted
    ]
    violation_paths = {blob.path for blob in violations}

    write_step_summary(
        args.max_bytes, blobs, violations, include_kind=args.include_kind
    )

    if not blobs:
        print("No changed files were detected.")
        return 0

    print(
        f"Checked {len(blobs)} changed file(s) against the {args.max_bytes}-byte limit."
    )
    for blob in blobs:
        status = blob_status(blob, violation_paths)
        kind = "binary" if blob.is_binary else "non-binary"
        size = f"{blob.size_bytes} bytes ({format_kib(blob.size_bytes)})"
        if args.include_kind:
            print(f"- {blob.path}: {size} [{kind}, {status}]")
        else:
            print(f"- {blob.path}: {size} [{status}]")

    if violations:
        print("\nFile(s) exceed the configured limit:")
        for blob in violations:
            print(f"- {blob.path}: {blob.size_bytes} bytes > {args.max_bytes} bytes")
        print(
            "\nIf one of these is a real checked-in asset we want to keep, add its "
            "repo-relative path to .github/blob-size-allowlist.txt. Otherwise, "
            "shrink it or keep it out of git."
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
