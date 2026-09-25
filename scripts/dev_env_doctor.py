#!/usr/bin/env python3
"""Check local development prerequisites before `just` is available."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from functools import partial
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Sequence

try:
    from scripts import tool_versions
except ModuleNotFoundError:
    import tool_versions


REPO_ROOT = Path(__file__).resolve().parents[1]
PACKAGE_JSON = REPO_ROOT / "package.json"
# Recipes run Rust tools here, where codex-rs/rust-toolchain.toml selects the toolchain.
RUST_WORKSPACE = REPO_ROOT / "codex-rs"


class PackageJsonError(RuntimeError):
    pass


@dataclass(frozen=True)
class ToolCheck:
    name: str
    command: tuple[str, ...]
    path: str | None
    version: str | None
    ok: bool
    required: bool
    guidance: str


def run_version(command: Sequence[str], cwd: Path = REPO_ROOT) -> str | None:
    try:
        completed = subprocess.run(
            list(command),
            cwd=cwd,
            # A probe must not start installing a missing pinned toolchain.
            env={**os.environ, "RUSTUP_AUTO_INSTALL": "0"},
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            errors="replace",
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if completed.returncode != 0:
        return None
    for stream in (completed.stdout, completed.stderr):
        for line in (stream or "").splitlines():
            line = line.strip()
            # Accept bare versions or the tool's name and optional "version";
            # banners and warnings can precede either output stream's version.
            if re.match(
                r"^(?:[\w.-]+\s+(?:version\s+)?)?v?\d+\.\d+(?:[.\s+-]|$)", line
            ):
                return line
    return None


def package_json() -> dict[str, object]:
    if not PACKAGE_JSON.exists():
        return {}
    try:
        data = json.loads(PACKAGE_JSON.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise PackageJsonError(f"could not read {PACKAGE_JSON}: {error}") from None
    if not isinstance(data, dict):
        raise PackageJsonError(f"{PACKAGE_JSON} must contain a JSON object")
    return data


def package_manager_pin() -> str:
    value = str(package_json().get("packageManager", "pnpm"))
    return value.split("+", 1)[0]


def node_engine_floor() -> tuple[int, ...] | None:
    """Return the `engines.node` floor that pnpm enforces for this workspace."""
    engines = package_json().get("engines")
    value = engines.get("node") if isinstance(engines, dict) else None
    if value is None:
        return None
    match = re.fullmatch(r"\s*>=\s*v?(\d+(?:\.\d+)*)\s*", str(value))
    if match is None:
        raise PackageJsonError(
            f"{PACKAGE_JSON} engines.node must be a '>=X.Y.Z' floor, got {value!r}"
        )
    return tuple(int(part) for part in match.group(1).split("."))


def numeric_version(version: str | None) -> tuple[int, ...] | None:
    if version is None:
        return None
    match = re.search(r"(?<!\d)(\d+(?:\.\d+)+)", version)
    if match is None:
        return None
    return tuple(int(part) for part in match.group(1).split("."))


def package_manager_version(pin: str) -> str | None:
    _name, separator, version = pin.rpartition("@")
    return version if separator and version else None


def check_tool(
    name: str,
    command: Sequence[str],
    *,
    required: bool,
    guidance: str,
    cwd: Path = REPO_ROOT,
    min_version: tuple[int, ...] | None = None,
    pinned_version: tuple[int, ...] | None = None,
    required_version: str | None = None,
) -> ToolCheck:
    executable = shutil.which(command[0])
    version = run_version(command, cwd) if executable else None
    ok = executable is not None and version is not None
    if min_version is not None:
        actual = numeric_version(version)
        ok = ok and actual is not None and actual >= min_version
    if pinned_version is not None:
        actual = numeric_version(version)
        ok = (
            ok
            and actual is not None
            and actual[: len(pinned_version)] == pinned_version
        )
    if required_version is not None:
        ok = ok and version == required_version
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
    node_floor = node_engine_floor()
    node_text = f" {'.'.join(map(str, node_floor))}+" if node_floor else ""
    rust_channel = tool_versions.rust_toolchain_channel()
    rustfmt_toolchain = tool_versions.RUSTFMT_TOOLCHAIN
    checks = [
        partial(
            check_tool,
            "python",
            [sys.executable, "--version"],
            required=True,
            guidance="Install Python 3.11+ and rerun this script.",
            min_version=(3, 11),
        ),
        partial(
            check_tool,
            "git",
            ["git", "--version"],
            required=True,
            guidance="Install Git before using repo status, diffs, and validation.",
        ),
        partial(
            check_tool,
            "cargo",
            ["cargo", "--version"],
            required=True,
            guidance=(
                "Install Rust with rustup; `cargo` on PATH must be the rustup proxy "
                f"so codex-rs/rust-toolchain.toml ({rust_channel}) applies."
            ),
            cwd=RUST_WORKSPACE,
            pinned_version=numeric_version(rust_channel),
        ),
        partial(
            check_tool,
            "rustfmt",
            # The exact toolchain invocation scripts/format.py uses.
            ["rustup", "run", rustfmt_toolchain, "cargo", "fmt", "--version"],
            required=True,
            guidance=(
                f"Install with `rustup toolchain install {rustfmt_toolchain} "
                "--profile minimal --component rustfmt`."
            ),
            cwd=RUST_WORKSPACE,
        ),
        partial(
            check_tool,
            "clippy",
            ["cargo", "clippy", "--version"],
            required=True,
            guidance="Install with `rustup component add clippy`.",
            cwd=RUST_WORKSPACE,
        ),
        partial(
            check_tool,
            "just",
            ["just", "--version"],
            required=True,
            guidance="Install with `cargo install just`.",
        ),
        partial(
            check_tool,
            "cargo-nextest",
            ["cargo", "nextest", "--version"],
            required=True,
            guidance="Install with `cargo install cargo-nextest`.",
            cwd=RUST_WORKSPACE,
        ),
        partial(
            check_tool,
            "uv",
            ["uv", "--version"],
            required=True,
            guidance="Install uv before running maintained Python workflows.",
        ),
        partial(
            check_tool,
            "node",
            ["node", "--version"],
            required=True,
            guidance=f"Install Node{node_text} (package.json engines.node).",
            min_version=node_floor,
        ),
        partial(
            check_tool,
            "pnpm",
            ["pnpm", "--version"],
            required=True,
            guidance=f"Enable the pinned pnpm with `corepack enable` and `corepack prepare {pnpm_pin} --activate`.",
            required_version=package_manager_version(pnpm_pin),
        ),
    ]
    checks.append(
        partial(
            check_tool,
            "rg",
            ["rg", "--version"],
            required=True,
            guidance="Install ripgrep and make rg available on PATH for repository search.",
        )
    )
    checks.append(
        partial(
            check_tool,
            "pwsh",
            [
                "pwsh",
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ],
            required=True,
            guidance="Install PowerShell 7.5 or newer for maintained Windows recipes.",
            min_version=(7, 5),
        )
    )
    with ThreadPoolExecutor(max_workers=4) as executor:
        return list(executor.map(lambda check: check(), checks))


def tool_status(check: ToolCheck) -> str:
    if check.ok:
        return "ok"
    if check.path is None:
        return "missing"
    if check.version is None:
        return "unavailable"
    return "mismatch"


def print_text(checks: Sequence[ToolCheck]) -> None:
    print("Local development tool check")
    for check in checks:
        status = tool_status(check)
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
        help="Always exit 0 after reporting tool-check failures.",
    )
    args = parser.parse_args(argv)

    try:
        checks = collect_checks()
    except RuntimeError as error:  # PackageJsonError or an unreadable toolchain pin.
        print(f"Development environment check failed: {error}", file=sys.stderr)
        return 1
    failed = [check for check in checks if check.required and not check.ok]
    if args.json:
        print(
            json.dumps(
                {"ok": not failed, "checks": [asdict(c) for c in checks]}, indent=2
            )
        )
    else:
        print_text(checks)
    return 0 if args.no_fail or not failed else 1


if __name__ == "__main__":
    raise SystemExit(main())
