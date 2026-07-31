#!/usr/bin/env python3
"""Give Windows libtest worker threads the repository's linked stack size."""

from __future__ import annotations

import os
from pathlib import Path


def main() -> int:
    nextest_env = os.environ.get("NEXTEST_ENV")
    if not nextest_env:
        raise SystemExit("NEXTEST_ENV is required")
    with Path(nextest_env).open("a", encoding="utf-8", newline="\n") as env_file:
        env_file.write("RUST_MIN_STACK=8388608\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
