#!/usr/bin/env python3
"""Regenerate config schema only when needed, then run schema freshness tests."""

from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path
from typing import Sequence


SCHEMA_INPUTS = (
    "codex-rs/config/Cargo.toml",
    "codex-rs/config/src",
    "codex-rs/core/Cargo.toml",
    "codex-rs/core/src/config/schema.rs",
    "codex-rs/core/src/config/schema_tests.rs",
    "codex-rs/core/src/bin/config_schema.rs",
    "codex-rs/features/Cargo.toml",
    "codex-rs/features/src",
    "codex-rs/protocol/Cargo.toml",
    "codex-rs/protocol/src",
)
GENERATED_OUTPUTS = ("codex-rs/core/config.schema.json",)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def run(args: Sequence[str], *, cwd: Path) -> int:
    print("$ " + " ".join(str(arg) for arg in args), flush=True)
    return subprocess.run(list(args), cwd=cwd).returncode


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
            "Could not inspect config schema input status; regenerating.",
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
    return snapshot


def changed_outputs(before: dict[str, str], after: dict[str, str]) -> list[str]:
    paths = sorted(set(before) | set(after))
    return [path for path in paths if before.get(path) != after.get(path)]


def regenerate_schema(root: Path) -> bool:
    before = snapshot_outputs(root)
    code = run(
        ["cargo", "run", "-p", "codex-core", "--bin", "codex-write-config-schema"],
        cwd=root / "codex-rs",
    )
    if code != 0:
        raise SystemExit(code)
    changed = changed_outputs(before, snapshot_outputs(root))
    if changed:
        print("Generated config schema outputs changed during regeneration:")
        for path in changed:
            print(f"  {path}")
        return True
    print("Generated config schema output was already up to date.")
    return False


def run_protocol_check(root: Path) -> int:
    return run(
        ["just", "--justfile", str(root / "justfile"), "config-schema-protocol-check"],
        cwd=root,
    )


def run_config_protocol_check_with_auto_regen(root: Path) -> tuple[int, bool]:
    check_code = run_protocol_check(root)
    if check_code == 0:
        return 0, False

    print(
        "Config schema freshness check failed after skipping regeneration; "
        "regenerating schema and retrying."
    )
    generated_changed = regenerate_schema(root)
    retry_code = run_protocol_check(root)
    return retry_code, generated_changed


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("auto", "force"), required=True)
    args = parser.parse_args(argv)

    root = repo_root()
    generated_changed = False
    skipped_regen = False
    if args.mode == "force":
        print("Forcing config schema regeneration.")
        generated_changed = regenerate_schema(root)
    elif schema_inputs_changed(root):
        print("Config schema inputs changed; regenerating schema.")
        generated_changed = regenerate_schema(root)
    else:
        print("Config schema inputs unchanged; skipping schema regeneration.")
        skipped_regen = True

    if skipped_regen:
        check_code, retry_changed = run_config_protocol_check_with_auto_regen(root)
        generated_changed = generated_changed or retry_changed
    else:
        check_code = run_protocol_check(root)
    if check_code != 0:
        return check_code
    if generated_changed:
        print(
            "Config schema regeneration changed generated output; review and include it."
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
