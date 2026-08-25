#!/usr/bin/env python3
"""Validate source_owners.toml and generate its marked SOURCEMAP.md block."""

from __future__ import annotations

import argparse
from collections import OrderedDict
import hashlib
import heapq
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
import threading
import tomllib
import unicodedata

try:
    from scripts.generated_output_lock import GenerationLockError, source_map_lock
except ModuleNotFoundError:
    from generated_output_lock import GenerationLockError, source_map_lock


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = REPO_ROOT / "source_owners.toml"
DEFAULT_SOURCEMAP = REPO_ROOT / "SOURCEMAP.md"
DEFAULT_ARCHITECTURE_INDEX = REPO_ROOT / "architecture_index.json"
SCHEMA_VERSION = 2
ARCHITECTURE_INDEX_VERSION = 1
BEGIN_PREFIX = "<!-- BEGIN KD4 SOURCE OWNERS"
END_MARKER = "<!-- END KD4 SOURCE OWNERS -->"
RELATIONSHIP_CATEGORIES = frozenset(
    {
        "control_flow",
        "callers_consumers",
        "configuration",
        "runtime_registration",
        "tests_contracts",
        "generated_artifacts",
    }
)
RELATIONSHIP_KINDS = frozenset(
    {
        "calls",
        "constructs",
        "consumed_by",
        "emits",
        "gated_by",
        "generates",
        "persists",
        "reads_config",
        "registers",
        "validated_by",
    }
)
CONFIDENCE_LEVELS = frozenset(
    {"declared", "compiler_resolved", "manifest_derived", "generated"}
)
INVARIANT_KINDS = frozenset({"semantic", "compatibility"})
TARGET_PREFIXES = ("owner:", "path:", "config:", "generated:", "contract:")
MAX_QUERY_RELATIONSHIPS = 64
MAX_SLICE_RELATIONSHIPS = 32
MAX_SNAPSHOT_CACHE_ENTRIES = 256
MAX_SNAPSHOT_CACHE_BYTES = 8 * 1024 * 1024
ARCHITECTURE_FACETS = (
    "control_and_data_flow",
    "callers_and_consumers",
    "configuration_and_gates",
    "registration_and_entrypoints",
    "tests_and_contracts",
    "generated_artifacts",
    "invariants",
)
CATEGORY_FACETS = {
    "control_flow": "control_and_data_flow",
    "callers_consumers": "callers_and_consumers",
    "configuration": "configuration_and_gates",
    "runtime_registration": "registration_and_entrypoints",
    "tests_contracts": "tests_and_contracts",
    "generated_artifacts": "generated_artifacts",
}
RELATIONSHIP_KINDS_FOR_CLOSURE = {
    "calls": "control_flow",
    "constructs": "control_flow",
    "consumed_by": "consumer",
    "emits": "data_flow",
    "gated_by": "feature_gate",
    "generates": "generated_by",
    "persists": "data_flow",
    "reads_config": "config_gate",
    "registers": "registration",
    "validated_by": "test",
}
FACET_KIND_PRIORITY = {
    "control_and_data_flow": {
        "control_flow": 4,
        "data_flow": 3,
    },
    "callers_and_consumers": {
        "caller": 4,
        "consumer": 3,
        "direct_builder": 2,
        "control_flow": 1,
    },
    "configuration_and_gates": {
        "config_gate": 4,
        "feature_gate": 3,
        "configuration": 2,
    },
    "registration_and_entrypoints": {
        "control_flow": 5,
        "registration": 4,
        "entrypoint": 3,
    },
    "tests_and_contracts": {
        "test": 4,
        "contract": 3,
    },
    "generated_artifacts": {
        "generated_by": 4,
        "generated_consumer": 3,
    },
    "invariants": {"invariant": 4},
}
RANKING_STOP_WORDS = frozenset(
    {
        "a",
        "an",
        "and",
        "be",
        "by",
        "for",
        "from",
        "in",
        "instead",
        "its",
        "must",
        "of",
        "or",
        "own",
        "the",
        "to",
        "with",
    }
)


def _evidence_errors(value: object, label: str) -> list[str]:
    if not isinstance(value, list) or not value:
        return [f"{label} must be a nonempty array of tables"]
    errors: list[str] = []
    for index, evidence in enumerate(value):
        item_label = f"{label}[{index}]"
        if not isinstance(evidence, dict):
            errors.append(f"{item_label} must be a table")
            continue
        if not isinstance(evidence.get("path"), str):
            errors.append(f"{item_label}.path must be a string")
        if "symbol" in evidence and not isinstance(evidence["symbol"], str):
            errors.append(f"{item_label}.symbol must be a string")
    return errors


def normalize(value: str) -> str:
    return " ".join(re.findall(r"[^\W_]+|_+", value.casefold()))


