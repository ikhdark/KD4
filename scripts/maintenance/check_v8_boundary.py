#!/usr/bin/env python3
"""Fail when V8 enters the normal dependency graph of Codex itself.

Code Mode runs V8 only in the standalone `codex-code-mode-host` process, so a
V8 crash or out-of-memory abort cannot take Codex down with it.
"""

from __future__ import annotations

import subprocess
import sys
from collections.abc import Callable, Sequence

Runner = Callable[..., subprocess.CompletedProcess[str]]
PACKAGES = ("codex-cli", "codex-core")


def check_v8_boundary(
    extra_args: Sequence[str], *, runner: Runner = subprocess.run
) -> int:
    for package in PACKAGES:
        result = runner(
            [
                "cargo",
                "tree",
                "-p",
                package,
                "-e",
                "normal",
                "--prefix",
                "none",
                "--format",
                "{p}",
                *extra_args,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            print(result.stderr, end="", file=sys.stderr)
            return result.returncode
        if any(line.startswith("v8 ") for line in result.stdout.splitlines()):
            print(
                f"{package} links V8; only codex-code-mode-host may",
                file=sys.stderr,
            )
            return 1
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    return check_v8_boundary(list(argv if argv is not None else sys.argv[1:]))


if __name__ == "__main__":
    raise SystemExit(main())
