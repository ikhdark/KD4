"""Strict dormant contract for focused replacement approval receipts.

The receipt digest establishes only the receipt's internal integrity.  Approval
authority comes from an independently trusted current-context value supplied by
the runtime to :func:`validate_against_trusted_current_context_v1`.
"""

from __future__ import annotations

import json
import re
import uuid
from typing import Any, Mapping

if __package__:
    from scripts.completion_proof_canonical import CanonicalJcsError
    from scripts.completion_proof_canonical import canonical_jcs
    from scripts.completion_proof_canonical import proof_hash
else:
    from completion_proof_canonical import CanonicalJcsError  # type: ignore[no-redef]
    from completion_proof_canonical import canonical_jcs  # type: ignore[no-redef]
    from completion_proof_canonical import proof_hash  # type: ignore[no-redef]


class FocusedReplacementApprovalReceiptError(ValueError):
    """Raised when a focused replacement approval receipt is not trustworthy."""


FORMAT_ID = "kd4.focused-replacement-approval-receipt.v1"
SCHEMA_VERSION = 1
CLASSIFICATION = "confirmed-pass"
FOCUSED_VALIDATION_ID = "inventory.frozen-reconciliation"
TRANSITION_VALIDATION_ID = "inventory.transition-readiness"
RECEIPT_HASH_DOMAIN = FORMAT_ID
MAX_EXACT_JSON_INTEGER = 2**53 - 1

RECEIPT_FIELDS = (
    "format_id",
    "schema_version",
    "attempt_id",
    "focused_validation_id",
    "classification",
    "frozen_inventory_hash",
    "focused_inventory_catalog_semantic_sha256",
    "inventory_discovery_processes_sha256",
    "policy_id",
    "policy_runner_bundle_sha256",
    "workspace_fingerprint",
    "mutation_epoch",
    "receipt_sha256",
)

# This explicit projection is part of the wire contract.  Keep it closed rather
# than deriving it by removing an arbitrary key from caller-controlled input.
RECEIPT_DIGEST_FIELDS = (
    "format_id",
    "schema_version",
    "attempt_id",
    "focused_validation_id",
    "classification",
    "frozen_inventory_hash",
    "focused_inventory_catalog_semantic_sha256",
    "inventory_discovery_processes_sha256",
    "policy_id",
    "policy_runner_bundle_sha256",
    "workspace_fingerprint",
    "mutation_epoch",
)

TRUSTED_CURRENT_CONTEXT_FIELDS = RECEIPT_DIGEST_FIELDS

_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_IDENTIFIER_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_UUID_V7_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)
_HASH_FIELDS = (
    "frozen_inventory_hash",
    "focused_inventory_catalog_semantic_sha256",
    "inventory_discovery_processes_sha256",
    "policy_runner_bundle_sha256",
    "workspace_fingerprint",
    "receipt_sha256",
)


def _contract_error(message: str) -> FocusedReplacementApprovalReceiptError:
    return FocusedReplacementApprovalReceiptError(
        f"FocusedReplacementApprovalReceiptV1 {message}"
    )


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise _contract_error(f"contains duplicate JSON key {key!r}")
        result[key] = value
    return result


def _reject_nonfinite_constant(value: str) -> None:
    raise _contract_error(f"contains forbidden non-finite JSON constant {value}")


def _require_exact_object(
    value: Any, expected_fields: tuple[str, ...], label: str
) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise _contract_error(f"{label} must be an object")
    if any(not isinstance(key, str) for key in value):
        raise _contract_error(f"{label} keys must be strings")
    actual = set(value)
    expected = set(expected_fields)
    if actual != expected:
        missing = sorted(expected - actual)
        unknown = sorted(actual - expected)
        raise _contract_error(
            f"{label} has unknown or missing fields "
            f"(missing={missing!r}, unknown={unknown!r})"
        )
    return value


def _require_sha256(value: Any, label: str) -> str:
    if not isinstance(value, str) or _SHA256_RE.fullmatch(value) is None:
        raise _contract_error(f"{label} must be lowercase 64-character SHA-256 hex")
    return value


def _require_identifier(value: Any, label: str) -> str:
    if not isinstance(value, str) or _IDENTIFIER_RE.fullmatch(value) is None:
        raise _contract_error(f"{label} must be a strict identifier")
    return value


def _require_uuid_v7(value: Any, label: str) -> str:
    if not isinstance(value, str) or _UUID_V7_RE.fullmatch(value) is None:
        raise _contract_error(f"{label} must be a canonical lowercase UUIDv7")
    try:
        parsed = uuid.UUID(value)
    except ValueError as error:
        raise _contract_error(f"{label} must be a canonical lowercase UUIDv7") from error
    if str(parsed) != value or parsed.version != 7 or parsed.variant != uuid.RFC_4122:
        raise _contract_error(f"{label} must be a canonical lowercase UUIDv7")
    return value


def _require_epoch(value: Any, label: str) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 0
        or value > MAX_EXACT_JSON_INTEGER
    ):
        raise _contract_error(
            f"{label} must be an integer from 0 through {MAX_EXACT_JSON_INTEGER}"
        )
    return value


def focused_replacement_approval_receipt_digest_projection_v1(
    receipt: Mapping[str, Any],
) -> dict[str, Any]:
    """Return the closed twelve-field projection committed by receipt_sha256."""

    _require_exact_object(receipt, RECEIPT_FIELDS, "receipt")
    return {field: receipt[field] for field in RECEIPT_DIGEST_FIELDS}


def focused_replacement_approval_receipt_digest_v1(
    receipt: Mapping[str, Any],
) -> str:
    """Compute the domain-separated digest over the explicit receipt projection."""

    projection = focused_replacement_approval_receipt_digest_projection_v1(receipt)
    try:
        return proof_hash(RECEIPT_HASH_DOMAIN, projection)
    except CanonicalJcsError as error:
        raise _contract_error(f"digest projection is not canonical JCS: {error}") from error