def confined_path(resolved_root: Path, raw_path: str) -> Path:
    candidate = Path(raw_path)
    if candidate.is_absolute() or ".." in candidate.parts:
        raise ValueError(f"path is not repository-relative: {raw_path}")
    resolved = (resolved_root / candidate).resolve(strict=False)
    if not resolved.is_relative_to(resolved_root):
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
        "feature_ids",
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
                    errors.append(
                        f"{validation_label}.argv must be an array of strings"
                    )
                if "role" in validation and not isinstance(validation["role"], str):
                    errors.append(f"{validation_label}.role must be a string")
        relationships = owner.get("relationships", [])
        if not isinstance(relationships, list):
            errors.append(f"{label}.relationships must be an array of tables")
        else:
            for relationship_index, relationship in enumerate(relationships):
                relationship_label = f"{label}.relationships[{relationship_index}]"
                if not isinstance(relationship, dict):
                    errors.append(f"{relationship_label} must be a table")
                    continue
                for field in ("category", "kind", "target", "confidence"):
                    if not isinstance(relationship.get(field), str):
                        errors.append(f"{relationship_label}.{field} must be a string")
                errors.extend(
                    _evidence_errors(
                        relationship.get("evidence"), f"{relationship_label}.evidence"
                    )
                )
        invariants = owner.get("invariants", [])
        if not isinstance(invariants, list):
            errors.append(f"{label}.invariants must be an array of tables")
        else:
            for invariant_index, invariant in enumerate(invariants):
                invariant_label = f"{label}.invariants[{invariant_index}]"
                if not isinstance(invariant, dict):
                    errors.append(f"{invariant_label} must be a table")
                    continue
                for field in ("id", "kind", "statement"):
                    if not isinstance(invariant.get(field), str):
                        errors.append(f"{invariant_label}.{field} must be a string")
                errors.extend(
                    _evidence_errors(
                        invariant.get("evidence"), f"{invariant_label}.evidence"
                    )
                )
                tests = invariant.get("tests", [])
                if not isinstance(tests, list) or any(
                    not isinstance(item, str) for item in tests
                ):
                    errors.append(
                        f"{invariant_label}.tests must be an array of strings"
                    )
        exclusions = owner.get("facet_exclusions", {})
        if not isinstance(exclusions, dict):
            errors.append(f"{label}.facet_exclusions must be a table")
        else:
            for facet, reason in exclusions.items():
                if facet not in ARCHITECTURE_FACETS:
                    errors.append(
                        f"{label}.facet_exclusions has unknown facet: {facet!r}"
                    )
                if not isinstance(reason, str) or not reason.strip():
                    errors.append(
                        f"{label}.facet_exclusions.{facet} must be a nonempty string"
                    )
    return errors


