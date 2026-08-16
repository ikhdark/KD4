#!/usr/bin/env python3
"""Validate source_owners.toml and generate its marked SOURCEMAP.md block."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = REPO_ROOT / "source_owners.toml"
DEFAULT_SOURCEMAP = REPO_ROOT / "SOURCEMAP.md"
DEFAULT_ARCHITECTURE_INDEX = REPO_ROOT / "architecture_index.json"
SCHEMA_VERSION = 2
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
    declared_owner_ids = {
        owner.get("id") for owner in owners if isinstance(owner.get("id"), str)
    }
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
        generated_mirrors = set(owner.get("generated_mirrors", []))
        declared = [
            *owner.get("roots", []),
            *owner.get("instructions", []),
            *owner.get("consumers", []),
            *owner.get("contracts", []),
            *owner.get("generated_mirrors", []),
            *owner.get("tests", []),
            *(entry.get("path", "") for entry in owner.get("primary_entries", [])),
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
        for raw_path in declared:
            try:
                candidate = confined_path(root, raw_path)
                if not candidate.exists() and raw_path not in generated_mirrors:
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
            elif target.startswith("path:"):
                try:
                    if not confined_path(root, target[5:]).exists():
                        errors.append(
                            f"{owner_id}: missing relationship target: {target!r}"
                        )
                except ValueError as error:
                    errors.append(f"{owner_id}: {error}")
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


def repository_revision(root: Path, manifest_digest: str) -> str:
    try:
        completed = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "HEAD"],
            check=True,
            capture_output=True,
            text=True,
            timeout=5,
        )
        revision = completed.stdout.strip()
        if revision:
            return revision
    except (OSError, subprocess.SubprocessError):
        pass
    return f"manifest:{manifest_digest}"


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
    relationships.sort(
        key=lambda item: (
            item["source"],
            item["category"],
            item["kind"],
            item["target"],
        )
    )
    omitted_relationships = max(0, len(relationships) - max_relationships)
    relationships = relationships[:max_relationships]

    selected_owners = []
    for owner_id in selected_ids:
        owner = owners_by_id[owner_id]
        selected_owners.append(
            {
                "id": owner_id,
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
        "repository_revision": repository_revision(root, digest),
        "manifest_sha256": digest,
        "status": "partial" if omitted_relationships else "complete",
        "owners": selected_owners,
        "relationships": relationships,
        "omitted": {"relationships": omitted_relationships},
        "unresolved": [],
    }


def _evidence_location(evidence: dict) -> str:
    symbol = evidence.get("symbol")
    return f"{evidence['path']}::{symbol}" if symbol else evidence["path"]


def _ranking_tokens(value: str | None) -> set[str]:
    if not value:
        return set()
    return {
        token
        for token in re.findall(r"[a-z0-9]+", value.casefold())
        if len(token) > 1 and token not in RANKING_STOP_WORDS
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


def _slice_source_snapshot(
    root: Path, graph: dict, owners: list[dict]
) -> tuple[str, int, int]:
    """Hash the exact declared files that support a bounded architecture slice."""
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
        candidate = root / relative_path
        if not candidate.is_file():
            digest.update(b"\0missing-or-not-a-file\0")
            continue
        contents = candidate.read_bytes()
        digest.update(b"\0file\0")
        digest.update(contents)
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
) -> dict:
    """Return a completeness-first slice ranked within each architecture facet."""
    if not 1 <= max_relationships <= MAX_SLICE_RELATIONSHIPS:
        raise ValueError(
            f"max_relationships must be between 1 and {MAX_SLICE_RELATIONSHIPS}"
        )
    graph = query_graph(
        manifest, digest, root, owner_ids, max_relationships=MAX_QUERY_RELATIONSHIPS
    )
    selected = {owner["id"]: owner for owner in manifest["owners"]}
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
        relationships = sorted(
            _deduplicate_relationships(facets[facet]),
            key=lambda item: _relationship_rank_key(
                facet, item, selected_id_set, focus_tokens
            ),
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

    relationship_total = sum(
        len(output[facet]["relationships"]) for facet in ARCHITECTURE_FACETS
    )
    omitted_relationships = graph["omitted"]["relationships"]
    if relationship_total > max_relationships:
        retained: dict[str, list[dict]] = {facet: [] for facet in ARCHITECTURE_FACETS}
        for rank in range(
            max(len(output[facet]["relationships"]) for facet in ARCHITECTURE_FACETS)
        ):
            for facet in ARCHITECTURE_FACETS:
                relationships = output[facet]["relationships"]
                if (
                    rank < len(relationships)
                    and sum(map(len, retained.values())) < max_relationships
                ):
                    retained[facet].append(relationships[rank])
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
            "bytes_read": bytes_read
            + (
                DEFAULT_MANIFEST.stat().st_size
                if DEFAULT_MANIFEST.exists() and root == REPO_ROOT
                else 0
            ),
            "late_relationship_discoveries": 0,
        },
    }


def expected_architecture_index(manifest: dict, digest: str, root: Path) -> str:
    return (
        json.dumps(query_graph(manifest, digest, root), indent=2, sort_keys=True) + "\n"
    )


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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("generate", "check", "query", "slice"))
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
        help="Task description used to rank relationships within each architecture facet.",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        help="Repository root for declared paths (defaults to the manifest directory).",
    )
    args = parser.parse_args()
    root = args.repo_root or args.manifest.resolve().parent
    try:
        if args.command in {"query", "slice"}:
            manifest, digest = load_and_validate(args.manifest, root)
            print(
                json.dumps(
                    (
                        query_graph(
                            manifest,
                            digest,
                            root,
                            args.owners,
                            args.max_relationships,
                        )
                        if args.command == "query"
                        else architecture_slice(
                            manifest,
                            digest,
                            root,
                            args.owners,
                            min(args.max_relationships, MAX_SLICE_RELATIONSHIPS),
                            args.focus,
                        )
                    ),
                    indent=2,
                    sort_keys=True,
                )
            )
            return 0
        manifest, digest = load_and_validate(args.manifest, root)
        expected = replace_managed_block(
            args.source_map.read_text(encoding="utf-8"), render_block(manifest, digest)
        )
        expected_index = expected_architecture_index(manifest, digest, root)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
        print(error, file=sys.stderr)
        return 1
    actual = args.source_map.read_text(encoding="utf-8")
    if args.command == "check":
        stale = False
        if actual != expected:
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
    write_text_atomic(args.source_map, expected)
    write_text_atomic(args.architecture_index, expected_index)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