def validate_focused_replacement_approval_receipt_v1(receipt: Any) -> None:
    """Validate closed wire shape and intrinsic digest integrity only.

    Passing this function does not establish approval authority.  Callers must
    also bind the receipt to independently trusted current runtime context.
    """

    receipt = _require_exact_object(receipt, RECEIPT_FIELDS, "receipt")
    if receipt["format_id"] != FORMAT_ID:
        raise _contract_error(f"format_id must be {FORMAT_ID!r}")
    if (
        isinstance(receipt["schema_version"], bool)
        or receipt["schema_version"] != SCHEMA_VERSION
    ):
        raise _contract_error(f"schema_version must be {SCHEMA_VERSION}")
    _require_uuid_v7(receipt["attempt_id"], "attempt_id")
    if receipt["focused_validation_id"] not in {FOCUSED_VALIDATION_ID, TRANSITION_VALIDATION_ID}:
        raise _contract_error(
            "focused_validation_id must name frozen reconciliation or transition readiness"
        )
    if receipt["classification"] != CLASSIFICATION:
        raise _contract_error(f"classification must be {CLASSIFICATION!r}")
    _require_identifier(receipt["policy_id"], "policy_id")
    _require_epoch(receipt["mutation_epoch"], "mutation_epoch")
    for field in _HASH_FIELDS:
        _require_sha256(receipt[field], field)
    expected_digest = focused_replacement_approval_receipt_digest_v1(receipt)
    if receipt["receipt_sha256"] != expected_digest:
        raise _contract_error("receipt_sha256 does not match its digest projection")


def parse_focused_replacement_approval_receipt_v1(raw: bytes) -> dict[str, Any]:
    """Parse exact canonical-JCS bytes and validate the intrinsic receipt contract."""

    if not isinstance(raw, bytes):
        raise _contract_error("input must be bytes")
    if raw.startswith(b"\xef\xbb\xbf"):
        raise _contract_error("canonical JSON must not contain a BOM")
    try:
        text = raw.decode("utf-8")
        value = json.loads(
            text,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_nonfinite_constant,
        )
    except FocusedReplacementApprovalReceiptError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise _contract_error(f"contains invalid JSON: {error}") from error
    try:
        if canonical_jcs(value) != raw:
            raise _contract_error("input is not the exact canonical JSON encoding")
    except CanonicalJcsError as error:
        raise _contract_error(f"input is outside canonical JCS: {error}") from error
    validate_focused_replacement_approval_receipt_v1(value)
    return value


def validate_against_trusted_current_context_v1(
    receipt: Any,
    trusted_current_context: Any,
) -> None:
    """Bind every receipt context field to independently trusted runtime state.

    ``receipt_sha256`` is a self-hash and is never approval authority.  This
    semantic validation is meaningful only when ``trusted_current_context`` was
    obtained independently by the trusted runtime, not copied from the receipt.
    """

    validate_focused_replacement_approval_receipt_v1(receipt)
    receipt = _require_exact_object(receipt, RECEIPT_FIELDS, "receipt")
    trusted = _require_exact_object(
        trusted_current_context,
        TRUSTED_CURRENT_CONTEXT_FIELDS,
        "trusted current context",
    )
    if trusted["format_id"] != FORMAT_ID:
        raise _contract_error(
            f"trusted format_id must be {FORMAT_ID!r}"
        )
    if (
        isinstance(trusted["schema_version"], bool)
        or trusted["schema_version"] != SCHEMA_VERSION
    ):
        raise _contract_error(
            f"trusted schema_version must be {SCHEMA_VERSION}"
        )
    _require_uuid_v7(trusted["attempt_id"], "trusted attempt_id")
    if trusted["focused_validation_id"] not in {FOCUSED_VALIDATION_ID, TRANSITION_VALIDATION_ID}:
        raise _contract_error(
            "trusted focused_validation_id must be "
            f"{FOCUSED_VALIDATION_ID!r} or {TRANSITION_VALIDATION_ID!r}"
        )
    if trusted["classification"] != CLASSIFICATION:
        raise _contract_error(
            f"trusted classification must be {CLASSIFICATION!r}"
        )
    _require_identifier(trusted["policy_id"], "trusted policy_id")
    _require_epoch(trusted["mutation_epoch"], "trusted mutation_epoch")
    for field in (
        "frozen_inventory_hash",
        "focused_inventory_catalog_semantic_sha256",
        "inventory_discovery_processes_sha256",
        "policy_runner_bundle_sha256",
        "workspace_fingerprint",
    ):
        _require_sha256(trusted[field], f"trusted {field}")
    for field in TRUSTED_CURRENT_CONTEXT_FIELDS:
        if receipt[field] != trusted[field]:
            raise _contract_error(
                f"{field} does not match independently trusted current context"
            )


__all__ = [
    "CLASSIFICATION",
    "FOCUSED_VALIDATION_ID",
    "FORMAT_ID",
    "FocusedReplacementApprovalReceiptError",
    "MAX_EXACT_JSON_INTEGER",
    "RECEIPT_DIGEST_FIELDS",
    "RECEIPT_FIELDS",
    "RECEIPT_HASH_DOMAIN",
    "SCHEMA_VERSION",
    "TRUSTED_CURRENT_CONTEXT_FIELDS",
    "focused_replacement_approval_receipt_digest_projection_v1",
    "focused_replacement_approval_receipt_digest_v1",
    "parse_focused_replacement_approval_receipt_v1",
    "validate_against_trusted_current_context_v1",
    "validate_focused_replacement_approval_receipt_v1",
]
