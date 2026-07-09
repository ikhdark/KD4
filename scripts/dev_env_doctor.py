#!/usr/bin/env python3
"""Check local development prerequisites before `just` is available."""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
PACKAGE_JSON = REPO_ROOT / "package.json"


@dataclass(frozen=True)
class ToolCheck:
    name: str
    command: tuple[str, ...]
    path: str | None
    version: str | None
    ok: bool
    required: bool
    guidance: str


def run_version(command: Sequence[str]) -> str | None:
    try:
        completed = subprocess.run(
            list(command),
            cwd=REPO_ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    output = completed.stdout.strip().splitlines()
    return output[0].strip() if completed.returncode == 0 and output else None


def package_manager_pin() -> str:
    if not PACKAGE_JSON.exists():
        return "pnpm"
    data = json.loads(PACKAGE_JSON.read_text(encoding="utf-8"))
    value = str(data.get("packageManager", "pnpm"))
    return value.split("+", 1)[0]


def node_major(version: str | None) -> int | None:
    if version is None:
        return None
    match = re.search(r"v?(\d+)", version)
    return int(match.group(1)) if match else None


def check_tool(
    name: str,
    command: Sequence[str],
    *,
    required: bool,
    guidance: str,
    min_node_major: int | None = None,
) -> ToolCheck:
    executable = shutil.which(command[0])
    version = run_version(command) if executable else None
    ok = executable is not None and version is not None
    if min_node_major is not None:
        major = node_major(version)
        ok = ok and major is not None and major >= min_node_major
    return ToolCheck(
        name=name,
        command=tuple(command),
        path=executable,
        version=version,
        ok=ok,
        required=required,
        guidance=guidance,
    )


def collect_checks() -> list[ToolCheck]:
    pnpm_pin = package_manager_pin()
    return [
        check_tool(
            "python",
            [sys.executable, "--version"],
            required=True,
            guidance="Install Python 3.11+ and rerun this script.",
        ),
        check_tool(
            "git",
            ["git", "--version"],
            required=True,
            guidance="Install Git before using repo status, diffs, and validation.",
        ),
        check_tool(
            "cargo",
            ["cargo", "--version"],
            required=True,
            guidance="Install Rust with rustup, then run `rustup component add rustfmt clippy`.",
        ),
        check_tool(
            "just",
            ["just", "--version"],
            required=True,
            guidance="Install with `cargo install --locked just`.",
        ),
        check_tool(
            "cargo-nextest",
            ["cargo", "nextest", "--version"],
            required=True,
            guidance="Install with `cargo install --locked cargo-nextest`.",
        ),
        check_tool(
            "node",
            ["node", "--version"],
            required=True,
            guidance="Install Node 22+; this repo uses the root packageManager pin.",
            min_node_major=22,
        ),
        check_tool(
            "pnpm",
            ["pnpm", "--version"],
            required=True,
            guidance=f"Enable the pinned pnpm with `corepack enable` and `corepack prepare {pnpm_pin} --activate`.",
        ),
    ]


def print_text(checks: Sequence[ToolCheck]) -> None:
    print("Local development tool check")
    for check in checks:
        status = "ok" if check.ok else "missing"
        detail = check.version or check.path or check.guidance
        print(f"- {check.name}: {status} ({detail})")
        if not check.ok:
            print(f"  fix: {check.guidance}")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--json", action="store_true", help="Emit machine-readable output."
    )
    parser.add_argument(
        "--no-fail",
        action="store_true",
        help="Always exit 0 after reporting missing tools.",
    )
    args = parser.parse_args(argv)

    checks = collect_checks()
    missing = [check for check in checks if check.required and not check.ok]
    if args.json:
        print(
            json.dumps(
                {"ok": not missing, "checks": [asdict(c) for c in checks]}, indent=2
            )
        )
    else:
        print_text(checks)
    return 0 if args.no_fail or not missing else 1


if __name__ == "__main__":
    raise SystemExit(main())
