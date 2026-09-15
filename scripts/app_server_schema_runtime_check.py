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


GENERATED_OUTPUTS = ("codex-rs/app-server-protocol/schema",)
STABLE_SCHEMA_BUNDLE = (
    "codex-rs/app-server-protocol/schema/json/codex_app_server_protocol.schemas.json"
)
IGNORED_SCHEMA_ANNOTATIONS = frozenset(
    {"$schema", "description", "title", "default", "examples"}
)
SCHEMA_MAPS = frozenset(
    {"definitions", "$defs", "properties", "patternProperties", "dependentSchemas"}
)
SCHEMA_VALUES = frozenset(
    {
        "items",
        "additionalItems",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedItems",
        "unevaluatedProperties",
    }
)
SCHEMA_ARRAYS = frozenset({"oneOf", "anyOf", "allOf", "prefixItems"})


def _child_role(role: str, key: str, path: str) -> str:
    if role == "bundle-definitions" and key in {"v1", "v2"}:
        return "additive-map"
    if role in {"map", "additive-map", "bundle-definitions"}:
        return "schema"
    if role != "schema":
        return "data"
    if key == "definitions" and path == "$":
        return "bundle-definitions"
    if key in {"definitions", "$defs", "properties"}:
        return "additive-map"
    if key in SCHEMA_MAPS:
        return "map"
    if key in SCHEMA_VALUES:
        return "schema"
    if key in SCHEMA_ARRAYS:
        return "schema-array"
    return "data"


def _without_annotations(value: object, role: str, path: str) -> object:
    if isinstance(value, dict):
        return {
            key: _without_annotations(
                child, _child_role(role, key, path), f"{path}/{key}"
            )
            for key, child in value.items()
            if role != "schema" or key not in IGNORED_SCHEMA_ANNOTATIONS
        }
    if isinstance(value, list):
        return [
            _without_annotations(
                child, "schema" if role in {"schema", "schema-array"} else "data", path
            )
            for child in value
        ]
    return value


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def run(args: Sequence[str], *, cwd: Path) -> int:
    print("$ " + shlex.join(str(arg) for arg in args), flush=True)
    try:
        return subprocess.run(list(args), cwd=cwd).returncode
    except OSError as error:
        print(f"Could not run {args[0]}: {error}", file=sys.stderr)
        return 127 if isinstance(error, FileNotFoundError) else 1


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
    *,
    _role: str = "schema",
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
            if _role == "schema" and key in IGNORED_SCHEMA_ANNOTATIONS:
                continue
            if key not in current:
                issues.append(f"{child_path}:removed")
                continue
            current_value = current[key]
            child_role = _child_role(_role, key, path)
            if _role == "schema" and key in {"required", "enum", *SCHEMA_ARRAYS}:
                if _canonical_json(
                    _without_annotations(baseline_value, child_role, child_path)
                ) != _canonical_json(
                    _without_annotations(current_value, child_role, child_path)
                ):
                    issues.append(f"{child_path}:changed")
                continue
            issues.extend(
                stable_schema_compatibility_issues(
                    baseline_value,
                    current_value,
                    child_path,
                    _role=child_role,
                )
            )
        for key in current.keys() - baseline.keys():
            if (_role == "schema" and key in IGNORED_SCHEMA_ANNOTATIONS) or _role in {
                "additive-map",
                "bundle-definitions",
            }:
                continue
            issues.append(f"{path}/{key}:added")
        return issues
    if isinstance(baseline, list):
        assert isinstance(current, list)
        if _canonical_json(
            _without_annotations(baseline, _role, path)
        ) != _canonical_json(_without_annotations(current, _role, path)):
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
                print("Running a check-only app-server schema freshness proof.")
            protocol_code = run_protocol_check(root)
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
                print(
                    "Skipping stable compatibility comparison for experimental schemas."
                )
            consumer_code = run_python_sdk_contract_check(root)
            if consumer_code != 0:
                return consumer_code
    except GenerationLockError as error:
        print(str(error), file=sys.stderr)
        return 2
    if generated_changed:
        print("Schema regeneration changed generated outputs; review and include them.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
