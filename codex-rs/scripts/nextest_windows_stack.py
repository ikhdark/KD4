#!/usr/bin/env python3
"""Set the isolated environment required by Windows nextest workers."""

from __future__ import annotations

import os
import sys
from pathlib import Path


def main() -> int:
    nextest_env = os.environ.get("NEXTEST_ENV")
    if not nextest_env:
        raise SystemExit("NEXTEST_ENV is required")
    stack = os.environ.get("RUST_MIN_STACK", "8388608").removeprefix("+")
    digits = stack.lstrip("0") or "0"
    if (
        not stack.isascii()
        or not stack.isdecimal()
        or len(digits) > 20
        or int(digits) > sys.maxsize * 2 + 1
    ):
        raise SystemExit(
            "RUST_MIN_STACK must be an unsigned pointer-sized integer in bytes"
        )
    # Keep the Windows minimum without undoing a larger explicit worker stack.
    stack = max(8388608, int(digits))
    with Path(nextest_env).open("a", encoding="utf-8", newline="\n") as env_file:
        # Desktop exports process-scoped CODEX_* state (homes, permission
        # profiles, helper paths, task IDs, and app pipes). Repository tests
        # must start clean; fixtures that exercise an override set it explicitly.
        for name in sorted(name for name in os.environ if name.startswith("CODEX_")):
            env_file.write(f"{name}=\n")
        env_file.write(f"RUST_MIN_STACK={stack}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
