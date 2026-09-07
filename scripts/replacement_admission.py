#!/usr/bin/env python3
"""Validate and project repository-owned replacement admissions.

This module is intentionally dormant: it does not modify the V2 inventory or
ledger. Nonempty manifests can be structurally checked, but materialization
fails closed until trusted in-process authority is supplied by a later
activation.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unicodedata
from typing import Any, NoReturn

if __package__:
    from scripts.completion_proof_inventory_v2 import (
        FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256,
        FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT,
        InventoryV2ContractError,
        derive_frozen_v1_historical_replacement_graph_v1,
        proof_hash,
        validate_test_replacement_ledger_v2,
        validate_trusted_defect_receipt_v1,
        validate_v2_historical_replacement_graph_closure_v1,
    )
    from scripts.focused_replacement_approval_receipt import (
        FORMAT_ID as FOCUSED_REPLACEMENT_APPROVAL_RECEIPT_FORMAT_ID,
        FocusedReplacementApprovalReceiptError,
        validate_focused_replacement_approval_receipt_v1,
    )
else:
    from completion_proof_inventory_v2 import (  # type: ignore[no-redef]
        FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256,
        FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT,
        InventoryV2ContractError,
        derive_frozen_v1_historical_replacement_graph_v1,
        proof_hash,
        validate_test_replacement_ledger_v2,
        validate_trusted_defect_receipt_v1,
        validate_v2_historical_replacement_graph_closure_v1,
    )
    from focused_replacement_approval_receipt import (  # type: ignore[no-redef]
        FORMAT_ID as FOCUSED_REPLACEMENT_APPROVAL_RECEIPT_FORMAT_ID,
        FocusedReplacementApprovalReceiptError,
        validate_focused_replacement_approval_receipt_v1,
    )


FORMAT_ID = "kd4.replacement-admissions.v1"
SUCCESSOR_CATALOG_FORMAT_ID = "kd4.replacement-successor-catalog.v1"
PROJECTION_FORMAT_ID = "kd4.replacement-admission-projection.v1"
HISTORICAL_REVIEW_PLAN_FORMAT_ID = "kd4.historical-replacement-review-plan.v1"
HISTORICAL_ACCEPTANCE_PROPOSAL_FORMAT_ID = (
    "kd4.historical-replacement-acceptance-proposal.v1"
)
HISTORICAL_SCOPE_REVIEW_DISPOSITION = "reviewed-no-incorrect-behavior"
SCHEMA_VERSION = 1

INVENTORY_PATH = ".codex/validation/frozen-test-inventory-v1.json"
LEDGER_PATH = ".codex/validation/test-replacements-v1.json"
V2_LEDGER_PATH = ".codex/validation/test-replacements-v2.json"
STAGE2_PATH = ".codex/validation/stage2-incorrect-behaviors-v1.txt"

FROZEN_INVENTORY_RAW_SHA256 = (
    "df230a7683f0f31f1aae4d3f7644af39cec67b09fadf8f3f1e6c60729d18196a"
)
FROZEN_INVENTORY_SEMANTIC_SHA256 = (
    "a2fb8c0b806853b6375d92cfa6daf985ea35a5c4d4ecd49cf1d6da6f23359152"
)
FROZEN_LEDGER_RAW_SHA256 = (
    "210acb8428be83b9c6acd021bde4e44905e738d557ca89db60cb1271f65da46b"
)
FROZEN_BASELINE_IDS_SHA256 = (
    "9100d0fe0dd4c270a1216b6d3ec6f6c39b7f93278cf68235f500edae17d25eb1"
)

HEX64 = set("0123456789abcdef")


class AdmissionError(ValueError):
    """A closed-contract admission validation failure."""


def _fail(message: str) -> NoReturn:
    raise AdmissionError(message)


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def content_digest(domain: str, payload: Any) -> str:
    return sha256_bytes(canonical_json({"domain": domain, "payload": payload}))


def file_sha256(path: Path) -> str:
    return sha256_bytes(path.read_bytes())


def _pairs_no_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            _fail(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_json(path: Path) -> Any:
    try:
        value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=_pairs_no_duplicates)
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        _fail(f"cannot read JSON {path}: {exc}")
    _validate_json_value(value, str(path))
    return value


def _validate_json_value(value: Any, location: str) -> None:
    if isinstance(value, float):
        _fail(f"floating-point JSON is forbidden at {location}")
    if isinstance(value, str) and unicodedata.normalize("NFC", value) != value:
        _fail(f"non-NFC text at {location}")
    if isinstance(value, list):
        for index, item in enumerate(value):
            _validate_json_value(item, f"{location}[{index}]")
    elif isinstance(value, dict):
        for key, item in value.items():
            _validate_json_value(key, f"{location}.<key>")
            _validate_json_value(item, f"{location}.{key}")


def _object(value: Any, location: str, required: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail(f"{location} must be an object")
    missing = required - set(value)
    extra = set(value) - required
    if missing:
        _fail(f"{location} is missing fields: {', '.join(sorted(missing))}")
    if extra:
        _fail(f"{location} has extra fields: {', '.join(sorted(extra))}")
    return value


def _array(value: Any, location: str, *, nonempty: bool = False) -> list[Any]:
    if not isinstance(value, list):
        _fail(f"{location} must be an array")
    if nonempty and not value:
        _fail(f"{location} must not be empty")
    return value


def _string(value: Any, location: str, *, nonempty: bool = True) -> str:
    if not isinstance(value, str) or (nonempty and not value):
        _fail(f"{location} must be {'a nonempty' if nonempty else 'a'} string")
    return value


def _hash(value: Any, location: str) -> str:
    text = _string(value, location)
    if len(text) != 64 or any(char not in HEX64 for char in text):
        _fail(f"{location} must be a lowercase SHA-256 digest")
    return text


def _count(value: Any, location: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        _fail(f"{location} must be a positive integer")
    return value


def _sorted_unique_strings(value: Any, location: str, *, nonempty: bool = False) -> list[str]:
    items = [_string(item, f"{location}[{index}]") for index, item in enumerate(_array(value, location, nonempty=nonempty))]
    if items != sorted(set(items)):
        _fail(f"{location} must be sorted and unique")
    return items


def _relative_path(value: Any, location: str) -> str:
    text = _string(value, location)
    path = Path(text)
    if path.is_absolute() or ".." in path.parts or "\\" in text:
        _fail(f"{location} must be a normalized repository-relative path")
    if path.as_posix() != text or text.startswith("./"):
        _fail(f"{location} must use normalized forward slashes")
    return text


def _require_digest(actual: str, domain: str, payload: Any, location: str) -> None:
    expected = content_digest(domain, payload)
    if actual != expected:
        _fail(f"{location} mismatch: expected {expected}, got {actual}")


def _load_predecessor_authority(repository_root: Path, manifest: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
    authority = _object(
        manifest["predecessor_authority"],
        "predecessor_authority",
        {
            "inventory_path",
            "inventory_raw_sha256",
            "inventory_semantic_sha256",
            "ledger_path",
            "ledger_raw_sha256",
            "baseline_ids_sha256",
        },
    )
    expected = {
        "inventory_path": INVENTORY_PATH,
        "inventory_raw_sha256": FROZEN_INVENTORY_RAW_SHA256,
        "inventory_semantic_sha256": FROZEN_INVENTORY_SEMANTIC_SHA256,
        "ledger_path": LEDGER_PATH,
        "ledger_raw_sha256": FROZEN_LEDGER_RAW_SHA256,
        "baseline_ids_sha256": FROZEN_BASELINE_IDS_SHA256,
    }
    if authority != expected:
        _fail("predecessor_authority does not name the frozen V1 authority")
    inventory_path = repository_root / INVENTORY_PATH
    ledger_path = repository_root / LEDGER_PATH
    if file_sha256(inventory_path) != FROZEN_INVENTORY_RAW_SHA256:
        _fail("frozen V1 inventory raw hash mismatch")
    if file_sha256(ledger_path) != FROZEN_LEDGER_RAW_SHA256:
        _fail("frozen V1 ledger raw hash mismatch")
    inventory = load_json(inventory_path)
    ledger = load_json(ledger_path)
    if inventory.get("inventory_hash") != FROZEN_INVENTORY_SEMANTIC_SHA256:
        _fail("frozen V1 inventory semantic hash mismatch")
    return inventory, ledger


def _baseline_maps(inventory: dict[str, Any], ledger: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
    inventory_rows = inventory.get("tests")
    ledger_rows = ledger.get("rows")
    if not isinstance(inventory_rows, list) or not isinstance(ledger_rows, list):
        _fail("frozen V1 authority has invalid rows")
    inventory_map: dict[str, Any] = {}
    ledger_map: dict[str, Any] = {}
    for row in inventory_rows:
        baseline_id = row.get("baseline_id") if isinstance(row, dict) else None
        if not isinstance(baseline_id, str) or baseline_id in inventory_map:
            _fail("frozen V1 inventory has a missing or duplicate baseline_id")
        inventory_map[baseline_id] = row
    for row in ledger_rows:
        baseline_id = row.get("baseline_id") if isinstance(row, dict) else None
        if not isinstance(baseline_id, str) or baseline_id in ledger_map:
            _fail("frozen V1 ledger has a missing or duplicate baseline_id")
        ledger_map[baseline_id] = row
    if set(inventory_map) != set(ledger_map):
        _fail("frozen V1 inventory and ledger baseline sets differ")
    return inventory_map, ledger_map


def _load_current_v2_rows(repository_root: Path) -> dict[str, dict[str, Any]]:
    ledger = load_json(repository_root / V2_LEDGER_PATH)
    try:
        validate_test_replacement_ledger_v2(ledger)
    except InventoryV2ContractError as exc:
        _fail(f"current V2 replacement ledger is invalid: {exc}")
    rows: dict[str, dict[str, Any]] = {}
    for row in ledger["rows"]:
        baseline_id = row["baseline_id"]
        if baseline_id is None:
            continue
        if baseline_id in rows:
            _fail(f"current V2 replacement ledger has duplicate baseline_id: {baseline_id}")
        rows[baseline_id] = row
    return rows


IDENTITY_FIELDS = {
    "test_id",
    "framework",
    "native_id",
    "source_path",
    "test_route_id",
    "validation_id",
    "runner_selector_sha256",
    "executable_identity_sha256",
    "execution_input_contract_sha256",
    "platform_applicability_sha256",
}


def _load_successor_catalog(path: Path | None) -> dict[str, dict[str, Any]]:
    if path is None:
        return {}
    catalog = _object(load_json(path), "successor_catalog", {"format_id", "schema_version", "successors"})
    if catalog["format_id"] != SUCCESSOR_CATALOG_FORMAT_ID or catalog["schema_version"] != SCHEMA_VERSION:
        _fail("successor catalog format or schema version mismatch")
    result: dict[str, dict[str, Any]] = {}
    for index, item in enumerate(_array(catalog["successors"], "successor_catalog.successors")):
        identity = _object(item, f"successor_catalog.successors[{index}]", IDENTITY_FIELDS)
        replacement_id = _string(identity["test_id"], f"successor_catalog.successors[{index}].test_id")
        _relative_path(identity["source_path"], f"successor_catalog.successors[{index}].source_path")
        for field in IDENTITY_FIELDS - {"source_path", "test_id", "framework", "native_id", "test_route_id", "validation_id"}:
            _hash(identity[field], f"successor_catalog.successors[{index}].{field}")
        for field in {"framework", "native_id", "test_route_id", "validation_id"}:
            _string(identity[field], f"successor_catalog.successors[{index}].{field}")
        if replacement_id in result:
            _fail(f"duplicate successor catalog identity: {replacement_id}")
        result[replacement_id] = identity
    return result


def _validate_file_bindings(repository_root: Path, value: Any, location: str) -> list[dict[str, str]]:
    bindings: list[dict[str, str]] = []
    previous = ""
    for index, item in enumerate(_array(value, location, nonempty=True)):
        binding = _object(item, f"{location}[{index}]", {"path", "raw_sha256"})
        rel = _relative_path(binding["path"], f"{location}[{index}].path")
        digest = _hash(binding["raw_sha256"], f"{location}[{index}].raw_sha256")
        if rel <= previous:
            _fail(f"{location} must be sorted by unique path")
        previous = rel
        absolute = repository_root / rel
        if not absolute.is_file():
            _fail(f"{location}[{index}] path does not exist: {rel}")
        if file_sha256(absolute) != digest:
            _fail(f"{location}[{index}] file hash is stale: {rel}")
        bindings.append({"path": rel, "raw_sha256": digest})
    return bindings


def _validate_provenance(value: Any, location: str, actor_kind: str) -> dict[str, Any]:
    provenance = _object(
        value,
        location,
        {"actor_kind", "source_kind", "source_locator", "exact_text", "text_sha256"},
    )
    if provenance["actor_kind"] != actor_kind:
        _fail(f"{location}.actor_kind must be {actor_kind}")
    if provenance["source_kind"] not in {"conversation", "repository-instruction"}:
        _fail(f"{location}.source_kind is not allowed")
    for field in {"source_locator", "exact_text"}:
        _string(provenance[field], f"{location}.{field}")
    _require_digest(
        _hash(provenance["text_sha256"], f"{location}.text_sha256"),
        "kd4.replacement-admission.provenance-text.v1",
        provenance["exact_text"],
        f"{location}.text_sha256",
    )
    return provenance


def _validate_approval(value: Any, index: int) -> dict[str, Any]:
    location = f"approval_receipts[{index}]"
    approval = _object(
        value,
        location,
        {"approval_receipt_id", "admission_ids", "scope_sha256", "current_user", "reviewer", "receipt_sha256"},
    )
    admission_ids = _sorted_unique_strings(approval["admission_ids"], f"{location}.admission_ids", nonempty=True)
    _require_digest(
        _hash(approval["scope_sha256"], f"{location}.scope_sha256"),
        "kd4.replacement-admission.approval-scope.v1",
        admission_ids,
        f"{location}.scope_sha256",
    )
    _validate_provenance(approval["current_user"], f"{location}.current_user", "current-user")
    reviewer = _object(
        approval["reviewer"],
        f"{location}.reviewer",
        {"actor_kind", "reviewer_id", "source_kind", "source_locator", "verdict", "exact_text", "text_sha256"},
    )
    if reviewer["actor_kind"] != "root-reviewer" or reviewer["verdict"] != "approved":
        _fail(f"{location}.reviewer must record an approved root-reviewer decision")
    for field in {"reviewer_id", "source_locator", "exact_text"}:
        _string(reviewer[field], f"{location}.reviewer.{field}")
    if reviewer["source_kind"] not in {"conversation", "repository-instruction"}:
        _fail(f"{location}.reviewer.source_kind is not allowed")
    _require_digest(
        _hash(reviewer["text_sha256"], f"{location}.reviewer.text_sha256"),
        "kd4.replacement-admission.provenance-text.v1",
        reviewer["exact_text"],
        f"{location}.reviewer.text_sha256",
    )
    receipt_payload = {key: approval[key] for key in approval if key not in {"approval_receipt_id", "receipt_sha256"}}
    receipt_hash = _hash(approval["receipt_sha256"], f"{location}.receipt_sha256")
    _require_digest(receipt_hash, "kd4.replacement-admission.approval-receipt.v1", receipt_payload, f"{location}.receipt_sha256")
    if approval["approval_receipt_id"] != f"replacement-approval-v1.{receipt_hash}":
        _fail(f"{location}.approval_receipt_id does not match its receipt")
    return approval


def _parse_stage2_register(repository_root: Path) -> tuple[str, dict[str, str]]:
    path = repository_root / STAGE2_PATH
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as exc:
        _fail(f"cannot read Stage2 incorrect-behavior register: {exc}")
    if not lines or lines[0] != "KD4_STAGE2_INCORRECT_BEHAVIORS_V1":
        _fail("Stage2 incorrect-behavior register header mismatch")
    entries = lines[1:]
    sentinel = "NO_QUALIFYING_STAGE2_PRODUCT_BEHAVIOR_FIXES_PROVEN_YET"
    if entries == [sentinel]:
        return file_sha256(path), {}
    if sentinel in entries or entries != sorted(set(entries)):
        _fail("Stage2 incorrect-behavior entries must be sorted, unique, and exclude the empty sentinel")
    descriptions_by_id: dict[str, str] = {}
    for line in entries:
        parts = line.split("\t", 1)
        if len(parts) != 2 or not parts[0].startswith("stage2-incorrect-behavior-v1.") or not parts[1]:
            _fail("invalid Stage2 incorrect-behavior entry")
        behavior_id, description = parts
        if behavior_id in descriptions_by_id:
            _fail(f"duplicate Stage2 incorrect-behavior ID: {behavior_id}")
        descriptions_by_id[behavior_id] = description
    return file_sha256(path), descriptions_by_id


def _load_trusted_defect_receipts(path: Path | None) -> dict[str, dict[str, Any]]:
    if path is None:
        return {}
    receipts_by_hash: dict[str, dict[str, Any]] = {}
    receipt_hash_by_behavior_id: dict[str, str] = {}
    for index, raw in enumerate(_array(load_json(path), "trusted_defect_receipts")):
        try:
            validate_trusted_defect_receipt_v1(raw)
        except InventoryV2ContractError as exc:
            _fail(f"trusted_defect_receipts[{index}] is invalid: {exc}")
        receipt_hash = raw["receipt_sha256"]
        behavior_id = raw["defect_id"]
        if receipt_hash in receipts_by_hash:
            _fail(f"duplicate trusted defect receipt hash: {receipt_hash}")
        if behavior_id in receipt_hash_by_behavior_id:
            _fail(f"duplicate trusted defect receipt behavior ID: {behavior_id}")
        receipts_by_hash[receipt_hash] = raw
        receipt_hash_by_behavior_id[behavior_id] = receipt_hash
    return receipts_by_hash


def _validate_focused_receipt(value: Any, location: str, admission_id: str, replacement_id: str, seen: set[tuple[str, str, str]]) -> dict[str, Any]:
    fields = {
        "admission_id", "replacement_id", "report_path", "report_sha256", "attempt_id",
        "private_nonce_sha256", "exact_command", "repository_identity_sha256", "host_identity_sha256",
        "parent_process_sha256", "runner_identity_sha256", "child_runner_identities_sha256",
        "validation_id", "intended_count", "selected_count", "executed_count",
        "executed_validation_ids", "classification", "starting_workspace_fingerprint",
        "ending_workspace_fingerprint", "mutation_epoch", "selection_sha256",
        "resolved_inputs_sha256", "validation_execution_id", "eligibility_receipt_sha256", "receipt_sha256",
    }
    receipt = _object(value, location, fields)
    if receipt["admission_id"] != admission_id or receipt["replacement_id"] != replacement_id:
        _fail(f"{location} was copied from another admission or successor")
    report_path = _relative_path(receipt["report_path"], f"{location}.report_path")
    report_hash = _hash(receipt["report_sha256"], f"{location}.report_sha256")
    report = Path(report_path)
    # Reports may be private runtime artifacts outside the repository.  A relative
    # path is still required so later activation can resolve it under its private root.
    if report.is_absolute():
        _fail(f"{location}.report_path must be relative")
    for field in {
        "private_nonce_sha256", "repository_identity_sha256", "host_identity_sha256",
        "parent_process_sha256", "runner_identity_sha256", "child_runner_identities_sha256",
        "starting_workspace_fingerprint", "ending_workspace_fingerprint", "selection_sha256",
        "resolved_inputs_sha256", "eligibility_receipt_sha256",
    }:
        _hash(receipt[field], f"{location}.{field}")
    for field in {"attempt_id", "exact_command", "validation_id", "validation_execution_id", "mutation_epoch"}:
        _string(receipt[field], f"{location}.{field}")
    if receipt["classification"] != "confirmed-pass":
        _fail(f"{location}.classification must be confirmed-pass")
    intended = _count(receipt["intended_count"], f"{location}.intended_count")
    selected = _count(receipt["selected_count"], f"{location}.selected_count")
    executed = _count(receipt["executed_count"], f"{location}.executed_count")
    if not (intended == selected == executed):
        _fail(f"{location} intended, selected, and executed counts must match")
    executed_ids = _sorted_unique_strings(receipt["executed_validation_ids"], f"{location}.executed_validation_ids", nonempty=True)
    if executed_ids != [replacement_id] or executed != 1:
        _fail(f"{location} must prove exactly its bound successor")
    if receipt["starting_workspace_fingerprint"] != receipt["ending_workspace_fingerprint"]:
        _fail(f"{location} workspace changed during validation")
    replay_key = (receipt["attempt_id"], receipt["validation_execution_id"], replacement_id)
    if replay_key in seen:
        _fail(f"{location} replays a focused validation execution")
    seen.add(replay_key)
    payload = {key: receipt[key] for key in receipt if key != "receipt_sha256"}
    _require_digest(
        _hash(receipt["receipt_sha256"], f"{location}.receipt_sha256"),
        "kd4.replacement-admission.focused-validation-receipt.v1",
        payload,
        f"{location}.receipt_sha256",
    )
    # Structural validation cannot establish trusted issuance. The report digest
    # remains a mandatory activation input and is included in the signed payload.
    del report_hash
    return receipt


def validate_manifest(
    repository_root: Path,
    manifest_path: Path,
    successor_catalog_path: Path | None = None,
    trusted_defect_receipts_path: Path | None = None,
) -> dict[str, Any]:
    repository_root = repository_root.resolve()
    manifest = _object(
        load_json(manifest_path),
        "manifest",
        {"format_id", "schema_version", "predecessor_authority", "approval_receipts", "admissions", "semantic_sha256", "self_hash"},
    )
    if manifest["format_id"] != FORMAT_ID or manifest["schema_version"] != SCHEMA_VERSION:
        _fail("replacement admission manifest format or schema version mismatch")
    inventory, ledger = _load_predecessor_authority(repository_root, manifest)
    inventory_map, ledger_map = _baseline_maps(inventory, ledger)
    current_v2_rows = _load_current_v2_rows(repository_root)
    catalog = _load_successor_catalog(successor_catalog_path)
    trusted_defect_receipts = _load_trusted_defect_receipts(
        trusted_defect_receipts_path
    )

    approvals: dict[str, dict[str, Any]] = {}
    approval_scope_owners: dict[str, str] = {}
    for index, raw in enumerate(_array(manifest["approval_receipts"], "approval_receipts")):
        approval = _validate_approval(raw, index)
        approval_id = approval["approval_receipt_id"]
        if approval_id in approvals:
            _fail(f"duplicate approval receipt: {approval_id}")
        for admission_id in approval["admission_ids"]:
            if admission_id in approval_scope_owners:
                _fail(f"admission {admission_id} appears in multiple approval scopes")
            approval_scope_owners[admission_id] = approval_id
        approvals[approval_id] = approval

    admissions = _array(manifest["admissions"], "admissions")
    if admissions and successor_catalog_path is None:
        _fail("nonempty admissions require --successor-inventory")
    seen_admission_ids: set[str] = set()
    seen_baselines: set[str] = set()
    seen_successors: set[str] = set()
    seen_focused: set[tuple[str, str, str]] = set()
    seen_stage2_behavior_ids: set[str] = set()
    seen_stage2_receipt_hashes: set[str] = set()
    stage2_register_hash, stage2_descriptions = _parse_stage2_register(
        repository_root
    )

    for admission_index, raw in enumerate(admissions):
        location = f"admissions[{admission_index}]"
        admission = _object(
            raw,
            location,
            {
                "admission_id", "state", "approval_receipt_id", "baseline_bindings", "successors", "edges",
                "mapping_sha256", "product_behavior_obligation_sha256", "candidate_receipt_sha256", "acceptance",
            },
        )
        admission_id = _string(admission["admission_id"], f"{location}.admission_id")
        if admission_id in seen_admission_ids:
            _fail(f"duplicate admission_id: {admission_id}")
        seen_admission_ids.add(admission_id)
        if admission["state"] not in {"focused-candidate", "accepted"}:
            _fail(f"{location}.state is invalid")

        baselines: list[dict[str, Any]] = []
        baseline_ids: list[str] = []
        for index, item in enumerate(_array(admission["baseline_bindings"], f"{location}.baseline_bindings", nonempty=True)):
            binding = _object(item, f"{location}.baseline_bindings[{index}]", {"baseline_id", "inventory_entry_sha256", "ledger_row_sha256", "obligation_id"})
            baseline_id = _string(binding["baseline_id"], f"{location}.baseline_bindings[{index}].baseline_id")
            if baseline_id not in inventory_map:
                _fail(f"{location} names missing baseline: {baseline_id}")
            if baseline_id in seen_baselines:
                _fail(f"baseline appears in multiple admissions: {baseline_id}")
            seen_baselines.add(baseline_id)
            baseline_ids.append(baseline_id)
            _require_digest(_hash(binding["inventory_entry_sha256"], f"{location}.baseline_bindings[{index}].inventory_entry_sha256"), "kd4.replacement-admission.baseline-inventory-entry.v1", inventory_map[baseline_id], f"{location}.baseline_bindings[{index}].inventory_entry_sha256")
            _require_digest(_hash(binding["ledger_row_sha256"], f"{location}.baseline_bindings[{index}].ledger_row_sha256"), "kd4.replacement-admission.baseline-ledger-row.v1", ledger_map[baseline_id], f"{location}.baseline_bindings[{index}].ledger_row_sha256")
            obligation_id = _string(binding["obligation_id"], f"{location}.baseline_bindings[{index}].obligation_id")
            current_row = current_v2_rows.get(baseline_id)
            if current_row is None:
                _fail(f"{location}.baseline_bindings[{index}] baseline {baseline_id} is missing from the current V2 ledger")
            disposition_kind = current_row["disposition"]["kind"]
            if disposition_kind != "unresolved":
                _fail(
                    f"{location}.baseline_bindings[{index}] baseline {baseline_id} "
                    f"has current V2 disposition {disposition_kind}; only unresolved "
                    "baselines are admissible"
                )
            if obligation_id != current_row["obligation_id"]:
                _fail(
                    f"{location}.baseline_bindings[{index}].obligation_id does not "
                    f"match current V2 ledger row for {baseline_id}"
                )
            baselines.append(binding)
        if baseline_ids != sorted(set(baseline_ids)):
            _fail(f"{location}.baseline_bindings must be sorted by unique baseline_id")

        successors: list[dict[str, Any]] = []
        successor_ids: list[str] = []
        validation_by_successor: dict[str, str] = {}
        for index, item in enumerate(_array(admission["successors"], f"{location}.successors", nonempty=True)):
            successor = _object(item, f"{location}.successors[{index}]", {"replacement_id", "current_identity", "current_identity_sha256", "source_files", "runtime_path", "runtime_path_sha256", "contract_sources", "contract_sources_sha256", "validation_contract_sha256"})
            replacement_id = _string(successor["replacement_id"], f"{location}.successors[{index}].replacement_id")
            identity = _object(successor["current_identity"], f"{location}.successors[{index}].current_identity", IDENTITY_FIELDS)
            if identity["test_id"] != replacement_id or catalog.get(replacement_id) != identity:
                _fail(f"{location} successor is missing or differs from the current catalog: {replacement_id}")
            _require_digest(_hash(successor["current_identity_sha256"], f"{location}.successors[{index}].current_identity_sha256"), "kd4.replacement-admission.successor-identity.v1", identity, f"{location}.successors[{index}].current_identity_sha256")
            _validate_file_bindings(repository_root, successor["source_files"], f"{location}.successors[{index}].source_files")
            runtime_path = _sorted_unique_strings(successor["runtime_path"], f"{location}.successors[{index}].runtime_path", nonempty=True)
            _require_digest(_hash(successor["runtime_path_sha256"], f"{location}.successors[{index}].runtime_path_sha256"), "kd4.replacement-admission.runtime-path.v1", runtime_path, f"{location}.successors[{index}].runtime_path_sha256")
            contract_sources = _validate_file_bindings(repository_root, successor["contract_sources"], f"{location}.successors[{index}].contract_sources")
            _require_digest(_hash(successor["contract_sources_sha256"], f"{location}.successors[{index}].contract_sources_sha256"), "kd4.replacement-admission.contract-sources.v1", contract_sources, f"{location}.successors[{index}].contract_sources_sha256")
            _require_digest(_hash(successor["validation_contract_sha256"], f"{location}.successors[{index}].validation_contract_sha256"), "kd4.replacement-admission.validation-contract.v1", {"validation_id": identity["validation_id"], "runner_selector_sha256": identity["runner_selector_sha256"], "execution_input_contract_sha256": identity["execution_input_contract_sha256"]}, f"{location}.successors[{index}].validation_contract_sha256")
            if replacement_id in seen_successors:
                _fail(f"successor appears in multiple admissions: {replacement_id}")
            seen_successors.add(replacement_id)
            successor_ids.append(replacement_id)
            validation_by_successor[replacement_id] = identity["validation_id"]
            successors.append(successor)
        if successor_ids != sorted(set(successor_ids)):
            _fail(f"{location}.successors must be sorted by unique replacement_id")

        edges: list[dict[str, Any]] = []
        edge_pairs: set[tuple[str, str]] = set()
        baseline_degree = {item: 0 for item in baseline_ids}
        successor_degree = {item: 0 for item in successor_ids}
        for index, item in enumerate(_array(admission["edges"], f"{location}.edges", nonempty=True)):
            edge = _object(item, f"{location}.edges[{index}]", {"baseline_id", "replacement_id", "role", "preserved_behavior", "behavior_sha256"})
            pair = (edge["baseline_id"], edge["replacement_id"])
            if pair in edge_pairs or pair[0] not in baseline_degree or pair[1] not in successor_degree:
                _fail(f"{location}.edges[{index}] is duplicate or references an unbound endpoint")
            edge_pairs.add(pair)
            baseline_degree[pair[0]] += 1
            successor_degree[pair[1]] += 1
            if edge["role"] not in {"primary", "supplementary"}:
                _fail(f"{location}.edges[{index}].role is invalid")
            behavior = _string(edge["preserved_behavior"], f"{location}.edges[{index}].preserved_behavior")
            _require_digest(_hash(edge["behavior_sha256"], f"{location}.edges[{index}].behavior_sha256"), "kd4.replacement-admission.behavior.v1", behavior, f"{location}.edges[{index}].behavior_sha256")
            edges.append(edge)
        if list(edge_pairs) and [(edge["baseline_id"], edge["replacement_id"]) for edge in edges] != sorted(edge_pairs):
            _fail(f"{location}.edges must be sorted by baseline_id and replacement_id")
        if any(degree == 0 for degree in baseline_degree.values()) or any(degree == 0 for degree in successor_degree.values()):
            _fail(f"{location}.edges must connect every baseline and successor")

        mapping_payload = {"baseline_bindings": baselines, "successors": successors, "edges": edges}
        mapping_hash = _hash(admission["mapping_sha256"], f"{location}.mapping_sha256")
        _require_digest(mapping_hash, "kd4.replacement-admission.mapping.v1", mapping_payload, f"{location}.mapping_sha256")
        if admission_id != f"replacement-admission-v1.{mapping_hash}":
            _fail(f"{location}.admission_id does not match mapping")
        _require_digest(_hash(admission["product_behavior_obligation_sha256"], f"{location}.product_behavior_obligation_sha256"), "kd4.replacement-admission.product-behavior-obligation.v1", {"obligation_ids": sorted(item["obligation_id"] for item in baselines), "behavior_hashes": sorted(edge["behavior_sha256"] for edge in edges)}, f"{location}.product_behavior_obligation_sha256")

        approval_id = _string(admission["approval_receipt_id"], f"{location}.approval_receipt_id")
        approval = approvals.get(approval_id)
        if approval is None or approval_scope_owners.get(admission_id) != approval_id:
            _fail(f"{location} has no exact scope-bound approval")
        candidate_payload = {key: admission[key] for key in admission if key not in {"candidate_receipt_sha256", "acceptance"}}
        candidate_payload["approval_receipt_sha256"] = approval["receipt_sha256"]
        _require_digest(_hash(admission["candidate_receipt_sha256"], f"{location}.candidate_receipt_sha256"), "kd4.replacement-admission.candidate-receipt.v1", candidate_payload, f"{location}.candidate_receipt_sha256")

        if admission["state"] == "focused-candidate":
            if admission["acceptance"] is not None:
                _fail(f"{location}.acceptance must be null for focused-candidate")
            continue

        acceptance = _object(admission["acceptance"], f"{location}.acceptance", {"focused_validation_receipts", "stage2", "accepted_receipt_sha256"})
        focused = _array(acceptance["focused_validation_receipts"], f"{location}.acceptance.focused_validation_receipts", nonempty=True)
        focused_ids: list[str] = []
        for index, receipt in enumerate(focused):
            replacement_id = receipt.get("replacement_id") if isinstance(receipt, dict) else None
            if replacement_id not in validation_by_successor:
                _fail(f"{location}.acceptance has an extra focused receipt")
            checked = _validate_focused_receipt(receipt, f"{location}.acceptance.focused_validation_receipts[{index}]", admission_id, replacement_id, seen_focused)
            if checked["validation_id"] != validation_by_successor[replacement_id]:
                _fail(f"{location}.acceptance focused validation_id mismatch")
            focused_ids.append(replacement_id)
        if focused_ids != successor_ids:
            _fail(f"{location}.acceptance must contain exactly one sorted receipt per successor")

        stage2 = _object(acceptance["stage2"], f"{location}.acceptance.stage2", {"disposition", "register_path", "register_raw_sha256", "behavior_ids", "defect_receipt_sha256s"})
        if stage2["register_path"] != STAGE2_PATH or stage2["register_raw_sha256"] != stage2_register_hash:
            _fail(f"{location}.acceptance Stage2 register binding is stale")
        behavior_ids = _sorted_unique_strings(stage2["behavior_ids"], f"{location}.acceptance.stage2.behavior_ids")
        defect_hashes = _sorted_unique_strings(stage2["defect_receipt_sha256s"], f"{location}.acceptance.stage2.defect_receipt_sha256s")
        for index, digest in enumerate(defect_hashes):
            _hash(digest, f"{location}.acceptance.stage2.defect_receipt_sha256s[{index}]")
        if stage2["disposition"] == "reviewed-no-incorrect-behavior":
            if behavior_ids or defect_hashes:
                _fail(f"{location}.acceptance no-defect disposition must have empty evidence")
        elif stage2["disposition"] == "incorrect-behavior-fixed":
            if not behavior_ids or not defect_hashes:
                _fail(f"{location}.acceptance defect disposition lacks registered evidence")
            receipts: list[dict[str, Any]] = []
            for digest in defect_hashes:
                receipt = trusted_defect_receipts.get(digest)
                if receipt is None:
                    _fail(
                        f"{location}.acceptance trusted defect receipt {digest} "
                        "is missing from --trusted-defect-receipts"
                    )
                if digest in seen_stage2_receipt_hashes:
                    _fail(f"trusted defect receipt appears in multiple admissions: {digest}")
                receipts.append(receipt)
            receipt_behavior_ids = sorted(receipt["defect_id"] for receipt in receipts)
            if behavior_ids != receipt_behavior_ids:
                _fail(
                    f"{location}.acceptance behavior IDs do not exactly match "
                    "the bound trusted defect receipts"
                )
            expected_baseline_ids = sorted(baseline_ids)
            expected_obligation_ids = sorted(
                binding["obligation_id"] for binding in baselines
            )
            for receipt in receipts:
                behavior_id = receipt["defect_id"]
                if behavior_id in seen_stage2_behavior_ids:
                    _fail(
                        f"Stage2 incorrect-behavior ID appears in multiple admissions: "
                        f"{behavior_id}"
                    )
                description = stage2_descriptions.get(behavior_id)
                if description is None:
                    _fail(
                        f"{location}.acceptance behavior ID is not registered: "
                        f"{behavior_id}"
                    )
                if description != receipt["incorrect_behavior"]:
                    _fail(
                        f"Stage2 incorrect-behavior description does not exactly "
                        f"match trusted receipt {behavior_id}"
                    )
                if receipt["baseline_ids"] != expected_baseline_ids:
                    _fail(
                        f"trusted defect receipt {behavior_id} does not exactly bind "
                        f"the admission baseline IDs"
                    )
                if receipt["baseline_obligation_ids"] != expected_obligation_ids:
                    _fail(
                        f"trusted defect receipt {behavior_id} does not exactly bind "
                        f"the admission obligation IDs"
                    )
                seen_stage2_behavior_ids.add(behavior_id)
                seen_stage2_receipt_hashes.add(receipt["receipt_sha256"])
        else:
            _fail(f"{location}.acceptance Stage2 disposition is invalid")
        accepted_payload = {"admission_id": admission_id, "candidate_receipt_sha256": admission["candidate_receipt_sha256"], "focused_validation_receipts": focused, "stage2": stage2}
        _require_digest(_hash(acceptance["accepted_receipt_sha256"], f"{location}.acceptance.accepted_receipt_sha256"), "kd4.replacement-admission.accepted-receipt.v1", accepted_payload, f"{location}.acceptance.accepted_receipt_sha256")

    if set(approval_scope_owners) != seen_admission_ids:
        missing = seen_admission_ids - set(approval_scope_owners)
        extra = set(approval_scope_owners) - seen_admission_ids
        _fail(f"approval scope does not close over admissions (missing={sorted(missing)}, extra={sorted(extra)})")
    if set(approvals) != set(approval_scope_owners.values()):
        _fail("unused approval receipt is forbidden")
    if set(stage2_descriptions) != seen_stage2_behavior_ids:
        missing = sorted(set(stage2_descriptions) - seen_stage2_behavior_ids)
        extra = sorted(seen_stage2_behavior_ids - set(stage2_descriptions))
        _fail(
            "Stage2 incorrect-behavior register does not close over accepted "
            f"unresolved-baseline admissions (missing={missing}, extra={extra})"
        )
    trusted_receipt_hashes = set(trusted_defect_receipts)
    if trusted_receipt_hashes != seen_stage2_receipt_hashes:
        missing = sorted(seen_stage2_receipt_hashes - trusted_receipt_hashes)
        extra = sorted(trusted_receipt_hashes - seen_stage2_receipt_hashes)
        _fail(
            "trusted defect receipt collection does not close over accepted "
            f"Stage2 incorrect behaviors (missing={missing}, extra={extra})"
        )

    semantic_payload = {key: manifest[key] for key in manifest if key not in {"semantic_sha256", "self_hash"}}
    semantic_hash = _hash(manifest["semantic_sha256"], "semantic_sha256")
    _require_digest(semantic_hash, "kd4.replacement-admission.manifest-semantic.v1", semantic_payload, "semantic_sha256")
    self_payload = {key: manifest[key] for key in manifest if key != "self_hash"}
    _require_digest(_hash(manifest["self_hash"], "self_hash"), "kd4.replacement-admission.manifest-self.v1", self_payload, "self_hash")
    return manifest


def materialize_projection(manifest: dict[str, Any]) -> dict[str, Any]:
    if manifest["admissions"]:
        _fail(
            "nonempty replacement admission materialization requires trusted "
            "in-process authority derived from the current user or a "
            "session-admitted repository instruction"
        )
    mappings: list[dict[str, Any]] = []
    for admission in manifest["admissions"]:
        by_baseline: dict[str, list[dict[str, str]]] = {
            item["baseline_id"]: [] for item in admission["baseline_bindings"]
        }
        for edge in admission["edges"]:
            by_baseline[edge["baseline_id"]].append(
                {
                    "replacement_id": edge["replacement_id"],
                    "role": edge["role"],
                    "preserved_behavior": edge["preserved_behavior"],
                    "behavior_sha256": edge["behavior_sha256"],
                }
            )
        validation_ids = {
            item["replacement_id"]: item["current_identity"]["validation_id"]
            for item in admission["successors"]
        }
        for baseline_id, edges in by_baseline.items():
            mappings.append(
                {
                    "baseline_id": baseline_id,
                    "admission_id": admission["admission_id"],
                    "state": admission["state"],
                    "successors": [
                        {**edge, "validation_id": validation_ids[edge["replacement_id"]]}
                        for edge in sorted(edges, key=lambda item: item["replacement_id"])
                    ],
                }
            )
    mappings.sort(key=lambda item: item["baseline_id"])
    payload = {
        "format_id": PROJECTION_FORMAT_ID,
        "schema_version": SCHEMA_VERSION,
        "source_manifest_semantic_sha256": manifest["semantic_sha256"],
        "mappings": mappings,
    }
    payload["projection_sha256"] = content_digest("kd4.replacement-admission.projection.v1", payload)
    return payload


def _historical_replacement_review_plan_v1(graph: dict[str, Any]) -> dict[str, Any]:
    actual_counts = (
        len(graph["baseline_ids"]),
        len(graph["edges"]),
        len(graph["successor_ids"]),
        len(graph["components"]),
    )
    expected_counts = (
        FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT,
    )
    if actual_counts != expected_counts:
        _fail(
            "historical replacement graph counts do not match the exact "
            "frozen V1 authority"
        )
    review_scopes = []
    for component in graph["components"]:
        review_scope_sha256 = proof_hash(
            "kd4.historical-replacement-review-scope.v1", component
        )
        review_scopes.append(
            {
                **component,
                "review_scope_id": f"historical-replacement-review-v1.{review_scope_sha256}",
                "review_scope_sha256": review_scope_sha256,
            }
        )
    payload = {
        "format_id": HISTORICAL_REVIEW_PLAN_FORMAT_ID,
        "schema_version": SCHEMA_VERSION,
        "frozen_graph_sha256": FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256,
        "baseline_count": FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT,
        "edge_count": FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT,
        "successor_count": FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT,
        "review_scope_count": FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT,
        "review_scopes": review_scopes,
    }
    payload["review_plan_sha256"] = proof_hash(
        "kd4.historical-replacement-review-plan.v1", payload
    )
    return payload


def validate_historical_replacement_review_plan_intrinsic_v1(
    plan: Any,
) -> dict[str, Any]:
    plan = _object(
        plan,
        "historical_review_plan",
        {
            "format_id",
            "schema_version",
            "frozen_graph_sha256",
            "baseline_count",
            "edge_count",
            "successor_count",
            "review_scope_count",
            "review_scopes",
            "review_plan_sha256",
        },
    )
    if plan["format_id"] != HISTORICAL_REVIEW_PLAN_FORMAT_ID:
        _fail("historical review plan format or schema version mismatch")
    if (
        not isinstance(plan["schema_version"], int)
        or isinstance(plan["schema_version"], bool)
        or plan["schema_version"] != SCHEMA_VERSION
    ):
        _fail("historical review plan format or schema version mismatch")
    if (
        _hash(plan["frozen_graph_sha256"], "historical_review_plan.frozen_graph_sha256")
        != FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256
    ):
        _fail("historical review plan frozen graph hash mismatch")

    components: list[dict[str, Any]] = []
    all_baseline_ids: list[str] = []
    all_edges: list[dict[str, str]] = []
    all_successor_ids: list[str] = []
    scopes = _array(
        plan["review_scopes"], "historical_review_plan.review_scopes", nonempty=True
    )
    for index, raw in enumerate(scopes):
        location = f"historical_review_plan.review_scopes[{index}]"
        scope = _object(
            raw,
            location,
            {
                "baseline_ids",
                "edges",
                "successor_ids",
                "review_scope_id",
                "review_scope_sha256",
            },
        )
        baseline_ids = _sorted_unique_strings(
            scope["baseline_ids"], f"{location}.baseline_ids", nonempty=True
        )
        successor_ids = _sorted_unique_strings(
            scope["successor_ids"], f"{location}.successor_ids", nonempty=True
        )
        edges: list[dict[str, str]] = []
        edge_pairs: list[tuple[str, str]] = []
        for edge_index, raw_edge in enumerate(
            _array(scope["edges"], f"{location}.edges", nonempty=True)
        ):
            edge_location = f"{location}.edges[{edge_index}]"
            edge = _object(
                raw_edge, edge_location, {"baseline_id", "replacement_id"}
            )
            baseline_id = _string(
                edge["baseline_id"], f"{edge_location}.baseline_id"
            )
            replacement_id = _string(
                edge["replacement_id"], f"{edge_location}.replacement_id"
            )
            if baseline_id not in baseline_ids or replacement_id not in successor_ids:
                _fail(f"{edge_location} references an unbound scope endpoint")
            edge_pairs.append((baseline_id, replacement_id))
            edges.append(
                {
                    "baseline_id": baseline_id,
                    "replacement_id": replacement_id,
                }
            )
        if edge_pairs != sorted(set(edge_pairs)):
            _fail(f"{location}.edges must be sorted and unique")
        if (
            {edge["baseline_id"] for edge in edges} != set(baseline_ids)
            or {edge["replacement_id"] for edge in edges} != set(successor_ids)
        ):
            _fail(f"{location}.edges do not connect every scope endpoint")
        component = {
            "baseline_ids": baseline_ids,
            "edges": edges,
            "successor_ids": successor_ids,
        }
        scope_hash = proof_hash(
            "kd4.historical-replacement-review-scope.v1", component
        )
        declared_scope_hash = _hash(
            scope["review_scope_sha256"], f"{location}.review_scope_sha256"
        )
        declared_scope_id = _string(
            scope["review_scope_id"], f"{location}.review_scope_id"
        )
        if declared_scope_hash != scope_hash:
            _fail(f"{location}.review_scope_sha256 mismatch")
        if declared_scope_id != f"historical-replacement-review-v1.{scope_hash}":
            _fail(f"{location}.review_scope_id mismatch")
        components.append(component)
        all_baseline_ids.extend(baseline_ids)
        all_edges.extend(edges)
        all_successor_ids.extend(successor_ids)

    if [component["baseline_ids"] for component in components] != sorted(
        component["baseline_ids"] for component in components
    ):
        _fail("historical review plan scopes must be sorted by baseline IDs")
    if (
        len(all_baseline_ids) != len(set(all_baseline_ids))
        or len(all_successor_ids) != len(set(all_successor_ids))
    ):
        _fail("historical review plan scopes must be endpoint-disjoint")
    baseline_ids = sorted(all_baseline_ids)
    successor_ids = sorted(all_successor_ids)
    edges = sorted(
        all_edges, key=lambda edge: (edge["baseline_id"], edge["replacement_id"])
    )
    if len(edges) != len(
        {(edge["baseline_id"], edge["replacement_id"]) for edge in edges}
    ):
        _fail("historical review plan scopes must contain globally unique edges")

    actual_counts = (
        len(all_baseline_ids),
        len(edges),
        len(all_successor_ids),
        len(components),
    )
    expected_counts = (
        FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT,
    )
    declared_counts = (
        plan["baseline_count"],
        plan["edge_count"],
        plan["successor_count"],
        plan["review_scope_count"],
    )
    if actual_counts != expected_counts or declared_counts != expected_counts:
        _fail("historical review plan counts do not close over its exact scopes")
    graph_projection = {
        "baseline_ids": baseline_ids,
        "components": components,
        "edges": edges,
        "successor_ids": successor_ids,
    }
    if (
        proof_hash(
            "kd4.frozen-v1-historical-replacement-graph.v1", graph_projection
        )
        != FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256
    ):
        _fail("historical review plan scopes do not match the exact frozen V1 graph")
    plan_projection = {
        key: plan[key] for key in plan if key != "review_plan_sha256"
    }
    declared_plan_hash = _hash(
        plan["review_plan_sha256"], "historical_review_plan.review_plan_sha256"
    )
    if declared_plan_hash != proof_hash(
        "kd4.historical-replacement-review-plan.v1", plan_projection
    ):
        _fail("historical review plan self-hash mismatch")
    return plan


def validate_historical_replacement_review_plan_v1(
    plan: Any,
    predecessor_ledger: Any,
    current_v2_ledger: Any,
) -> dict[str, Any]:
    validate_historical_replacement_review_plan_intrinsic_v1(plan)
    try:
        validate_test_replacement_ledger_v2(current_v2_ledger)
        validate_v2_historical_replacement_graph_closure_v1(
            current_v2_ledger, predecessor_ledger
        )
        graph = derive_frozen_v1_historical_replacement_graph_v1(
            predecessor_ledger
        )
    except InventoryV2ContractError as exc:
        _fail(f"historical replacement authority is invalid: {exc}")
    expected = _historical_replacement_review_plan_v1(graph)
    if plan != expected:
        _fail(
            "historical replacement review plan does not exactly close over "
            "the frozen V1 connected components"
        )
    return expected


def compile_historical_replacement_review_plan(
    repository_root: Path,
) -> dict[str, Any]:
    predecessor_path = repository_root / LEDGER_PATH
    if file_sha256(predecessor_path) != FROZEN_LEDGER_RAW_SHA256:
        _fail("frozen V1 ledger raw hash mismatch")
    predecessor_ledger = load_json(predecessor_path)
    current_v2_ledger = load_json(repository_root / V2_LEDGER_PATH)
    try:
        graph = derive_frozen_v1_historical_replacement_graph_v1(
            predecessor_ledger
        )
    except InventoryV2ContractError as exc:
        _fail(f"historical replacement authority is invalid: {exc}")
    plan = _historical_replacement_review_plan_v1(graph)
    return validate_historical_replacement_review_plan_v1(
        plan, predecessor_ledger, current_v2_ledger
    )


def _focused_replacement_approval_receipt_ref_v1(
    receipt: Any,
) -> dict[str, Any]:
    try:
        validate_focused_replacement_approval_receipt_v1(receipt)
    except FocusedReplacementApprovalReceiptError as exc:
        _fail(f"focused replacement approval receipt is invalid: {exc}")
    return {
        "format_id": FOCUSED_REPLACEMENT_APPROVAL_RECEIPT_FORMAT_ID,
        "schema_version": SCHEMA_VERSION,
        "attempt_id": receipt["attempt_id"],
        "focused_validation_id": receipt["focused_validation_id"],
        "receipt_sha256": receipt["receipt_sha256"],
    }


def _validate_historical_scope_reviews_v1(
    review_plan: dict[str, Any], scope_reviews: Any
) -> list[dict[str, str]]:
    expected_by_id = {
        scope["review_scope_id"]: {
            "review_scope_id": scope["review_scope_id"],
            "review_scope_sha256": scope["review_scope_sha256"],
            "disposition": HISTORICAL_SCOPE_REVIEW_DISPOSITION,
        }
        for scope in review_plan["review_scopes"]
    }
    reviews: list[dict[str, str]] = []
    for index, raw in enumerate(
        _array(scope_reviews, "scope_reviews", nonempty=True)
    ):
        location = f"scope_reviews[{index}]"
        review = _object(
            raw,
            location,
            {"review_scope_id", "review_scope_sha256", "disposition"},
        )
        scope_id = _string(review["review_scope_id"], f"{location}.review_scope_id")
        scope_hash = _hash(
            review["review_scope_sha256"], f"{location}.review_scope_sha256"
        )
        if scope_id != f"historical-replacement-review-v1.{scope_hash}":
            _fail(f"{location} ID does not match its scope hash")
        if review["disposition"] != HISTORICAL_SCOPE_REVIEW_DISPOSITION:
            _fail(
                f"{location}.disposition must be "
                f"{HISTORICAL_SCOPE_REVIEW_DISPOSITION} during Stage 1"
            )
        reviews.append(
            {
                "review_scope_id": scope_id,
                "review_scope_sha256": scope_hash,
                "disposition": HISTORICAL_SCOPE_REVIEW_DISPOSITION,
            }
        )
    review_ids = [review["review_scope_id"] for review in reviews]
    if review_ids != sorted(set(review_ids)):
        _fail("scope_reviews must be sorted by unique review_scope_id")
    actual_by_id = {review["review_scope_id"]: review for review in reviews}
    if actual_by_id != expected_by_id:
        missing = sorted(set(expected_by_id) - set(actual_by_id))
        extra = sorted(set(actual_by_id) - set(expected_by_id))
        rewired = sorted(
            scope_id
            for scope_id in set(expected_by_id) & set(actual_by_id)
            if expected_by_id[scope_id] != actual_by_id[scope_id]
        )
        _fail(
            "scope_reviews do not exactly close over the historical review plan "
            f"(missing={missing}, extra={extra}, rewired={rewired})"
        )
    return reviews


def build_historical_replacement_acceptance_proposal_v1(
    review_plan: dict[str, Any],
    focused_replacement_approval_receipt: Any,
    scope_reviews: Any,
) -> dict[str, Any]:
    review_plan = validate_historical_replacement_review_plan_intrinsic_v1(
        review_plan
    )
    expected_counts = (
        FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT,
        FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT,
    )
    actual_counts = (
        review_plan.get("baseline_count"),
        review_plan.get("edge_count"),
        review_plan.get("successor_count"),
        review_plan.get("review_scope_count"),
    )
    if actual_counts != expected_counts:
        _fail("historical acceptance proposal review-plan counts are not exact")
    review_plan_hash = _hash(
        review_plan.get("review_plan_sha256"), "review_plan.review_plan_sha256"
    )
    receipt_ref = _focused_replacement_approval_receipt_ref_v1(
        focused_replacement_approval_receipt
    )
    reviews = _validate_historical_scope_reviews_v1(review_plan, scope_reviews)
    payload = {
        "format_id": HISTORICAL_ACCEPTANCE_PROPOSAL_FORMAT_ID,
        "schema_version": SCHEMA_VERSION,
        "frozen_graph_sha256": FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256,
        "review_plan_sha256": review_plan_hash,
        "baseline_count": expected_counts[0],
        "edge_count": expected_counts[1],
        "successor_count": expected_counts[2],
        "review_scope_count": expected_counts[3],
        "focused_replacement_approval_receipt_ref": receipt_ref,
        "scope_reviews": reviews,
        "scope_review_set_sha256": proof_hash(
            "kd4.historical-replacement-scope-review-set.v1", reviews
        ),
        # A caller-authored proposal and a receipt self-hash are never activation
        # authority. Core must supply and persist that authority in-process later.
        "activation_authority": None,
    }
    payload["proposal_sha256"] = proof_hash(
        HISTORICAL_ACCEPTANCE_PROPOSAL_FORMAT_ID, payload
    )
    return payload


def validate_historical_replacement_acceptance_proposal_v1(
    proposal: Any,
    repository_root: Path,
    focused_replacement_approval_receipt: Any,
) -> dict[str, Any]:
    proposal = _object(
        proposal,
        "historical_acceptance_proposal",
        {
            "format_id",
            "schema_version",
            "frozen_graph_sha256",
            "review_plan_sha256",
            "baseline_count",
            "edge_count",
            "successor_count",
            "review_scope_count",
            "focused_replacement_approval_receipt_ref",
            "scope_reviews",
            "scope_review_set_sha256",
            "activation_authority",
            "proposal_sha256",
        },
    )
    exact_plan = compile_historical_replacement_review_plan(repository_root)
    expected = build_historical_replacement_acceptance_proposal_v1(
        exact_plan,
        focused_replacement_approval_receipt,
        proposal["scope_reviews"],
    )
    if proposal != expected:
        _fail(
            "historical acceptance proposal does not exactly bind its review "
            "plan, focused approval receipt, and scope-review set"
        )
    return expected


def compile_historical_replacement_acceptance_proposal(
    repository_root: Path,
    review_plan_path: Path,
    focused_replacement_approval_receipt_path: Path,
    scope_reviews_path: Path,
) -> dict[str, Any]:
    exact_plan = compile_historical_replacement_review_plan(repository_root)
    supplied_plan = load_json(review_plan_path)
    if supplied_plan != exact_plan:
        _fail(
            "historical acceptance review plan or review-plan hash does not "
            "match the exact current historical review plan"
        )
    focused_receipt = load_json(focused_replacement_approval_receipt_path)
    scope_reviews = load_json(scope_reviews_path)
    proposal = build_historical_replacement_acceptance_proposal_v1(
        exact_plan, focused_receipt, scope_reviews
    )
    return validate_historical_replacement_acceptance_proposal_v1(
        proposal, repository_root, focused_receipt
    )


def materialize_historical_replacement_acceptance_proposal(
    proposal: dict[str, Any],
) -> NoReturn:
    del proposal
    _fail(
        "historical replacement acceptance materialization requires a trusted "
        "in-process Core capability"
    )


def _write_atomic(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
            json.dump(value, handle, ensure_ascii=False, allow_nan=False, indent=2, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for name in ("check", "materialize"):
        child = subparsers.add_parser(name)
        child.add_argument("--repository-root", type=Path, required=True)
        child.add_argument("--manifest", type=Path, required=True)
        child.add_argument("--successor-inventory", type=Path)
        child.add_argument("--trusted-defect-receipts", type=Path)
        if name == "materialize":
            child.add_argument("--output", type=Path, required=True)
    review = subparsers.add_parser("review-historical")
    review.add_argument("--repository-root", type=Path, required=True)
    review.add_argument("--output", type=Path, required=True)
    for name in (
        "check-historical-acceptance",
        "materialize-historical-acceptance",
    ):
        historical = subparsers.add_parser(name)
        historical.add_argument("--repository-root", type=Path, required=True)
        historical.add_argument("--review-plan", type=Path, required=True)
        historical.add_argument(
            "--focused-approval-receipt", type=Path, required=True
        )
        historical.add_argument("--scope-reviews", type=Path, required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "review-historical":
            plan = compile_historical_replacement_review_plan(
                args.repository_root
            )
            _write_atomic(args.output, plan)
            print(
                "replacement-admissions: ok historical-review-scopes="
                f"{plan['review_scope_count']}"
            )
            return 0
        if args.command in {
            "check-historical-acceptance",
            "materialize-historical-acceptance",
        }:
            proposal = compile_historical_replacement_acceptance_proposal(
                args.repository_root,
                args.review_plan,
                args.focused_approval_receipt,
                args.scope_reviews,
            )
            if args.command == "materialize-historical-acceptance":
                materialize_historical_replacement_acceptance_proposal(proposal)
            print(
                "replacement-admissions: ok historical-acceptance-proposal "
                f"baselines={proposal['baseline_count']} "
                f"edges={proposal['edge_count']} "
                f"successors={proposal['successor_count']} "
                f"scopes={proposal['review_scope_count']} "
                f"proposal={proposal['proposal_sha256']} "
                "authority=structural-only"
            )
            return 0
        manifest = validate_manifest(
            args.repository_root,
            args.manifest,
            args.successor_inventory,
            args.trusted_defect_receipts,
        )
        if args.command == "materialize":
            _write_atomic(args.output, materialize_projection(manifest))
        print(f"replacement-admissions: ok admissions={len(manifest['admissions'])}")
        return 0
    except (AdmissionError, OSError) as exc:
        print(f"replacement-admissions: error: {exc}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
