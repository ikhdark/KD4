#!/usr/bin/env python3
"""Check config schema freshness, or explicitly regenerate under a shared lock."""

from __future__ import annotations

import argparse
import hashlib
import os
import shlex
import sys
from collections.abc import Sequence
from pathlib import Path

try:
    from scripts.generated_output_lock import GenerationLockError, generated_output_lock
    from scripts.process_owner import run_finite
except ModuleNotFoundError:
    from generated_output_lock import GenerationLockError, generated_output_lock
    from process_owner import run_finite


GENERATED_OUTPUTS = ("codex-rs/core/config.schema.json",)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def run(args: Sequence[str], *, cwd: Path) -> int:
    print("$ " + shlex.join(str(arg) for arg in args), flush=True)
    try:
        result = run_finite(args, cwd=cwd)
    except OSError as error:
        print(f"Could not run {args[0]}: {error}", file=sys.stderr)
        return 127 if isinstance(error, FileNotFoundError) else 1
    if result.stdout:
        print(result.stdout, end="" if result.stdout.endswith("\n") else "\n")
    if result.output_truncated:
        print("[output truncated; retaining final 65536 bytes]", file=sys.stderr)
    if result.status == "could_not_start":
        print(f"Could not run {args[0]}: {result.stdout}", file=sys.stderr)
    elif result.status not in {"passed", "failed"}:
        print(f"Command {result.status}: {args[0]}", file=sys.stderr)
    return result.returncode


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
    parser.add_argument(
        "--owner",
        help="Required identity for the serialized force-regeneration lane.",
    )
    parser.add_argument(
        "--lock-timeout",
        type=float,
        default=None,
        help="Lock wait seconds (check: 60, force: 0; 0 fails immediately).",
    )
    args = parser.parse_args(argv)
    if args.lock_timeout is not None and (not 0 <= args.lock_timeout <= 3600):
        parser.error("--lock-timeout must be between 0 and 3600")
    lock_timeout = (
        args.lock_timeout
        if args.lock_timeout is not None
        else (60 if args.mode == "check" else 0)
    )

    root = repo_root()
    if args.mode == "force" and (not args.owner or not args.owner.strip()):
        parser.error("--owner is required with --mode force")
    lock_owner = args.owner if args.mode == "force" else f"check:{os.getpid()}"
    generated_changed = False
    try:
        with generated_output_lock(root, lock_owner, timeout=lock_timeout):
            if args.mode == "force":
                print("Forcing config schema regeneration.")
                generated_changed = regenerate_schema(root, args.owner)
            else:
                print("Running a check-only config schema freshness proof.")
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
