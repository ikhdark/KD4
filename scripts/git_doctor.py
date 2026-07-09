#!/usr/bin/env python3
"""Inspect local Git performance settings for this checkout."""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
PYTEST_CACHE_DIRS = (Path(".pytest_cache"), Path("sdk/python/.pytest_cache"))


@dataclass(frozen=True)
class GitDoctorReport:
    repo_root: str
    platform: str
    path_kind: str
    fsmonitor: str | None
    untracked_cache: str | None
    unreadable_pytest_caches: tuple[str, ...]
    status_seconds: float | None
    status_timed_out: bool
    recommendations: tuple[str, ...]


def run_git(
    args: Sequence[str], *, timeout: float = 5.0
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        cwd=REPO_ROOT,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )


def git_config(name: str) -> str | None:
    try:
        completed = run_git(["config", "--get", name])
    except (OSError, subprocess.TimeoutExpired):
        return None
    value = completed.stdout.strip()
    return value or None


def path_kind(path: Path) -> str:
    text = path.as_posix()
    if text.startswith("/mnt/") or text.startswith("/run/desktop/mnt/host/"):
        return "wsl-windows-mount"
    if os.name == "nt":
        return "windows"
    if "microsoft" in platform.uname().release.lower():
        return "wsl-native"
    return platform.system().lower() or "unknown"


def timed_status(timeout: float) -> tuple[float | None, bool]:
    started = time.monotonic()
    try:
        completed = run_git(
            ["status", "--short", "--untracked-files=no"], timeout=timeout
        )
    except subprocess.TimeoutExpired:
        return None, True
    elapsed = time.monotonic() - started
    if completed.returncode != 0:
        return elapsed, False
    return elapsed, False


def directory_is_readable(path: Path) -> bool:
    try:
        if not path.is_dir():
            return True
        with os.scandir(path) as entries:
            next(entries, None)
    except OSError:
        return False
    return True


def unreadable_pytest_cache_dirs(repo_root: Path) -> tuple[str, ...]:
    unreadable: list[str] = []
    for relative_path in PYTEST_CACHE_DIRS:
        path = repo_root / relative_path
        if not directory_is_readable(path):
            unreadable.append(f"{relative_path.as_posix()}/")
    return tuple(unreadable)


def recommendations(
    kind: str,
    fsmonitor: str | None,
    untracked_cache: str | None,
    unreadable_pytest_caches: Sequence[str] = (),
) -> tuple[str, ...]:
    items: list[str] = []
    if fsmonitor not in {"true", "builtin"}:
        items.append(
            "Enable Git FSMonitor for this repo: `git config core.fsmonitor true`."
        )
    if untracked_cache != "true":
        items.append(
            "Enable the untracked cache: `git config core.untrackedCache true`."
        )
    if kind == "wsl-windows-mount":
        items.append(
            "This checkout is on a Windows-mounted WSL path; move heavy Rust work to a WSL-native path such as `~/src/KD4` if status/builds remain slow."
        )
    if unreadable_pytest_caches:
        paths = ", ".join(f"`{path}`" for path in unreadable_pytest_caches)
        items.append(
            f"Fix unreadable generated pytest cache state ({paths}); delete the "
            "cache directories or restore directory read/list permissions. These "
            "caches are ignored local state, not source dirt."
        )
    return tuple(items)


def build_report(timeout: float) -> GitDoctorReport:
    try:
        root = run_git(["rev-parse", "--show-toplevel"]).stdout.strip() or str(
            REPO_ROOT
        )
    except (OSError, subprocess.TimeoutExpired):
        root = str(REPO_ROOT)
    kind = path_kind(Path(root))
    fsmonitor = git_config("core.fsmonitor")
    untracked_cache = git_config("core.untrackedCache")
    unreadable_pytest_caches = unreadable_pytest_cache_dirs(Path(root))
    seconds, timed_out = timed_status(timeout)
    recs = recommendations(
        kind, fsmonitor, untracked_cache, unreadable_pytest_caches
    )
    if timed_out:
        recs = (
            *recs,
            f"`git status --short --untracked-files=no` exceeded {timeout:g}s.",
        )
    return GitDoctorReport(
        repo_root=root,
        platform=platform.platform(),
        path_kind=kind,
        fsmonitor=fsmonitor,
        untracked_cache=untracked_cache,
        unreadable_pytest_caches=unreadable_pytest_caches,
        status_seconds=seconds,
        status_timed_out=timed_out,
        recommendations=recs,
    )


def print_report(report: GitDoctorReport) -> None:
    print("Git performance doctor")
    print(f"- repo: {report.repo_root}")
    print(f"- path kind: {report.path_kind}")
    print(f"- core.fsmonitor: {report.fsmonitor or '<unset>'}")
    print(f"- core.untrackedCache: {report.untracked_cache or '<unset>'}")
    if report.unreadable_pytest_caches:
        print(
            "- unreadable pytest caches: "
            + ", ".join(report.unreadable_pytest_caches)
        )
    if report.status_timed_out:
        print("- status check: timed out")
    elif report.status_seconds is not None:
        print(f"- status check: {report.status_seconds:.2f}s")
    if report.recommendations:
        print("Recommendations:")
        for item in report.recommendations:
            print(f"- {item}")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--json", action="store_true", help="Emit machine-readable output."
    )
    parser.add_argument(
        "--timeout", type=float, default=8.0, help="Status timeout in seconds."
    )
    args = parser.parse_args(argv)

    try:
        report = build_report(args.timeout)
    except OSError as exc:
        print(f"git doctor failed: {exc}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(asdict(report), indent=2))
    else:
        print_report(report)
    return 1 if report.status_timed_out else 0


if __name__ == "__main__":
    raise SystemExit(main())
