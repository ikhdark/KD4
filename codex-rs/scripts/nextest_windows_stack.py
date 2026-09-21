#!/usr/bin/env python3
"""Set the isolated environment required by Windows nextest workers."""

from __future__ import annotations

import os
from pathlib import Path


def main() -> int:
    nextest_env = os.environ.get("NEXTEST_ENV")
    if not nextest_env:
        raise SystemExit("NEXTEST_ENV is required")
    with Path(nextest_env).open("a", encoding="utf-8", newline="\n") as env_file:
        # Desktop exports process-scoped CODEX_* state (homes, permission
        # profiles, helper paths, task IDs, and app pipes). Repository tests
        # must start clean; fixtures that exercise an override set it explicitly.
        for name in sorted(name for name in os.environ if name.startswith("CODEX_")):
            env_file.write(f"{name}=\n")
        env_file.write("RUST_MIN_STACK=8388608\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