def load_and_validate(
    manifest_path: Path,
    root: Path | None = None,
    owner_ids: list[str] | None = None,
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
    root = root.resolve()
    confined_paths: dict[str, tuple[Path | None, str | None]] = {}
    path_observations: dict[Path, tuple[bool, bool]] = {}
    source_text: dict[Path, str] = {}

    def resolve_confined(raw_path: str) -> tuple[Path | None, str | None]:
        cached = confined_paths.get(raw_path)
        if cached is not None:
            return cached
        try:
            result = (confined_path(root, raw_path), None)
        except ValueError as error:
            result = (None, str(error))
        confined_paths[raw_path] = result
        return result

    def observe_path(candidate: Path) -> tuple[bool, bool]:
        observation = path_observations.get(candidate)
        if observation is None:
            try:
                mode = candidate.stat().st_mode
                observation = (True, stat.S_ISDIR(mode))
            except OSError:
                observation = (False, False)
            path_observations[candidate] = observation
        return observation

    def path_exists(candidate: Path) -> bool:
        return observe_path(candidate)[0]

    def path_is_dir(candidate: Path) -> bool:
        return observe_path(candidate)[1]

    def validate_symbol(owner_id: str, label: str, evidence: dict) -> None:
        symbol = evidence.get("symbol")
        if not isinstance(symbol, str) or not symbol:
            return
        raw_path = evidence.get("path", "")
        candidate, path_error = resolve_confined(raw_path)
        if path_error is not None or candidate is None or not path_exists(candidate):
            return
        if path_is_dir(candidate):
            errors.append(
                f"{owner_id}: {label} symbol evidence must name a file: {raw_path}"
            )
            return
        try:
            if candidate not in source_text:
                source_text[candidate] = candidate.read_text(encoding="utf-8")
        except (OSError, UnicodeError) as error:
            errors.append(f"{owner_id}: unreadable symbol evidence {raw_path}: {error}")
            return
        if symbol not in source_text[candidate]:
            errors.append(
                f"{owner_id}: stale symbol evidence {raw_path}::{symbol}"
            )

    if manifest.get("schema_version") != SCHEMA_VERSION:
        errors.append(f"unsupported schema_version: {manifest.get('schema_version')!r}")
    owners = manifest.get("owners", [])
    declared_owner_ids = {
        owner.get("id") for owner in owners if isinstance(owner.get("id"), str)
    }
    selected_owner_ids = declared_owner_ids if not owner_ids else set(owner_ids)
    unknown_owner_ids = sorted(selected_owner_ids - declared_owner_ids)
    if unknown_owner_ids:
        errors.append(f"unknown owner ids: {', '.join(unknown_owner_ids)}")
    seen_ids: set[str] = set()
    phrases: dict[str, list[dict]] = {}
    symbols: dict[str, list[tuple[dict, dict]]] = {}
    for owner in owners:
        owner_id = owner.get("id", "")
        validate_owner_paths = owner_id in selected_owner_ids
        if not owner_id or owner_id in seen_ids:
            errors.append(f"duplicate or empty owner id: {owner_id!r}")
        seen_ids.add(owner_id)
        for phrase in [*owner.get("aliases", []), *owner.get("phrases", [])]:
            phrases.setdefault(normalize(phrase), []).append(owner)
        for entry in owner.get("primary_entries", []):
            symbols.setdefault(entry.get("symbol", ""), []).append((owner, entry))
        if validate_owner_paths:
            for index, entry in enumerate(owner.get("primary_entries", [])):
                validate_symbol(owner_id, f"primary_entries[{index}]", entry)
        for relationship_index, relationship in enumerate(
            owner.get("relationships", [])
        ):
            target_owner = (
                relationship.get("target", "")[6:]
                if relationship.get("target", "").startswith("owner:")
                else None
            )
            if validate_owner_paths or target_owner in selected_owner_ids:
                for evidence_index, evidence in enumerate(
                    relationship.get("evidence", [])
                ):
                    validate_symbol(
                        owner_id,
                        f"relationships[{relationship_index}].evidence[{evidence_index}]",
                        evidence,
                    )
        if validate_owner_paths:
            for invariant_index, invariant in enumerate(owner.get("invariants", [])):
                for evidence_index, evidence in enumerate(
                    invariant.get("evidence", [])
                ):
                    validate_symbol(
                        owner_id,
                        f"invariants[{invariant_index}].evidence[{evidence_index}]",
                        evidence,
                    )
        generated_mirrors = set(owner.get("generated_mirrors", []))
        declared = []
        if validate_owner_paths:
            declared.extend(
                [
                    *owner.get("roots", []),
                    *owner.get("instructions", []),
                    *owner.get("consumers", []),
                    *owner.get("contracts", []),
                    *owner.get("generated_mirrors", []),
                    *owner.get("tests", []),
                    *(
                        entry.get("path", "")
                        for entry in owner.get("primary_entries", [])
                    ),
                    *(
                        evidence.get("path", "")
                        for relationship in owner.get("relationships", [])
                        for evidence in relationship.get("evidence", [])
                    ),
                    *(
                        evidence.get("path", "")
                        for invariant in owner.get("invariants", [])
                        for evidence in invariant.get("evidence", [])
                    ),
                    *(
                        test
                        for invariant in owner.get("invariants", [])
                        for test in invariant.get("tests", [])
                    ),
                ]
            )
        else:
            declared.extend(
                evidence.get("path", "")
                for relationship in owner.get("relationships", [])
                if relationship.get("target", "").startswith("owner:")
                and relationship.get("target", "")[6:] in selected_owner_ids
                for evidence in relationship.get("evidence", [])
            )
        for raw_path in declared:
            candidate, path_error = resolve_confined(raw_path)
            if path_error is not None:
                errors.append(f"{owner_id}: {path_error}")
            elif (
                candidate is not None
                and not path_exists(candidate)
                and raw_path not in generated_mirrors
            ):
                errors.append(f"{owner_id}: missing declared path: {raw_path}")
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
            if validate_owner_paths:
                cwd, path_error = resolve_confined(validation.get("cwd", ""))
                if path_error is not None:
                    errors.append(f"{owner_id}: {validation_id}: {path_error}")
                elif cwd is not None and not path_is_dir(cwd):
                    errors.append(f"{owner_id}: {validation_id}: invalid cwd")
        invariant_ids: set[str] = set()
        for invariant in owner.get("invariants", []):
            invariant_id = invariant.get("id", "")
            if not invariant_id or invariant_id in invariant_ids:
                errors.append(
                    f"{owner_id}: duplicate or empty invariant id: {invariant_id!r}"
                )
            invariant_ids.add(invariant_id)
            kind = invariant.get("kind")
            if kind not in INVARIANT_KINDS:
                errors.append(
                    f"{owner_id}: {invariant_id}: unknown invariant kind: {kind!r}"
                )
        for relationship in owner.get("relationships", []):
            category = relationship.get("category")
            kind = relationship.get("kind")
            confidence = relationship.get("confidence")
            target = relationship.get("target", "")
            if category not in RELATIONSHIP_CATEGORIES:
                errors.append(
                    f"{owner_id}: unknown relationship category: {category!r}"
                )
            if kind not in RELATIONSHIP_KINDS:
                errors.append(f"{owner_id}: unknown relationship kind: {kind!r}")
            if confidence not in CONFIDENCE_LEVELS:
                errors.append(
                    f"{owner_id}: unknown relationship confidence: {confidence!r}"
                )
            if not target.startswith(TARGET_PREFIXES):
                errors.append(f"{owner_id}: invalid relationship target: {target!r}")
            elif target.startswith("owner:") and target[6:] not in declared_owner_ids:
                errors.append(
                    f"{owner_id}: unknown relationship owner target: {target!r}"
                )
            elif target.startswith("path:") and validate_owner_paths:
                candidate, path_error = resolve_confined(target[5:])
                if path_error is not None:
                    errors.append(f"{owner_id}: {path_error}")
                elif candidate is not None and not path_exists(candidate):
                    errors.append(
                        f"{owner_id}: missing relationship target: {target!r}"
                    )
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


def _supporting_source_digest(manifest: dict, root: Path) -> str:
    root = root.resolve()
    paths: set[str] = set()
    generated_mirrors = {
        path
        for owner in manifest.get("owners", [])
        for path in owner.get("generated_mirrors", [])
    }
    for owner in manifest.get("owners", []):
        paths.update(owner.get("tests", []))
        paths.update(
            entry.get("path", "") for entry in owner.get("primary_entries", [])
        )
        paths.update(
            evidence.get("path", "")
            for relationship in owner.get("relationships", [])
            for evidence in relationship.get("evidence", [])
        )
        paths.update(
            evidence.get("path", "")
            for invariant in owner.get("invariants", [])
            for evidence in invariant.get("evidence", [])
        )
        paths.update(
            test
            for invariant in owner.get("invariants", [])
            for test in invariant.get("tests", [])
        )
    digest = hashlib.sha256()
    for raw_path in sorted(
        path for path in paths if path and path not in generated_mirrors
    ):
        digest.update(raw_path.encode("utf-8"))
        digest.update(b"\0")
        try:
            candidate = confined_path(root, raw_path)
            if candidate.is_file():
                digest.update(hashlib.sha256(candidate.read_bytes()).digest())
            else:
                digest.update(b"missing-or-directory")
        except (OSError, ValueError):
            digest.update(b"unreadable")
        digest.update(b"\0")
    return digest.hexdigest()


_snapshot_cache: OrderedDict[
    tuple[str, int, int, int, int, int], bytes
] = OrderedDict()
_snapshot_cache_bytes = 0
_snapshot_cache_lock = threading.Lock()


def _metadata_change_token(path: Path, observation: os.stat_result) -> int | None:
    if os.name != "nt":
        return observation.st_ctime_ns
    try:
        import ctypes
        import msvcrt

        class FileBasicInfo(ctypes.Structure):
            _fields_ = [
                ("creation_time", ctypes.c_longlong),
                ("last_access_time", ctypes.c_longlong),
                ("last_write_time", ctypes.c_longlong),
                ("change_time", ctypes.c_longlong),
                ("file_attributes", ctypes.c_ulong),
            ]

        with path.open("rb") as source:
            handle = msvcrt.get_osfhandle(source.fileno())
            info = FileBasicInfo()
            get_file_information = (
                ctypes.windll.kernel32.GetFileInformationByHandleEx
            )
            get_file_information.argtypes = [
                ctypes.c_void_p,
                ctypes.c_int,
                ctypes.c_void_p,
                ctypes.c_ulong,
            ]
            get_file_information.restype = ctypes.c_int
            succeeded = get_file_information(
                ctypes.c_void_p(handle), 0, ctypes.byref(info), ctypes.sizeof(info)
            )
            return info.change_time if succeeded else None
    except (OSError, ValueError):
        return None


def _snapshot_file_signature(path: Path) -> tuple[str, int, int, int, int, int] | None:
    try:
        observation = path.stat()
    except OSError:
        return None
    if not stat.S_ISREG(observation.st_mode):
        return None
    change_token = _metadata_change_token(path, observation)
    if change_token is None:
        return None
    return (
        os.fspath(path),
        observation.st_dev,
        observation.st_ino,
        observation.st_size,
        observation.st_mtime_ns,
        change_token,
    )


def _read_snapshot_file(path: Path) -> tuple[bytes | None, bool]:
    """Read a stable regular file, reusing only an identity-keyed observation."""
    global _snapshot_cache_bytes

    before = _snapshot_file_signature(path)
    if before is None:
        return None, False
    with _snapshot_cache_lock:
        cached = _snapshot_cache.get(before)
    if cached is not None:
        # Windows change timestamps can share a filesystem clock tick with a
        # same-size rewrite whose write timestamp is restored. Re-read before
        # trusting the metadata-keyed entry so the cache never returns stale
        # source bytes; an equal payload still avoids downstream reprocessing.
        try:
            observed_contents = path.read_bytes()
        except OSError:
            return None, False
        if _snapshot_file_signature(path) == before and observed_contents == cached:
            with _snapshot_cache_lock:
                if before in _snapshot_cache:
                    _snapshot_cache.move_to_end(before)
            return cached, False
        contents = observed_contents
    else:
        contents = None
    for _ in range(2):
        if contents is None:
            try:
                contents = path.read_bytes()
            except OSError:
                return None, False
        after = _snapshot_file_signature(path)
        if after == before:
            break
        before = after
        contents = None
        if before is None:
            return None, True
    else:
        raise OSError(f"file changed while snapshotting: {path}")
    assert contents is not None
    if len(contents) <= MAX_SNAPSHOT_CACHE_BYTES:
        with _snapshot_cache_lock:
            previous = _snapshot_cache.pop(before, None)
            if previous is not None:
                _snapshot_cache_bytes -= len(previous)
            _snapshot_cache[before] = contents
            _snapshot_cache_bytes += len(contents)
            while (
                len(_snapshot_cache) > MAX_SNAPSHOT_CACHE_ENTRIES
                or _snapshot_cache_bytes > MAX_SNAPSHOT_CACHE_BYTES
            ):
                _, evicted = _snapshot_cache.popitem(last=False)
                _snapshot_cache_bytes -= len(evicted)
    return contents, True


def repository_revision(root: Path, manifest_digest: str, manifest: dict) -> str:
    # A generated file cannot reproducibly embed the commit that contains it:
    # committing that value creates a new commit and immediately makes the
    # file stale. Key the graph by its authoritative manifest and supporting
    # source identities instead.
    return f"manifest:{manifest_digest}:sources:{_supporting_source_digest(manifest, root)}"


def query_graph(
    manifest: dict,
    digest: str,
    root: Path,
    owner_ids: list[str] | None = None,
    max_relationships: int = MAX_QUERY_RELATIONSHIPS,
) -> dict:
    if not 1 <= max_relationships <= MAX_QUERY_RELATIONSHIPS:
        raise ValueError(
            f"max_relationships must be between 1 and {MAX_QUERY_RELATIONSHIPS}"
        )
    return _query_graph(manifest, digest, root, owner_ids, max_relationships)


def _query_graph(
    manifest: dict,
    digest: str,
    root: Path,
    owner_ids: list[str] | None,
    max_relationships: int | None,
) -> dict:
    owners_by_id = {owner["id"]: owner for owner in manifest["owners"]}
    selected_ids = sorted(set(owner_ids or owners_by_id))
    unknown = sorted(set(selected_ids) - set(owners_by_id))
    if unknown:
        raise ValueError(f"unknown owner ids: {', '.join(unknown)}")

    relationships: list[dict] = []
    for source_id, owner in owners_by_id.items():
        for relationship in owner.get("relationships", []):
            target_owner = (
                relationship["target"][6:]
                if relationship["target"].startswith("owner:")
                else None
            )
            if source_id not in selected_ids and target_owner not in selected_ids:
                continue
            relationships.append({"source": f"owner:{source_id}", **relationship})
    relationships = _deduplicate_graph_relationships(relationships)
    relationship_count = len(relationships)
    def relationship_key(item: dict) -> tuple[str, str, str, str]:
        return (
            item["source"],
            item["category"],
            item["kind"],
            item["target"],
        )
    relationships = _bounded_sorted(
        relationships, relationship_key, max_relationships
    )
    omitted_relationships = (
        0
        if max_relationships is None
        else max(0, relationship_count - max_relationships)
    )

    selected_owners = []
    for owner_id in selected_ids:
        owner = owners_by_id[owner_id]
        selected_owners.append(
            {
                "id": owner_id,
                "feature_ids": owner.get("feature_ids", []),
                "roots": owner.get("roots", []),
                "primary_entries": owner.get("primary_entries", []),
                "configuration": owner.get("contracts", []),
                "generated_artifacts": owner.get("generated_mirrors", []),
                "facet_exclusions": owner.get("facet_exclusions", {}),
                "tests": owner.get("tests", []),
                "invariants": owner.get("invariants", []),
                "validation": owner.get("validation", []),
            }
        )
    return {
        "schema_version": SCHEMA_VERSION,
        "repository_revision": repository_revision(root, digest, manifest),
        "manifest_sha256": digest,
        "status": "partial" if omitted_relationships else "complete",
        "owners": selected_owners,
        "relationships": relationships,
        "omitted": {"relationships": omitted_relationships},
        "unresolved": [],
    }


def _manifest_projection_from_graph(graph: dict) -> dict:
    owners: list[dict] = []
    owners_by_id: dict[str, dict] = {}
    for projected in graph["owners"]:
        owner = {
            **projected,
            "contracts": projected.get("configuration", []),
            "generated_mirrors": projected.get("generated_artifacts", []),
            "relationships": [],
        }
        owners.append(owner)
        owners_by_id[owner["id"]] = owner
    for relationship in graph["relationships"]:
        source_id = relationship["source"].removeprefix("owner:")
        owner = owners_by_id.get(source_id)
        if owner is not None:
            owner["relationships"].append(
                {key: value for key, value in relationship.items() if key != "source"}
            )
    return {"owners": owners}


def _architecture_index_is_usable(index: object, digest: str) -> bool:
    if not isinstance(index, dict):
        return False
    if (
        index.get("index_version") != ARCHITECTURE_INDEX_VERSION
        or index.get("schema_version") != SCHEMA_VERSION
        or index.get("manifest_sha256") != digest
        or index.get("status") != "complete"
        or index.get("omitted") != {"relationships": 0}
        or not isinstance(index.get("owners"), list)
        or not isinstance(index.get("relationships"), list)
    ):
        return False
    stored_checksum = index.get("index_sha256")
    if not isinstance(stored_checksum, str):
        return False
    checksum_payload = {
        key: value for key, value in index.items() if key != "index_sha256"
    }
    if stored_checksum != hashlib.sha256(
        json.dumps(
            checksum_payload,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")
    ).hexdigest():
        return False
    owner_ids: set[str] = set()
    for owner in index["owners"]:
        if not isinstance(owner, dict) or not isinstance(owner.get("id"), str):
            return False
        if owner["id"] in owner_ids:
            return False
        owner_ids.add(owner["id"])
    for relationship in index["relationships"]:
        if not isinstance(relationship, dict):
            return False
        if not all(
            isinstance(relationship.get(field), str)
            for field in ("source", "category", "kind", "target", "confidence")
        ) or not isinstance(relationship.get("evidence"), list):
            return False
        if relationship["source"].removeprefix("owner:") not in owner_ids:
            return False
    return True


def load_architecture_index(
    index_path: Path, digest: str, root: Path
) -> dict | None:
    try:
        candidate = json.loads(index_path.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, UnicodeError, json.JSONDecodeError):
        return None
    if not _architecture_index_is_usable(candidate, digest):
        return None
    candidate["repository_revision"] = repository_revision(
        root, digest, _manifest_projection_from_graph(candidate)
    )
    return candidate


def _select_index_graph(
    index: dict,
    owner_ids: list[str] | None,
    max_relationships: int | None,
) -> dict:
    owners_by_id = {owner["id"]: owner for owner in index["owners"]}
    selected_ids = sorted(set(owner_ids or owners_by_id))
    unknown = sorted(set(selected_ids) - set(owners_by_id))
    if unknown:
        raise ValueError(f"unknown owner ids: {', '.join(unknown)}")
    relationships = [
        relationship
        for relationship in index["relationships"]
        if relationship["source"].removeprefix("owner:") in selected_ids
        or (
            relationship["target"].startswith("owner:")
            and relationship["target"].removeprefix("owner:") in selected_ids
        )
    ]
    relationships = _deduplicate_graph_relationships(relationships)
    relationship_count = len(relationships)
    relationships = _bounded_sorted(
        relationships,
        lambda item: (
            item["source"],
            item["category"],
            item["kind"],
            item["target"],
        ),
        max_relationships,
    )
    omitted_relationships = (
        0
        if max_relationships is None
        else max(0, relationship_count - max_relationships)
    )
    return {
        key: value
        for key, value in index.items()
        if key
        not in {
            "index_version",
            "index_sha256",
            "owners",
            "relationships",
            "status",
            "omitted",
        }
    } | {
        "owners": [owners_by_id[owner_id] for owner_id in selected_ids],
        "relationships": relationships,
        "status": "partial" if omitted_relationships else "complete",
        "omitted": {"relationships": omitted_relationships},
    }


def _evidence_location(evidence: dict) -> str:
    symbol = evidence.get("symbol")
    return f"{evidence['path']}::{symbol}" if symbol else evidence["path"]


def _ranking_tokens(value: str | None) -> set[str]:
    if not value:
        return set()
    normalized = unicodedata.normalize("NFKC", value).casefold()
    return {
        token
        for token in re.findall(r"[^\W_]+", normalized)
        if (len(token) > 1 or not token.isascii()) and token not in RANKING_STOP_WORDS
    }


def _relationship_rank_key(
    facet: str,
    relationship: dict,
    selected_ids: set[str],
    focus_tokens: set[str],
) -> tuple:
    searchable = " ".join(
        str(relationship.get(field, ""))
        for field in ("kind", "source", "target", "evidence")
    )
    overlap = len(focus_tokens & _ranking_tokens(searchable))
    endpoints = {
        relationship[endpoint].removeprefix("owner:")
        for endpoint in ("source", "target")
        if relationship.get(endpoint, "").startswith("owner:")
    }
    selected_endpoint_count = len(endpoints & selected_ids)
    kind_priority = FACET_KIND_PRIORITY.get(facet, {}).get(
        relationship.get("kind", ""), 0
    )
    provenance_priority = {
        "exact": 2,
        "declared": 1,
        "heuristic": 0,
    }.get(relationship.get("provenance", ""), 0)
    return (
        -kind_priority,
        -overlap,
        -provenance_priority,
        -selected_endpoint_count,
        relationship.get("source", ""),
        relationship.get("kind", ""),
        relationship.get("target", ""),
        relationship.get("evidence", ""),
    )


def _deduplicate_relationships(relationships: list[dict]) -> list[dict]:
    unique: list[dict] = []
    seen: set[tuple[str, str, str, str, str]] = set()
    for relationship in relationships:
        identity = tuple(
            relationship.get(field, "")
            for field in ("kind", "source", "target", "evidence", "provenance")
        )
        if identity in seen:
            continue
        seen.add(identity)
        unique.append(relationship)
    return unique


def _deduplicate_graph_relationships(relationships: list[dict]) -> list[dict]:
    unique: list[dict] = []
    seen: set[str] = set()
    for relationship in relationships:
        identity = json.dumps(
            relationship,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        )
        if identity in seen:
            continue
        seen.add(identity)
        unique.append(relationship)
    return unique


def _bounded_sorted(
    items: list[dict], key: object, limit: int | None
) -> list[dict]:
    if limit is None or limit >= len(items):
        return sorted(items, key=key)
    return heapq.nsmallest(limit, items, key=key)


def _round_robin_relationships(
    relationships_by_facet: dict[str, list[dict]], limit: int
) -> dict[str, list[dict]]:
    retained: dict[str, list[dict]] = {
        facet: [] for facet in relationships_by_facet
    }
    retained_count = 0
    for rank in range(max(map(len, relationships_by_facet.values()), default=0)):
        if retained_count >= limit:
            break
        for facet, relationships in relationships_by_facet.items():
            if rank < len(relationships) and retained_count < limit:
                retained[facet].append(relationships[rank])
                retained_count += 1
    return retained


def _slice_source_snapshot(
    root: Path, graph: dict, owners: list[dict]
) -> tuple[str, int, int]:
    """Hash the exact declared files that support a bounded architecture slice."""
    root = root.resolve()
    paths: set[str] = set()
    for relationship in graph["relationships"]:
        paths.update(item["path"] for item in relationship["evidence"])
    for owner in owners:
        paths.update(entry["path"] for entry in owner.get("primary_entries", []))
        paths.update(owner.get("tests", []))
        paths.update(owner.get("generated_mirrors", []))
        for invariant in owner.get("invariants", []):
            paths.update(item["path"] for item in invariant["evidence"])
            paths.update(invariant.get("tests", []))

    digest = hashlib.sha256()
    files_read = 0
    bytes_read = 0
    for relative_path in sorted(paths):
        digest.update(relative_path.encode("utf-8"))
        try:
            candidate = confined_path(root, relative_path)
        except ValueError:
            digest.update(b"\0missing-or-not-a-file\0")
            continue
        contents, was_read = _read_snapshot_file(candidate)
        if contents is None:
            digest.update(b"\0missing-or-not-a-file\0")
            continue
        digest.update(b"\0file\0")
        digest.update(contents)
        if was_read:
            files_read += 1
            bytes_read += len(contents)
    return digest.hexdigest(), files_read, bytes_read


def architecture_slice(
    manifest: dict,
    digest: str,
    root: Path,
    owner_ids: list[str] | None = None,
    max_relationships: int = MAX_SLICE_RELATIONSHIPS,
    focus: str | None = None,
    manifest_bytes_read: int = 0,
    graph: dict | None = None,
) -> dict:
    """Return a completeness-first slice ranked within each architecture facet."""
    if not 1 <= max_relationships <= MAX_SLICE_RELATIONSHIPS:
        raise ValueError(
            f"max_relationships must be between 1 and {MAX_SLICE_RELATIONSHIPS}"
        )
    if graph is None:
        graph = _query_graph(manifest, digest, root, owner_ids, max_relationships=None)
        selected = {owner["id"]: owner for owner in manifest["owners"]}
    else:
        graph = _select_index_graph(graph, owner_ids, max_relationships=None)
        selected = {
            owner["id"]: {
                **owner,
                "contracts": owner.get("configuration", []),
                "generated_mirrors": owner.get("generated_artifacts", []),
            }
            for owner in graph["owners"]
        }
    selected_ids = sorted(set(owner_ids or selected))
    selected_id_set = set(selected_ids)
    focus_tokens = _ranking_tokens(focus)
    selected_owners = [selected[owner_id] for owner_id in selected_ids]
    source_snapshot, files_read, bytes_read = _slice_source_snapshot(
        root, graph, selected_owners
    )
    facets: dict[str, list[dict]] = {name: [] for name in ARCHITECTURE_FACETS}
    coverage: dict[str, set[str]] = {name: set() for name in ARCHITECTURE_FACETS}

    for relationship in graph["relationships"]:
        source_id = relationship["source"].removeprefix("owner:")
        target_id = relationship["target"].removeprefix("owner:")
        involved = set(selected_ids) & {source_id, target_id}
        facet = CATEGORY_FACETS[relationship["category"]]
        coverage[facet].update(involved or {source_id})
        facets[facet].append(
            {
                "kind": RELATIONSHIP_KINDS_FOR_CLOSURE[relationship["kind"]],
                "source": relationship["source"],
                "target": relationship["target"],
                "evidence": ", ".join(
                    _evidence_location(item) for item in relationship["evidence"]
                ),
                "provenance": (
                    "exact"
                    if relationship["confidence"] == "compiler_resolved"
                    else "declared"
                ),
            }
        )

    for owner_id in selected_ids:
        owner = selected[owner_id]
        for entry in owner.get("primary_entries", []):
            facets["registration_and_entrypoints"].append(
                {
                    "kind": "entrypoint",
                    "source": f"owner:{owner_id}",
                    "target": f"path:{entry['path']}::{entry['symbol']}",
                    "evidence": f"source_owners.toml::{owner_id}.primary_entries",
                    "provenance": "declared",
                }
            )
            coverage["registration_and_entrypoints"].add(owner_id)
        for test in owner.get("tests", []):
            facets["tests_and_contracts"].append(
                {
                    "kind": "test",
                    "source": f"owner:{owner_id}",
                    "target": f"path:{test}",
                    "evidence": f"source_owners.toml::{owner_id}.tests",
                    "provenance": "declared",
                }
            )
            coverage["tests_and_contracts"].add(owner_id)
        for contract in owner.get("contracts", []):
            facets["tests_and_contracts"].append(
                {
                    "kind": "contract",
                    "source": f"owner:{owner_id}",
                    "target": f"contract:{contract}",
                    "evidence": f"source_owners.toml::{owner_id}.contracts",
                    "provenance": "declared",
                }
            )
            coverage["tests_and_contracts"].add(owner_id)
        for generated in owner.get("generated_mirrors", []):
            facets["generated_artifacts"].append(
                {
                    "kind": "generated_consumer",
                    "source": f"owner:{owner_id}",
                    "target": f"generated:{generated}",
                    "evidence": f"source_owners.toml::{owner_id}.generated_mirrors",
                    "provenance": "declared",
                }
            )
            coverage["generated_artifacts"].add(owner_id)
        for invariant in owner.get("invariants", []):
            facets["invariants"].append(
                {
                    "kind": "invariant",
                    "source": f"owner:{owner_id}",
                    "target": f"contract:{invariant['id']}",
                    "evidence": ", ".join(
                        _evidence_location(item) for item in invariant["evidence"]
                    ),
                    "provenance": "declared",
                }
            )
            coverage["invariants"].add(owner_id)

    unknowns: list[str] = []
    output: dict[str, object] = {}
    facet_relationship_totals: dict[str, int] = {}
    for facet in ARCHITECTURE_FACETS:
        uncovered = []
        reasons = []
        for owner_id in selected_ids:
            if owner_id in coverage[facet]:
                continue
            reason = selected[owner_id].get("facet_exclusions", {}).get(facet)
            if reason:
                reasons.append(f"{owner_id}: {reason}")
            else:
                uncovered.append(owner_id)
        if uncovered:
            unknowns.append(f"{facet}: missing declarations for {', '.join(uncovered)}")
        unique_relationships = _deduplicate_relationships(facets[facet])
        facet_relationship_totals[facet] = len(unique_relationships)
        relationships = _bounded_sorted(
            unique_relationships,
            lambda item: _relationship_rank_key(
                facet, item, selected_id_set, focus_tokens
            ),
            max_relationships,
        )
        if relationships:
            output[facet] = {"status": "established", "relationships": relationships}
        else:
            output[facet] = {
                "status": "not_applicable"
                if reasons and not uncovered
                else "established",
                "relationships": [],
                **({"not_applicable_reason": "; ".join(reasons)} if reasons else {}),
            }

    relationship_total = sum(facet_relationship_totals.values())
    omitted_relationships = graph["omitted"]["relationships"]
    if relationship_total > max_relationships:
        retained = _round_robin_relationships(
            {
                facet: output[facet]["relationships"]
                for facet in ARCHITECTURE_FACETS
            },
            max_relationships,
        )
        omitted_relationships += relationship_total - max_relationships
        for facet in ARCHITECTURE_FACETS:
            output[facet]["relationships"] = retained[facet]

    ranking_limitation = (
        "Relationships are ordered within each facet by facet-specific kind, "
        "focus-term overlap, provenance, and selected-owner directness."
    )
    return {
        "snapshot": f"{graph['repository_revision']}:{digest}:{source_snapshot}",
        **output,
        "truncated": graph["status"] != "complete" or omitted_relationships > 0,
        "omitted_relationships": omitted_relationships,
        "material_unknowns": unknowns,
        "limitations": [
            "Manifest relationships are declarative; inspect exact source before mutation.",
            ranking_limitation,
        ],
        "metrics": {
            "tool_calls": 1,
            "files_read": files_read + 1,
            "bytes_read": bytes_read + manifest_bytes_read,
            "late_relationship_discoveries": 0,
        },
    }


def expected_architecture_index(manifest: dict, digest: str, root: Path) -> str:
    graph = _query_graph(manifest, digest, root, None, max_relationships=None)
    graph["index_version"] = ARCHITECTURE_INDEX_VERSION
    graph["index_sha256"] = hashlib.sha256(
        json.dumps(
            graph,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        ).encode("utf-8")
    ).hexdigest()
    return json.dumps(graph, indent=2, sort_keys=True) + "\n"


def render_block(manifest: dict, digest: str) -> str:
    lines = [
        f"{BEGIN_PREFIX} schema={SCHEMA_VERSION} manifest_sha256={digest} -->",
        "### Managed KD4 source-owner index",
        "",
        "This table is generated by `scripts/source_owners.py`; edit `source_owners.toml`, not this block.",
        "",
        "| Owner ID | Owning roots | Primary entries | Relationships | Invariants | Focused validation |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for owner in sorted(manifest["owners"], key=lambda owner: owner["id"]):
        roots = "<br>".join(f"`{path}`" for path in owner.get("roots", [])) or "-"
        entries = (
            "<br>".join(
                f"`{entry['path']}::{entry['symbol']}`"
                for entry in owner.get("primary_entries", [])
            )
            or "-"
        )
        validations = (
            "<br>".join(f"`{entry['id']}`" for entry in owner.get("validation", []))
            or "-"
        )
        relationships = owner.get("relationships", [])
        relationship_summary = (
            "<br>".join(
                f"`{entry['category']}:{entry['kind']}` -> `{entry['target']}`"
                for entry in relationships[:3]
            )
            or "-"
        )
        if len(relationships) > 3:
            relationship_summary += f"<br>+{len(relationships) - 3} more"
        invariants = owner.get("invariants", [])
        invariant_summary = (
            "<br>".join(f"`{entry['kind']}:{entry['id']}`" for entry in invariants[:3])
            or "-"
        )
        if len(invariants) > 3:
            invariant_summary += f"<br>+{len(invariants) - 3} more"
        lines.append(
            f"| `{owner['id']}` | {roots} | {entries} | {relationship_summary} | "
            f"{invariant_summary} | {validations} |"
        )
    lines.extend([END_MARKER, ""])
    return "\n".join(lines)


def replace_managed_block(source_map: str, block: str) -> str:
    begin = source_map.find(BEGIN_PREFIX)
    end = source_map.find(END_MARKER)
    if begin == -1 and end == -1:
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


def owner_catalog(manifest: dict) -> dict:
    """Return the compact owner identifiers needed to bootstrap a slice."""
    return {
        "schema_version": manifest["schema_version"],
        "owners": [
            {
                "id": owner["id"],
                "aliases": owner.get("aliases", []),
                "phrases": owner.get("phrases", []),
            }
            for owner in manifest["owners"]
        ],
        "next": "python scripts/source_owners.py slice --owner <owner-id> --focus <task> --max-relationships 32",
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog="Use `list` before `slice` when the owner ID is not already known.",
    )
    parser.add_argument(
        "command", choices=("generate", "check", "list", "query", "slice")
    )
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--source-map", type=Path, default=DEFAULT_SOURCEMAP)
    parser.add_argument(
        "--architecture-index", type=Path, default=DEFAULT_ARCHITECTURE_INDEX
    )
    parser.add_argument(
        "--owner",
        action="append",
        dest="owners",
        help="Owner ID to include in a graph query; repeat for multiple owners.",
    )
    parser.add_argument(
        "--max-relationships",
        type=int,
        default=MAX_QUERY_RELATIONSHIPS,
        help=f"Maximum relationships returned by query (1-{MAX_QUERY_RELATIONSHIPS}).",
    )
    parser.add_argument(
        "--focus",
        help="Slice-only task description used to rank architecture relationships.",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        help="Repository root for declared paths (defaults to the manifest directory).",
    )
    args = parser.parse_args()
    if args.command == "query" and args.focus is not None:
        print(
            "--focus is only valid with slice; select an owner with query, then run "
            "slice --owner <id> --focus <task>",
            file=sys.stderr,
        )
        return 2
    root = (args.repo_root or args.manifest.resolve().parent).resolve()
    if args.command == "generate":
        try:
            with source_map_lock(root, f"source-owners:{os.getpid()}"):
                manifest, digest = load_and_validate(args.manifest, root)
                source_map = args.source_map.read_text(encoding="utf-8")
                expected = replace_managed_block(
                    source_map, render_block(manifest, digest)
                )
                expected_index = expected_architecture_index(manifest, digest, root)
                write_text_atomic(args.source_map, expected)
                write_text_atomic(args.architecture_index, expected_index)
            return 0
        except (GenerationLockError, OSError, ValueError, tomllib.TOMLDecodeError) as error:
            print(error, file=sys.stderr)
            return 1
    if args.command == "list":
        try:
            manifest, _ = load_and_validate(args.manifest, root)
            print(json.dumps(owner_catalog(manifest), indent=2, sort_keys=True))
            return 0
        except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
            print(error, file=sys.stderr)
            return 1
    try:
        if args.command in {"query", "slice"}:
            manifest_bytes = args.manifest.read_bytes()
            digest = hashlib.sha256(manifest_bytes).hexdigest()
            cached_graph = load_architecture_index(
                args.architecture_index, digest, root
            )
            if cached_graph is None:
                manifest, digest = load_and_validate(
                    args.manifest, root, owner_ids=args.owners
                )
            else:
                manifest = None
            if args.command == "query":
                if not 1 <= args.max_relationships <= MAX_QUERY_RELATIONSHIPS:
                    raise ValueError(
                        "max_relationships must be between "
                        f"1 and {MAX_QUERY_RELATIONSHIPS}"
                    )
                result = (
                    query_graph(
                        manifest,
                        digest,
                        root,
                        args.owners,
                        args.max_relationships,
                    )
                    if cached_graph is None
                    else _select_index_graph(
                        cached_graph, args.owners, args.max_relationships
                    )
                )
            else:
                result = architecture_slice(
                    manifest or {"owners": []},
                    digest,
                    root,
                    args.owners,
                    min(args.max_relationships, MAX_SLICE_RELATIONSHIPS),
                    args.focus,
                    len(manifest_bytes),
                    graph=cached_graph,
                )
            print(
                json.dumps(
                    result,
                    indent=2,
                    sort_keys=True,
                )
            )
            return 0
        manifest, digest = load_and_validate(args.manifest, root)
        source_map = args.source_map.read_text(encoding="utf-8")
        expected = replace_managed_block(source_map, render_block(manifest, digest))
        expected_index = expected_architecture_index(manifest, digest, root)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
        print(error, file=sys.stderr)
        return 1
    if args.command == "check":
        stale = False
        if source_map != expected:
            print(
                "SOURCEMAP.md managed source-owner block is stale; run source_owners.py generate",
                file=sys.stderr,
            )
            stale = True
        try:
            actual_index = args.architecture_index.read_text(encoding="utf-8")
        except FileNotFoundError:
            actual_index = ""
        if actual_index != expected_index:
            print(
                "architecture_index.json is stale; run source_owners.py generate",
                file=sys.stderr,
            )
            stale = True
        return int(stale)
    raise AssertionError(f"unhandled source-owner command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
