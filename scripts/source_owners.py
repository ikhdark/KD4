#!/usr/bin/env python3
"""Validate source_owners.toml and generate its marked SOURCEMAP.md block."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import re
import sys
import tempfile
import tomllib


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = REPO_ROOT / "source_owners.toml"
DEFAULT_SOURCEMAP = REPO_ROOT / "SOURCEMAP.md"
SCHEMA_VERSION = 1
BEGIN_PREFIX = "<!-- BEGIN KD4 SOURCE OWNERS"
END_MARKER = "<!-- END KD4 SOURCE OWNERS -->"


def normalize(value: str) -> str:
    return " ".join(re.findall(r"[^\W_]+|_+", value.casefold()))


def confined_path(root: Path, raw_path: str) -> Path:
    candidate = Path(raw_path)
    if candidate.is_absolute() or ".." in candidate.parts:
        raise ValueError(f"path is not repository-relative: {raw_path}")
    resolved = (root / candidate).resolve(strict=False)
    if not resolved.is_relative_to(root.resolve()):
        raise ValueError(f"path escapes repository: {raw_path}")
    return resolved


def _schema_errors(manifest: object) -> list[str]:
    if not isinstance(manifest, dict):
        return ["manifest root must be a table"]
    errors: list[str] = []
    owners = manifest.get("owners")
    if not isinstance(owners, list):
        return ["owners must be an array of tables"]
    string_lists = (
        "concern_ids",
        "aliases",
        "phrases",
        "ambiguous_with",
        "roots",
        "instructions",
        "consumers",
        "contracts",
        "generated_mirrors",
        "tests",
    )
    for index, owner in enumerate(owners):
        label = f"owners[{index}]"
        if not isinstance(owner, dict):
            errors.append(f"{label} must be a table")
            continue
        if not isinstance(owner.get("id"), str):
            errors.append(f"{label}.id must be a string")
        for field in string_lists:
            value = owner.get(field, [])
            if not isinstance(value, list) or any(
                not isinstance(item, str) for item in value
            ):
                errors.append(f"{label}.{field} must be an array of strings")
        primary_entries = owner.get("primary_entries", [])
        if not isinstance(primary_entries, list):
            errors.append(f"{label}.primary_entries must be an array of tables")
        else:
            for entry_index, entry in enumerate(primary_entries):
                entry_label = f"{label}.primary_entries[{entry_index}]"
                if not isinstance(entry, dict):
                    errors.append(f"{entry_label} must be a table")
                    continue
                for field in ("path", "symbol"):
                    if not isinstance(entry.get(field), str):
                        errors.append(f"{entry_label}.{field} must be a string")
                if "ambiguous" in entry and not isinstance(entry["ambiguous"], bool):
                    errors.append(f"{entry_label}.ambiguous must be a boolean")
        validations = owner.get("validation", [])
        if not isinstance(validations, list):
            errors.append(f"{label}.validation must be an array of tables")
        else:
            for validation_index, validation in enumerate(validations):
                validation_label = f"{label}.validation[{validation_index}]"
                if not isinstance(validation, dict):
                    errors.append(f"{validation_label} must be a table")
                    continue
                for field in ("id", "cwd"):
                    if not isinstance(validation.get(field), str):
                        errors.append(f"{validation_label}.{field} must be a string")
                argv = validation.get("argv")
                if not isinstance(argv, list) or any(
                    not isinstance(item, str) for item in argv
                ):
                    errors.append(f"{validation_label}.argv must be an array of strings")
                if "role" in validation and not isinstance(validation["role"], str):
                    errors.append(f"{validation_label}.role must be a string")
    return errors


def load_and_validate(
    manifest_path: Path, root: Path | None = None
) -> tuple[dict, str]:
    raw = manifest_path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    manifest = tomllib.loads(raw.decode("utf-8"))
    errors = _schema_errors(manifest)
    if errors:
        raise ValueError(
            "routing_manifest_invalid:\n"
            + "\n".join(f"- {error}" for error in sorted(set(errors)))
        )
    if root is None:
        root = manifest_path.resolve().parent
    if manifest.get("schema_version") != SCHEMA_VERSION:
        errors.append(f"unsupported schema_version: {manifest.get('schema_version')!r}")
    owners = manifest.get("owners", [])
    seen_ids: set[str] = set()
    phrases: dict[str, list[dict]] = {}
    symbols: dict[str, list[tuple[dict, dict]]] = {}
    for owner in owners:
        owner_id = owner.get("id", "")
        if not owner_id or owner_id in seen_ids:
            errors.append(f"duplicate or empty owner id: {owner_id!r}")
        seen_ids.add(owner_id)
        for phrase in [*owner.get("aliases", []), *owner.get("phrases", [])]:
            phrases.setdefault(normalize(phrase), []).append(owner)
        for entry in owner.get("primary_entries", []):
            symbols.setdefault(entry.get("symbol", ""), []).append((owner, entry))
        declared = [
            *owner.get("roots", []),
            *owner.get("instructions", []),
            *owner.get("consumers", []),
            *owner.get("contracts", []),
            *owner.get("generated_mirrors", []),
            *owner.get("tests", []),
            *(entry.get("path", "") for entry in owner.get("primary_entries", [])),
        ]
        for raw_path in declared:
            try:
                candidate = confined_path(root, raw_path)
                if not candidate.exists():
                    errors.append(f"{owner_id}: missing declared path: {raw_path}")
            except ValueError as error:
                errors.append(f"{owner_id}: {error}")
        validation_ids: set[str] = set()
        for validation in owner.get("validation", []):
            validation_id = validation.get("id", "")
            if not validation_id or validation_id in validation_ids:
                errors.append(
                    f"{owner_id}: duplicate or empty validation id: {validation_id!r}"
                )
            validation_ids.add(validation_id)
            argv = validation.get("argv", [])
            if not argv or not isinstance(argv[0], str) or not argv[0].strip():
                errors.append(f"{owner_id}: {validation_id}: argv must be nonempty")
            try:
                cwd = confined_path(root, validation.get("cwd", ""))
                if not cwd.is_dir():
                    errors.append(f"{owner_id}: {validation_id}: invalid cwd")
            except ValueError as error:
                errors.append(f"{owner_id}: {validation_id}: {error}")
    for phrase, candidates in phrases.items():
        if phrase and len(candidates) > 1:
            for owner in candidates:
                peers = {
                    candidate["id"]
                    for candidate in candidates
                    if candidate["id"] != owner["id"]
                }
                if not peers.issubset(set(owner.get("ambiguous_with", []))):
                    errors.append(
                        f"phrase collision without explicit ambiguity: {phrase!r}"
                    )
                    break
    for symbol, candidates in symbols.items():
        if (
            symbol
            and len(candidates) > 1
            and any(not entry.get("ambiguous", False) for _, entry in candidates)
        ):
            errors.append(f"entry symbol is not explicitly ambiguous: {symbol!r}")
    if errors:
        raise ValueError(
            "routing_manifest_invalid:\n"
            + "\n".join(f"- {error}" for error in sorted(set(errors)))
        )
    return manifest, digest


def render_block(manifest: dict, digest: str) -> str:
    lines = [
        f"{BEGIN_PREFIX} schema={SCHEMA_VERSION} manifest_sha256={digest} -->",
        "### Managed KD4 source-owner index",
        "",
        "This table is generated by `scripts/source_owners.py`; edit `source_owners.toml`, not this block.",
        "",
        "| Owner ID | Owning roots | Primary entries | Focused validation |",
        "| --- | --- | --- | --- |",
    ]
    for owner in sorted(manifest["owners"], key=lambda owner: owner["id"]):
        roots = "<br>".join(f"`{path}`" for path in owner.get("roots", [])) or "—"
        entries = (
            "<br>".join(
                f"`{entry['path']}::{entry['symbol']}`"
                for entry in owner.get("primary_entries", [])
            )
            or "—"
        )
        validations = (
            "<br>".join(f"`{entry['id']}`" for entry in owner.get("validation", []))
            or "—"
        )
        lines.append(f"| `{owner['id']}` | {roots} | {entries} | {validations} |")
    lines.extend([END_MARKER, ""])
    return "\n".join(lines)


def replace_managed_block(source_map: str, block: str) -> str:
    begin = source_map.find(BEGIN_PREFIX)
    end = source_map.find(END_MARKER)
    if begin == -1 and end == -1:
        separator = "" if source_map.endswith("\n\n") else "\n"
        return f"{source_map.rstrip()}\n\n{block}" if source_map else block
    if begin == -1 or end == -1 or end < begin:
        raise ValueError("SOURCEMAP.md has incomplete KD4 source-owner markers")
    end += len(END_MARKER)
    return source_map[:begin] + block.rstrip() + source_map[end:]


def expected_source_map(
    manifest_path: Path, source_map_path: Path, root: Path | None = None
) -> str:
    manifest, digest = load_and_validate(manifest_path, root)
    return replace_managed_block(
        source_map_path.read_text(encoding="utf-8"), render_block(manifest, digest)
    )


def write_text_atomic(path: Path, content: str) -> None:
    path = path.resolve()
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            newline="\n",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(content)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_path, path)
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("generate", "check"))
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--source-map", type=Path, default=DEFAULT_SOURCEMAP)
    parser.add_argument(
        "--repo-root",
        type=Path,
        help="Repository root for declared paths (defaults to the manifest directory).",
    )
    args = parser.parse_args()
    try:
        expected = expected_source_map(args.manifest, args.source_map, args.repo_root)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
        print(error, file=sys.stderr)
        return 1
    actual = args.source_map.read_text(encoding="utf-8")
    if args.command == "check":
        if actual != expected:
            print(
                "SOURCEMAP.md managed source-owner block is stale; run source_owners.py generate",
                file=sys.stderr,
            )
            return 1
        return 0
    write_text_atomic(args.source_map, expected)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
