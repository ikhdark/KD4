#!/usr/bin/env python3

"""Prepare declared ignored files for Codex-created worktrees."""

import argparse
import hashlib
import shutil
import subprocess
from dataclasses import dataclass
from functools import cache
from pathlib import Path


@dataclass(frozen=True)
class CopySpec:
    repo_relative_path: str
    reason: str


@dataclass(frozen=True)
class WorktreePaths:
    current: Path
    main: Path


@dataclass
class CopyResult:
    status: str
    path: str


class SetupError(RuntimeError):
    pass


COPY_SPECS: tuple[CopySpec, ...] = (
    CopySpec(
        repo_relative_path="user.bazelrc",
        reason="Bazel user config imported by checked-in .bazelrc",
    ),
)


@cache
def worktree_paths() -> WorktreePaths:
    script_dir = Path(__file__).resolve().parent
    worktree_root = git_path(script_dir / "../..", "--show-toplevel")
    common_git_dir = git_path(worktree_root, "--git-common-dir")
    main_worktree = common_git_dir.parent

    if not main_worktree.is_dir():
        raise SetupError(f"could not resolve main worktree from git common dir: {common_git_dir}")

    return WorktreePaths(current=worktree_root, main=main_worktree)


def git_path(working_directory: Path, argument: str) -> Path:
    command = [
        "git",
        "-C",
        str(working_directory),
        "rev-parse",
        "--path-format=absolute",
        argument,
    ]
    try:
        output = subprocess.check_output(command, stderr=subprocess.PIPE, text=True)
    except FileNotFoundError as exc:
        raise SetupError("git is required for Codex environment setup") from exc
    except subprocess.CalledProcessError as exc:
        stderr = exc.stderr.strip()
        detail = f": {stderr}" if stderr else ""
        raise SetupError(f"failed to run {' '.join(command)}{detail}") from exc

    return Path(output.strip())


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_repo_relative_path(repo_relative_path: str) -> Path:
    relative_path = Path(repo_relative_path)
    if relative_path.is_absolute() or ".." in relative_path.parts:
        raise ValueError(f"path must be repository-relative: {repo_relative_path}")
    return relative_path


def copy_from_main_worktree_to_worktree(
    spec: CopySpec,
    *,
    dry_run: bool,
    force: bool,
) -> CopyResult:
    relative_path = validate_repo_relative_path(spec.repo_relative_path)
    paths = worktree_paths()
    source_path = paths.main / relative_path
    destination_path = paths.current / relative_path

    print(f"* {spec.repo_relative_path}")
    print(f"  reason: {spec.reason}")
    print(f"  source: {source_path}")
    print(f"  destination: {destination_path}")

    if source_path == destination_path:
        print("  result: running in the main worktree; nothing to copy")
        return CopyResult(status="skipped", path=spec.repo_relative_path)

    if not source_path.is_file():
        print("  result: source does not exist; nothing to copy")
        return CopyResult(status="missing", path=spec.repo_relative_path)

    if destination_path.exists():
        if not destination_path.is_file():
            print("  result: destination exists but is not a file")
            return CopyResult(status="failed", path=spec.repo_relative_path)
        if file_sha256(source_path) == file_sha256(destination_path):
            print("  result: destination already current")
            return CopyResult(status="current", path=spec.repo_relative_path)
        if not force:
            print("  result: destination differs; use --force to overwrite")
            return CopyResult(status="skipped", path=spec.repo_relative_path)

    action = "overwrote" if destination_path.exists() else "copied"
    if dry_run:
        print(f"  result: would {action} {spec.repo_relative_path}")
        return CopyResult(status="planned", path=spec.repo_relative_path)

    destination_path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source_path, destination_path)
    print(f"  result: {action} {spec.repo_relative_path}")
    return CopyResult(status="copied", path=spec.repo_relative_path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Prepare declared ignored files for Codex worktrees."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print planned copies without changing files.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Overwrite destination files that differ from the main worktree.",
    )
    return parser.parse_args()


def print_summary(results: list[CopyResult]) -> None:
    statuses = ("copied", "planned", "current", "skipped", "missing", "failed")
    counts = {status: 0 for status in statuses}
    for result in results:
        counts[result.status] += 1

    summary = ", ".join(f"{status}: {counts[status]}" for status in statuses)
    print(f"Summary: {summary}")


def main() -> None:
    args = parse_args()
    print("Codex environment setup:")
    print(
        "Declared setup files only; generated, vendored, cache, and broad ignored "
        "paths are not copied unless listed."
    )
    # See codex-rs/docs/bazel.md for the repository's Bazel workflow.
    results = [
        copy_from_main_worktree_to_worktree(spec, dry_run=args.dry_run, force=args.force)
        for spec in COPY_SPECS
    ]
    print_summary(results)
    if any(result.status == "failed" for result in results):
        raise SystemExit(1)


if __name__ == "__main__":
    try:
        main()
    except SetupError as exc:
        raise SystemExit(f"error: {exc}") from exc
