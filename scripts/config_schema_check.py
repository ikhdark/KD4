#!/usr/bin/env python3
"""Check config schema freshness, or explicitly regenerate under a shared lock."""

from __future__ import annotations

import argparse
import hashlib
import os
import shlex
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

try:
    from scripts.generated_output_lock import GenerationLockError, generated_output_lock
except ModuleNotFoundError:
    from generated_output_lock import GenerationLockError, generated_output_lock


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
    print("$ " + shlex.join(str(arg) for arg in args), flush=True)
    try:
        return subprocess.run(list(args), cwd=cwd).returncode
    except OSError as error:
        print(f"Could not run {args[0]}: {error}", file=sys.stderr)
        return 127 if isinstance(error, FileNotFoundError) else 1


def schema_inputs_changed(root: Path, baseline: str = "HEAD") -> bool:
    try:
        completed = subprocess.run(
            ["git", "diff", "--name-only", baseline, "--", *SCHEMA_INPUTS],
            cwd=root,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except OSError as error:
        print(
            f"Could not compare config schema inputs with {baseline}: {error}",
            file=sys.stderr,
        )
        return True
    if completed.returncode != 0:
        print(
            "Could not inspect config schema input status.",
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


def regenerate_schema(root: Path, owner: str) -> bool:
    del owner
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


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mode",
        choices=("check", "force"),
        required=True,
    )
    parser.add_argument("--baseline", default="HEAD")
    parser.add_argument(
        "--owner",
        help="Required identity for the serialized force-regeneration lane.",
    )
    args = parser.parse_args(argv)

    root = repo_root()
    if args.mode == "force" and (not args.owner or not args.owner.strip()):
        parser.error("--owner is required with --mode force")
    lock_owner = args.owner if args.mode == "force" else f"check:{os.getpid()}"
    generated_changed = False
    try:
        with generated_output_lock(root, lock_owner):
            if args.mode == "force":
                print("Forcing config schema regeneration.")
                generated_changed = regenerate_schema(root, args.owner)
            else:
                changed = schema_inputs_changed(root, args.baseline)
                state = "changed" if changed else "unchanged"
                print(
                    f"Config schema inputs are {state} relative to {args.baseline}; "
                    "running a check-only freshness proof."
                )
            check_code = run_protocol_check(root)
    except GenerationLockError as error:
        print(str(error), file=sys.stderr)
        return 2
    if check_code != 0:
        if args.mode != "force":
            print(
                "Freshness failed without modifying generated output. "
                "Use `just config-schema-regenerate <owner>` in the serialized "
                "generation lane.",
                file=sys.stderr,
            )
        return check_code
    if generated_changed:
        print(
            "Config schema regeneration changed generated output; review and include it."
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
