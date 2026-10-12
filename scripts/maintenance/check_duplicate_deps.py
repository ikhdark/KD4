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
    return check_duplicate_deps(list(argv if argv is not None else sys.argv[1:]))


if __name__ == "__main__":
    raise SystemExit(main())
