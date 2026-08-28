#!/usr/bin/env python3
"""Check app-server schemas, or explicitly regenerate under a shared lock."""

from __future__ import annotations

import argparse
import hashlib
import json
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
    "codex-rs/app-server-protocol/Cargo.toml",
    "codex-rs/app-server-protocol/src",
    "codex-rs/protocol/Cargo.toml",
    "codex-rs/protocol/src",
)
GENERATED_OUTPUTS = ("codex-rs/app-server-protocol/schema",)
STABLE_SCHEMA_BUNDLE = (
    "codex-rs/app-server-protocol/schema/json/codex_app_server_protocol.schemas.json"
)
IGNORED_SCHEMA_ANNOTATIONS = frozenset(
    {"$schema", "description", "title", "default", "examples"}
)
ADDITIVE_SCHEMA_MAPS = frozenset({"definitions", "properties"})


def _is_additive_schema_map(path: str) -> bool:
    """Return whether new keys at ``path`` are additive schema entries.

    The stable bundle namespaces definitions by protocol version, so both
    ``$/definitions`` and ``$/definitions/v2`` are definition maps.
    """
    parts = path.split("/")
    parent_key = parts[-1]
    return parent_key in ADDITIVE_SCHEMA_MAPS or (
        len(parts) == 3 and parts[0] == "$" and parts[1] == "definitions"
    )


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


def _canonical_json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def stable_schema_compatibility_issues(
    baseline: object,
    current: object,
    path: str = "$",
) -> list[str]:
    """Return stable-schema changes that can break generated clients.

    Optional properties and new definitions are additive. Other schema changes
    require an explicit acknowledgement because the bundle describes both
    client-produced requests and server-produced responses.
    """
    if type(baseline) is not type(current):
        return [f"{path}:type"]
    if isinstance(baseline, dict):
        assert isinstance(current, dict)
        issues: list[str] = []
        for key, baseline_value in baseline.items():
            child_path = f"{path}/{key}"
            if key in IGNORED_SCHEMA_ANNOTATIONS:
                continue
            if key not in current:
                issues.append(f"{child_path}:removed")
                continue
            current_value = current[key]
            if key in {"required", "enum", "oneOf", "anyOf", "allOf"}:
                if _canonical_json(baseline_value) != _canonical_json(current_value):
                    issues.append(f"{child_path}:changed")
                continue
            issues.extend(
                stable_schema_compatibility_issues(
                    baseline_value,
                    current_value,
                    child_path,
                )
            )
        for key in current.keys() - baseline.keys():
            if key in IGNORED_SCHEMA_ANNOTATIONS or _is_additive_schema_map(path):
                continue
            issues.append(f"{path}/{key}:added")
        return issues
    if isinstance(baseline, list):
        assert isinstance(current, list)
        if _canonical_json(baseline) != _canonical_json(current):
            return [f"{path}:changed"]
        return []
    if baseline != current:
        return [f"{path}:changed"]
    return []


def load_schema_at_baseline(root: Path, baseline: str) -> object | None:
    try:
        completed = subprocess.run(
            ["git", "show", f"{baseline}:{STABLE_SCHEMA_BUNDLE}"],
            cwd=root,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except OSError as error:
        print(f"Could not read stable schema at {baseline}: {error}", file=sys.stderr)
        return None
    if completed.returncode != 0:
        print(
            f"Could not read {STABLE_SCHEMA_BUNDLE} at {baseline}.",
            file=sys.stderr,
        )
        if completed.stderr:
            print(completed.stderr, file=sys.stderr, end="")
        return None
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        print(f"Stable schema at {baseline} is invalid JSON: {error}", file=sys.stderr)
        return None


def run_stable_compatibility_check(
    root: Path,
    baseline: str,
    allowed_breaks: Sequence[str] = (),
) -> int:
    baseline_schema = load_schema_at_baseline(root, baseline)
    if baseline_schema is None:
        return 2
    try:
        current_schema = json.loads((root / STABLE_SCHEMA_BUNDLE).read_text("utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        print(f"Could not read current stable schema: {error}", file=sys.stderr)
        return 2
    issues = stable_schema_compatibility_issues(baseline_schema, current_schema)
    unapproved = [issue for issue in issues if issue not in set(allowed_breaks)]
    if unapproved:
        print(
            f"Stable app-server schema is incompatible with {baseline}:",
            file=sys.stderr,
        )
        for issue in unapproved:
            print(f"  {issue}", file=sys.stderr)
        print(
            "Use --allow-stable-break <issue> only for an intentionally reviewed "
            "stable API break.",
            file=sys.stderr,
        )
        return 1
    print(f"Stable app-server schema is compatible with {baseline}.")
    return 0


def run_python_sdk_contract_check(root: Path) -> int:
    return run(
        [
            "uv",
            "run",
            "--directory",
            str(root / "sdk" / "python"),
            "--group",
            "dev",
            "pytest",
            "tests/test_contract_generation.py",
        ],
        cwd=root,
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--mode",
        choices=("check", "force"),
        required=True,
    )
    parser.add_argument("--baseline", default="HEAD")
    parser.add_argument(
        "--compatibility-baseline",
        default="HEAD^",
        help="Committed schema revision used as the independent stable API baseline.",
    )
    parser.add_argument(
        "--allow-stable-break",
        action="append",
        default=[],
        metavar="ISSUE",
        help="Acknowledge one compatibility issue emitted by the stable-schema check.",
    )
    parser.add_argument(
        "--owner",
        help="Required identity for the serialized force-regeneration lane.",
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
    stable_lane = "--experimental" not in generator_args
    if stable_lane:
        compatibility_code = run_stable_compatibility_check(
            root,
            args.compatibility_baseline,
            args.allow_stable_break,
        )
        if compatibility_code != 0:
            return compatibility_code
    else:
        print("Skipping stable compatibility comparison for experimental schemas.")
    consumer_code = run_python_sdk_contract_check(root)
    if consumer_code != 0:
        return consumer_code
    if generated_changed:
        print("Schema regeneration changed generated outputs; review and include them.")
        if args.mode != "force":
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
