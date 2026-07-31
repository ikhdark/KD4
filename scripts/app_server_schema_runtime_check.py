#!/usr/bin/env python3
"""Check app-server schemas, or explicitly regenerate under a shared lock."""

from __future__ import annotations

import argparse
import hashlib
import os
import shlex
import subprocess
import sys
from pathlib import Path
from typing import Sequence

try:
    from scripts.generated_output_lock import GenerationLockError
    from scripts.generated_output_lock import generated_output_lock
except ModuleNotFoundError:
    from generated_output_lock import GenerationLockError
    from generated_output_lock import generated_output_lock


SCHEMA_INPUTS = (
    "codex-rs/app-server-protocol/Cargo.toml",
    "codex-rs/app-server-protocol/src",
    "codex-rs/protocol/Cargo.toml",
    "codex-rs/protocol/src",
)
GENERATED_OUTPUTS = ("codex-rs/app-server-protocol/schema",)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def run(args: Sequence[str], *, cwd: Path) -> int:
    print("$ " + shlex.join(str(arg) for arg in args), flush=True)
    try:
        return subprocess.run(list(args), cwd=cwd).returncode
    except OSError as error:
        print(f"Could not run {args[0]}: {error}", file=sys.stderr)
        return 127


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
            f"Could not compare app-server schema inputs with {baseline}: {error}",
            file=sys.stderr,
        )
        return True
    if completed.returncode != 0:
        print(
            "Could not inspect schema input status.",
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


def regenerate_schemas(
    root: Path, owner: str, generator_args: Sequence[str] = ()
) -> bool:
    del owner
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
            *generator_args,
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


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--mode",
        choices=("auto", "check", "force"),
        required=True,
        help="auto is a backwards-compatible alias for the check-only mode",
    )
    parser.add_argument("--baseline", default="HEAD")
    parser.add_argument(
        "--owner",
        help="Required identity for the serialized force-regeneration lane.",
    )
    parser.add_argument(
        "--runtime",
        action="store_true",
        help="Also run the focused app-server runtime checks.",
    )
    parser.add_argument(
        "generator_args",
        nargs=argparse.REMAINDER,
        help="Arguments forwarded to write_schema_fixtures in force mode.",
    )
    args = parser.parse_args(argv)
    generator_args = args.generator_args
    if generator_args[:1] == ["--"]:
        generator_args = generator_args[1:]

    root = repo_root()
    if args.mode != "force" and generator_args:
        parser.error("generator arguments are only valid with --mode force")
    if args.mode == "force" and (not args.owner or not args.owner.strip()):
        parser.error("--owner is required with --mode force")
    lock_owner = args.owner if args.mode == "force" else f"check:{os.getpid()}"
    generated_changed = False
    try:
        with generated_output_lock(root, lock_owner):
            if args.mode == "force":
                print("Forcing app-server schema regeneration.")
                if generator_args:
                    generated_changed = regenerate_schemas(
                        root, args.owner, generator_args
                    )
                else:
                    generated_changed = regenerate_schemas(root, args.owner)
            else:
                changed = schema_inputs_changed(root, args.baseline)
                state = "changed" if changed else "unchanged"
                print(
                    f"App-server schema inputs are {state} relative to {args.baseline}; "
                    "running a check-only freshness proof."
                )
            protocol_code = run_protocol_check(root)
            if protocol_code == 0 and args.runtime:
                runtime_code = run_runtime_check(root)
                if runtime_code != 0:
                    return runtime_code
    except GenerationLockError as error:
        print(str(error), file=sys.stderr)
        return 2
    if protocol_code != 0:
        if args.mode != "force":
            print(
                "Freshness failed without modifying generated output. "
                "Use `just app-server-schema-regenerate <owner>` in the serialized "
                "generation lane.",
                file=sys.stderr,
            )
        return protocol_code
    if generated_changed:
        print("Schema regeneration changed generated outputs; review and include them.")
        if args.mode != "force":
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
