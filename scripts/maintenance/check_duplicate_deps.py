#!/usr/bin/env python3
"""Fail when Cargo reports duplicate versions in the selected dependency graph."""

from __future__ import annotations

import subprocess
import sys
from collections.abc import Callable, Sequence


Runner = Callable[..., subprocess.CompletedProcess[str]]


def check_duplicate_deps(
    extra_args: Sequence[str], *, runner: Runner = subprocess.run
) -> int:
    result = runner(
        ["cargo", "tree", "-d", "-p", "codex-cli", *extra_args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if result.returncode != 0:
        return result.returncode
    if result.stdout.strip():
        print("duplicate dependency versions detected", file=sys.stderr)
        return 1
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    args = list(argv if argv is not None else sys.argv[1:])
    if args[:1] == ["--policy"]:
        return check_dependency_policy(args[1:])
    return check_duplicate_deps(args)


def check_dependency_policy(
    extra_args: Sequence[str], *, runner: Runner = subprocess.run
) -> int:
    """Report every independent policy result without weakening duplicate policy."""
    commands = [
        ["just", "deps-advisories-check"],
        ["just", "deps-duplicates-check", "--locked", *extra_args],
        ["just", "deps-v8-boundary-check", "--locked", *extra_args],
        ["cargo", "deny", "check", "bans", "sources", "licenses"],
    ]
    failed = False
    for command in commands:
        try:
            result = runner(command, check=False)
            code = result.returncode
        except OSError as error:
            print(f"{' '.join(command)}: {error}", file=sys.stderr)
            code = 2
        print(f"dependency policy: {' '.join(command)}: exit {code}", file=sys.stderr)
        failed |= code != 0
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
