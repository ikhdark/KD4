#!/usr/bin/env python3
"""Regenerate app-server schemas only when needed, with optional runtime checks."""

from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path
from typing import Sequence


SCHEMA_INPUTS = (
    "codex-rs/app-server-protocol/Cargo.toml",
    "codex-rs/app-server-protocol/src",
    "codex-rs/protocol/Cargo.toml",
    "codex-rs/protocol/src",
)
GENERATED_OUTPUTS = ("codex-rs/app-server-protocol/schema",)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def run(args: list[str], *, cwd: Path) -> int:
    print("$ " + " ".join(args), flush=True)
    return subprocess.run(args, cwd=cwd).returncode


def schema_inputs_changed(root: Path) -> bool:
    completed = subprocess.run(
        ["git", "status", "--porcelain", "--", *SCHEMA_INPUTS],
        cwd=root,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        print(
            "Could not inspect schema input status; regenerating schemas.",
            file=sys.stderr,
        )
        if completed.stderr:
            print(completed.stderr, file=sys.stderr, end="")
        return True
    return bool(completed.stdout.strip())


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def snapshot_outputs(root: Path) -> dict[str, str]:
    snapshot: dict[str, str] = {}
    for output in GENERATED_OUTPUTS:
        path = root / output
        if path.is_file():
            snapshot[output] = hash_file(path)
        elif path.is_dir():
            for child in sorted(p for p in path.rglob("*") if p.is_file()):
                snapshot[child.relative_to(root).as_posix()] = hash_file(child)
    return snapshot


def changed_outputs(before: dict[str, str], after: dict[str, str]) -> list[str]:
    paths = sorted(set(before) | set(after))
    return [path for path in paths if before.get(path) != after.get(path)]


def regenerate_schemas(root: Path) -> bool:
    before = snapshot_outputs(root)
    code = run(
        [
            "cargo",
            "run",
            "-p",
            "codex-app-server-protocol",
            "--bin",
            "write_schema_fixtures",
            "--",
        ],
        cwd=root / "codex-rs",
    )
    if code != 0:
        raise SystemExit(code)

    changed = changed_outputs(before, snapshot_outputs(root))
    if changed:
        print("Generated app-server schema outputs changed during regeneration:")
        for path in changed:
            print(f"  {path}")
        return True

    print("Generated app-server schema outputs were already up to date.")
    return False


def run_runtime_check(root: Path) -> int:
    return run(
        ["just", "--justfile", str(root / "justfile"), "app-server-runtime-check"],
        cwd=root,
    )


def run_protocol_check(root: Path) -> int:
    return run(
        [
            "just",
            "--justfile",
            str(root / "justfile"),
            "app-server-schema-protocol-check",
        ],
        cwd=root,
    )


def run_app_server_protocol_check_with_auto_regen(root: Path) -> tuple[int, bool]:
    check_code = run_protocol_check(root)
    if check_code == 0:
        return 0, False

    print(
        "App-server schema freshness check failed after skipping regeneration; "
        "regenerating schemas and retrying."
    )
    generated_changed = regenerate_schemas(root)
    retry_code = run_protocol_check(root)
    return retry_code, generated_changed


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=("auto", "force"), required=True)
    parser.add_argument(
        "--runtime",
        action="store_true",
        help="Also run the focused app-server runtime checks.",
    )
    args = parser.parse_args(argv)

    root = repo_root()
    generated_changed = False
    skipped_regen = False
    if args.mode == "force":
        print("Forcing app-server schema regeneration.")
        generated_changed = regenerate_schemas(root)
    elif schema_inputs_changed(root):
        print("App-server schema inputs changed; regenerating schemas.")
        generated_changed = regenerate_schemas(root)
    else:
        print("App-server schema inputs unchanged; skipping schema regeneration.")
        skipped_regen = True

    if skipped_regen:
        protocol_code, retry_changed = run_app_server_protocol_check_with_auto_regen(root)
        generated_changed = generated_changed or retry_changed
    else:
        protocol_code = run_protocol_check(root)
    if protocol_code != 0:
        return protocol_code
    if args.runtime:
        runtime_code = run_runtime_check(root)
        if runtime_code != 0:
            return runtime_code
    if generated_changed:
        print("Schema regeneration changed generated outputs; review and include them.")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
