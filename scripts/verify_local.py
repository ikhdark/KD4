#!/usr/bin/env python3
"""Compatibility launcher for the Rust-owned local verifier."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
from collections.abc import Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]


def rust_command(argv: Sequence[str]) -> list[str]:
    configured = os.environ.get("CODEX_VERIFY_LOCAL_COMMAND")
    if configured:
        decoded = json.loads(configured)
        if not isinstance(decoded, list) or not all(
            isinstance(part, str) for part in decoded
        ):
            raise ValueError("CODEX_VERIFY_LOCAL_COMMAND must be a JSON string array")
        return [*decoded, *argv]
    binary = os.environ.get("CODEX_VERIFY_LOCAL_BIN")
    if binary:
        return [binary, *argv]
    cargo = os.environ.get("CARGO", "cargo")
    return [
        cargo,
        "run",
        "--quiet",
        "--manifest-path",
        str(REPO_ROOT / "codex-rs" / "Cargo.toml"),
        "-p",
        "codex-verify-local",
        "--",
        *argv,
    ]


def main(argv: Sequence[str] | None = None) -> int:
    forwarded = list(sys.argv[1:] if argv is None else argv)
    env = os.environ.copy()
    env.setdefault("CODEX_VERIFY_LOCAL_PYTHON", sys.executable)
    process = subprocess.Popen(rust_command(forwarded), env=env)
    return process.wait()


if __name__ == "__main__":
    raise SystemExit(main())
