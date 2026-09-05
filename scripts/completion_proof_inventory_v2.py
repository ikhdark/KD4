"""Inventory V2 contract helpers and read-only artifact validation CLI.

This module intentionally has no runner, filesystem-mutation, or completion-gate
behavior. It exists so Python and Rust parse and hash the same dormant wire data.
"""

from __future__ import annotations

import base64
import argparse
import binascii
import hashlib
import hmac
import json
import math
import re
import sys
import struct
import unicodedata
import uuid
from pathlib import Path
from types import ModuleType
from typing import Any


class InventoryV2ContractError(ValueError):
    """Raised when a value is outside the closed Inventory V2 contract."""


_CANONICAL_MODULE_NAME = "_kd4_completion_proof_inventory_v2_canonical"
_CANONICAL_MODULE_PATH = Path(__file__).resolve().with_name(
    "completion_proof_canonical.py"
)
_canonical_module = ModuleType(_CANONICAL_MODULE_NAME)
_canonical_module.__file__ = str(_CANONICAL_MODULE_PATH)
_canonical_module.__package__ = ""
_previous_canonical_module = sys.modules.get(_CANONICAL_MODULE_NAME)
sys.modules[_CANONICAL_MODULE_NAME] = _canonical_module
try:
    _canonical_source = _CANONICAL_MODULE_PATH.read_bytes()
    _canonical_code = compile(
        _canonical_source,
        str(_CANONICAL_MODULE_PATH),
        "exec",
        dont_inherit=True,
    )
    exec(_canonical_code, _canonical_module.__dict__)
finally:
    if sys.modules.get(_CANONICAL_MODULE_NAME) is _canonical_module:
        if _previous_canonical_module is None:
            sys.modules.pop(_CANONICAL_MODULE_NAME, None)
        else:
            sys.modules[_CANONICAL_MODULE_NAME] = _previous_canonical_module


_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_BASE64URL_RE = re.compile(r"^[A-Za-z0-9_-]+$")
_STANDARD_BASE64_32_RE = re.compile(r"^[A-Za-z0-9+/]{42}[A-Za-z0-9+/]$")
_CFG_IDENTIFIER_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_FEATURE_RE = re.compile(r"^[!-+\--~]+$")
_RESERVED_CFG_IDENTIFIERS = {"all", "any", "not", "true", "false"}
_IJSON_MAX_INTEGER = 9_007_199_254_740_991
MAX_SELECTION_REQUEST_BYTES = 18_432
MAX_SELECTION_REQUEST_SELECTORS = 256
MAX_SELECTION_REQUEST_TOKEN_BYTES = 24_576
FROZEN_V1_INVENTORY_RAW_SHA256 = "df230a7683f0f31f1aae4d3f7644af39cec67b09fadf8f3f1e6c60729d18196a"
FROZEN_V1_INVENTORY_SEMANTIC_SHA256 = "a2fb8c0b806853b6375d92cfa6daf985ea35a5c4d4ecd49cf1d6da6f23359152"
FROZEN_V1_LEDGER_RAW_SHA256 = "210acb8428be83b9c6acd021bde4e44905e738d557ca89db60cb1271f65da46b"
FROZEN_V1_BASELINE_IDS_SHA256 = "9100d0fe0dd4c270a1216b6d3ec6f6c39b7f93278cf68235f500edae17d25eb1"
FROZEN_V1_BASELINE_ASSOCIATIONS_SHA256 = "fe4ee73177d7192ad505e30d20dd851243d981ccf2ab42de49d7d83899624908"
FROZEN_V1_WORKSPACE_FINGERPRINT = "7d5c019e4af3720e1188704b05a098e95fdb200b901cbccf5cf13363643f34ca"
FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT = 644
FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT = 685
FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT = 572
FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT = 531
FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256 = (
    "137ab36ace74657f218d2cc66918ac8261decae44376073299a570cbbc0b0882"
)
INVENTORY_V2_SCHEMA_PATHS = (
    ".codex/validation/frozen-test-inventory-v2-recoveries.schema.json",
    ".codex/validation/frozen-test-inventory-v2.schema.json",
    ".codex/validation/inventory-shared-types-v1.schema.json",
    ".codex/validation/selection-request-v1.schema.json",
    ".codex/validation/test-replacements-v2.schema.json",
)
INVENTORY_V2_SCHEMA_IDS = (
    "kd4://validation/frozen-test-inventory-v2-recoveries.schema.json",
    "kd4://validation/frozen-test-inventory-v2.schema.json",
    "kd4://validation/inventory-shared-types-v1.schema.json",
    "kd4://validation/selection-request-v1.schema.json",
    "kd4://validation/test-replacements-v2.schema.json",
)
INVENTORY_V2_SCHEMA_RAW_SHA256S = (
    "a3ef6ec3486368c3e3c1bc468ba2a16255d75dbe250aa4681cbef696d923a6f4",
    "06d7134aa77d6fa63fea9e7a0094fa76f4c2cca74c6ddad4a55078a09909732a",
    "1753d9bbca80f85ace51f9622e4cf228e91e0dfae94f82691a9a4935b2482c19",
    "b83d9fd5a535d358d3da6a9b06652dfc9390877813b5b5f85e01fd56b90e9c78",
    "4a20813531b293ca3a25354aa095c5e9f417aa2169a00f8cbe3edd1442e0f2e6",
)

DOCTEST_RECAPTURE_FORMAT_ID = "kd4.doctest-recapture.v1"
DOCTEST_RECAPTURE_BASELINE_COMMIT = "60bb133fa0a4f25e83851ab16d8c462e5f42ff95"
DOCTEST_RECAPTURE_SOURCE_TREE_SHA256 = (
    "654591dd1ddda7a77312172ec7c70e60e80990590c7c74a7c3b08a445279d90e"
)
DOCTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256 = (
    "f386e4786f3a61829ecdd61e764fa9d65eddbd08f2902745c6480bce448573cc"
)
DOCTEST_RECAPTURE_TOOLCHAIN = "1.95.0-x86_64-pc-windows-msvc"
DOCTEST_RECAPTURE_PACKAGE_SPECS = (
    ("codex-core", "codex-core::lib::codex_core"),
    ("codex-rollout", "codex-rollout::lib::codex_rollout"),
    ("codex-state", "codex-state::lib::codex_state"),
    ("codex-tui", "codex-tui::lib::codex_tui"),
)
DOCTEST_RECAPTURE_PARENT_TARGETS = {
    "rust-doctest::core\\src\\client.rs - client::ModelClient (line 1696)":
        "codex-core::lib::codex_core",
    "rust-doctest::rollout\\src\\recorder.rs - recorder::RolloutRecorder (line 84)":
        "codex-rollout::lib::codex_rollout",
    "rust-doctest::state\\src\\log_db.rs - log_db (line 10)":
        "codex-state::lib::codex_state",
    "rust-doctest::tui\\src\\bottom_pane\\multi_select_picker.rs - "
    "bottom_pane::multi_select_picker (line 14)": "codex-tui::lib::codex_tui",
    "rust-doctest::tui\\src\\bottom_pane\\multi_select_picker.rs - "
    "bottom_pane::multi_select_picker::MultiSelectPickerBuilder (line 706)":
        "codex-tui::lib::codex_tui",
}

UNITTEST_RECAPTURE_FORMAT_ID = "kd4.unittest-recapture.v1"
UNITTEST_RECAPTURE_BASELINE_COMMIT = DOCTEST_RECAPTURE_BASELINE_COMMIT
UNITTEST_RECAPTURE_SOURCE_TREE_SHA256 = DOCTEST_RECAPTURE_SOURCE_TREE_SHA256
UNITTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256 = (
    DOCTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256
)
UNITTEST_RECAPTURE_PARENT_RECORDS_SHA256 = (
    "a46a941721c872655dcb1c4ca55c070b9f48008a451d0df283f2d69957c2dd07"
)
# The V1 freeze recorded a dirty-workspace fingerprint, but the authenticated
# overlay bytes needed to reproduce its 893 executable unittest parents have
# not yet been recovered.  A packet cannot be accepted from the bare commit
# or from a self-asserted site manifest.  This authority is populated only from
# independently recovered freeze provenance.
UNITTEST_RECAPTURE_SOURCE_SITE_MANIFEST_SHA256: str | None = None
# Amendment 1 authenticates only the exception, never the missing source bodies.
UNITTEST_SOURCE_EXCEPTIONS_SHA256 = "63fd50f8f5838408cf19b9b18abdc5353cc52412dbf36a275eb77f31c8a452f9"
UNITTEST_APPROVED_SOURCE_SITE_MANIFEST_SHA256 = "ea2d3175afe632a4347d182d2dd045d28b11d58afda62d1848d0729356aba2c2"
UNITTEST_V1_HIDDEN_PARENT_IDS_SHA256 = (
    "936330f9e9a23c8d628f651a1ed31b3f4ea836a06cf152a6acf09cff898ebc40"
)
UNITTEST_V1_REPLACEMENT_PARENT_IDS_SHA256 = (
    "59202883ef32488ae9a488345b2401400794d961e5d539c58fd6364e86f25fe1"
)
UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_IDS_SHA256 = (
    "208d6735c1413d94c503a453331889fe5709b8eca540cf9c13993c42f9bb8cf7"
)
UNITTEST_V1_UNRESOLVED_PARENT_IDS_SHA256 = (
    "d1e66a89d1a943b60f6516bc9550102306919d6ae673bee4027595a3df8036f7"
)


def require_nfc(value: str) -> None:
    if unicodedata.normalize("NFC", value) != value:
        raise InventoryV2ContractError(f"string is not NFC: {value!r}")


def require_sha256(value: str) -> str:
    if _SHA256_RE.fullmatch(value) is None:
        raise InventoryV2ContractError("invalid SHA-256 hex value")
    return value


def require_identifier(value: str) -> str:
    if not isinstance(value, str):
        raise InventoryV2ContractError("identifier must be a string")
    if _ID_RE.fullmatch(value) is None:
        raise InventoryV2ContractError(f"invalid identifier: {value!r}")
    return value


def require_strict_repository_path(value: str) -> str:
    if not isinstance(value, str):
        raise InventoryV2ContractError("repository path must be a string")
    require_nfc(value)
    parts = value.split("/")
    if (
        not value
        or value.startswith(("/", "\\"))
        or "\\" in value
        or ":" in value
        or (len(value) >= 2 and value[0].isalpha() and value[1] == ":")
        or any(part in ("", ".", "..") for part in parts)
    ):
        raise InventoryV2ContractError(f"invalid repository path: {value!r}")
    return value


def canonical_jcs(value: Any) -> bytes:
    """Encode the shared no-float RFC 8785/JCS subset."""

    try:
        return _canonical_module.canonical_jcs(value)
    except _canonical_module.CanonicalJcsError as error:
        raise InventoryV2ContractError(str(error)) from error


def parse_canonical_jcs(raw: bytes) -> Any:
    if raw.startswith(b"\xef\xbb\xbf"):
        raise InventoryV2ContractError("canonical JSON must not contain a BOM")
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise InventoryV2ContractError(f"invalid JSON: {error}") from error
    if canonical_jcs(value) != raw:
        raise InventoryV2ContractError("input is not the exact canonical JSON encoding")
    return value


def proof_hash(domain: str, value: Any) -> str:
    try:
        return _canonical_module.proof_hash(domain, value)
    except _canonical_module.CanonicalJcsError as error:
        raise InventoryV2ContractError(str(error)) from error


def raw_jcs_sha256(value: Any) -> str:
    return hashlib.sha256(canonical_jcs(value)).hexdigest()


def validate_selection_request_v1(value: Any) -> None:
    if not isinstance(value, dict) or set(value) != {"schema_version", "selectors"}:
        raise InventoryV2ContractError("SelectionRequestV1 has unknown or missing fields")
    if isinstance(value["schema_version"], bool) or value["schema_version"] != 1:
        raise InventoryV2ContractError("SelectionRequestV1 schema_version must be 1")
    selectors = value["selectors"]
    if (
        not isinstance(selectors, list)
        or not selectors
        or len(selectors) > MAX_SELECTION_REQUEST_SELECTORS
    ):
        raise InventoryV2ContractError("SelectionRequestV1 selectors must be nonempty")
    encoded: list[bytes] = []
    for selector in selectors:
        if not isinstance(selector, dict):
            raise InventoryV2ContractError("selector must be an object")
        if set(selector) == {"kind", "test_id"} and selector["kind"] == "test":
            if not isinstance(selector["test_id"], str) or not selector["test_id"]:
                raise InventoryV2ContractError("test_id must be a nonempty string")
            require_nfc(selector["test_id"])
        elif set(selector) == {"action_id", "kind"} and selector["kind"] == "action":
            if not isinstance(selector["action_id"], str):
                raise InventoryV2ContractError("action_id must be a string")
            require_identifier(selector["action_id"])
        else:
            raise InventoryV2ContractError("selector does not match a closed union member")
        encoded.append(canonical_jcs(selector))
    if any(left >= right for left, right in zip(encoded, encoded[1:])):
        raise InventoryV2ContractError(
            "selectors must be strictly sorted and unique by canonical JCS bytes"
        )


def _require_object(value: Any, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        raise InventoryV2ContractError(f"{label} has unknown or missing fields")
    return value


def _require_integer(value: Any, minimum: int, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise InventoryV2ContractError(f"{label} must be an integer >= {minimum}")
    return value


def _require_nonempty_nfc(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise InventoryV2ContractError(f"{label} must be a nonempty string")
    require_nfc(value)
    return value


def _require_nfc_string(value: Any, label: str) -> str:
    if not isinstance(value, str):
        raise InventoryV2ContractError(f"{label} must be a string")
    require_nfc(value)
    return value


def _require_sha256_field(value: Any, label: str) -> str:
    if not isinstance(value, str):
        raise InventoryV2ContractError(f"{label} must be a SHA-256 string")
    return require_sha256(value)


def _require_sorted_unique_jcs(values: Any, label: str, *, nonempty: bool = False) -> list[Any]:
    if not isinstance(values, list) or (nonempty and not values):
        suffix = " and nonempty" if nonempty else ""
        raise InventoryV2ContractError(f"{label} must be an array{suffix}")
    encodings = [canonical_jcs(item) for item in values]
    if any(left >= right for left, right in zip(encodings, encodings[1:])):
        raise InventoryV2ContractError(
            f"{label} must be strictly sorted and unique by canonical JCS bytes"
        )
    return values


def _require_sorted_unique_strings(values: Any, label: str, *, nonempty: bool = False) -> list[str]:
    if not isinstance(values, list) or (nonempty and not values):
        suffix = " and nonempty" if nonempty else ""
        raise InventoryV2ContractError(f"{label} must be an array{suffix}")
    for item in values:
        _require_nonempty_nfc(item, label)
    if any(left >= right for left, right in zip(values, values[1:])):
        raise InventoryV2ContractError(f"{label} must be strictly sorted and unique")
    return values


def _require_schema_version(value: Any, expected: int, label: str) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value != expected:
        raise InventoryV2ContractError(f"{label} schema_version must be {expected}")


def validate_inventory_authority_ref_v1(value: Any) -> None:
    _require_object(
        value,
        {"path", "raw_sha256", "self_hash", "semantic_sha256"},
        "InventoryAuthorityRefV1",
    )
    require_strict_repository_path(value["path"])
    for field in ("raw_sha256", "self_hash", "semantic_sha256"):
        _require_sha256_field(value[field], field)


def validate_executable_identity_v1(value: Any) -> None:
    if not isinstance(value, dict) or not isinstance(value.get("kind"), str):
        raise InventoryV2ContractError("ExecutableIdentityV1 must be a tagged object")
    kind = value["kind"]
    fields_by_kind = {
        "test": {"kind", "route_id", "test_id", "validation_id"},
        "action": {"action_id", "kind", "validation_id"},
    }
    fields = fields_by_kind.get(kind)
    if fields is None:
        raise InventoryV2ContractError("unknown executable identity kind")
    _require_object(value, fields, "ExecutableIdentityV1")
    if kind == "action":
        require_identifier(value["action_id"])
        require_identifier(value["validation_id"])
        return
    _require_nonempty_nfc(value["test_id"], "test_id")
    require_identifier(value["validation_id"])
    if value["route_id"] not in _ROUTE_BY_KIND.values():
        raise InventoryV2ContractError("unknown executable identity route")


def validate_runner_selector_v1(value: Any) -> None:
    if not isinstance(value, dict) or not isinstance(value.get("kind"), str):
        raise InventoryV2ContractError("RunnerSelectorV1 must be a tagged object")
    kind = value["kind"]
    fields_by_kind = {
        "rust-nextest": {"cargo_target_context_spec_sha256", "harness_test_name", "kind", "nextest_binary_id"},
        "rust-doctest": {"cargo_target_context_spec_sha256", "declaration_ordinal", "harness_test_name", "item_path", "kind", "source_path"},
        "python-unittest": {"kind", "parent_test_id", "selection_unit", "subtest_manifest_sha256"},
        "python-pytest": {"kind", "node_id"},
        "javascript-jest": {"ancestor_titles", "column", "config_path", "file_path", "full_title", "kind", "line", "registration_ordinal"},
        "argument-comment-lint-native": {"case_id", "kind"},
        "windows-sandbox-smoke-native": {"case_id", "kind"},
        "non-test-action": {"action_id", "kind"},
    }
    fields = fields_by_kind.get(kind)
    if fields is None:
        raise InventoryV2ContractError("unknown runner selector kind")
    _require_object(value, fields, "RunnerSelectorV1")
    for path_field in ("config_path", "file_path", "source_path"):
        if path_field in value:
            require_strict_repository_path(value[path_field])
    for integer_field in ("line", "column"):
        if integer_field in value:
            _require_integer(value[integer_field], 1, integer_field)
    for integer_field in ("registration_ordinal", "declaration_ordinal"):
        if integer_field in value:
            _require_integer(value[integer_field], 0, integer_field)
    for field, item in value.items():
        if field not in {"kind", "config_path", "file_path", "source_path", "line", "column", "registration_ordinal", "declaration_ordinal", "ancestor_titles"}:
            _require_nonempty_nfc(item, field)
    if "ancestor_titles" in value:
        if not isinstance(value["ancestor_titles"], list):
            raise InventoryV2ContractError("ancestor_titles must be an array")
        for title in value["ancestor_titles"]:
            _require_nonempty_nfc(title, "ancestor title")
    if kind == "python-unittest":
        if value["selection_unit"] != "parent-with-all-declared-subtests":
            raise InventoryV2ContractError("invalid Python unittest selection unit")
        _require_sha256_field(value["subtest_manifest_sha256"], "subtest_manifest_sha256")
    if kind in {"rust-nextest", "rust-doctest"}:
        _require_sha256_field(
            value["cargo_target_context_spec_sha256"],
            "cargo_target_context_spec_sha256",
        )


def validate_path_spec_v1(value: Any) -> None:
    if not isinstance(value, dict) or not isinstance(value.get("kind"), str):
        raise InventoryV2ContractError("PathSpecV1 must be a tagged object")
    if value["kind"] == "exact":
        _require_object(value, {"kind", "path"}, "exact PathSpecV1")
        require_strict_repository_path(value["path"])
    elif value["kind"] == "glob":
        _require_object(value, {"kind", "pattern", "root"}, "glob PathSpecV1")
        require_strict_repository_path(value["root"])
        pattern = _require_nonempty_nfc(value["pattern"], "glob pattern")
        segments = pattern.split("/")
        if (
            pattern.startswith("/")
            or ":" in pattern
            or "\\" in pattern
            or any(segment in {"", ".", ".."} for segment in segments)
        ):
            raise InventoryV2ContractError("glob pattern is not repository-local")
    elif value["kind"] == "semantic":
        _require_object(value, {"kind", "semantic_input_id"}, "semantic PathSpecV1")
        require_identifier(value["semantic_input_id"])
    else:
        raise InventoryV2ContractError("unknown PathSpecV1 kind")


def validate_execution_input_contract_v1(value: Any) -> None:
    _require_object(value, {"contract_id", "contract_sha256", "consumed", "owned", "schema_version"}, "ExecutionInputContractV1")
    if isinstance(value["schema_version"], bool) or value["schema_version"] != 1:
        raise InventoryV2ContractError("invalid ExecutionInputContractV1 version")
    if not isinstance(value["owned"], list) or not isinstance(value["consumed"], list) or not (value["owned"] or value["consumed"]):
        raise InventoryV2ContractError("execution input contract requires inputs")
    for group in (value["owned"], value["consumed"]):
        encodings = []
        for spec in group:
            validate_path_spec_v1(spec)
            encodings.append(canonical_jcs(spec))
        if any(a >= b for a, b in zip(encodings, encodings[1:])):
            raise InventoryV2ContractError("path specs must be sorted and unique")
    require_sha256(value["contract_sha256"])
    projection = {"consumed": value["consumed"], "owned": value["owned"], "schema_version": 1}
    expected = proof_hash("kd4.execution-input-contract.v1", projection)
    if value["contract_sha256"] != expected or value["contract_id"] != f"execution-input-contract-v1.{expected}":
        raise InventoryV2ContractError("execution input contract ID/hash mismatch")


def _require_cfg_identifier(value: Any, label: str) -> str:
    value = _require_nonempty_nfc(value, label)
    if (
        _CFG_IDENTIFIER_RE.fullmatch(value) is None
        or value in _RESERVED_CFG_IDENTIFIERS
    ):
        raise InventoryV2ContractError(f"{label} is not a valid cfg identifier")
    return value


def validate_rust_cfg_predicate_v1(
    value: Any, *, _depth: int = 1, _counter: list[int] | None = None
) -> None:
    if _depth > 64:
        raise InventoryV2ContractError("Rust cfg predicate exceeds maximum depth")
    if _counter is None:
        _counter = [0]
    _counter[0] += 1
    if _counter[0] > 4096:
        raise InventoryV2ContractError("Rust cfg predicate exceeds maximum node count")
    if not isinstance(value, dict) or not isinstance(value.get("kind"), str):
        raise InventoryV2ContractError("RustCfgPredicateV1 must be a tagged object")
    kind = value["kind"]
    if kind in {"true", "false"}:
        _require_object(value, {"kind"}, "RustCfgPredicateV1")
    elif kind == "flag":
        _require_object(value, {"kind", "name"}, "RustCfgPredicateV1")
        _require_cfg_identifier(value["name"], "cfg flag")
    elif kind == "key-value":
        _require_object(value, {"key", "kind", "value"}, "RustCfgPredicateV1")
        _require_cfg_identifier(value["key"], "cfg key")
        _require_nfc_string(value["value"], "cfg value")
    elif kind in {"all", "any"}:
        _require_object(value, {"kind", "predicates"}, "RustCfgPredicateV1")
        predicates = value["predicates"]
        if not isinstance(predicates, list):
            raise InventoryV2ContractError("cfg predicates must be an array")
        for predicate in predicates:
            validate_rust_cfg_predicate_v1(
                predicate, _depth=_depth + 1, _counter=_counter
            )
        _require_sorted_unique_jcs(predicates, "cfg predicates")
    elif kind == "not":
        _require_object(value, {"kind", "predicate"}, "RustCfgPredicateV1")
        validate_rust_cfg_predicate_v1(
            value["predicate"], _depth=_depth + 1, _counter=_counter
        )
    else:
        raise InventoryV2ContractError("unknown Rust cfg predicate kind")


def validate_rust_cfg_expression_v1(value: Any) -> None:
    _require_object(
        value, {"root", "schema_version", "semantic_sha256"}, "RustCfgExpressionV1"
    )
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "RustCfgExpressionV1")
    validate_rust_cfg_predicate_v1(value["root"])
    expected = proof_hash(
        "kd4.rust-cfg-expression.semantic.v1",
        {"root": value["root"], "schema_version": 1},
    )
    if value["semantic_sha256"] != expected:
        raise InventoryV2ContractError("Rust cfg expression semantic hash mismatch")


def validate_rust_cfg_atom_v1(value: Any) -> None:
    if not isinstance(value, dict):
        raise InventoryV2ContractError("RustCfgAtomV1 must be an object")
    if value.get("kind") == "flag":
        _require_object(value, {"kind", "name"}, "RustCfgAtomV1")
        _require_cfg_identifier(value["name"], "cfg atom flag")
    elif value.get("kind") == "key-value":
        _require_object(value, {"key", "kind", "value"}, "RustCfgAtomV1")
        _require_cfg_identifier(value["key"], "cfg atom key")
        _require_nfc_string(value["value"], "cfg atom value")
    else:
        raise InventoryV2ContractError("unknown Rust cfg atom kind")


def validate_rust_cfg_atom_set_v1(values: Any, expected_sha256: Any) -> None:
    atoms = _require_sorted_unique_jcs(values, "Rust cfg atoms", nonempty=True)
    for atom in atoms:
        validate_rust_cfg_atom_v1(atom)
    if expected_sha256 != proof_hash("kd4.rust-cfg-atom-set.v1", atoms):
        raise InventoryV2ContractError("Rust cfg atom-set hash mismatch")


def validate_platform_applicability_v1(value: Any) -> None:
    if not isinstance(value, dict) or value.get("kind") not in {"host-set", "rust-cfg"}:
        raise InventoryV2ContractError("invalid PlatformApplicabilityV1")
    if value["kind"] == "host-set":
        _require_object(value, {"kind", "required_hosts"}, "host-set applicability")
        hosts = value["required_hosts"]
        if not isinstance(hosts, list) or not hosts or hosts != sorted(set(hosts)) or any(host not in {"windows", "linux", "darwin"} for host in hosts):
            raise InventoryV2ContractError("required_hosts must be sorted unique host tokens")
    else:
        _require_object(value, {"expression", "kind"}, "rust-cfg applicability")
        validate_rust_cfg_expression_v1(value["expression"])


def _validate_feature_array(value: Any, label: str) -> list[str]:
    values = _require_sorted_unique_strings(value, label)
    for feature in values:
        if any(ch.isspace() or ch == "," or not 0x21 <= ord(ch) <= 0x7E for ch in feature):
            raise InventoryV2ContractError(f"{label} contains an invalid Cargo feature")
    return values


def validate_cargo_target_context_spec_v1(value: Any) -> None:
    fields = {
        "cargo_profile", "context_sha256", "feature_selection",
        "package_manifest_path", "package_name", "schema_version", "target_kind",
        "target_name", "target_source_path", "workspace_manifest_path",
    }
    _require_object(value, fields, "CargoTargetContextSpecV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "CargoTargetContextSpecV1")
    if value["cargo_profile"] != "test":
        raise InventoryV2ContractError("Cargo target context profile must be test")
    for field in ("package_manifest_path", "target_source_path", "workspace_manifest_path"):
        require_strict_repository_path(value[field])
    for field in ("package_name", "target_name"):
        _require_nonempty_nfc(value[field], field)
    if value["target_kind"] not in {"lib", "proc-macro", "bin", "example", "test", "bench"}:
        raise InventoryV2ContractError("unknown Cargo target kind")
    selection = value["feature_selection"]
    if not isinstance(selection, dict):
        raise InventoryV2ContractError("CargoFeatureSelectionV1 must be an object")
    if selection.get("kind") == "default":
        _require_object(selection, {"additional_features", "kind"}, "CargoFeatureSelectionV1")
        _validate_feature_array(selection["additional_features"], "additional_features")
    elif selection.get("kind") == "no-default":
        _require_object(selection, {"features", "kind"}, "CargoFeatureSelectionV1")
        _validate_feature_array(selection["features"], "features")
    elif selection.get("kind") == "all":
        _require_object(selection, {"kind"}, "CargoFeatureSelectionV1")
    else:
        raise InventoryV2ContractError("unknown Cargo feature selection kind")
    projection = {key: value[key] for key in fields if key != "context_sha256"}
    if value["context_sha256"] != proof_hash("kd4.cargo-target-context-spec.v1", projection):
        raise InventoryV2ContractError("Cargo target context hash mismatch")


def _require_uuid(value: Any, label: str) -> str:
    value = _require_nonempty_nfc(value, label)
    try:
        parsed = uuid.UUID(value)
    except ValueError as error:
        raise InventoryV2ContractError(f"{label} must be a UUID") from error
    if str(parsed) != value:
        raise InventoryV2ContractError(f"{label} must be a lowercase hyphenated UUID")
    return value


def _require_standard_base64_32(value: Any, label: str) -> bytes:
    value = _require_nonempty_nfc(value, label)
    if len(value) != 43 or _STANDARD_BASE64_32_RE.fullmatch(value) is None:
        raise InventoryV2ContractError(f"{label} must be unpadded standard Base64")
    try:
        decoded = base64.b64decode(value + "=", validate=True)
    except binascii.Error as error:
        raise InventoryV2ContractError(f"{label} is invalid Base64") from error
    if len(decoded) != 32 or base64.b64encode(decoded).decode("ascii").rstrip("=") != value:
        raise InventoryV2ContractError(f"{label} must canonically encode 32 bytes")
    return decoded


def validate_launch_target_identity_v1(value: Any) -> None:
    _require_object(
        value, {"requested", "resolved_path", "sha256_after", "sha256_before"},
        "LaunchTargetIdentityV1",
    )
    _require_nonempty_nfc(value["requested"], "requested launch target")
    _require_nonempty_nfc(value["resolved_path"], "resolved launch path")
    before = _require_sha256_field(value["sha256_before"], "sha256_before")
    after = _require_sha256_field(value["sha256_after"], "sha256_after")
    if before != after:
        raise InventoryV2ContractError("launch target changed during observation")


def validate_runner_process_identity_v1(value: Any) -> None:
    _require_object(value, {"argv_sha256", "ended_at_unix_ms", "entrypoint_identity", "executable_identity", "parent_pid", "pid", "started_at_unix_ms"}, "RunnerProcessIdentityV1")
    _require_sha256_field(value["argv_sha256"], "argv_sha256")
    validate_launch_target_identity_v1(value["entrypoint_identity"])
    validate_launch_target_identity_v1(value["executable_identity"])
    for field in ("parent_pid", "pid"):
        _require_integer(value[field], 1, field)
    for field in ("started_at_unix_ms", "ended_at_unix_ms"):
        stamp = _require_integer(value[field], 0, field)
        if stamp > _IJSON_MAX_INTEGER:
            raise InventoryV2ContractError(f"{field} is outside I-JSON range")
    if value["ended_at_unix_ms"] < value["started_at_unix_ms"]:
        raise InventoryV2ContractError("runner timestamps are reversed")


def validate_child_process_identity_v1(value: Any) -> None:
    _require_object(value, {"argv_sha256", "ended_at_unix_ms", "executable_identity", "execution_id", "exit_code", "parent_pid", "pid", "started_at_unix_ms"}, "ChildProcessIdentityV1")
    _require_sha256_field(value["argv_sha256"], "argv_sha256")
    validate_launch_target_identity_v1(value["executable_identity"])
    _require_uuid(value["execution_id"], "execution_id")
    if isinstance(value["exit_code"], bool) or not isinstance(value["exit_code"], int):
        raise InventoryV2ContractError("exit_code must be an integer")
    for field in ("parent_pid", "pid"):
        _require_integer(value[field], 1, field)
    for field in ("started_at_unix_ms", "ended_at_unix_ms"):
        stamp = _require_integer(value[field], 0, field)
        if stamp > _IJSON_MAX_INTEGER:
            raise InventoryV2ContractError(f"{field} is outside I-JSON range")
    if value["ended_at_unix_ms"] < value["started_at_unix_ms"]:
        raise InventoryV2ContractError("child timestamps are reversed")


def validate_rust_cfg_invocation_receipt_v1(value: Any, authentication_key: bytes | None = None) -> None:
    _require_object(value, {"authentication_tag", "body", "key_id", "receipt_sha256", "schema_version"}, "RustCfgInvocationReceiptV1")
    _require_schema_version(value["schema_version"], 1, "RustCfgInvocationReceiptV1")
    _require_uuid(value["key_id"], "key_id")
    _require_standard_base64_32(value["authentication_tag"], "authentication_tag")
    body = value["body"]
    body_fields = {"attempt_id", "cargo_process_identity", "cargo_target_context_spec_sha256", "cargo_version_verbose_sha256", "channel_binding_sha256", "channel_id", "command_argv", "environment_block_sha256", "invocation_nonce", "lockfile_raw_sha256", "parsed_cfg_atoms_sha256", "repository_identity_sha256", "runner_process_identity", "rustc_executable_identity", "rustc_version_verbose_sha256", "schema_version", "stderr_raw_sha256", "stdout_raw_sha256", "target_triple", "working_directory", "workspace_fingerprint"}
    _require_object(body, body_fields, "RustCfgInvocationReceiptBodyV1")
    _require_schema_version(body["schema_version"], 1, "RustCfgInvocationReceiptBodyV1")
    _require_uuid(body["attempt_id"], "attempt_id")
    validate_child_process_identity_v1(body["cargo_process_identity"])
    validate_runner_process_identity_v1(body["runner_process_identity"])
    validate_launch_target_identity_v1(body["rustc_executable_identity"])
    for field in ("cargo_target_context_spec_sha256", "cargo_version_verbose_sha256", "channel_binding_sha256", "environment_block_sha256", "lockfile_raw_sha256", "parsed_cfg_atoms_sha256", "repository_identity_sha256", "rustc_version_verbose_sha256", "stderr_raw_sha256", "stdout_raw_sha256", "workspace_fingerprint"):
        _require_sha256_field(body[field], field)
    _require_standard_base64_32(body["invocation_nonce"], "invocation_nonce")
    _require_nonempty_nfc(body["channel_id"], "channel_id")
    _require_nonempty_nfc(body["target_triple"], "target_triple")
    require_strict_repository_path(body["working_directory"])
    if not isinstance(body["command_argv"], list) or not body["command_argv"]:
        raise InventoryV2ContractError("command_argv must be nonempty")
    for argument in body["command_argv"]:
        _require_nonempty_nfc(argument, "command argument")
    if body["cargo_process_identity"]["argv_sha256"] != raw_jcs_sha256(body["command_argv"]):
        raise InventoryV2ContractError("Cargo process identity does not bind command_argv")
    if body["cargo_process_identity"]["parent_pid"] != body["runner_process_identity"]["pid"]:
        raise InventoryV2ContractError("Cargo process is not a child of the runner")
    expected = proof_hash("kd4.rust-cfg-invocation-receipt.v1", body)
    if value["receipt_sha256"] != expected:
        raise InventoryV2ContractError("Rust cfg receipt hash mismatch")
    if authentication_key is not None:
        if len(authentication_key) != 32:
            raise InventoryV2ContractError("receipt authentication key must be 32 bytes")
        payload = {"body": body, "key_id": value["key_id"], "receipt_sha256": expected, "schema_version": 1}
        tag = hmac.new(authentication_key, b"kd4.rust-cfg-invocation-receipt.authentication.v1\0" + canonical_jcs(payload), hashlib.sha256).digest()
        if not hmac.compare_digest(base64.b64decode(value["authentication_tag"] + "="), tag):
            raise InventoryV2ContractError("Rust cfg receipt authentication mismatch")


def validate_cargo_build_context_observation_v1(value: Any) -> None:
    fields = {"actual_cfg_atoms", "actual_cfg_atoms_sha256", "cargo_profile", "cargo_target_context_spec_sha256", "enabled_features", "invocation_receipt_sha256", "observation_sha256", "schema_version", "target_features", "target_triple", "test_cfg_present"}
    _require_object(value, fields, "CargoBuildContextObservationV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "CargoBuildContextObservationV1")
    validate_rust_cfg_atom_set_v1(value["actual_cfg_atoms"], value["actual_cfg_atoms_sha256"])
    if value["cargo_profile"] != "test":
        raise InventoryV2ContractError("Cargo observation profile must be test")
    for field in ("cargo_target_context_spec_sha256", "invocation_receipt_sha256"):
        _require_sha256_field(value[field], field)
    _validate_feature_array(value["enabled_features"], "enabled_features")
    _validate_feature_array(value["target_features"], "target_features")
    _require_nonempty_nfc(value["target_triple"], "target_triple")
    if value["test_cfg_present"] is not True:
        raise InventoryV2ContractError("Cargo observation must contain cfg(test)")
    feature_values = sorted(atom["value"] for atom in value["actual_cfg_atoms"] if atom["kind"] == "key-value" and atom["key"] == "feature")
    target_features = sorted(atom["value"] for atom in value["actual_cfg_atoms"] if atom["kind"] == "key-value" and atom["key"] == "target_feature")
    has_test = any(atom == {"kind": "flag", "name": "test"} for atom in value["actual_cfg_atoms"])
    if value["enabled_features"] != feature_values or value["target_features"] != target_features or value["test_cfg_present"] != has_test:
        raise InventoryV2ContractError("Cargo observation derived cfg fields disagree")
    projection = {key: value[key] for key in fields if key != "observation_sha256"}
    if value["observation_sha256"] != proof_hash("kd4.cargo-build-context-observation.v1", projection):
        raise InventoryV2ContractError("Cargo build-context observation hash mismatch")


def _evaluate_cfg_predicate(predicate: dict[str, Any], atoms: set[bytes]) -> bool:
    kind = predicate["kind"]
    if kind == "true": return True
    if kind == "false": return False
    if kind in {"flag", "key-value"}: return canonical_jcs(predicate) in atoms
    if kind == "all": return all(_evaluate_cfg_predicate(item, atoms) for item in predicate["predicates"])
    if kind == "any": return any(_evaluate_cfg_predicate(item, atoms) for item in predicate["predicates"])
    return not _evaluate_cfg_predicate(predicate["predicate"], atoms)


def validate_applicability_result_v1(
    value: Any, *, rust_route: bool | None = None, platform: Any = None
) -> None:
    fields = {"cargo_build_context_observation_sha256", "cargo_target_context_spec_sha256", "executable_identity_sha256", "host", "platform_applicability_sha256", "result_sha256", "rust_cfg_expression_semantic_sha256", "schema_version", "verdict"}
    _require_object(value, fields, "ApplicabilityResultV1")
    _require_schema_version(value["schema_version"], 1, "ApplicabilityResultV1")
    if value["host"] not in {"windows", "linux", "darwin"}:
        raise InventoryV2ContractError("invalid applicability host")
    if value["verdict"] not in {"applicable", "not-applicable"}:
        raise InventoryV2ContractError("invalid applicability verdict")
    for field in ("executable_identity_sha256", "platform_applicability_sha256"):
        _require_sha256_field(value[field], field)
    for field in ("cargo_build_context_observation_sha256", "cargo_target_context_spec_sha256", "rust_cfg_expression_semantic_sha256"):
        if value[field] is not None:
            _require_sha256_field(value[field], field)
    nullability = (
        value["cargo_target_context_spec_sha256"] is not None,
        value["cargo_build_context_observation_sha256"] is not None,
        value["rust_cfg_expression_semantic_sha256"] is not None,
    )
    if nullability not in {(True, True, True), (True, False, False), (False, False, False)}:
        raise InventoryV2ContractError("invalid applicability result hash nullability")
    projection = {key: value[key] for key in fields if key != "result_sha256"}
    if value["result_sha256"] != proof_hash("kd4.applicability-result.v1", projection):
        raise InventoryV2ContractError("applicability result hash mismatch")
    if platform is not None:
        validate_platform_applicability_v1(platform)
        if value["platform_applicability_sha256"] != proof_hash(
            "kd4.platform-applicability.v1", platform
        ):
            raise InventoryV2ContractError("applicability result platform binding mismatch")
        if rust_route and platform["kind"] == "rust-cfg":
            expected_shape = (True, True, True)
            if value["rust_cfg_expression_semantic_sha256"] != platform["expression"]["semantic_sha256"]:
                raise InventoryV2ContractError("applicability result cfg binding mismatch")
        elif rust_route and platform["kind"] == "host-set":
            expected_shape = (True, False, False)
        elif not rust_route and platform["kind"] == "host-set":
            expected_shape = (False, False, False)
        else:
            raise InventoryV2ContractError("invalid route/platform applicability shape")
        if nullability != expected_shape:
            raise InventoryV2ContractError("invalid route/platform applicability-result shape")


def validate_rust_compiled_test_id_v1(value: Any) -> None:
    if not isinstance(value, dict):
        raise InventoryV2ContractError("RustCompiledTestIdV1 must be an object")
    if value.get("kind") == "harness":
        _require_object(
            value,
            {"harness_test_name", "kind", "nextest_binary_id"},
            "RustCompiledTestIdV1",
        )
        _require_nonempty_nfc(value["nextest_binary_id"], "nextest_binary_id")
    elif value.get("kind") == "doctest":
        _require_object(value, {"harness_test_name", "kind"}, "RustCompiledTestIdV1")
    else:
        raise InventoryV2ContractError("unknown compiled Rust test ID kind")
    _require_nonempty_nfc(value["harness_test_name"], "harness_test_name")


def validate_rust_compiled_listing_run_v1(value: Any) -> None:
    fields = {"cargo_build_context_observation_sha256", "cargo_target_context_spec_sha256", "child_process_identity", "command_argv", "listed_test_ids", "listed_test_ids_sha256", "listing_kind", "schema_version", "stderr_raw_sha256", "stdout_raw_sha256"}
    _require_object(value, fields, "RustCompiledListingRunV1")
    _require_schema_version(value["schema_version"], 1, "RustCompiledListingRunV1")
    if value["listing_kind"] not in {"harness", "doctest"}:
        raise InventoryV2ContractError("unknown Rust listing kind")
    for field in ("cargo_target_context_spec_sha256", "stderr_raw_sha256", "stdout_raw_sha256"):
        _require_sha256_field(value[field], field)
    observation = value["cargo_build_context_observation_sha256"]
    if observation is not None:
        _require_sha256_field(observation, "cargo_build_context_observation_sha256")
    validate_child_process_identity_v1(value["child_process_identity"])
    if not isinstance(value["command_argv"], list) or not value["command_argv"]:
        raise InventoryV2ContractError("listing command_argv must be nonempty")
    for argument in value["command_argv"]:
        _require_nonempty_nfc(argument, "listing command argument")
    if value["child_process_identity"]["argv_sha256"] != raw_jcs_sha256(value["command_argv"]):
        raise InventoryV2ContractError("listing process identity does not bind command_argv")
    ids = _require_sorted_unique_jcs(value["listed_test_ids"], "compiled test IDs")
    for compiled_id in ids:
        validate_rust_compiled_test_id_v1(compiled_id)
        if compiled_id["kind"] != value["listing_kind"]:
            raise InventoryV2ContractError("compiled test ID and listing kind disagree")
    if value["listed_test_ids_sha256"] != proof_hash("kd4.rust-compiled-test-id-set.v1", ids):
        raise InventoryV2ContractError("compiled test ID set hash mismatch")


def validate_rust_compiled_listing_entry_v1(value: Any) -> None:
    _require_object(value, {"applicability_result_sha256", "cargo_target_context_spec_sha256", "compiled_test_id", "identity", "identity_sha256", "listed", "listing_execution_id"}, "RustCompiledListingEntryV1")
    for field in ("applicability_result_sha256", "cargo_target_context_spec_sha256"):
        _require_sha256_field(value[field], field)
    validate_rust_compiled_test_id_v1(value["compiled_test_id"])
    validate_executable_identity_v1(value["identity"])
    if value["identity_sha256"] != proof_hash("kd4.executable-identity.v1", value["identity"]):
        raise InventoryV2ContractError("compiled listing identity hash mismatch")
    if not isinstance(value["listed"], bool):
        raise InventoryV2ContractError("listed must be a boolean")
    _require_uuid(value["listing_execution_id"], "listing_execution_id")


def validate_active_rust_compiled_listing_authority_v1(
    value: Any, authentication_key: bytes | None = None
) -> None:
    _require_object(value, {"authentication_tag", "authority_sha256", "body", "key_id", "schema_version"}, "ActiveRustCompiledListingAuthorityV1")
    _require_schema_version(value["schema_version"], 1, "ActiveRustCompiledListingAuthorityV1")
    _require_uuid(value["key_id"], "key_id")
    _require_standard_base64_32(value["authentication_tag"], "authentication_tag")
    body = value["body"]
    fields = {"attempt_id", "authority_nonce", "channel_binding_sha256", "channel_id", "doctest_entries", "doctest_run_count", "doctest_runs", "doctest_test_count", "ending_workspace_fingerprint", "harness_entries", "harness_run_count", "harness_runs", "harness_test_count", "host", "inventory_authority", "repository_identity_sha256", "runner_process_identity", "schema_version", "starting_workspace_fingerprint", "target_applicability_projection_sha256"}
    _require_object(body, fields, "ActiveRustCompiledListingAuthorityBodyV1")
    _require_schema_version(body["schema_version"], 1, "ActiveRustCompiledListingAuthorityBodyV1")
    _require_uuid(body["attempt_id"], "attempt_id")
    _require_standard_base64_32(body["authority_nonce"], "authority_nonce")
    for field in ("channel_binding_sha256", "ending_workspace_fingerprint", "repository_identity_sha256", "starting_workspace_fingerprint", "target_applicability_projection_sha256"):
        _require_sha256_field(body[field], field)
    if body["ending_workspace_fingerprint"] != body["starting_workspace_fingerprint"]:
        raise InventoryV2ContractError("workspace changed during compiled listing")
    _require_nonempty_nfc(body["channel_id"], "channel_id")
    if body["host"] not in {"windows", "linux", "darwin"}:
        raise InventoryV2ContractError("invalid compiled listing host")
    validate_inventory_authority_ref_v1(body["inventory_authority"])
    validate_runner_process_identity_v1(body["runner_process_identity"])
    for kind in ("harness", "doctest"):
        runs = body[f"{kind}_runs"]
        entries = body[f"{kind}_entries"]
        if not isinstance(runs, list) or not isinstance(entries, list):
            raise InventoryV2ContractError("compiled listing runs and entries must be arrays")
        for run in runs:
            validate_rust_compiled_listing_run_v1(run)
            if run["listing_kind"] != kind:
                raise InventoryV2ContractError("compiled listing run is in the wrong collection")
            if run["child_process_identity"]["parent_pid"] != body["runner_process_identity"]["pid"]:
                raise InventoryV2ContractError("listing process is not a child of the runner")
        run_contexts = [run["cargo_target_context_spec_sha256"] for run in runs]
        if run_contexts != sorted(set(run_contexts)):
            raise InventoryV2ContractError("compiled listing runs must be context-sorted and unique")
        identities: list[bytes] = []
        run_ids = {run["child_process_identity"]["execution_id"] for run in runs}
        for entry in entries:
            validate_rust_compiled_listing_entry_v1(entry)
            if entry["compiled_test_id"]["kind"] != kind:
                raise InventoryV2ContractError("compiled listing entry is in the wrong collection")
            if entry["listing_execution_id"] not in run_ids:
                raise InventoryV2ContractError("compiled listing entry refers to an unknown run")
            identities.append(canonical_jcs(entry["identity"]))
        if any(left >= right for left, right in zip(identities, identities[1:])):
            raise InventoryV2ContractError("compiled listing entries must be identity-sorted and unique")
        if _require_integer(body[f"{kind}_run_count"], 0, f"{kind}_run_count") != len(runs):
            raise InventoryV2ContractError("compiled listing run count mismatch")
        listed_count = sum(len(run["listed_test_ids"]) for run in runs)
        if _require_integer(body[f"{kind}_test_count"], 0, f"{kind}_test_count") != listed_count:
            raise InventoryV2ContractError("compiled listing test count mismatch")
    expected = proof_hash("kd4.active-rust-compiled-listing-authority.v1", body)
    if value["authority_sha256"] != expected:
        raise InventoryV2ContractError("compiled listing authority hash mismatch")
    if authentication_key is not None:
        if len(authentication_key) != 32:
            raise InventoryV2ContractError("authority authentication key must be 32 bytes")
        payload = {"authority_sha256": expected, "body": body, "key_id": value["key_id"], "schema_version": 1}
        tag = hmac.new(authentication_key, b"kd4.active-rust-compiled-listing-authority.authentication.v1\0" + canonical_jcs(payload), hashlib.sha256).digest()
        if not hmac.compare_digest(base64.b64decode(value["authentication_tag"] + "="), tag):
            raise InventoryV2ContractError("compiled listing authority authentication mismatch")


def validate_target_applicability_projection_v1(value: Any) -> None:
    _require_object(value, {"entries", "host", "inventory_authority", "schema_version"}, "TargetApplicabilityProjectionV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "TargetApplicabilityProjectionV1")
    if value["host"] not in {"windows", "linux", "darwin"}:
        raise InventoryV2ContractError("invalid target applicability host")
    validate_inventory_authority_ref_v1(value["inventory_authority"])
    entries = value["entries"]
    if not isinstance(entries, list) or not entries:
        raise InventoryV2ContractError("target applicability entries must be nonempty")
    identities: list[bytes] = []
    for entry in entries:
        _require_object(
            entry,
            {"applicability_result", "identity", "identity_sha256", "platform_applicability_sha256"},
            "TargetApplicabilityProjectionEntryV1",
        )
        validate_executable_identity_v1(entry["identity"])
        identities.append(canonical_jcs(entry["identity"]))
        if entry["identity_sha256"] != proof_hash("kd4.executable-identity.v1", entry["identity"]):
            raise InventoryV2ContractError("target applicability identity hash mismatch")
        _require_sha256_field(
            entry["platform_applicability_sha256"], "platform_applicability_sha256"
        )
        validate_applicability_result_v1(entry["applicability_result"])
        if (
            entry["applicability_result"]["executable_identity_sha256"]
            != entry["identity_sha256"]
            or entry["applicability_result"]["platform_applicability_sha256"]
            != entry["platform_applicability_sha256"]
            or entry["applicability_result"]["host"] != value["host"]
        ):
            raise InventoryV2ContractError("applicability result does not bind projection entry")
    if any(left >= right for left, right in zip(identities, identities[1:])):
        raise InventoryV2ContractError("target applicability entries must be sorted and identity-unique")


def validate_active_host_applicability_authority_v1(
    value: Any, authentication_key: bytes | None = None
) -> None:
    fields = {
        "authentication_tag", "authority_sha256", "body", "key_id", "schema_version",
    }
    _require_object(value, fields, "ActiveHostApplicabilityAuthorityV1")
    _require_schema_version(value["schema_version"], 1, "ActiveHostApplicabilityAuthorityV1")
    _require_uuid(value["key_id"], "key_id")
    _require_standard_base64_32(value["authentication_tag"], "authentication_tag")
    body = value["body"]
    body_fields = {
        "authority_nonce", "inventory_authority", "schema_version",
        "target_applicability_projection", "target_applicability_projection_sha256",
    }
    _require_object(body, body_fields, "ActiveHostApplicabilityAuthorityBodyV1")
    _require_schema_version(body["schema_version"], 1, "ActiveHostApplicabilityAuthorityBodyV1")
    _require_standard_base64_32(body["authority_nonce"], "authority_nonce")
    validate_inventory_authority_ref_v1(body["inventory_authority"])
    validate_target_applicability_projection_v1(body["target_applicability_projection"])
    if body["target_applicability_projection"]["inventory_authority"] != body["inventory_authority"]:
        raise InventoryV2ContractError(
            "active-host applicability projection does not bind its inventory authority"
        )
    expected_projection_sha256 = proof_hash(
        "kd4.target-applicability-projection.v1",
        body["target_applicability_projection"],
    )
    if body["target_applicability_projection_sha256"] != expected_projection_sha256:
        raise InventoryV2ContractError("active-host applicability projection hash mismatch")
    expected_authority_sha256 = proof_hash(
        "kd4.active-host-applicability-authority.v1", body
    )
    if value["authority_sha256"] != expected_authority_sha256:
        raise InventoryV2ContractError("active-host applicability authority hash mismatch")
    if authentication_key is not None:
        if len(authentication_key) != 32:
            raise InventoryV2ContractError("authority authentication key must be 32 bytes")
        payload = {
            "authority_sha256": expected_authority_sha256,
            "body": body,
            "key_id": value["key_id"],
            "schema_version": 1,
        }
        tag = hmac.new(
            authentication_key,
            b"kd4.active-host-applicability-authority.authentication.v1\0"
            + canonical_jcs(payload),
            hashlib.sha256,
        ).digest()
        if not hmac.compare_digest(
            base64.b64decode(value["authentication_tag"] + "="), tag
        ):
            raise InventoryV2ContractError(
                "active-host applicability authority authentication mismatch"
            )


def validate_resolved_input_leaves_v1(value: Any) -> None:
    fields = {
        "contract_input_state_sha256", "execution_input_contract_sha256", "leaves", "matched_path_set_sha256", "raw_sha256",
        "schema_version", "semantic_inputs_sha256", "semantic_sha256", "self_hash", "workspace_fingerprint",
    }
    _require_object(value, fields, "ResolvedInputLeavesV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "ResolvedInputLeavesV1")
    for field in (
        "contract_input_state_sha256", "execution_input_contract_sha256", "matched_path_set_sha256", "raw_sha256",
        "semantic_inputs_sha256", "semantic_sha256", "self_hash", "workspace_fingerprint",
    ):
        _require_sha256_field(value[field], field)
    leaves = value["leaves"]
    if not isinstance(leaves, list) or not leaves:
        raise InventoryV2ContractError("resolved input leaves must be nonempty")
    paths: list[str] = []
    for leaf in leaves:
        _require_object(leaf, {"path", "provenance", "raw_sha256"}, "ResolvedInputLeafV1")
        paths.append(require_strict_repository_path(leaf["path"]))
        if leaf["provenance"] not in {"tracked", "nonignored-untracked"}:
            raise InventoryV2ContractError("invalid resolved input leaf provenance")
        _require_sha256_field(leaf["raw_sha256"], "leaf raw_sha256")
    if any(left >= right for left, right in zip(paths, paths[1:])):
        raise InventoryV2ContractError("resolved input leaves must be sorted and path-unique")
    contract_projection = {
        "execution_input_contract_sha256": value["execution_input_contract_sha256"],
        "leaves": leaves,
        "matched_path_set_sha256": value["matched_path_set_sha256"],
        "semantic_inputs_sha256": value["semantic_inputs_sha256"],
    }
    if value["contract_input_state_sha256"] != proof_hash(
        "kd4.contract-input-state.v1", contract_projection
    ):
        raise InventoryV2ContractError("resolved input leaves contract-state hash mismatch")
    semantic_projection = {
        "contract_input_state_sha256": value["contract_input_state_sha256"],
        "execution_input_contract_sha256": value["execution_input_contract_sha256"],
        "leaves": leaves,
        "matched_path_set_sha256": value["matched_path_set_sha256"],
        "raw_sha256": value["raw_sha256"],
        "schema_version": 1,
        "semantic_inputs_sha256": value["semantic_inputs_sha256"],
        "workspace_fingerprint": value["workspace_fingerprint"],
    }
    if value["semantic_sha256"] != proof_hash("kd4.resolved-input-leaves.semantic.v1", semantic_projection):
        raise InventoryV2ContractError("resolved input leaves semantic hash mismatch")
    self_projection = dict(semantic_projection)
    self_projection["semantic_sha256"] = value["semantic_sha256"]
    if value["self_hash"] != proof_hash("kd4.resolved-input-leaves.self.v1", self_projection):
        raise InventoryV2ContractError("resolved input leaves self hash mismatch")


def validate_resolved_executable_entry_v1(value: Any) -> None:
    fields = {"inventory_entry", "inventory_entry_semantic_sha256"}
    _require_object(value, fields, "ResolvedExecutableEntryV1")
    canonical_jcs(value)
    validate_executable_inventory_entry_v2(value["inventory_entry"])
    expected = proof_hash("kd4.executable-inventory-entry.v2", value["inventory_entry"])
    if value["inventory_entry_semantic_sha256"] != expected:
        raise InventoryV2ContractError(
            "resolved executable does not bind the full inventory entry"
        )


def _validate_identity_selector_binding(
    identity: Any, identity_sha256: Any, runner_selector: Any, runner_selector_sha256: Any
) -> None:
    validate_executable_identity_v1(identity)
    validate_runner_selector_v1(runner_selector)
    if identity_sha256 != proof_hash("kd4.executable-identity.v1", identity):
        raise InventoryV2ContractError("executable identity hash mismatch")
    if runner_selector_sha256 != proof_hash("kd4.runner-selector.v1", runner_selector):
        raise InventoryV2ContractError("runner selector hash mismatch")
    if identity["kind"] == "action":
        if runner_selector != {
            "action_id": identity["action_id"],
            "kind": "non-test-action",
        }:
            raise InventoryV2ContractError("action selection has a test route or non-action selector")
    else:
        runner_kind_by_route = {
            route_id: runner_kind for runner_kind, route_id in _ROUTE_BY_KIND.items()
        }
        if runner_selector["kind"] != runner_kind_by_route.get(identity["route_id"]):
            raise InventoryV2ContractError("test selection route or selector fields mismatch")


def validate_selection_v1(value: Any) -> None:
    fields = {
        "activated_policy_sha256", "host", "intended_count", "inventory_authority",
        "repository_identity_sha256", "request_sha256", "resolved_entries", "resolved_entries_sha256",
        "schema_version", "target_applicability_sha256", "validation_execution_contract_sha256", "validation_id",
    }
    _require_object(value, fields, "SelectionV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "SelectionV1")
    if value["host"] not in {"windows", "linux", "darwin"}:
        raise InventoryV2ContractError("invalid SelectionV1 host")
    require_identifier(value["validation_id"])
    validate_inventory_authority_ref_v1(value["inventory_authority"])
    entries = value["resolved_entries"]
    if not isinstance(entries, list) or not entries:
        raise InventoryV2ContractError("resolved_entries must be nonempty")
    _require_integer(value["intended_count"], 1, "intended_count")
    if value["intended_count"] != len(entries):
        raise InventoryV2ContractError("SelectionV1 intended count mismatch")
    for entry in entries:
        validate_resolved_executable_entry_v1(entry)
        if entry["inventory_entry"]["executable_identity"]["validation_id"] != value["validation_id"]:
            raise InventoryV2ContractError("selected entry maps to a different validation")
    identity_keys = [canonical_jcs(entry["inventory_entry"]["executable_identity"]) for entry in entries]
    if identity_keys != sorted(set(identity_keys)):
        raise InventoryV2ContractError(
            "SelectionV1 resolved entry identities must be strictly sorted and unique"
        )
    expected = proof_hash("kd4.resolved-executable-entry-set.v1", entries)
    if value["resolved_entries_sha256"] != expected:
        raise InventoryV2ContractError("SelectionV1 resolved entry set hash mismatch")
    for field in (
        "activated_policy_sha256", "repository_identity_sha256", "request_sha256",
        "target_applicability_sha256", "validation_execution_contract_sha256",
    ):
        _require_sha256_field(value[field], field)


def validate_trusted_defect_receipt_v1(value: Any) -> None:
    fields = {
        "baseline_ids", "baseline_obligation_ids", "defect_id", "failure", "incorrect_behavior", "mutation", "pass",
        "receipt_sha256", "replacement_edge_ids", "resolved_entry_set_sha256", "schema_version", "selection_v1_sha256",
    }
    _require_object(value, fields, "TrustedDefectReceiptV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "TrustedDefectReceiptV1")
    _require_sorted_unique_strings(value["baseline_ids"], "baseline_ids", nonempty=True)
    _require_sorted_unique_strings(value["baseline_obligation_ids"], "baseline_obligation_ids", nonempty=True)
    _require_nonempty_nfc(value["defect_id"], "defect_id")
    _require_nonempty_nfc(value["incorrect_behavior"], "incorrect_behavior")
    _require_sorted_unique_strings(value["replacement_edge_ids"], "replacement_edge_ids", nonempty=True)
    failure = _require_object(value["failure"], {"attempt_id", "classification", "execution_ids", "focused_projection_sha256", "mutation_epoch", "workspace_fingerprint"}, "ConfirmedFailureReceiptV1")
    passed = _require_object(value["pass"], {"attempt_id", "classification", "execution_ids", "focused_projection_sha256", "mutation_epoch", "workspace_fingerprint"}, "ConfirmedPassReceiptV1")
    mutation = _require_object(value["mutation"], {"changed_input_leaf_ids", "classification", "from_epoch", "input_contract_set_sha256", "production_delta_sha256", "to_epoch"}, "RelevantMutationReceiptV1")
    if failure["classification"] != "confirmed-validation-failure" or passed["classification"] != "confirmed-pass" or mutation["classification"] != "relevant-non-test-product-runtime":
        raise InventoryV2ContractError("trusted defect receipt classification mismatch")
    for receipt, label in ((failure, "failure"), (passed, "pass")):
        _require_nonempty_nfc(receipt["attempt_id"], f"{label} attempt_id")
        _require_sorted_unique_strings(receipt["execution_ids"], f"{label} execution_ids", nonempty=True)
        _require_integer(receipt["mutation_epoch"], 0, f"{label} mutation_epoch")
        _require_sha256_field(receipt["focused_projection_sha256"], "focused_projection_sha256")
        _require_sha256_field(receipt["workspace_fingerprint"], "workspace_fingerprint")
    _require_sorted_unique_strings(mutation["changed_input_leaf_ids"], "changed_input_leaf_ids", nonempty=True)
    _require_integer(mutation["from_epoch"], 0, "from_epoch")
    _require_integer(mutation["to_epoch"], 1, "to_epoch")
    if failure["mutation_epoch"] != mutation["from_epoch"] or passed["mutation_epoch"] != mutation["to_epoch"] or mutation["to_epoch"] <= mutation["from_epoch"]:
        raise InventoryV2ContractError("trusted defect receipt epoch flow mismatch")
    for field in ("input_contract_set_sha256", "production_delta_sha256"):
        _require_sha256_field(mutation[field], field)
    for field in ("receipt_sha256", "resolved_entry_set_sha256", "selection_v1_sha256"):
        _require_sha256_field(value[field], field)
    projection = {key: item for key, item in value.items() if key != "receipt_sha256"}
    if value["receipt_sha256"] != proof_hash("kd4.trusted-defect-receipt.v1", projection):
        raise InventoryV2ContractError("trusted defect receipt hash mismatch")


def validate_intended_execution_projection_v1(value: Any) -> None:
    _require_object(
        value,
        {"attempt_id", "executable_identities", "intended_count", "inventory_authority", "selection_v1_sha256", "validation_id"},
        "IntendedExecutionProjectionV1",
    )
    canonical_jcs(value)
    _require_nonempty_nfc(value["attempt_id"], "attempt_id")
    require_identifier(value["validation_id"])
    validate_inventory_authority_ref_v1(value["inventory_authority"])
    _require_sha256_field(value["selection_v1_sha256"], "selection_v1_sha256")
    identities = _require_sorted_unique_jcs(
        value["executable_identities"], "intended executable identities", nonempty=True
    )
    _require_integer(value["intended_count"], 1, "intended_count")
    if value["intended_count"] != len(identities):
        raise InventoryV2ContractError("intended execution count mismatch")
    for identity in identities:
        validate_executable_identity_v1(identity)
        if identity["validation_id"] != value["validation_id"]:
            raise InventoryV2ContractError("intended identity belongs to another validation")


def validate_validation_receipt_projection_v1(value: Any) -> None:
    _require_object(
        value,
        {"attempt_id", "classification", "executed_count", "intended_execution_projection", "intended_execution_projection_sha256", "mismatch_codes", "outcomes", "schema_version", "selected_count", "started_count", "terminal_count", "validation_id"},
        "ValidationReceiptProjectionV1",
    )
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "ValidationReceiptProjectionV1")
    _require_nonempty_nfc(value["attempt_id"], "attempt_id")
    require_identifier(value["validation_id"])
    intended = value["intended_execution_projection"]
    validate_intended_execution_projection_v1(intended)
    if (
        value["attempt_id"] != intended["attempt_id"]
        or value["validation_id"] != intended["validation_id"]
        or value["intended_execution_projection_sha256"]
        != proof_hash("kd4.intended-execution-projection.v1", intended)
    ):
        raise InventoryV2ContractError("validation receipt does not bind its intended execution projection")
    _require_sorted_unique_strings(value["mismatch_codes"], "mismatch_codes")
    counts = {
        field: _require_integer(value[field], 0, field)
        for field in ("selected_count", "started_count", "terminal_count", "executed_count")
    }
    outcomes = value["outcomes"]
    if not isinstance(outcomes, list):
        raise InventoryV2ContractError("validation outcomes must be an array")
    intended_encodings = {canonical_jcs(identity) for identity in intended["executable_identities"]}
    observed_encodings: list[bytes] = []
    execution_ids: set[str] = set()
    for outcome in outcomes:
        _require_object(outcome, {"execution_id", "identity", "outcome"}, "ExecutedOutcomeV1")
        execution_id = _require_nonempty_nfc(outcome["execution_id"], "execution_id")
        if execution_id in execution_ids:
            raise InventoryV2ContractError("execution IDs must be unique")
        execution_ids.add(execution_id)
        validate_executable_identity_v1(outcome["identity"])
        encoded = canonical_jcs(outcome["identity"])
        if encoded not in intended_encodings:
            raise InventoryV2ContractError("validation outcome is outside intended selection")
        observed_encodings.append(encoded)
        if outcome["outcome"] not in {"passed", "failed"}:
            raise InventoryV2ContractError("invalid executed outcome")
    if any(left >= right for left, right in zip(observed_encodings, observed_encodings[1:])):
        raise InventoryV2ContractError("validation outcomes must follow intended identity order")
    intended_count = intended["intended_count"]
    observed_consistent = (
        counts["selected_count"] <= intended_count
        and counts["started_count"] <= counts["selected_count"]
        and counts["terminal_count"] <= counts["started_count"]
        and counts["terminal_count"] == counts["executed_count"] == len(outcomes)
    )
    complete = (
        observed_consistent
        and counts["selected_count"]
        == counts["started_count"]
        == counts["terminal_count"]
        == intended_count
    )
    classification = value["classification"]
    if classification == "confirmed-pass":
        valid = complete and not value["mismatch_codes"] and all(outcome["outcome"] == "passed" for outcome in outcomes)
    elif classification == "confirmed-validation-failure":
        valid = (
            observed_consistent
            and counts["selected_count"] == intended_count
            and bool(outcomes)
            and any(outcome["outcome"] == "failed" for outcome in outcomes)
        )
    elif classification == "pre-result-error":
        valid = (
            observed_consistent
            and not complete
            and bool(value["mismatch_codes"])
            and all(outcome["outcome"] == "passed" for outcome in outcomes)
        )
    else:
        valid = False
    if not valid:
        raise InventoryV2ContractError("validation receipt classification/count/outcome mismatch")


_ROUTE_BY_KIND = {
    "argument-comment-lint-native": "test-route.argument-comment-lint-native.v1",
    "javascript-jest": "test-route.javascript-jest.v1",
    "python-pytest": "test-route.python-pytest.v1",
    "python-unittest": "test-route.python-unittest.v1",
    "rust-doctest": "test-route.rust-doctest.v1",
    "rust-nextest": "test-route.rust-nextest.v1",
    "windows-sandbox-smoke-native": "test-route.windows-sandbox-smoke-native.v1",
}


def validate_executable_inventory_entry_v2(value: Any) -> None:
    fields = {
        "cargo_target_context_spec_sha256", "executable_identity",
        "executable_identity_sha256", "execution_input_contract_sha256",
        "platform_applicability", "platform_applicability_sha256", "runner_selector",
        "runner_selector_sha256", "test_route_id", "validation_id",
    }
    _require_object(value, fields, "ExecutableInventoryEntryV2")
    canonical_jcs(value)
    validate_executable_identity_v1(value["executable_identity"])
    validate_platform_applicability_v1(value["platform_applicability"])
    validate_runner_selector_v1(value["runner_selector"])
    expected_applicability = proof_hash(
        "kd4.platform-applicability.v1", value["platform_applicability"]
    )
    if value["platform_applicability_sha256"] != expected_applicability:
        raise InventoryV2ContractError("inventory entry applicability hash mismatch")
    _validate_identity_selector_binding(
        value["executable_identity"], value["executable_identity_sha256"],
        value["runner_selector"], value["runner_selector_sha256"],
    )
    identity = value["executable_identity"]
    if identity["validation_id"] != value["validation_id"]:
        raise InventoryV2ContractError("inventory entry validation disagrees with identity")
    if identity["kind"] == "action":
        if value["test_route_id"] is not None:
            raise InventoryV2ContractError("action inventory entry cannot carry a test route")
    elif value["test_route_id"] != identity["route_id"]:
        raise InventoryV2ContractError("inventory entry route disagrees with identity")
    cargo_sha = value["cargo_target_context_spec_sha256"]
    rust_identity = identity.get("route_id") in {
        "test-route.rust-nextest.v1",
        "test-route.rust-doctest.v1",
    }
    if rust_identity:
        _require_sha256_field(cargo_sha, "cargo_target_context_spec_sha256")
        if value["runner_selector"]["cargo_target_context_spec_sha256"] != cargo_sha:
            raise InventoryV2ContractError(
                "Rust selector and inventory entry disagree on Cargo target context"
            )
    elif cargo_sha is not None:
        raise InventoryV2ContractError(
            "Cargo target context is allowed only for Rust test entries"
        )
    if value["platform_applicability"]["kind"] == "rust-cfg" and not rust_identity:
        raise InventoryV2ContractError(
            "Rust cfg applicability is allowed only for Rust test entries"
        )


def validate_provenance_receipt_v1(value: Any) -> None:
    fields = {
        "evidence_paths", "evidence_sha256", "kind", "receipt_sha256", "schema_version"
    }
    _require_object(value, fields, "ProvenanceReceiptV1")
    _require_schema_version(value["schema_version"], 1, "ProvenanceReceiptV1")
    if value["kind"] not in {
        "helper-driven", "generated", "protected", "live-service", "off-host",
        "platform-pending", "source-declaration",
    }:
        raise InventoryV2ContractError("unknown provenance receipt kind")
    paths = value["evidence_paths"]
    if not isinstance(paths, list) or not paths:
        raise InventoryV2ContractError("provenance receipt requires evidence paths")
    for path in paths:
        require_strict_repository_path(path)
    if paths != sorted(set(paths)):
        raise InventoryV2ContractError("provenance evidence paths must be sorted and unique")
    _require_sha256_field(value["evidence_sha256"], "evidence_sha256")
    _require_sha256_field(value["receipt_sha256"], "receipt_sha256")
    projection = {key: value[key] for key in fields if key != "receipt_sha256"}
    if value["receipt_sha256"] != proof_hash("kd4.provenance-receipt.v1", projection):
        raise InventoryV2ContractError("provenance receipt hash mismatch")


def inventory_declaration_id_v2(
    kind: str, entry: Any, source_provenance: Any
) -> str:
    if kind == "missing-baseline":
        domain = "kd4.missing-baseline-declaration-id.v2"
        prefix = "missing-baseline-declaration-v2."
    elif kind == "post-baseline-current":
        domain = "kd4.post-baseline-declaration-id.v2"
        prefix = "post-baseline-declaration-v2."
    else:
        raise InventoryV2ContractError("declaration ID requires a mutable declaration kind")
    return prefix + proof_hash(
        domain,
        {"entry": entry, "kind": kind, "source_provenance": source_provenance},
    )


def inventory_declaration_obligation_id_v2(
    kind: str, entry: Any, source_provenance: Any
) -> str:
    declaration_id = inventory_declaration_id_v2(kind, entry, source_provenance)
    if kind == "missing-baseline":
        prefix = "inventory-obligation-v2.missing-declaration."
    else:
        prefix = "inventory-obligation-v2.post-baseline."
    return prefix + proof_hash(
        "kd4.inventory-declaration-obligation-id.v2",
        {"declaration_id": declaration_id, "kind": kind},
    )


def frozen_baseline_obligation_id_v2(declaration: Any) -> str:
    _require_object(
        declaration,
        {"baseline_id", "entry", "kind", "predecessor_entry_sha256"},
        "frozen baseline declaration",
    )
    if declaration["kind"] != "frozen-baseline":
        raise InventoryV2ContractError("frozen obligation ID requires a frozen declaration")
    return "inventory-obligation-v2.frozen-baseline." + proof_hash(
        "kd4.frozen-baseline-obligation-id.v2", declaration
    )


def validate_inventory_declaration_v2(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise InventoryV2ContractError("InventoryDeclarationV2 must be an object")
    kind = value.get("kind")
    if kind == "frozen-baseline":
        _require_object(value, {"baseline_id", "entry", "kind", "predecessor_entry_sha256"}, "frozen baseline declaration")
        _require_nonempty_nfc(value["baseline_id"], "baseline_id")
        _require_sha256_field(value["predecessor_entry_sha256"], "predecessor_entry_sha256")
        validate_executable_inventory_entry_v2(value["entry"])
    elif kind in {"missing-baseline", "post-baseline-current"}:
        _require_object(
            value,
            {"declaration_id", "entry", "kind", "obligation_id", "source_provenance"},
            f"{kind} declaration",
        )
        validate_provenance_receipt_v1(value["source_provenance"])
        validate_executable_inventory_entry_v2(value["entry"])
        expected_declaration_id = inventory_declaration_id_v2(
            kind, value["entry"], value["source_provenance"]
        )
        expected_obligation_id = inventory_declaration_obligation_id_v2(
            kind, value["entry"], value["source_provenance"]
        )
        if (
            value["declaration_id"] != expected_declaration_id
            or value["obligation_id"] != expected_obligation_id
        ):
            raise InventoryV2ContractError(
                f"{kind} declaration IDs do not bind the exact entry and typed provenance"
            )
    else:
        raise InventoryV2ContractError("unknown inventory declaration kind")
    return value["entry"]


def validate_predecessor_artifact_reconciliation_v1(value: Any) -> None:
    fields = {
        "frozen_baseline_associations", "frozen_baseline_associations_sha256",
        "frozen_baseline_ids", "frozen_baseline_ids_sha256",
        "frozen_inventory_raw_sha256", "frozen_inventory_semantic_sha256",
        "frozen_ledger_raw_sha256", "projection_sha256", "schema_version",
    }
    _require_object(value, fields, "PredecessorArtifactReconciliationV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "PredecessorArtifactReconciliationV1")
    ids = _require_sorted_unique_strings(
        value["frozen_baseline_ids"], "frozen baseline IDs", nonempty=True
    )
    if len(ids) != 15_544:
        raise InventoryV2ContractError(
            "predecessor reconciliation requires every one of the 15,544 baseline IDs"
        )
    associations = value["frozen_baseline_associations"]
    if not isinstance(associations, list) or len(associations) != 15_544:
        raise InventoryV2ContractError(
            "predecessor reconciliation requires 15,544 baseline associations"
        )
    for association in associations:
        _require_object(
            association, {"baseline_id", "predecessor_entry_sha256"},
            "FrozenBaselineAssociationV1",
        )
        _require_nonempty_nfc(association["baseline_id"], "baseline association ID")
        _require_sha256_field(
            association["predecessor_entry_sha256"], "predecessor_entry_sha256"
        )
    association_ids = [association["baseline_id"] for association in associations]
    if association_ids != ids:
        raise InventoryV2ContractError(
            "frozen baseline associations must exactly bind every predecessor ID"
        )
    if value["frozen_baseline_associations_sha256"] != FROZEN_V1_BASELINE_ASSOCIATIONS_SHA256:
        raise InventoryV2ContractError("frozen baseline association anchor mismatch")
    if proof_hash(
        "kd4.frozen-baseline-association-set.v1", associations
    ) != value["frozen_baseline_associations_sha256"]:
        raise InventoryV2ContractError("frozen baseline association set hash mismatch")
    expected_anchors = {
        "frozen_baseline_ids_sha256": FROZEN_V1_BASELINE_IDS_SHA256,
        "frozen_baseline_associations_sha256": FROZEN_V1_BASELINE_ASSOCIATIONS_SHA256,
        "frozen_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "frozen_inventory_semantic_sha256": FROZEN_V1_INVENTORY_SEMANTIC_SHA256,
        "frozen_ledger_raw_sha256": FROZEN_V1_LEDGER_RAW_SHA256,
    }
    for field, expected in expected_anchors.items():
        _require_sha256_field(value[field], field)
        if value[field] != expected:
            raise InventoryV2ContractError(
                "predecessor reconciliation does not bind the exact frozen V1 artifacts"
            )
    if proof_hash("kd4.frozen-baseline-id-set.v1", ids) != value["frozen_baseline_ids_sha256"]:
        raise InventoryV2ContractError("frozen baseline ID set hash mismatch")
    projection = {key: value[key] for key in fields if key != "projection_sha256"}
    _require_sha256_field(value["projection_sha256"], "projection_sha256")
    if value["projection_sha256"] != proof_hash(
        "kd4.predecessor-artifact-reconciliation.v1", projection
    ):
        raise InventoryV2ContractError("predecessor reconciliation projection hash mismatch")


def _validate_observed_predecessor_ids(
    reconciliation: dict[str, Any], observed: list[str], label: str
) -> None:
    _require_sorted_unique_strings(observed, label, nonempty=True)
    if observed != reconciliation["frozen_baseline_ids"]:
        raise InventoryV2ContractError(
            f"{label} do not exactly reconcile every frozen V1 baseline ID"
        )


def validate_schema_resource_set_v1(
    resources: Any, resources_sha256: Any
) -> None:
    resources = _require_sorted_unique_jcs(
        resources, "schema resources", nonempty=True
    )
    if len(resources) != 5:
        raise InventoryV2ContractError("inventory requires exactly five schema resources")
    for resource in resources:
        _require_object(
            resource, {"path", "raw_sha256", "schema_id"}, "SchemaResourceRefV1"
        )
        require_strict_repository_path(resource["path"])
        _require_sha256_field(resource["raw_sha256"], "schema resource raw_sha256")
        _require_nonempty_nfc(resource["schema_id"], "schema resource schema_id")
    actual = tuple(
        (resource["path"], resource["raw_sha256"], resource["schema_id"])
        for resource in resources
    )
    expected = tuple(
        zip(INVENTORY_V2_SCHEMA_PATHS, INVENTORY_V2_SCHEMA_RAW_SHA256S, INVENTORY_V2_SCHEMA_IDS)
    )
    if actual != expected:
        raise InventoryV2ContractError(
            "inventory schema resource paths/raw hashes/schema IDs are substituted or reordered"
        )
    _require_sha256_field(resources_sha256, "schema_resources_sha256")
    if resources_sha256 != proof_hash(
        "kd4.inventory-v2-schema-resource-set.v1", resources
    ):
        raise InventoryV2ContractError("inventory schema resource set hash mismatch")


def validate_frozen_test_inventory_v2(value: Any) -> None:
    fields = {
        "action_routes", "authority", "cargo_target_context_specs", "declaration_universe", "execution_input_contracts",
        "format_id", "predecessor", "predecessor_reconciliation", "recovery_authority",
        "routes", "schema_resources", "schema_resources_sha256", "schema_version",
    }
    _require_object(value, fields, "FrozenTestInventoryV2")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 2, "FrozenTestInventoryV2")
    if value["format_id"] != "kd4-frozen-test-inventory-v2":
        raise InventoryV2ContractError("invalid FrozenTestInventoryV2 format")
    authority = _require_object(
        value["authority"],
        {"format_id", "raw_sha256", "schema_sha256", "semantic_sha256", "self_hash"},
        "InventoryAuthorityV1",
    )
    if authority["format_id"] != value["format_id"]:
        raise InventoryV2ContractError("inventory authority format mismatch")
    for field in ("raw_sha256", "schema_sha256", "semantic_sha256", "self_hash"):
        _require_sha256_field(authority[field], field)
    predecessor = _require_object(
        value["predecessor"],
        {"inventory_hash", "raw_sha256", "recorded_baseline_workspace_fingerprint", "test_count"},
        "PredecessorInventoryV1",
    )
    for field in ("inventory_hash", "raw_sha256", "recorded_baseline_workspace_fingerprint"):
        _require_sha256_field(predecessor[field], field)
    if _require_integer(predecessor["test_count"], 0, "predecessor test_count") != 15544:
        raise InventoryV2ContractError("predecessor test_count must remain 15544")
    if (
        predecessor["inventory_hash"] != FROZEN_V1_INVENTORY_SEMANTIC_SHA256
        or predecessor["raw_sha256"] != FROZEN_V1_INVENTORY_RAW_SHA256
        or predecessor["recorded_baseline_workspace_fingerprint"]
        != FROZEN_V1_WORKSPACE_FINGERPRINT
    ):
        raise InventoryV2ContractError("predecessor metadata does not bind the exact frozen V1 inventory")
    reconciliation = value["predecessor_reconciliation"]
    validate_predecessor_artifact_reconciliation_v1(reconciliation)
    validate_inventory_authority_ref_v1(value["recovery_authority"])

    context_specs = value["cargo_target_context_specs"]
    if not isinstance(context_specs, list):
        raise InventoryV2ContractError("cargo_target_context_specs must be an array")
    context_hashes: list[str] = []
    context_by_hash: dict[str, dict[str, Any]] = {}
    for context in context_specs:
        validate_cargo_target_context_spec_v1(context)
        context_hash = context["context_sha256"]
        context_hashes.append(context_hash)
        context_by_hash[context_hash] = context
    if context_hashes != sorted(set(context_hashes)):
        raise InventoryV2ContractError(
            "Cargo target contexts must be strictly sorted by context_sha256"
        )

    expected_routes = list(_ROUTE_BY_KIND.values())
    routes = value["routes"]
    if not isinstance(routes, list) or [route.get("route_id") if isinstance(route, dict) else None for route in routes] != expected_routes:
        raise InventoryV2ContractError("inventory must contain all seven routes in canonical order")
    route_validations: dict[str, str] = {}
    for route, runner_kind in zip(routes, _ROUTE_BY_KIND):
        _require_object(route, {"route_id", "runner_kind", "validation_id"}, "TestRouteV1")
        if route["runner_kind"] != runner_kind:
            raise InventoryV2ContractError("test route and runner kind disagree")
        require_identifier(route["validation_id"])
        route_validations[route["route_id"]] = route["validation_id"]

    action_routes = _require_sorted_unique_jcs(value["action_routes"], "action routes", nonempty=True)
    if [route.get("action_id") if isinstance(route, dict) else None for route in action_routes] != [
        "documentation.markdown", "maintenance.source-map"
    ]:
        raise InventoryV2ContractError(
            "inventory must contain exactly the documentation and source-map action routes"
        )
    for route in action_routes:
        _require_object(
            route,
            {"action_id", "execution_input_contract_sha256", "validation_id"},
            "ActionRouteV1",
        )
        require_identifier(route["action_id"])
        require_identifier(route["validation_id"])
        _require_sha256_field(route["execution_input_contract_sha256"], "execution_input_contract_sha256")

    contracts = _require_sorted_unique_jcs(
        value["execution_input_contracts"], "execution input contracts", nonempty=True
    )
    contract_hashes: set[str] = set()
    contract_ids: set[str] = set()
    for contract in contracts:
        validate_execution_input_contract_v1(contract)
        if contract["contract_id"] in contract_ids or contract["contract_sha256"] in contract_hashes:
            raise InventoryV2ContractError("execution input contract IDs and hashes must be unique")
        contract_ids.add(contract["contract_id"])
        contract_hashes.add(contract["contract_sha256"])
    for route in action_routes:
        if route["execution_input_contract_sha256"] not in contract_hashes:
            raise InventoryV2ContractError("action route refers to an unknown execution input contract")
    if len({route["execution_input_contract_sha256"] for route in action_routes}) != len(action_routes):
        raise InventoryV2ContractError("each action route requires its own execution input contract")

    resources = value["schema_resources"]
    validate_schema_resource_set_v1(resources, value["schema_resources_sha256"])

    declarations = _require_sorted_unique_jcs(value["declaration_universe"], "inventory declarations")
    identity_encodings: list[bytes] = []
    for declaration in declarations:
        entry = validate_inventory_declaration_v2(declaration)
        if entry["execution_input_contract_sha256"] not in contract_hashes:
            raise InventoryV2ContractError("inventory entry refers to an unknown execution input contract")
        identity = entry["executable_identity"]
        context_sha = entry["cargo_target_context_spec_sha256"]
        if context_sha is not None:
            if context_sha not in context_by_hash:
                raise InventoryV2ContractError(
                    "inventory entry refers to an unknown Cargo target context"
                )
            if (
                entry["runner_selector"]["kind"] == "rust-doctest"
                and context_by_hash[context_sha]["target_kind"] not in {"lib", "proc-macro"}
            ):
                raise InventoryV2ContractError(
                    "Rust doctest entry requires a lib or proc-macro Cargo target"
                )
        identity_encodings.append(canonical_jcs(identity))
        if identity["kind"] == "action":
            matches = [
                route for route in action_routes
            if route["action_id"] == identity["action_id"]
                and route["validation_id"] == identity["validation_id"]
                and route["execution_input_contract_sha256"] == entry["execution_input_contract_sha256"]
            ]
        else:
            matches = [
                route for route in routes
                if route["route_id"] == identity["route_id"]
                and route["validation_id"] == identity["validation_id"]
            ]
        if len(matches) != 1:
            raise InventoryV2ContractError("inventory declaration does not map to exactly one route")
    if len(identity_encodings) != len(set(identity_encodings)):
        raise InventoryV2ContractError("inventory declaration executable identities must be unique")
    observed_baseline_ids = sorted(
        declaration["baseline_id"]
        for declaration in declarations
        if declaration["kind"] == "frozen-baseline"
    )
    _validate_observed_predecessor_ids(
        reconciliation, observed_baseline_ids, "inventory frozen-baseline declarations"
    )
    association_by_id = {
        association["baseline_id"]: association["predecessor_entry_sha256"]
        for association in reconciliation["frozen_baseline_associations"]
    }
    for declaration in declarations:
        if declaration["kind"] == "frozen-baseline" and association_by_id.get(
            declaration["baseline_id"]
        ) != declaration["predecessor_entry_sha256"]:
            raise InventoryV2ContractError(
                "frozen baseline declaration substituted its predecessor entry association"
            )

    semantic_projection = {key: value[key] for key in (
        "action_routes", "cargo_target_context_specs", "declaration_universe", "execution_input_contracts", "format_id",
        "predecessor", "predecessor_reconciliation", "recovery_authority", "routes",
        "schema_resources", "schema_resources_sha256", "schema_version",
    )}
    if authority["semantic_sha256"] != proof_hash("kd4.frozen-test-inventory-v2.semantic", semantic_projection):
        raise InventoryV2ContractError("inventory authority semantic hash mismatch")
    authority_projection = {key: authority[key] for key in (
        "format_id", "raw_sha256", "schema_sha256", "semantic_sha256"
    )}
    if authority["self_hash"] != proof_hash(
        "kd4.frozen-test-inventory-v2.authority.self.v1", authority_projection
    ):
        raise InventoryV2ContractError("inventory authority self hash mismatch")


def _validate_candidate_replacement_contract_v1(value: Any) -> None:
    fields = {
        "candidate_receipt_sha256", "executable_identity", "executable_identity_sha256",
        "execution_input_contract_sha256", "platform_applicability_sha256", "replacement_id",
        "runner_selector", "runner_selector_sha256", "test_route_id", "validation_id",
    }
    _require_object(value, fields, "CandidateReplacementContractV1")
    _require_nonempty_nfc(value["replacement_id"], "replacement_id")
    _require_sha256_field(value["candidate_receipt_sha256"], "candidate_receipt_sha256")
    _validate_identity_selector_binding(
        value["executable_identity"], value["executable_identity_sha256"],
        value["runner_selector"], value["runner_selector_sha256"],
    )
    identity = value["executable_identity"]
    if (
        identity["kind"] != "test"
        or identity["route_id"] != value["test_route_id"]
        or identity["validation_id"] != value["validation_id"]
    ):
        raise InventoryV2ContractError(
            "replacement candidate identity, route, and validation disagree"
        )


def _validate_legacy_replacement_hint_v1(value: Any) -> None:
    _require_object(value, {"predecessor_row_sha256", "replacement_ids"}, "LegacyReplacementHintV1")
    _require_sha256_field(value["predecessor_row_sha256"], "predecessor_row_sha256")
    _require_sorted_unique_strings(value["replacement_ids"], "replacement_ids", nonempty=True)


def _validate_replacement_contract_disposition_v1(value: Any) -> None:
    _require_object(value, {"accepted", "candidate", "legacy_replacement_hint", "state"}, "ReplacementContractDispositionV1")
    _validate_legacy_replacement_hint_v1(value["legacy_replacement_hint"])
    state = value["state"]
    if state == "pending-review":
        if value["accepted"] is not None or value["candidate"] is not None:
            raise InventoryV2ContractError("pending review contract must not carry candidate data")
    elif state == "focused-candidate":
        if value["accepted"] is not None or not isinstance(value["candidate"], dict):
            raise InventoryV2ContractError("focused candidate contract has invalid nullable fields")
        _validate_candidate_replacement_contract_v1(value["candidate"])
    elif state == "accepted":
        if value["candidate"] is not None or not isinstance(value["accepted"], dict):
            raise InventoryV2ContractError("accepted contract has invalid nullable fields")
        accepted = _require_object(
            value["accepted"],
            {"accepted_receipt_sha256", "candidate", "contract_sources_sha256", "product_behavior_obligation_sha256", "resolved_entry_set_sha256", "runtime_path_sha256", "selection_v1_sha256", "trusted_defect_receipt_sha256s"},
            "AcceptedReplacementContractV1",
        )
        _validate_candidate_replacement_contract_v1(accepted["candidate"])
        for field in ("accepted_receipt_sha256", "contract_sources_sha256", "product_behavior_obligation_sha256", "runtime_path_sha256"):
            _require_sha256_field(accepted[field], field)
        for field in ("resolved_entry_set_sha256", "selection_v1_sha256"):
            if accepted[field] is not None:
                _require_sha256_field(accepted[field], field)
        hashes = accepted["trusted_defect_receipt_sha256s"]
        if hashes is not None:
            if not isinstance(hashes, list) or not hashes:
                raise InventoryV2ContractError(
                    "trusted defect receipt hashes must be null or nonempty"
                )
            for digest in hashes:
                _require_sha256_field(digest, "trusted_defect_receipt_sha256")
            if hashes != sorted(set(hashes)):
                raise InventoryV2ContractError(
                    "trusted defect receipt hashes must be sorted and unique"
                )
    else:
        raise InventoryV2ContractError("unknown replacement contract state")


def _validate_exception_provenance(tag: str, provenance: Any) -> None:
    validate_provenance_receipt_v1(provenance)
    if provenance["kind"] != tag:
        raise InventoryV2ContractError("exception tag and typed provenance kind disagree")


def _validate_ledger_disposition_v2(value: Any) -> None:
    if not isinstance(value, dict):
        raise InventoryV2ContractError("replacement ledger disposition must be an object")
    kind = value.get("kind")
    if kind == "unresolved":
        _require_object(value, {"kind"}, "unresolved disposition")
    elif kind == "current":
        _require_object(value, {"inventory_entry_semantic_sha256", "kind"}, "current disposition")
        _require_sha256_field(value["inventory_entry_semantic_sha256"], "inventory_entry_semantic_sha256")
    elif kind == "replacement":
        _require_object(value, {"contract", "edge_ids", "kind", "stage2_incorrect_behavior_ids"}, "replacement disposition")
        _require_sorted_unique_strings(value["edge_ids"], "replacement edge IDs", nonempty=True)
        stage2_ids = value["stage2_incorrect_behavior_ids"]
        if stage2_ids is not None:
            _require_sorted_unique_strings(stage2_ids, "Stage2 incorrect behavior IDs")
        _validate_replacement_contract_disposition_v1(value["contract"])
    elif kind == "recovered-container":
        _require_object(value, {"child_obligation_ids", "kind", "transition_receipt_sha256"}, "recovered-container disposition")
        _require_sorted_unique_strings(value["child_obligation_ids"], "child obligation IDs", nonempty=True)
        _require_sha256_field(value["transition_receipt_sha256"], "transition_receipt_sha256")
    elif kind == "exception":
        _require_object(value, {"exception", "kind"}, "exception disposition")
        exception = value["exception"]
        exception_kind = exception.get("kind") if isinstance(exception, dict) else None
        if exception_kind == "pending-legacy":
            _require_object(exception, {"kind", "provenance_receipt", "tag"}, "pending legacy exception")
            _validate_exception_provenance(exception["tag"], exception["provenance_receipt"])
        elif exception_kind == "accepted":
            fields = {
                "active_host_authority", "kind", "provenance_receipt",
                "receipt_sha256", "tag",
            }
            _require_object(exception, fields, "accepted exception")
            validate_active_host_applicability_authority_v1(
                exception["active_host_authority"]
            )
            _validate_exception_provenance(exception["tag"], exception["provenance_receipt"])
            _require_sha256_field(exception["receipt_sha256"], "receipt_sha256")
            projection = {key: exception[key] for key in fields if key not in {"kind", "receipt_sha256"}}
            if exception["receipt_sha256"] != proof_hash("kd4.accepted-exception-receipt.v1", projection):
                raise InventoryV2ContractError("accepted exception receipt hash mismatch")
        else:
            raise InventoryV2ContractError("unknown exception disposition")
        if exception["tag"] not in {"protected", "generated", "live-service", "off-host", "platform-pending"}:
            raise InventoryV2ContractError("unknown exception tag")
    else:
        raise InventoryV2ContractError("unknown replacement ledger disposition")


def validate_test_replacement_ledger_v2(value: Any) -> None:
    _require_object(
        value,
        {"format_id", "inventory_authority", "rows", "schema_version", "self_hash", "semantic_sha256", "trusted_defect_receipts"},
        "TestReplacementLedgerV2",
    )
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 2, "TestReplacementLedgerV2")
    if value["format_id"] != "kd4.test-replacement-ledger.v2":
        raise InventoryV2ContractError("invalid TestReplacementLedgerV2 format")
    validate_inventory_authority_ref_v1(value["inventory_authority"])
    rows = _require_sorted_unique_jcs(value["rows"], "replacement ledger rows")
    for row in rows:
        _require_object(row, {"baseline_id", "disposition", "obligation_id"}, "ReplacementLedgerRowV2")
        if row["baseline_id"] is not None:
            _require_nonempty_nfc(row["baseline_id"], "baseline_id")
        _require_nonempty_nfc(row["obligation_id"], "obligation_id")
        _validate_ledger_disposition_v2(row["disposition"])
        stage2_ids = row["disposition"].get("stage2_incorrect_behavior_ids")
        if row["disposition"]["kind"] == "replacement" and row["disposition"]["contract"]["state"] == "accepted":
            accepted = row["disposition"]["contract"]["accepted"]
            bindings = (
                accepted["resolved_entry_set_sha256"],
                accepted["selection_v1_sha256"],
                accepted["trusted_defect_receipt_sha256s"],
            )
            if stage2_ids is None:
                raise InventoryV2ContractError("accepted replacement requires reviewed Stage2 state")
            if not stage2_ids and any(binding is not None for binding in bindings):
                raise InventoryV2ContractError("reviewed no-defect replacement requires null receipt bindings")
            if stage2_ids and any(binding is None for binding in bindings):
                raise InventoryV2ContractError("Stage2 defects require accepted receipt bindings")
        elif stage2_ids is not None:
            raise InventoryV2ContractError("Stage2 IDs require an accepted replacement")
    receipts = value["trusted_defect_receipts"]
    referenced_ids = sorted(
        {
            defect_id
            for row in rows
            for defect_id in (row["disposition"].get("stage2_incorrect_behavior_ids") or [])
        }
    )
    if receipts is None:
        if referenced_ids:
            raise InventoryV2ContractError("Stage2 IDs require trusted defect receipts")
    else:
        if not isinstance(receipts, list) or not receipts:
            raise InventoryV2ContractError("trusted defect receipts must be null or nonempty")
        for receipt in receipts:
            validate_trusted_defect_receipt_v1(receipt)
        defect_ids = [receipt["defect_id"] for receipt in receipts]
        receipt_hashes = [receipt["receipt_sha256"] for receipt in receipts]
        if defect_ids != sorted(set(defect_ids)) or len(receipt_hashes) != len(set(receipt_hashes)):
            raise InventoryV2ContractError("trusted defect receipt IDs and hashes must be unique")
        if defect_ids != referenced_ids:
            raise InventoryV2ContractError("row IDs and trusted defect receipts must form a closed set")
        by_id = {receipt["defect_id"]: receipt for receipt in receipts}
        for defect_id, receipt in by_id.items():
            bound_rows = [row for row in rows if defect_id in (row["disposition"].get("stage2_incorrect_behavior_ids") or [])]
            if any(row["baseline_id"] is None for row in bound_rows):
                raise InventoryV2ContractError("trusted defect rows require baseline IDs")
            baseline_ids = sorted({row["baseline_id"] for row in bound_rows})
            obligations = sorted({row["obligation_id"] for row in bound_rows})
            edges = sorted({edge for row in bound_rows for edge in row["disposition"]["edge_ids"]})
            if (
                baseline_ids != receipt["baseline_ids"]
                or obligations != receipt["baseline_obligation_ids"]
                or edges != receipt["replacement_edge_ids"]
            ):
                raise InventoryV2ContractError("trusted defect row closure mismatch")
            for row in bound_rows:
                accepted = row["disposition"]["contract"]["accepted"]
                row_hashes = sorted(by_id[item]["receipt_sha256"] for item in row["disposition"]["stage2_incorrect_behavior_ids"])
                if (
                    accepted["trusted_defect_receipt_sha256s"] != row_hashes
                    or accepted["selection_v1_sha256"] != receipt["selection_v1_sha256"]
                    or accepted["resolved_entry_set_sha256"] != receipt["resolved_entry_set_sha256"]
                ):
                    raise InventoryV2ContractError("accepted replacement receipt selection mismatch")
    for field in ("semantic_sha256", "self_hash"):
        _require_sha256_field(value[field], field)
    semantic_projection = {key: value[key] for key in (
        "format_id", "inventory_authority", "rows", "schema_version", "trusted_defect_receipts"
    )}
    if value["semantic_sha256"] != proof_hash("kd4.test-replacement-ledger.v2.semantic", semantic_projection):
        raise InventoryV2ContractError("replacement ledger semantic hash mismatch")
    self_projection = dict(semantic_projection)
    self_projection["semantic_sha256"] = value["semantic_sha256"]
    if value["self_hash"] != proof_hash("kd4.test-replacement-ledger.v2.self", self_projection):
        raise InventoryV2ContractError("replacement ledger self hash mismatch")


class ActiveHostApplicabilityIssuerV1:
    """Trusted issuer that derives, signs, and re-verifies a complete inventory projection."""

    def __init__(
        self,
        key_id: str,
        authentication_key: bytes,
        cargo_observations: dict[str, Any] | None = None,
    ) -> None:
        self.key_id = _require_uuid(key_id, "active-host issuer key ID")
        if len(authentication_key) != 32:
            raise InventoryV2ContractError("active-host issuer key must be 32 bytes")
        self._authentication_key = authentication_key
        self._cargo_observations = dict(cargo_observations or {})
        for context_sha256, observation in self._cargo_observations.items():
            _require_sha256_field(context_sha256, "Cargo observation context hash")
            validate_cargo_build_context_observation_v1(observation)
            if observation["cargo_target_context_spec_sha256"] != context_sha256:
                raise InventoryV2ContractError("Cargo observation map key mismatch")

    def _projection(self, inventory: Any, host: str) -> dict[str, Any]:
        if host not in {"windows", "linux", "darwin"}:
            raise InventoryV2ContractError("invalid active-host issuer host")
        expected_authority = {
            "path": ".codex/validation/frozen-test-inventory-v2.json",
            "raw_sha256": inventory["authority"]["raw_sha256"],
            "semantic_sha256": inventory["authority"]["semantic_sha256"],
            "self_hash": inventory["authority"]["self_hash"],
        }
        entries: list[Any] = []
        for declaration in inventory["declaration_universe"]:
            entry = declaration["entry"]
            identity = entry["executable_identity"]
            platform = entry["platform_applicability"]
            rust_route = entry["test_route_id"] in {
                "test-route.rust-nextest.v1",
                "test-route.rust-doctest.v1",
            }
            cargo_context = entry["cargo_target_context_spec_sha256"] if rust_route else None
            observation_hash = None
            expression_hash = None
            if platform["kind"] == "host-set":
                verdict = "applicable" if host in platform["required_hosts"] else "not-applicable"
            else:
                if not rust_route or cargo_context not in self._cargo_observations:
                    raise InventoryV2ContractError(
                        "trusted issuer needs a validated Cargo observation for every Rust cfg context"
                    )
                observation = self._cargo_observations[cargo_context]
                atoms = {canonical_jcs(atom) for atom in observation["actual_cfg_atoms"]}
                verdict = (
                    "applicable"
                    if _evaluate_cfg_predicate(platform["expression"]["root"], atoms)
                    else "not-applicable"
                )
                observation_hash = observation["observation_sha256"]
                expression_hash = platform["expression"]["semantic_sha256"]
            result = {
                "cargo_build_context_observation_sha256": observation_hash,
                "cargo_target_context_spec_sha256": cargo_context,
                "executable_identity_sha256": entry["executable_identity_sha256"],
                "host": host,
                "platform_applicability_sha256": entry["platform_applicability_sha256"],
                "result_sha256": "0" * 64,
                "rust_cfg_expression_semantic_sha256": expression_hash,
                "schema_version": 1,
                "verdict": verdict,
            }
            result["result_sha256"] = proof_hash(
                "kd4.applicability-result.v1",
                {key: value for key, value in result.items() if key != "result_sha256"},
            )
            entries.append(
                {
                    "applicability_result": result,
                    "identity": identity,
                    "identity_sha256": entry["executable_identity_sha256"],
                    "platform_applicability_sha256": entry["platform_applicability_sha256"],
                }
            )
        entries.sort(key=lambda item: canonical_jcs(item["identity"]))
        return {
            "entries": entries,
            "host": host,
            "inventory_authority": expected_authority,
            "schema_version": 1,
        }

    def issue(self, inventory: Any, host: str, authority_nonce: str) -> dict[str, Any]:
        validate_frozen_test_inventory_v2(inventory)
        _require_standard_base64_32(authority_nonce, "authority_nonce")
        projection = self._projection(inventory, host)
        body = {
            "authority_nonce": authority_nonce,
            "inventory_authority": projection["inventory_authority"],
            "schema_version": 1,
            "target_applicability_projection": projection,
            "target_applicability_projection_sha256": proof_hash(
                "kd4.target-applicability-projection.v1", projection
            ),
        }
        authority_sha256 = proof_hash(
            "kd4.active-host-applicability-authority.v1", body
        )
        authentication_payload = {
            "authority_sha256": authority_sha256,
            "body": body,
            "key_id": self.key_id,
            "schema_version": 1,
        }
        authentication_tag = base64.b64encode(
            hmac.new(
                self._authentication_key,
                b"kd4.active-host-applicability-authority.authentication.v1\0"
                + canonical_jcs(authentication_payload),
                hashlib.sha256,
            ).digest()
        ).decode("ascii").rstrip("=")
        return {
            "authentication_tag": authentication_tag,
            "authority_sha256": authority_sha256,
            "body": body,
            "key_id": self.key_id,
            "schema_version": 1,
        }

    def validate_complete_authority(self, authority: Any, inventory: Any) -> None:
        validate_active_host_applicability_authority_v1(
            authority, self._authentication_key
        )
        if authority["key_id"] != self.key_id:
            raise InventoryV2ContractError("active-host authority uses an untrusted issuer key")
        expected = self.issue(
            inventory,
            authority["body"]["target_applicability_projection"]["host"],
            authority["body"]["authority_nonce"],
        )
        if authority != expected:
            raise InventoryV2ContractError(
                "active-host authority is not the issuer-derived complete inventory projection"
            )


def derive_frozen_v1_historical_replacement_graph_v1(
    predecessor_ledger: Any,
) -> dict[str, Any]:
    if not isinstance(predecessor_ledger, dict) or not isinstance(
        predecessor_ledger.get("rows"), list
    ):
        raise InventoryV2ContractError("predecessor ledger has no canonical row array")
    replacement_ids_by_baseline: dict[str, list[str]] = {}
    for row in predecessor_ledger["rows"]:
        if not isinstance(row, dict) or row.get("resolution") != "replacement":
            continue
        baseline_id = row.get("baseline_id")
        replacement_ids = row.get("replacement_ids")
        if not isinstance(baseline_id, str) or not baseline_id:
            raise InventoryV2ContractError(
                "historical replacement row requires a nonempty baseline ID"
            )
        require_nfc(baseline_id)
        if (
            not isinstance(replacement_ids, list)
            or not replacement_ids
            or any(
                not isinstance(replacement_id, str) or not replacement_id
                for replacement_id in replacement_ids
            )
            or len(set(replacement_ids)) != len(replacement_ids)
        ):
            raise InventoryV2ContractError(
                "historical replacement IDs must be nonempty and unique"
            )
        for replacement_id in replacement_ids:
            require_nfc(replacement_id)
        if baseline_id in replacement_ids_by_baseline:
            raise InventoryV2ContractError(
                "predecessor ledger repeats a historical replacement baseline"
            )
        replacement_ids_by_baseline[baseline_id] = sorted(replacement_ids)

    baseline_ids = sorted(replacement_ids_by_baseline)
    edges = [
        {"baseline_id": baseline_id, "replacement_id": replacement_id}
        for baseline_id in baseline_ids
        for replacement_id in replacement_ids_by_baseline[baseline_id]
    ]
    successor_ids = sorted({edge["replacement_id"] for edge in edges})
    successor_owners: dict[str, set[str]] = {successor_id: set() for successor_id in successor_ids}
    for edge in edges:
        successor_owners[edge["replacement_id"]].add(edge["baseline_id"])

    components: list[dict[str, Any]] = []
    visited_baselines: set[str] = set()
    for initial_baseline in baseline_ids:
        if initial_baseline in visited_baselines:
            continue
        pending_baselines = [initial_baseline]
        component_baselines: set[str] = set()
        component_successors: set[str] = set()
        while pending_baselines:
            baseline_id = pending_baselines.pop()
            if baseline_id in component_baselines:
                continue
            component_baselines.add(baseline_id)
            visited_baselines.add(baseline_id)
            for successor_id in replacement_ids_by_baseline[baseline_id]:
                if successor_id in component_successors:
                    continue
                component_successors.add(successor_id)
                pending_baselines.extend(successor_owners[successor_id])
        sorted_baselines = sorted(component_baselines)
        sorted_successors = sorted(component_successors)
        component_edges = [
            {"baseline_id": edge["baseline_id"], "replacement_id": edge["replacement_id"]}
            for edge in edges
            if edge["baseline_id"] in component_baselines
        ]
        components.append(
            {
                "baseline_ids": sorted_baselines,
                "edges": component_edges,
                "successor_ids": sorted_successors,
            }
        )
    components.sort(key=lambda component: component["baseline_ids"])
    projection = {
        "baseline_ids": baseline_ids,
        "components": components,
        "edges": edges,
        "successor_ids": successor_ids,
    }
    if (
        len(baseline_ids) != FROZEN_V1_HISTORICAL_REPLACEMENT_BASELINE_COUNT
        or len(edges) != FROZEN_V1_HISTORICAL_REPLACEMENT_EDGE_COUNT
        or len(successor_ids) != FROZEN_V1_HISTORICAL_REPLACEMENT_SUCCESSOR_COUNT
        or len(components) != FROZEN_V1_HISTORICAL_REPLACEMENT_COMPONENT_COUNT
        or proof_hash("kd4.frozen-v1-historical-replacement-graph.v1", projection)
        != FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256
    ):
        raise InventoryV2ContractError(
            "historical replacement graph does not match the exact frozen V1 graph"
        )
    return projection


def validate_v2_historical_replacement_graph_closure_v1(
    ledger: Any,
    predecessor_ledger: Any,
) -> None:
    graph = derive_frozen_v1_historical_replacement_graph_v1(predecessor_ledger)
    predecessor_rows = {
        row["baseline_id"]: row
        for row in predecessor_ledger["rows"]
        if isinstance(row, dict) and row.get("resolution") == "replacement"
    }
    rows_by_baseline: dict[str, Any] = {}
    for row in ledger["rows"]:
        baseline_id = row["baseline_id"]
        if baseline_id is None:
            continue
        if baseline_id in rows_by_baseline:
            raise InventoryV2ContractError(
                "replacement ledger repeats a baseline ID"
            )
        rows_by_baseline[baseline_id] = row
    replacement_ids_by_baseline: dict[str, list[str]] = {
        baseline_id: [] for baseline_id in graph["baseline_ids"]
    }
    for edge in graph["edges"]:
        replacement_ids_by_baseline[edge["baseline_id"]].append(
            edge["replacement_id"]
        )
    for baseline_id in graph["baseline_ids"]:
        predecessor_row = predecessor_rows[baseline_id]
        predecessor_row_sha256 = proof_hash(
            "kd4.frozen-v1-replacement-ledger-row.v1", predecessor_row
        )
        replacement_ids = replacement_ids_by_baseline[baseline_id]
        expected_edge_ids = sorted(
            "replacement-edge-v2."
            + proof_hash(
                "kd4.legacy-replacement-edge.v1",
                {
                    "baseline_id": baseline_id,
                    "predecessor_row_sha256": predecessor_row_sha256,
                    "replacement_id": replacement_id,
                },
            )
            for replacement_id in replacement_ids
        )
        current = rows_by_baseline.get(baseline_id, {}).get("disposition", {})
        if (
            current.get("kind") != "replacement"
            or current.get("contract", {}).get("legacy_replacement_hint")
            != {
                "predecessor_row_sha256": predecessor_row_sha256,
                "replacement_ids": replacement_ids,
            }
            or current.get("edge_ids") != expected_edge_ids
        ):
            raise InventoryV2ContractError(
                "historical replacement mapping changed from the exact frozen V1 graph"
            )


def validate_inventory_ledger_predecessor_closure(
    inventory: Any,
    ledger: Any,
    recovery_raw: bytes,
    transition_receipts: list[Any],
    applicability_issuer: "ActiveHostApplicabilityIssuerV1",
    doctest_recapture_raw: bytes | None = None,
    unittest_recapture_raw: bytes | None = None,
    predecessor_ledger_raw: bytes | None = None,
) -> None:
    validate_frozen_test_inventory_v2(inventory)
    validate_test_replacement_ledger_v2(ledger)
    if not isinstance(recovery_raw, bytes):
        raise InventoryV2ContractError("recovery authority input must be exact bytes")
    try:
        recovery = json.loads(recovery_raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise InventoryV2ContractError("recovery authority is invalid JSON") from exc
    if canonical_jcs(recovery) != recovery_raw:
        raise InventoryV2ContractError(
            "recovery authority input must be exact canonical JSON bytes"
        )
    validate_inventory_recovery_authority_v1(recovery)
    expected_authority = {
        "path": ".codex/validation/frozen-test-inventory-v2.json",
        "raw_sha256": inventory["authority"]["raw_sha256"],
        "semantic_sha256": inventory["authority"]["semantic_sha256"],
        "self_hash": inventory["authority"]["self_hash"],
    }
    if ledger["inventory_authority"] != expected_authority:
        raise InventoryV2ContractError(
            "replacement ledger does not bind the exact Inventory V2 authority"
        )
    expected_recovery_authority = {
        "path": ".codex/validation/frozen-test-inventory-v2-recoveries.json",
        "raw_sha256": hashlib.sha256(recovery_raw).hexdigest(),
        "semantic_sha256": recovery["semantic_sha256"],
        "self_hash": recovery["self_hash"],
    }
    if inventory["recovery_authority"] != expected_recovery_authority:
        raise InventoryV2ContractError(
            "inventory does not bind the exact recovery authority bytes"
        )
    observed = sorted(
        row["baseline_id"] for row in ledger["rows"] if row["baseline_id"] is not None
    )
    _validate_observed_predecessor_ids(
        inventory["predecessor_reconciliation"],
        observed,
        "replacement ledger baseline rows",
    )
    declarations_by_baseline: dict[str, Any] = {}
    declarations_by_obligation: dict[str, Any] = {}
    expected_baseline_by_obligation: dict[str, str | None] = {}
    for declaration in inventory["declaration_universe"]:
        if declaration["kind"] == "frozen-baseline":
            obligation_id = frozen_baseline_obligation_id_v2(declaration)
            baseline_id = declaration["baseline_id"]
            declarations_by_baseline[baseline_id] = declaration
        else:
            obligation_id = declaration["obligation_id"]
            baseline_id = None
        if obligation_id in declarations_by_obligation:
            raise InventoryV2ContractError(
                "inventory declarations have duplicate obligation IDs"
            )
        declarations_by_obligation[obligation_id] = declaration
        expected_baseline_by_obligation[obligation_id] = baseline_id
    rows_by_obligation: dict[str, Any] = {}
    for row in ledger["rows"]:
        obligation_id = row["obligation_id"]
        if obligation_id in rows_by_obligation:
            raise InventoryV2ContractError("ledger has duplicate obligation rows")
        rows_by_obligation[obligation_id] = row
    if not set(declarations_by_obligation).issubset(rows_by_obligation):
        raise InventoryV2ContractError(
            "ledger rows do not exactly cover every inventory declaration obligation"
        )
    for obligation_id, declaration in declarations_by_obligation.items():
        row = rows_by_obligation[obligation_id]
        if row["baseline_id"] != expected_baseline_by_obligation[obligation_id]:
            raise InventoryV2ContractError(
                "ledger baseline identity does not match its exact inventory declaration"
            )
        if (
            declaration["kind"] == "post-baseline-current"
            and row["disposition"]["kind"] == "current"
            and row["disposition"]["inventory_entry_semantic_sha256"]
            != proof_hash("kd4.executable-inventory-entry.v2", declaration["entry"])
        ):
            raise InventoryV2ContractError(
                "current disposition does not bind its exact inventory declaration entry"
            )

    if not isinstance(transition_receipts, list):
        raise InventoryV2ContractError("transition receipts must be an array")
    receipts_by_hash: dict[str, Any] = {}
    for receipt in transition_receipts:
        validate_recovery_transition_receipt_v1(receipt)
        if receipt["receipt_sha256"] in receipts_by_hash:
            raise InventoryV2ContractError("duplicate recovery transition receipt")
        receipts_by_hash[receipt["receipt_sha256"]] = receipt
    expected_receipt_hashes = sorted(
        record["transition_receipt_sha256"]
        for record in recovery["records"]
        if record["state"] == "resolved"
    )
    if sorted(receipts_by_hash) != expected_receipt_hashes:
        raise InventoryV2ContractError(
            "transition receipts do not exactly cover resolved recovery records"
        )
    frozen_authority_sha256 = proof_hash(
        "kd4.frozen-source-authority.v1", recovery["frozen_source_authority"]
    )
    doctest_recapture: Any = None
    if doctest_recapture_raw is not None:
        if not isinstance(doctest_recapture_raw, bytes):
            raise InventoryV2ContractError("doctest recapture input must be exact bytes")
        try:
            doctest_recapture = json.loads(doctest_recapture_raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise InventoryV2ContractError("doctest recapture is invalid JSON") from exc
        if canonical_jcs(doctest_recapture) != doctest_recapture_raw:
            raise InventoryV2ContractError(
                "doctest recapture input must be exact canonical JSON bytes"
            )
        validate_doctest_recapture_packet_v1(doctest_recapture)
    unittest_recapture: Any = None
    if unittest_recapture_raw is not None:
        if not isinstance(unittest_recapture_raw, bytes):
            raise InventoryV2ContractError("unittest recapture input must be exact bytes")
        try:
            unittest_recapture = json.loads(unittest_recapture_raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise InventoryV2ContractError("unittest recapture is invalid JSON") from exc
        if canonical_jcs(unittest_recapture) != unittest_recapture_raw:
            raise InventoryV2ContractError(
                "unittest recapture input must be exact canonical JSON bytes"
            )
        validate_unittest_recapture_packet_v1(unittest_recapture)
    predecessor_ledger: Any = None
    if predecessor_ledger_raw is not None:
        if not isinstance(predecessor_ledger_raw, bytes):
            raise InventoryV2ContractError("predecessor ledger input must be exact bytes")
        if hashlib.sha256(predecessor_ledger_raw).hexdigest() != FROZEN_V1_LEDGER_RAW_SHA256:
            raise InventoryV2ContractError("predecessor ledger raw SHA-256 mismatch")
        try:
            predecessor_ledger = json.loads(predecessor_ledger_raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise InventoryV2ContractError("predecessor ledger is invalid JSON") from exc
        validate_v2_historical_replacement_graph_closure_v1(
            ledger, predecessor_ledger
        )
    authority_state_records = json.loads(json.dumps(recovery["records"]))
    for index, record in enumerate(authority_state_records):
        if record["state"] != "resolved":
            continue
        if record["kind"] == "doctest":
            record["current_audit"]["raw_count"] = None
            record["legacy_evidence"]["historical_raw_count"] = None
            record["pending_requirement"] = {
                "baseline_commit": recovery["frozen_source_authority"]["baseline_commit"],
                "kind": "doctest",
                "reasons": ["historical-raw-count-unknown", "off-host-recapture-required"],
                "required_package_targets": sorted(
                    target for _, target in DOCTEST_RECAPTURE_PACKAGE_SPECS
                ),
            }
        else:
            if unittest_recapture is None:
                raise InventoryV2ContractError(
                    "resolved unittest recovery requires the exact typed recapture packet"
                )
            record["legacy_evidence"]["historical_subtest_call_count"] = None
            record["pending_requirement"] = {
                "baseline_commit": recovery["frozen_source_authority"]["baseline_commit"],
                "expected_parent_output_sha256s": [
                    {
                        "output_sha256": parent["predecessor_entry_sha256"],
                        "parent_id": parent["baseline_id"],
                    }
                    for parent in unittest_recapture["parent_records"]
                ],
                "kind": "unittest",
                "reasons": [
                    "historical-subtest-count-unknown",
                    "parent-output-recapture-required",
                ],
                "required_parent_count": 893,
                "required_parent_ids": [
                    parent["baseline_id"]
                    for parent in unittest_recapture["parent_records"]
                ],
            }
        record["resolution"] = None
        record["state"] = "pending"
        record["transition_receipt_sha256"] = None
        authority_state_records[index] = record
    for index, final_record in enumerate(recovery["records"]):
        if final_record["state"] != "resolved":
            continue
        receipt = receipts_by_hash[final_record["transition_receipt_sha256"]]
        authority_before = {
            "format_id": recovery["format_id"],
            "frozen_source_authority": recovery["frozen_source_authority"],
            "records": authority_state_records,
            "schema_version": recovery["schema_version"],
        }
        expected_before_sha256 = proof_hash(
            "kd4.inventory-recovery-authority.semantic.v1", authority_before
        )
        if receipt["authority_before_semantic_sha256"] != expected_before_sha256:
            raise InventoryV2ContractError(
                "recovery transition does not bind the actual predecessor authority"
            )
        authority_state_records[index] = json.loads(json.dumps(final_record))
    recovered_parents: dict[str, tuple[list[str], str]] = {}
    recovered_child_ids: set[str] = set()
    for record in recovery["records"]:
        if record["state"] != "resolved":
            continue
        resolution = record["resolution"]
        receipt = receipts_by_hash[record["transition_receipt_sha256"]]
        child_pairs = [
            (child, _validate_recovered_child_source_v1(child))
            for child in resolution["child_sources"]
        ]
        child_ids = sorted(child_id for _, child_id in child_pairs)
        if record["kind"] == "doctest":
            if doctest_recapture is None:
                raise InventoryV2ContractError(
                    "resolved doctest recovery requires the exact typed recapture packet"
                )
            expected_children = doctest_recovered_child_sources_v1(doctest_recapture)
            if (
                resolution["recapture_receipt_sha256"]
                != doctest_recapture["receipt_sha256"]
                or resolution["child_sources"] != expected_children
                or resolution["parent_container_ids"]
                != sorted(DOCTEST_RECAPTURE_PARENT_TARGETS)
            ):
                raise InventoryV2ContractError(
                    "doctest recovery does not exactly materialize its typed recapture packet"
                )
        elif record["kind"] == "unittest":
            if unittest_recapture is None:
                raise InventoryV2ContractError(
                    "resolved unittest recovery requires the exact typed recapture packet"
                )
            expected_children = unittest_recovered_child_sources_v1(unittest_recapture)
            expected_outputs = [
                {
                    "output_sha256": parent["predecessor_entry_sha256"],
                    "parent_id": parent["baseline_id"],
                }
                for parent in unittest_executable_parent_records_v1(unittest_recapture)
            ]
            if (
                resolution["recapture_receipt_sha256"]
                != unittest_recapture["receipt_sha256"]
                or resolution["child_sources"] != expected_children
                or resolution["parent_container_ids"]
                != [record["baseline_id"] for record in unittest_executable_parent_records_v1(unittest_recapture)]
                or resolution["parent_recapture_outputs"] != expected_outputs
                or record["legacy_evidence"]["historical_subtest_call_count"]
                != unittest_recapture["total_counts"]["subtest_occurrence_count"]
            ):
                raise InventoryV2ContractError(
                    "unittest recovery does not exactly materialize its typed recapture packet"
                )
        if (
            receipt["child_obligation_ids"] != child_ids
            or receipt["parent_container_ids"] != resolution["parent_container_ids"]
            or receipt["recapture_receipt_sha256"]
            != resolution["recapture_receipt_sha256"]
            or receipt["frozen_source_authority_sha256"] != frozen_authority_sha256
        ):
            raise InventoryV2ContractError(
                "recovery transition receipt does not bind its exact resolved record"
            )
        for parent in resolution["parent_container_ids"]:
            parent_children = sorted(
                child_id
                for child, child_id in child_pairs
                if child["parent_baseline_id"] == parent
            )
            if parent in recovered_parents:
                raise InventoryV2ContractError(
                    f"recovery parent {parent!r} appears in more than one recovery record"
                )
            recovered_parents[parent] = (parent_children, receipt["receipt_sha256"])
        if record["kind"] == "unittest":
            recovered_child_ids.update(child_ids)
    expected_obligations = set(declarations_by_obligation) | recovered_child_ids
    if set(rows_by_obligation) != expected_obligations:
        raise InventoryV2ContractError(
            "ledger rows do not exactly cover every inventory declaration and recovered-child obligation"
        )
    for child_id in recovered_child_ids:
        child_row = rows_by_obligation[child_id]
        if child_row["baseline_id"] is not None or child_row["disposition"] != {"kind": "unresolved"}:
            raise InventoryV2ContractError(
                "recovered child obligation must be a separate baseline-null unresolved row"
            )
    observed_recovered_parents: set[str] = set()
    for row in ledger["rows"]:
        disposition = row["disposition"]
        if disposition["kind"] != "recovered-container":
            continue
        baseline_id = row["baseline_id"]
        expected = recovered_parents.get(baseline_id)
        if expected is None or (
            disposition["child_obligation_ids"],
            disposition["transition_receipt_sha256"],
        ) != expected:
            raise InventoryV2ContractError(
                "recovered-container row does not bind its recovery transition"
            )
        observed_recovered_parents.add(baseline_id)
    expected_container_parents = {
        parent
        for record in recovery["records"]
        if record["state"] == "resolved" and record["kind"] == "doctest"
        for parent in record["resolution"]["parent_container_ids"]
    }
    if observed_recovered_parents != expected_container_parents:
        raise InventoryV2ContractError(
            "recovered-container rows do not exactly cover resolved recovery parents"
        )
    if unittest_recapture is not None:
        if predecessor_ledger is None:
            raise InventoryV2ContractError(
                "resolved unittest recovery requires exact predecessor ledger bytes"
            )
        executable_parent_ids = {
            record["baseline_id"] for record in unittest_recapture["parent_records"]
        }
        predecessor_unittest_ids = sorted(
            row["baseline_id"]
            for row in predecessor_ledger.get("rows", [])
            if isinstance(row, dict)
            and isinstance(row.get("baseline_id"), str)
            and (
                row["baseline_id"].startswith("python-unittest::")
                or row["baseline_id"].startswith(
                    "hidden-at-freeze-v1::python-unittest::"
                )
            )
        )
        hidden_parent_ids = sorted(
            parent
            for parent in predecessor_unittest_ids
            if parent.startswith("hidden-at-freeze-v1::python-unittest::")
        )
        unittest_parent_ids = set(predecessor_unittest_ids)
        if (
            len(unittest_parent_ids) != 909
            or len(executable_parent_ids) != 893
            or len(hidden_parent_ids) != 16
            or proof_hash(
                "kd4.unittest-hidden-ledger-parent-ids.v1", hidden_parent_ids
            )
            != UNITTEST_V1_HIDDEN_PARENT_IDS_SHA256
            or unittest_parent_ids != executable_parent_ids | set(hidden_parent_ids)
            or executable_parent_ids & set(hidden_parent_ids)
        ):
            raise InventoryV2ContractError(
                "unittest predecessor partition must preserve 909 ledger parents, "
                "893 executable parents, and 16 hidden replacement-only parents"
            )
        replacement_ids = sorted(
            row["baseline_id"] for row in ledger["rows"]
            if row["baseline_id"] in unittest_parent_ids
            and row["disposition"]["kind"] == "replacement"
        )
        unresolved_ids = sorted(
            row["baseline_id"] for row in ledger["rows"]
            if row["baseline_id"] in unittest_parent_ids
            and row["disposition"]["kind"] == "unresolved"
        )
        executable_replacement_ids = sorted(
            parent for parent in replacement_ids if parent in executable_parent_ids
        )
        if (
            len(replacement_ids) != 536
            or proof_hash("kd4.unittest-parent-replacement-ids.v1", replacement_ids)
            != UNITTEST_V1_REPLACEMENT_PARENT_IDS_SHA256
            or len(executable_replacement_ids) != 520
            or proof_hash(
                "kd4.unittest-parent-replacement-ids.v1",
                executable_replacement_ids,
            )
            != UNITTEST_V1_EXECUTABLE_REPLACEMENT_PARENT_IDS_SHA256
            or len(unresolved_ids) != 373
            or proof_hash("kd4.unittest-parent-unresolved-ids.v1", unresolved_ids)
            != UNITTEST_V1_UNRESOLVED_PARENT_IDS_SHA256
            or set(replacement_ids) | set(unresolved_ids) != unittest_parent_ids
            or not set(hidden_parent_ids).issubset(replacement_ids)
        ):
            raise InventoryV2ContractError(
                "unittest parent dispositions do not preserve the exact 536 replacement "
                "(520 executable plus 16 hidden) and 373 unresolved split"
            )
        predecessor_rows = {
            row["baseline_id"]: row
            for row in predecessor_ledger.get("rows", [])
            if isinstance(row, dict) and row.get("baseline_id") in unittest_parent_ids
        }
        if set(predecessor_rows) != unittest_parent_ids:
            raise InventoryV2ContractError(
                "predecessor ledger does not exactly cover the 909 unittest parents"
            )
        for parent in unittest_parent_ids:
            old = predecessor_rows[parent]
            current = rows_by_obligation[
                next(
                    obligation_id
                    for obligation_id, baseline_id in expected_baseline_by_obligation.items()
                    if baseline_id == parent
                )
            ]["disposition"]
            old_hash = proof_hash("kd4.frozen-v1-replacement-ledger-row.v1", old)
            if old.get("resolution") == "unresolved":
                if current != {"kind": "unresolved"}:
                    raise InventoryV2ContractError(
                        "unittest unresolved parent disposition changed"
                    )
            elif old.get("resolution") == "replacement":
                hint = current.get("contract", {}).get("legacy_replacement_hint")
                if (
                    current.get("kind") != "replacement"
                    or hint != {
                        "predecessor_row_sha256": old_hash,
                        "replacement_ids": sorted(old.get("replacement_ids", [])),
                    }
                ):
                    raise InventoryV2ContractError(
                        "unittest replacement parent mapping changed"
                    )
            else:
                raise InventoryV2ContractError(
                    "unittest predecessor parent has an unknown disposition"
                )
    for row in ledger["rows"]:
        disposition = row["disposition"]
        if disposition["kind"] != "exception" or disposition["exception"]["kind"] != "accepted":
            continue
        exception = disposition["exception"]
        declaration = declarations_by_obligation[row["obligation_id"]]
        if row["baseline_id"] is None:
            if (
                declaration["kind"] not in {"missing-baseline", "post-baseline-current"}
                or exception["tag"] not in {"off-host", "platform-pending"}
                or exception["provenance_receipt"] != declaration["source_provenance"]
            ):
                raise InventoryV2ContractError(
                    "baseline-null accepted exception must bind the exact nonbaseline declaration provenance"
                )
        active_host_authority = exception["active_host_authority"]
        applicability_issuer.validate_complete_authority(
            active_host_authority, inventory
        )
        body = active_host_authority["body"]
        projection = body["target_applicability_projection"]
        if (
            body["inventory_authority"] != ledger["inventory_authority"]
            or projection["inventory_authority"] != ledger["inventory_authority"]
        ):
            raise InventoryV2ContractError(
                "accepted exception projection does not bind the active inventory authority"
            )
        entry = declaration["entry"]
        candidates = [
            item for item in projection["entries"]
            if item["identity"] == entry["executable_identity"]
            and item["platform_applicability_sha256"] == entry["platform_applicability_sha256"]
        ]
        if len(candidates) != 1:
            raise InventoryV2ContractError(
                "accepted exception is not present in its authenticated active-host projection"
            )
        result = candidates[0]["applicability_result"]
        rust_route = entry["test_route_id"] in {
            "test-route.rust-nextest.v1", "test-route.rust-doctest.v1"
        }
        validate_applicability_result_v1(
            result, rust_route=rust_route, platform=entry["platform_applicability"]
        )
        if exception["tag"] in {"off-host", "platform-pending"} and result["verdict"] != "not-applicable":
            raise InventoryV2ContractError(
                "off-host/platform-pending exception requires authenticated not-applicable verdict"
            )


def _decode_recapture_artifact(value: Any, sha256: Any, label: str) -> bytes:
    if not isinstance(value, str):
        raise InventoryV2ContractError(f"{label} must be standard base64")
    _require_sha256_field(sha256, f"{label}_sha256")
    try:
        raw = base64.b64decode(value, validate=True)
    except (ValueError, binascii.Error) as exc:
        raise InventoryV2ContractError(f"{label} must be standard base64") from exc
    if base64.b64encode(raw).decode("ascii") != value:
        raise InventoryV2ContractError(f"{label} base64 is not canonical")
    if hashlib.sha256(raw).hexdigest() != sha256:
        raise InventoryV2ContractError(f"{label} artifact hash mismatch")
    return raw


def _expected_doctest_command(package_name: str) -> list[str]:
    return [
        "cargo", "test", "--locked", "--offline", "-p", package_name,
        "--doc", "--", "--list", "--format", "terse",
    ]


def _validate_unittest_artifact_v1(value: Any, label: str) -> bytes:
    artifact = _require_object(
        value, {"base64", "byte_count", "sha256"}, f"{label} artifact"
    )
    raw = _decode_recapture_artifact(
        artifact["base64"], artifact["sha256"], label
    )
    if _require_integer(artifact["byte_count"], 0, f"{label} byte_count") != len(raw):
        raise InventoryV2ContractError(f"{label} artifact byte count mismatch")
    return raw


def _unittest_site_id_v1(site: Any) -> str:
    projection = {
        key: site[key]
        for key in ("column", "line", "parent_baseline_id", "path")
    }
    return "unittest-site." + proof_hash(
        "kd4.unittest-recapture-source-site.v1", projection
    )


def _unittest_manifest_projection_v1(
    parent_baseline_id: str, occurrences: list[Any]
) -> dict[str, Any]:
    return {
        "method_body_observed": True,
        "parent_baseline_id": parent_baseline_id,
        "site_ids": sorted({item["declared_site_id"] for item in occurrences}),
        "subtest_occurrence_count": len(occurrences),
    }


def unittest_subtest_manifests_v1(value: Any) -> list[dict[str, Any]]:
    """Return the canonical per-parent manifests bound by a unittest packet."""

    validate_unittest_recapture_packet_v1(value)
    occurrences_by_parent: dict[str, list[Any]] = {
        record["baseline_id"]: [] for record in unittest_executable_parent_records_v1(value)
    }
    for occurrence in value["subtest_occurrences"]:
        occurrences_by_parent[occurrence["parent_baseline_id"]].append(occurrence)
    manifests = []
    for parent, occurrences in occurrences_by_parent.items():
        projection = _unittest_manifest_projection_v1(parent, occurrences)
        manifests.append(
            {
                **projection,
                "manifest_sha256": proof_hash(
                    "kd4.unittest-subtest-manifest.v1",
                    {"occurrences": occurrences, **projection},
                ),
            }
        )
    return sorted(manifests, key=lambda item: item["parent_baseline_id"])


def unittest_recovered_child_sources_v1(value: Any) -> list[dict[str, Any]]:
    """Materialize separate unresolved child obligations from a unittest packet."""

    validate_unittest_recapture_packet_v1(value)
    children: list[dict[str, Any]] = []
    for record in unittest_executable_parent_records_v1(value):
        parent = record["baseline_id"]
        children.append(
            {
                "canonical_parameter_projection": None,
                "child_kind": "unittest-method-body",
                "declared_site_id": None,
                "executable_identity": {
                    "kind": "test",
                    "route_id": "test-route.python-unittest.v1",
                    "test_id": parent,
                    "validation_id": "python.unittest.recapture",
                },
                "gap_id": "gap.unittest-subtest-expansion",
                "occurrence_ordinal": None,
                "parent_baseline_id": parent,
            }
        )
    for occurrence in value["subtest_occurrences"]:
        parent = occurrence["parent_baseline_id"]
        children.append(
            {
                "canonical_parameter_projection": occurrence[
                    "canonical_context_projection"
                ],
                "child_kind": "unittest-subtest",
                "declared_site_id": occurrence["declared_site_id"],
                "executable_identity": {
                    "kind": "test",
                    "route_id": "test-route.python-unittest.v1",
                    "test_id": parent,
                    "validation_id": "python.unittest.recapture",
                },
                "gap_id": "gap.unittest-subtest-expansion",
                "occurrence_ordinal": occurrence["occurrence_ordinal"],
                "parent_baseline_id": parent,
            }
        )
    return sorted(children, key=_validate_recovered_child_source_v1)


def validate_unittest_source_provenance_exception_v1(exception: Any) -> None:
    if exception is None:
        raise InventoryV2ContractError(
            "unittest recapture freeze-overlay source authority is unavailable"
        )
    if hashlib.sha256(canonical_jcs(exception)).hexdigest() != UNITTEST_SOURCE_EXCEPTIONS_SHA256:
        raise InventoryV2ContractError("unittest source exception is not the exact approved five-source and 29-parent amendments")


def unittest_executable_parent_records_v1(value: Any) -> list[dict[str, Any]]:
    """Select authenticated bodies after validating the exact approved exception."""
    exception = value.get("source_provenance_exception")
    validate_unittest_source_provenance_exception_v1(exception)
    excepted = set(exception["baseline_ids"]) | set(exception["historical_execution_extension"]["baseline_ids"])
    return [row for row in value["parent_records"] if row["baseline_id"] not in excepted]


def validate_unittest_recapture_packet_v1(value: Any) -> None:
    """Validate a complete baseline unittest recapture without rewriting parents."""

    fields = {
        "artifacts", "attempt_id", "baseline_commit", "format_id",
        "frozen_inventory_raw_sha256", "output_bindings", "parent_manifests",
        "network_isolation", "parent_records", "parent_results", "python_identity", "receipt_sha256",
        "repository_identity_sha256", "schema_version", "source_audit",
        "source_isolation", "source_site_manifest", "source_tree_sha256", "subtest_occurrences",
        "total_counts", "worker_identity",
    }
    if isinstance(value, dict) and "source_provenance_exception" in value:
        fields.add("source_provenance_exception")
    _require_object(value, fields, "UnittestRecapturePacketV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "UnittestRecapturePacketV1")
    if value["format_id"] != UNITTEST_RECAPTURE_FORMAT_ID:
        raise InventoryV2ContractError("invalid unittest recapture packet format")
    _require_nonempty_nfc(value["attempt_id"], "attempt_id")
    expected_authorities = {
        "baseline_commit": UNITTEST_RECAPTURE_BASELINE_COMMIT,
        "frozen_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "repository_identity_sha256": UNITTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256,
        "source_tree_sha256": UNITTEST_RECAPTURE_SOURCE_TREE_SHA256,
    }
    if any(value[key] != expected for key, expected in expected_authorities.items()):
        raise InventoryV2ContractError("unittest recapture frozen authority mismatch")
    executable_parents = unittest_executable_parent_records_v1(value)
    source_site_sha256 = UNITTEST_APPROVED_SOURCE_SITE_MANIFEST_SHA256

    isolation = _require_object(
        value["source_isolation"],
        {
            "checkout_command", "clean_after", "clean_before", "clone_command",
            "clone_config", "clone_kind", "clone_source_path", "core_autocrlf",
            "execution_working_directory", "git_hooks_disabled", "head_commit",
            "isolated_checkout_path", "kind",
            "source_repository_identity_sha256", "source_tree_sha256",
            "global_git_config_disabled", "system_git_config_disabled",
        },
        "UnittestSourceIsolationV1",
    )
    clone_source_path = _require_nonempty_nfc(
        isolation["clone_source_path"], "clone source path"
    )
    isolated_checkout_path = _require_nonempty_nfc(
        isolation["isolated_checkout_path"], "isolated checkout path"
    )
    hooks_path = _require_nonempty_nfc(
        isolation["clone_config"].get("git_hooks_path")
        if isinstance(isolation["clone_config"], dict)
        else None,
        "disabled Git hooks path",
    )
    if (
        isolation["kind"] != "detached-checkout"
        or isolation["clone_kind"] != "local-no-hardlinks-no-checkout"
        or isolation["clone_command"] != [
            "git", "clone", "--local", "--no-hardlinks", "--no-checkout",
            "--config", "core.autocrlf=false", "--config",
            f"core.hooksPath={hooks_path}", clone_source_path,
            isolated_checkout_path,
        ]
        or isolation["clone_config"] != {
            "core_autocrlf": False,
            "git_hooks_path": hooks_path,
            "hardlinks": False,
            "local": True,
            "no_checkout": True,
        }
        or isolation["checkout_command"] != [
            "git", "-C", isolated_checkout_path, "checkout", "--detach",
            "--force", UNITTEST_RECAPTURE_BASELINE_COMMIT,
        ]
        or isolation["clean_before"] is not True
        or isolation["clean_after"] is not True
        or isolation["core_autocrlf"] is not False
        or isolation["git_hooks_disabled"] is not True
        or isolation["global_git_config_disabled"] is not True
        or isolation["system_git_config_disabled"] is not True
        or isolation["execution_working_directory"] != "."
        or isolation["head_commit"] != UNITTEST_RECAPTURE_BASELINE_COMMIT
        or isolation["source_repository_identity_sha256"]
        != UNITTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256
        or isolation["source_tree_sha256"] != UNITTEST_RECAPTURE_SOURCE_TREE_SHA256
    ):
        raise InventoryV2ContractError("unittest recapture was not an isolated clean checkout")

    python_identity = _require_object(
        value["python_identity"],
        {
            "executable_path", "executable_sha256", "implementation", "major", "minor", "micro", "soabi",
            "version_base64", "version_sha256",
        },
        "UnittestPythonIdentityV1",
    )
    if python_identity["implementation"] != "CPython":
        raise InventoryV2ContractError("unittest recapture requires CPython")
    _require_nonempty_nfc(python_identity["executable_path"], "Python executable path")
    _require_sha256_field(python_identity["executable_sha256"], "Python executable_sha256")
    for field in ("major", "minor", "micro"):
        _require_integer(python_identity[field], 0, f"Python {field}")
    _require_nonempty_nfc(python_identity["soabi"], "Python SOABI")
    _decode_recapture_artifact(
        python_identity["version_base64"], python_identity["version_sha256"],
        "Python version",
    )
    worker = _require_object(
        value["worker_identity"],
        {"command_argv", "environment_sha256", "worker_id", "worker_sha256"},
        "UnittestWorkerIdentityV1",
    )
    _require_nonempty_nfc(worker["worker_id"], "worker_id")
    _require_sha256_field(worker["environment_sha256"], "environment_sha256")
    _require_sha256_field(worker["worker_sha256"], "worker_sha256")
    if not isinstance(worker["command_argv"], list) or not worker["command_argv"]:
        raise InventoryV2ContractError("unittest worker command must be nonempty")
    for arg in worker["command_argv"]:
        _require_nonempty_nfc(arg, "worker command argument")
    network = _require_object(
        value["network_isolation"],
        {
            "codex_executable_path", "codex_executable_sha256",
            "codex_network_allow_local_binding", "command_argv", "fail_closed",
            "kind", "profile", "proxy_environment", "sandbox_available",
        },
        "UnittestNetworkIsolationV1",
    )
    codex_path = _require_nonempty_nfc(
        network["codex_executable_path"], "Codex executable path"
    )
    _require_sha256_field(
        network["codex_executable_sha256"], "Codex executable SHA-256"
    )
    command = network["command_argv"]
    if (
        network["kind"] != "codex-windows-sandbox"
        or network["profile"] != ":workspace"
        or network["sandbox_available"] is not True
        or network["fail_closed"] is not True
        or not isinstance(command, list)
        or len(command) < 10
        or command[:7] != [codex_path, "-c", 'windows.sandbox="elevated"', "sandbox", "-P", ":workspace", "-C"]
        or command[7] != isolated_checkout_path
        or command[8] != "--"
        or command[9:] != worker["command_argv"]
        or worker["command_argv"][0] != python_identity["executable_path"]
    ):
        raise InventoryV2ContractError(
            "unittest worker did not use the exact public Codex sandbox boundary"
        )
    for arg in command:
        _require_nonempty_nfc(arg, "sandbox command argument")
    if network["proxy_environment"] != {
        "ALL_PROXY": None,
        "HTTPS_PROXY": None,
        "HTTP_PROXY": None,
        "NO_PROXY": None,
        "all_proxy": None,
        "https_proxy": None,
        "http_proxy": None,
        "no_proxy": None,
    } or network["codex_network_allow_local_binding"] != "1":
        raise InventoryV2ContractError(
            "unittest sandbox must clear proxies and explicitly allow loopback binding"
        )

    parents = value["parent_records"]
    if not isinstance(parents, list) or len(parents) != 893:
        raise InventoryV2ContractError(
            "unittest recapture must bind exactly 893 executable parents"
        )
    parent_ids: list[str] = []
    for record in parents:
        _require_object(
            record, {"baseline_id", "native_id", "predecessor_entry_sha256"},
            "UnittestParentRecordV1",
        )
        baseline_id = _require_nonempty_nfc(record["baseline_id"], "baseline_id")
        native_id = _require_nonempty_nfc(record["native_id"], "native_id")
        _require_sha256_field(
            record["predecessor_entry_sha256"], "predecessor_entry_sha256"
        )
        if not baseline_id.endswith("python-unittest::" + native_id):
            raise InventoryV2ContractError("unittest parent native identity mismatch")
        parent_ids.append(baseline_id)
    if parent_ids != sorted(set(parent_ids)):
        raise InventoryV2ContractError("unittest parent records must be sorted and unique")
    if proof_hash("kd4.unittest-recapture-parent-record-set.v1", parents) != UNITTEST_RECAPTURE_PARENT_RECORDS_SHA256:
        raise InventoryV2ContractError(
            "unittest parent record set is not the frozen 893-parent executable set"
        )
    parent_ids = [row["baseline_id"] for row in executable_parents]
    if len(parent_ids) != 859:
        raise InventoryV2ContractError("unittest approved source exception must leave 859 executable parents")
    parent_set = set(parent_ids)

    sites = value["source_site_manifest"]
    if not isinstance(sites, list):
        raise InventoryV2ContractError("unittest source-site manifest must be an array")
    site_by_id: dict[str, Any] = {}
    for site in sites:
        _require_object(
            site,
            {"column", "declared_site_id", "line", "parent_baseline_id", "path"},
            "UnittestSourceSiteV1",
        )
        require_strict_repository_path(site["path"])
        _require_integer(site["line"], 1, "source site line")
        _require_integer(site["column"], 1, "source site column")
        if site["parent_baseline_id"] not in parent_set:
            raise InventoryV2ContractError("unittest source site has an unknown parent")
        if site["declared_site_id"] != _unittest_site_id_v1(site):
            raise InventoryV2ContractError("unittest source site ID mismatch")
        if site["declared_site_id"] in site_by_id:
            raise InventoryV2ContractError("duplicate unittest source site ID")
        site_by_id[site["declared_site_id"]] = site
    if sites != sorted(sites, key=lambda item: item["declared_site_id"]):
        raise InventoryV2ContractError("unittest source-site manifest must be sorted")
    source_audit = _require_object(
        value["source_audit"],
        {
            "embedded_non_ast_marker_count", "executable_ast_site_count", "excepted_ast_site_count",
            "source_file_count", "source_site_manifest_sha256", "textual_marker_count",
        },
        "UnittestSourceAuditV1",
    )
    expected_manifest_sha256 = proof_hash(
        "kd4.unittest-recapture-source-site-manifest.v1", sites
    )
    if source_audit != {
        "embedded_non_ast_marker_count": 1,
        "executable_ast_site_count": 59,
        "excepted_ast_site_count": 9,
        "source_file_count": 21,
        "source_site_manifest_sha256": source_site_sha256,
        "textual_marker_count": 69,
    } or expected_manifest_sha256 != source_site_sha256 or len(sites) != 59 or len({site["path"] for site in sites}) != 21:
        raise InventoryV2ContractError("unittest recapture source audit mismatch")

    occurrences = value["subtest_occurrences"]
    if not isinstance(occurrences, list):
        raise InventoryV2ContractError("unittest subtest occurrences must be an array")
    occurrence_keys: list[tuple[str, str, int]] = []
    occurrences_by_parent: dict[str, list[Any]] = {parent: [] for parent in parent_ids}
    for occurrence in occurrences:
        _require_object(
            occurrence,
            {
                "canonical_context_projection", "declared_site_id",
                "occurrence_ordinal", "parent_baseline_id",
            },
            "UnittestSubtestOccurrenceV1",
        )
        parent = occurrence["parent_baseline_id"]
        site_id = occurrence["declared_site_id"]
        ordinal = _require_integer(
            occurrence["occurrence_ordinal"], 0, "subtest occurrence ordinal"
        )
        if parent not in parent_set or site_id not in site_by_id or site_by_id[site_id]["parent_baseline_id"] != parent:
            raise InventoryV2ContractError("unittest subtest occurrence has an unknown parent/site")
        validate_canonical_parameter_projection_v1(
            occurrence["canonical_context_projection"]
        )
        occurrence_keys.append((parent, site_id, ordinal))
        occurrences_by_parent[parent].append(occurrence)
    if occurrence_keys != sorted(set(occurrence_keys)):
        raise InventoryV2ContractError("unittest subtest occurrences must be sorted and unique")
    ordinal_groups: dict[tuple[str, str], list[int]] = {}
    for parent, site_id, ordinal in occurrence_keys:
        ordinal_groups.setdefault((parent, site_id), []).append(ordinal)
    if any(ordinals != list(range(len(ordinals))) for ordinals in ordinal_groups.values()):
        raise InventoryV2ContractError("unittest subtest ordinals must be contiguous per parent/site")

    expected_manifests = []
    for parent in parent_ids:
        projection = _unittest_manifest_projection_v1(parent, occurrences_by_parent[parent])
        expected_manifests.append(
            {
                **projection,
                "manifest_sha256": proof_hash(
                    "kd4.unittest-subtest-manifest.v1",
                    {"occurrences": occurrences_by_parent[parent], **projection},
                ),
            }
        )
    if value["parent_manifests"] != expected_manifests:
        raise InventoryV2ContractError("unittest parent manifests do not match occurrences")

    artifacts = _require_object(
        value["artifacts"],
        {"parent_manifest", "report", "stderr", "stdout"},
        "UnittestArtifactsV1",
    )
    artifact_raw = {
        name: _validate_unittest_artifact_v1(artifacts[name], f"unittest {name}")
        for name in ("parent_manifest", "report", "stderr", "stdout")
    }
    try:
        parent_manifest = parse_canonical_jcs(artifact_raw["parent_manifest"])
    except InventoryV2ContractError as exc:
        raise InventoryV2ContractError(
            "unittest parent manifest artifact must be canonical JSON"
        ) from exc
    if parent_manifest != {
        "baseline_commit": UNITTEST_RECAPTURE_BASELINE_COMMIT,
        "format_id": "kd4.unittest-parent-manifest.v1",
        "frozen_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "parent_records": executable_parents,
        "schema_version": 1,
        "source_tree_sha256": UNITTEST_RECAPTURE_SOURCE_TREE_SHA256,
    }:
        raise InventoryV2ContractError(
            "unittest parent manifest does not bind the frozen parent records"
        )
    results = value["parent_results"]
    bindings = value["output_bindings"]
    if (
        not isinstance(results, list)
        or not isinstance(bindings, list)
        or len(results) != 859
        or len(bindings) != 859
    ):
        raise InventoryV2ContractError(
            "unittest results/output bindings must cover 859 authenticated parents"
        )
    binding_by_parent: dict[str, Any] = {}
    for binding in bindings:
        _require_object(
            binding,
            {"parent_baseline_id", "parent_result_sha256", "report_sha256", "stderr_sha256", "stdout_sha256"},
            "UnittestOutputBindingV1",
        )
        parent = binding["parent_baseline_id"]
        if parent in binding_by_parent or parent not in parent_set:
            raise InventoryV2ContractError("unittest output binding parent mismatch")
        for name in ("report", "stderr", "stdout"):
            if binding[f"{name}_sha256"] != artifacts[name]["sha256"]:
                raise InventoryV2ContractError("unittest output binding artifact mismatch")
        _require_sha256_field(binding["parent_result_sha256"], "parent_result_sha256")
        binding_by_parent[parent] = binding
    if [item["parent_baseline_id"] for item in bindings] != parent_ids:
        raise InventoryV2ContractError("unittest output bindings must be parent sorted")
    terminal_count = 0
    for result, parent in zip(results, parent_ids):
        _require_object(
            result,
            {"parent_baseline_id", "selected", "skip_reason", "started", "terminal_result"},
            "UnittestParentResultV1",
        )
        if result["parent_baseline_id"] != parent or result["selected"] is not True or result["started"] is not True:
            raise InventoryV2ContractError("unittest parent was not exactly selected and started")
        terminal = result["terminal_result"]
        if terminal != "passed" or result["skip_reason"] is not None:
            raise InventoryV2ContractError("every unittest parent must pass without a skip reason")
        result_sha = proof_hash("kd4.unittest-parent-result.v1", result)
        if binding_by_parent[parent]["parent_result_sha256"] != result_sha:
            raise InventoryV2ContractError("unittest output binding parent result mismatch")
        terminal_count += 1

    try:
        report = parse_canonical_jcs(artifact_raw["report"])
    except InventoryV2ContractError as exc:
        raise InventoryV2ContractError(
            "unittest report artifact must be canonical JSON"
        ) from exc
    report_counts = {
        "selected_parent_count": 859,
        "started_parent_count": 859,
        "subtest_occurrence_count": len(occurrences),
        "terminal_parent_count": terminal_count,
    }
    expected_report = {
        "format_id": "kd4.unittest-execution-report.v1",
        "frozen_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "output_binding_results": [
            {
                "parent_baseline_id": binding["parent_baseline_id"],
                "parent_result_sha256": binding["parent_result_sha256"],
            }
            for binding in bindings
        ],
        "parent_manifest_sha256": artifacts["parent_manifest"]["sha256"],
        "parent_results": results,
        "schema_version": 1,
        "selection": {
            "intended_count": 859,
            "intended_native_ids": [record["native_id"] for record in executable_parents],
            "selected_count": 859,
            "selected_native_ids": [record["native_id"] for record in executable_parents],
        },
        "source_site_manifest": sites,
        "subtest_occurrences": occurrences,
        "total_counts": report_counts,
        "untrusted_observations": {
            "socket_policy": "loopback-only",
            "checkout": {
                "clean_after": isolation["clean_after"],
                "clean_before": isolation["clean_before"],
                "execution_working_directory": isolation[
                    "execution_working_directory"
                ],
                "head_commit": isolation["head_commit"],
                "source_tree_sha256": isolation["source_tree_sha256"],
            },
            "environment": {
                "codex_network_allow_local_binding": network[
                    "codex_network_allow_local_binding"
                ],
                "proxy_environment": network["proxy_environment"],
            },
            "process": {
                "command_argv": worker["command_argv"],
                "python_executable_path": python_identity["executable_path"],
                "python_executable_sha256": python_identity["executable_sha256"],
                "worker_sha256": worker["worker_sha256"],
            },
        },
    }
    if report != expected_report:
        raise InventoryV2ContractError(
            "unittest report semantic execution content mismatch"
        )

    counts = _require_object(
        value["total_counts"],
        {
            "method_body_child_count", "parent_record_count", "recovered_child_count",
            "selected_parent_count", "started_parent_count", "subtest_occurrence_count",
            "terminal_parent_count",
        },
        "UnittestTotalCountsV1",
    )
    expected_counts = {
        "method_body_child_count": 859,
        "parent_record_count": 893,
        "recovered_child_count": 859 + len(occurrences),
        "selected_parent_count": 859,
        "started_parent_count": 859,
        "subtest_occurrence_count": len(occurrences),
        "terminal_parent_count": terminal_count,
    }
    if counts != expected_counts:
        raise InventoryV2ContractError("unittest recapture total counts mismatch")
    _require_sha256_field(value["receipt_sha256"], "receipt_sha256")
    projection = {key: item for key, item in value.items() if key != "receipt_sha256"}
    if value["receipt_sha256"] != proof_hash("kd4.unittest-recapture-receipt.v1", projection):
        raise InventoryV2ContractError("unittest recapture receipt hash mismatch")


def validate_doctest_recapture_packet_v1(value: Any) -> None:
    """Validate a complete, successful, lossless baseline doctest recapture."""

    fields = {
        "attempt_id", "baseline_commit", "format_id", "parent_counts",
        "raw_occurrence_count", "receipt_sha256", "repository_identity_sha256",
        "runs", "schema_version", "source_isolation", "source_tree_sha256",
        "toolchain",
    }
    _require_object(value, fields, "DoctestRecapturePacketV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "DoctestRecapturePacketV1")
    if value["format_id"] != DOCTEST_RECAPTURE_FORMAT_ID:
        raise InventoryV2ContractError("invalid doctest recapture packet format")
    _require_nonempty_nfc(value["attempt_id"], "attempt_id")
    if value["baseline_commit"] != DOCTEST_RECAPTURE_BASELINE_COMMIT:
        raise InventoryV2ContractError("doctest recapture baseline commit mismatch")
    if value["source_tree_sha256"] != DOCTEST_RECAPTURE_SOURCE_TREE_SHA256:
        raise InventoryV2ContractError("doctest recapture source-tree identity mismatch")
    if (
        value["repository_identity_sha256"]
        != DOCTEST_RECAPTURE_REPOSITORY_IDENTITY_SHA256
    ):
        raise InventoryV2ContractError("doctest recapture repository identity mismatch")

    isolation = _require_object(
        value["source_isolation"],
        {"archive_command", "archive_sha256", "kind"},
        "DoctestSourceIsolationV1",
    )
    if isolation["kind"] != "git-archive" or isolation["archive_command"] != [
        "git", "archive", "--format=tar", DOCTEST_RECAPTURE_BASELINE_COMMIT,
    ]:
        raise InventoryV2ContractError("doctest recapture did not use the exact git archive")
    _require_sha256_field(isolation["archive_sha256"], "archive_sha256")

    toolchain = _require_object(
        value["toolchain"], {"cargo", "name", "rustc", "rustdoc"},
        "DoctestToolchainV1",
    )
    if toolchain["name"] != DOCTEST_RECAPTURE_TOOLCHAIN:
        raise InventoryV2ContractError("doctest recapture toolchain mismatch")
    for tool_name in ("cargo", "rustc", "rustdoc"):
        identity = _require_object(
            toolchain[tool_name],
            {
                "executable_path", "executable_sha256", "version_verbose_base64",
                "version_verbose_sha256",
            },
            "DoctestToolIdentityV1",
        )
        _require_nonempty_nfc(identity["executable_path"], "tool executable path")
        _require_sha256_field(identity["executable_sha256"], "executable_sha256")
        version_raw = _decode_recapture_artifact(
            identity["version_verbose_base64"],
            identity["version_verbose_sha256"],
            f"{tool_name} version output",
        )
        try:
            version = version_raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise InventoryV2ContractError(
                f"{tool_name} version output is not UTF-8"
            ) from exc
        if not version.startswith(f"{tool_name} 1.95.0 ") or (
            tool_name != "cargo" and "host: x86_64-pc-windows-msvc" not in version
        ) or (tool_name == "cargo" and "host: x86_64-pc-windows-msvc" not in version):
            raise InventoryV2ContractError(
                f"{tool_name} is not the exact Rust 1.95.0 MSVC tool"
            )

    runs = value["runs"]
    if not isinstance(runs, list) or len(runs) != len(DOCTEST_RECAPTURE_PACKAGE_SPECS):
        raise InventoryV2ContractError(
            "doctest recapture must contain exactly four direct package runs"
        )
    parent_ordinals = {parent: 0 for parent in DOCTEST_RECAPTURE_PARENT_TARGETS}
    flattened_count = 0
    for run, (expected_package, expected_target) in zip(
        runs, DOCTEST_RECAPTURE_PACKAGE_SPECS
    ):
        _require_object(
            run,
            {
                "command_argv", "exit_code", "package_name", "raw_occurrences",
                "selected_count", "stderr_base64", "stderr_sha256", "stdout_base64",
                "stdout_sha256", "target_id", "working_directory",
            },
            "DoctestPackageRunV1",
        )
        if (
            run["package_name"] != expected_package
            or run["target_id"] != expected_target
            or run["command_argv"] != _expected_doctest_command(expected_package)
            or run["working_directory"] != "codex-rs"
        ):
            raise InventoryV2ContractError(
                "doctest recapture package target or direct command mismatch"
            )
        if _require_integer(run["exit_code"], 0, "exit_code") != 0:
            raise InventoryV2ContractError("doctest recapture package run returned nonzero")
        stdout = _decode_recapture_artifact(
            run["stdout_base64"], run["stdout_sha256"], "doctest stdout"
        )
        _decode_recapture_artifact(
            run["stderr_base64"], run["stderr_sha256"], "doctest stderr"
        )
        try:
            listing_lines = [
                line for line in stdout.decode("utf-8").splitlines()
                if line.endswith(": test")
            ]
        except UnicodeDecodeError as exc:
            raise InventoryV2ContractError("doctest stdout is not UTF-8") from exc
        occurrences = run["raw_occurrences"]
        if not isinstance(occurrences, list) or not occurrences:
            raise InventoryV2ContractError("doctest package run selected zero doctests")
        if run["selected_count"] != len(occurrences) or len(listing_lines) != len(occurrences):
            raise InventoryV2ContractError("doctest package run is partial or count-mismatched")
        for occurrence, listing_line in zip(occurrences, listing_lines):
            _require_object(
                occurrence,
                {
                    "global_ordinal", "parent_baseline_id", "parent_ordinal",
                    "raw_listing_line",
                },
                "DoctestRawOccurrenceV1",
            )
            normalized = listing_line[:-len(": test")].replace("/", "\\")
            parent_id = "rust-doctest::" + normalized
            if occurrence["raw_listing_line"] != listing_line:
                raise InventoryV2ContractError(
                    "doctest raw listing occurrence does not match captured stdout"
                )
            if occurrence["global_ordinal"] != flattened_count:
                raise InventoryV2ContractError(
                    "doctest global occurrence ordinals must preserve output order"
                )
            if occurrence["parent_baseline_id"] != parent_id:
                raise InventoryV2ContractError(
                    "doctest occurrence does not map to its immutable frozen parent"
                )
            if DOCTEST_RECAPTURE_PARENT_TARGETS.get(parent_id) != expected_target:
                raise InventoryV2ContractError(
                    "doctest occurrence is foreign to its direct package target"
                )
            if occurrence["parent_ordinal"] != parent_ordinals[parent_id]:
                raise InventoryV2ContractError(
                    "doctest parent ordinals must preserve raw duplicate multiplicity"
                )
            parent_ordinals[parent_id] += 1
            flattened_count += 1

    if any(count == 0 for count in parent_ordinals.values()):
        raise InventoryV2ContractError(
            "doctest recapture does not cover all five immutable parents"
        )
    expected_parent_counts = [
        {"parent_baseline_id": parent, "raw_count": parent_ordinals[parent]}
        for parent in sorted(parent_ordinals)
    ]
    if value["parent_counts"] != expected_parent_counts:
        raise InventoryV2ContractError("doctest recapture parent counts mismatch")
    if value["raw_occurrence_count"] != flattened_count:
        raise InventoryV2ContractError("doctest recapture raw occurrence count mismatch")
    _require_sha256_field(value["receipt_sha256"], "receipt_sha256")
    projection = {key: item for key, item in value.items() if key != "receipt_sha256"}
    if value["receipt_sha256"] != proof_hash(
        "kd4.doctest-recapture-receipt.v1", projection
    ):
        raise InventoryV2ContractError("doctest recapture receipt hash mismatch")


def doctest_recovered_child_sources_v1(value: Any) -> list[dict[str, Any]]:
    validate_doctest_recapture_packet_v1(value)
    children: list[dict[str, Any]] = []
    for run in value["runs"]:
        for occurrence in run["raw_occurrences"]:
            parent = occurrence["parent_baseline_id"]
            ordinal = occurrence["parent_ordinal"]
            child = {
                "canonical_parameter_projection": None,
                "child_kind": "doctest",
                "declared_site_id": None,
                "executable_identity": {
                    "kind": "test",
                    "route_id": "test-route.rust-doctest.v1",
                    "test_id": f"{parent}::recovered-raw-occurrence:{ordinal}",
                    "validation_id": "rust.doctest.workspace",
                },
                "gap_id": "gap.doctest-raw-versus-unique",
                "occurrence_ordinal": ordinal,
                "parent_baseline_id": parent,
            }
            _validate_recovered_child_source_v1(child)
            children.append(child)
    return sorted(children, key=_validate_recovered_child_source_v1)


def _validate_parent_output(value: Any) -> str:
    _require_object(value, {"output_sha256", "parent_id"}, "ParentRecaptureOutputV1")
    parent_id = _require_nonempty_nfc(value["parent_id"], "parent_id")
    _require_sha256_field(value["output_sha256"], "output_sha256")
    return parent_id


def validate_canonical_parameter_projection_v1(value: Any) -> None:
    if not isinstance(value, dict):
        raise InventoryV2ContractError("CanonicalParameterProjectionV1 must be an object")
    kind = value.get("kind")
    if kind == "null":
        _require_object(value, {"kind"}, "null parameter")
    elif kind == "boolean":
        _require_object(value, {"kind", "value"}, "boolean parameter")
        if not isinstance(value["value"], bool):
            raise InventoryV2ContractError("boolean parameter value must be a boolean")
    elif kind == "integer":
        _require_object(value, {"kind", "value"}, "integer parameter")
        _require_integer(value["value"], -(1 << 53) + 1, "integer parameter value")
        if value["value"] > (1 << 53) - 1:
            raise InventoryV2ContractError("integer parameter exceeds exact I-JSON range")
    elif kind == "float64":
        _require_object(value, {"kind", "bits"}, "float64 parameter")
        bits = value["bits"]
        if not isinstance(bits, str) or re.fullmatch(r"[0-9a-f]{16}", bits) is None:
            raise InventoryV2ContractError("float64 parameter must use 16 lowercase hexadecimal digits")
        if not math.isfinite(struct.unpack(">d", bytes.fromhex(bits))[0]):
            raise InventoryV2ContractError("non-finite float64 parameter is unsupported")
    elif kind == "string":
        _require_object(value, {"kind", "value"}, "string parameter")
        if not isinstance(value["value"], str) or not unicodedata.is_normalized("NFC", value["value"]):
            raise InventoryV2ContractError("string parameter value must be an NFC string")
    elif kind == "repository-path":
        _require_object(value, {"kind", "value"}, "repository-path parameter")
        require_strict_repository_path(value["value"])
    elif kind == "bytes":
        _require_object(value, {"base64url", "kind"}, "bytes parameter")
        encoded = value["base64url"]
        if not isinstance(encoded, str) or "=" in encoded or _BASE64URL_RE.fullmatch(encoded) is None:
            raise InventoryV2ContractError("bytes parameter must be unpadded base64url")
        try:
            decoded = base64.urlsafe_b64decode(encoded + "=" * ((4 - len(encoded) % 4) % 4))
        except (ValueError, binascii.Error) as error:
            raise InventoryV2ContractError("bytes parameter must be valid base64url") from error
        if base64.urlsafe_b64encode(decoded).rstrip(b"=").decode("ascii") != encoded:
            raise InventoryV2ContractError("bytes parameter must be canonical base64url")
    elif kind in {"list", "tuple", "set"}:
        _require_object(value, {"items", "kind"}, f"{kind} parameter")
        items = value["items"]
        if not isinstance(items, list):
            raise InventoryV2ContractError(f"{kind} parameter items must be an array")
        for item in items:
            validate_canonical_parameter_projection_v1(item)
        if kind == "set":
            _require_sorted_unique_jcs(items, "set parameter items")
    elif kind == "mapping":
        _require_object(value, {"entries", "kind"}, "mapping parameter")
        entries = value["entries"]
        if not isinstance(entries, list):
            raise InventoryV2ContractError("mapping parameter entries must be an array")
        for entry in entries:
            _require_object(entry, {"key", "value"}, "canonical parameter mapping entry")
            validate_canonical_parameter_projection_v1(entry["key"])
            validate_canonical_parameter_projection_v1(entry["value"])
        _require_sorted_unique_jcs(entries, "mapping parameter entries")
    elif kind == "enum":
        _require_object(value, {"kind", "type_name", "variant"}, "enum parameter")
        require_identifier(value["type_name"])
        require_identifier(value["variant"])
    else:
        raise InventoryV2ContractError("unknown canonical parameter projection kind")
    canonical_jcs(value)


def _validate_recovered_child_source_v1(value: Any) -> str:
    _require_object(
        value,
        {
            "canonical_parameter_projection", "child_kind", "declared_site_id",
            "executable_identity", "gap_id", "occurrence_ordinal", "parent_baseline_id",
        },
        "RecoveredChildSourceV1",
    )
    _require_nonempty_nfc(value["gap_id"], "gap_id")
    _require_nonempty_nfc(value["parent_baseline_id"], "parent_baseline_id")
    validate_executable_identity_v1(value["executable_identity"])
    child_kind = value["child_kind"]
    parameters = value["canonical_parameter_projection"]
    site = value["declared_site_id"]
    ordinal = value["occurrence_ordinal"]
    if child_kind == "unittest-method-body":
        if parameters is not None or site is not None or ordinal is not None:
            raise InventoryV2ContractError("unittest method-body child fields must be null")
    elif child_kind == "unittest-subtest":
        if parameters is None or site is None or ordinal is None:
            raise InventoryV2ContractError("unittest subtest child fields must be non-null")
        validate_canonical_parameter_projection_v1(parameters)
        _require_nonempty_nfc(site, "declared_site_id")
        _require_integer(ordinal, 0, "occurrence_ordinal")
    elif child_kind == "doctest":
        if parameters is not None or site is not None or ordinal is None:
            raise InventoryV2ContractError("doctest child fields must use only an ordinal")
        _require_integer(ordinal, 0, "occurrence_ordinal")
    else:
        raise InventoryV2ContractError("invalid recovered child kind")
    identity = value["executable_identity"]
    if child_kind in {"unittest-method-body", "unittest-subtest"}:
        if identity.get("kind") != "test" or identity.get("route_id") != "test-route.python-unittest.v1" or identity.get("test_id") != value["parent_baseline_id"]:
            raise InventoryV2ContractError("unittest recovered child must use its parent unittest identity")
    elif identity.get("kind") != "test" or identity.get("route_id") != "test-route.rust-doctest.v1":
        raise InventoryV2ContractError("doctest recovered child must use the doctest route")
    elif identity.get("test_id") != (
        f"{value['parent_baseline_id']}::recovered-raw-occurrence:{ordinal}"
    ):
        raise InventoryV2ContractError(
            "doctest recovered child test ID must bind its immutable parent and raw ordinal"
        )
    return "inventory-v2-recovered." + proof_hash(
        "kd4.recovered-child-identity.v1", value
    )


def validate_recovery_transition_receipt_v1(value: Any) -> None:
    fields = {
        "authority_before_semantic_sha256", "child_obligation_ids",
        "frozen_source_authority_sha256", "parent_container_ids",
        "recapture_receipt_sha256", "receipt_sha256", "schema_version",
    }
    _require_object(value, fields, "RecoveryTransitionReceiptV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "RecoveryTransitionReceiptV1")
    _require_sorted_unique_strings(value["child_obligation_ids"], "transition child obligations", nonempty=True)
    _require_sorted_unique_strings(value["parent_container_ids"], "transition parent containers", nonempty=True)
    for field in (
        "authority_before_semantic_sha256", "frozen_source_authority_sha256",
        "recapture_receipt_sha256", "receipt_sha256",
    ):
        _require_sha256_field(value[field], field)
    projection = {key: item for key, item in value.items() if key != "receipt_sha256"}
    if value["receipt_sha256"] != proof_hash(
        "kd4.recovery-transition-receipt.v1", projection
    ):
        raise InventoryV2ContractError("recovery transition receipt hash mismatch")


def _validate_recovery_record_v1(value: Any) -> None:
    fields = {
        "current_audit", "gap_id", "kind", "legacy_evidence", "pending_requirement",
        "recovery_id", "resolution", "state", "transition_receipt_sha256",
    }
    _require_object(value, fields, "InventoryRecoveryRecordV1")
    canonical_jcs(value)
    kind = value["kind"]
    if kind not in {"doctest", "unittest"}:
        raise InventoryV2ContractError("invalid recovery kind")
    _require_nonempty_nfc(value["gap_id"], "gap_id")
    _require_nonempty_nfc(value["recovery_id"], "recovery_id")
    audit = value["current_audit"]
    legacy = value["legacy_evidence"]
    if not isinstance(audit, dict) or not isinstance(legacy, dict) or audit.get("kind") != kind or legacy.get("kind") != kind:
        raise InventoryV2ContractError("recovery kind, legacy evidence, and current audit disagree")
    if kind == "unittest":
        _require_object(legacy, {"frozen_parent_count", "historical_subtest_call_count", "kind"}, "unittest legacy evidence")
        if _require_integer(legacy["frozen_parent_count"], 0, "frozen_parent_count") != 909:
            raise InventoryV2ContractError("unittest recovery must bind 909 frozen parents")
        if legacy["historical_subtest_call_count"] is not None:
            _require_integer(legacy["historical_subtest_call_count"], 0, "historical_subtest_call_count")
        _require_object(audit, {"executable_ast_call_count", "excluded_embedded_fixture_count", "kind", "runner_site_observations", "text_call_count"}, "unittest current audit")
        observed = (
            _require_integer(audit["text_call_count"], 0, "text_call_count"),
            _require_integer(audit["executable_ast_call_count"], 0, "executable_ast_call_count"),
            _require_integer(audit["excluded_embedded_fixture_count"], 0, "excluded_embedded_fixture_count"),
        )
        sites = audit["runner_site_observations"]
        if observed != (63, 62, 1) or not isinstance(sites, list) or len(sites) != 5:
            raise InventoryV2ContractError("unittest current audit must bind exact 63/62/1 and five runner sites")
        actual_sites: list[tuple[int, int, str, str]] = []
        for site in sites:
            _require_object(site, {"column", "line", "parent_id", "path"}, "SubtestSiteObservationV1")
            if site["path"] != "scripts/test_rust_test_runner.py":
                raise InventoryV2ContractError("unittest runner site has the wrong path")
            _require_integer(site["line"], 1, "site line")
            _require_integer(site["column"], 1, "site column")
            _require_nonempty_nfc(site["parent_id"], "site parent_id")
            actual_sites.append((site["line"], site["column"], site["parent_id"], site["path"]))
        if (
            actual_sites != sorted(actual_sites)
            or len(set(actual_sites)) != len(actual_sites)
        ):
            raise InventoryV2ContractError(
                "unittest recovery evidence must use deterministic unique source anchors"
            )
    else:
        _require_object(legacy, {"frozen_unique_count", "historical_raw_count", "kind"}, "doctest legacy evidence")
        if _require_integer(legacy["frozen_unique_count"], 0, "frozen_unique_count") != 5:
            raise InventoryV2ContractError("doctest recovery must bind five frozen doctests")
        if legacy["historical_raw_count"] is not None:
            _require_integer(legacy["historical_raw_count"], 0, "historical_raw_count")
        _require_object(audit, {"declared_count", "kind", "raw_count", "unique_count"}, "doctest current audit")
        _require_integer(audit["declared_count"], 0, "declared_count")
        if audit["raw_count"] is not None:
            _require_integer(audit["raw_count"], 0, "raw_count")
        if (
            _require_integer(audit["unique_count"], 0, "unique_count") != 5
            or _require_integer(audit["declared_count"], 0, "declared_count") != 5
        ):
            raise InventoryV2ContractError("doctest current audit must bind five unique doctests")

    state = value["state"]
    if state == "pending":
        if value["resolution"] is not None or value["transition_receipt_sha256"] is not None or not isinstance(value["pending_requirement"], dict):
            raise InventoryV2ContractError("pending recovery nullable fields disagree")
        pending = value["pending_requirement"]
        if pending.get("kind") != kind:
            raise InventoryV2ContractError("recovery kind and pending requirement disagree")
        _require_nonempty_nfc(pending.get("baseline_commit"), "baseline_commit")
        _require_sorted_unique_strings(pending.get("reasons"), "recovery reasons", nonempty=True)
        if kind == "unittest":
            _require_object(pending, {"baseline_commit", "expected_parent_output_sha256s", "kind", "reasons", "required_parent_count", "required_parent_ids"}, "unittest pending requirement")
            if _require_integer(pending["required_parent_count"], 0, "required_parent_count") != 893:
                raise InventoryV2ContractError(
                    "unittest pending requirement must cover 893 executable parents"
                )
            parent_ids = _require_sorted_unique_strings(pending["required_parent_ids"], "required_parent_ids", nonempty=True)
            outputs = pending["expected_parent_output_sha256s"]
            if len(parent_ids) != 893 or not isinstance(outputs, list) or len(outputs) != 893:
                raise InventoryV2ContractError(
                    "unittest pending requirement must cover all 893 executable parents"
                )
            output_ids = [_validate_parent_output(output) for output in outputs]
            if output_ids != parent_ids or any(a >= b for a, b in zip(output_ids, output_ids[1:])):
                raise InventoryV2ContractError("parent output tuples must exactly cover required parent IDs")
            if legacy["historical_subtest_call_count"] is not None:
                raise InventoryV2ContractError(
                    "pending unittest recovery cannot claim a historical subtest count"
                )
        else:
            _require_object(pending, {"baseline_commit", "kind", "reasons", "required_package_targets"}, "doctest pending requirement")
            _require_sorted_unique_strings(pending["required_package_targets"], "required_package_targets", nonempty=True)
            if audit["raw_count"] is not None or legacy["historical_raw_count"] is not None:
                raise InventoryV2ContractError(
                    "pending doctest recovery cannot claim a historical raw count"
                )
    elif state == "resolved":
        if value["pending_requirement"] is not None or not isinstance(value["resolution"], dict):
            raise InventoryV2ContractError("resolved recovery nullable fields disagree")
        _require_sha256_field(value["transition_receipt_sha256"], "transition_receipt_sha256")
        resolution = _require_object(value["resolution"], {"child_sources", "parent_container_ids", "parent_recapture_outputs", "recapture_receipt_sha256"}, "RecoveryResolutionV1")
        children = resolution["child_sources"]
        if not isinstance(children, list) or not children:
            raise InventoryV2ContractError("recovered child sources must be nonempty")
        effective_child_ids: list[str] = []
        for child in children:
            effective_child_ids.append(_validate_recovered_child_source_v1(child))
            if child["gap_id"] != value["gap_id"]:
                raise InventoryV2ContractError("recovered child points at a different gap")
        if effective_child_ids != sorted(set(effective_child_ids)):
            raise InventoryV2ContractError("recovered child sources must be sorted and unique by effective child ID")
        parents = _require_sorted_unique_strings(
            resolution["parent_container_ids"], "parent_container_ids", nonempty=True
        )
        outputs = resolution["parent_recapture_outputs"]
        if not isinstance(outputs, list):
            raise InventoryV2ContractError("parent_recapture_outputs must be an array")
        output_ids = [_validate_parent_output(output) for output in outputs]
        if output_ids and any(a >= b for a, b in zip(output_ids, output_ids[1:])):
            raise InventoryV2ContractError("parent_recapture_outputs must be sorted and unique")
        if kind == "unittest":
            if len(outputs) != 859 or output_ids != parents:
                raise InventoryV2ContractError(
                    "resolved unittest recovery must cover all 859 authenticated parent containers"
                )
            if any(
                child["child_kind"] not in {"unittest-method-body", "unittest-subtest"}
                or child["parent_baseline_id"] not in set(parents)
                for child in children
            ):
                raise InventoryV2ContractError("unittest recovery contains a foreign child")
            method_body_parents = sorted(
                child["parent_baseline_id"]
                for child in children
                if child["child_kind"] == "unittest-method-body"
            )
            if method_body_parents != parents:
                raise InventoryV2ContractError("every unittest parent needs exactly one method-body child")
            occurrence_groups: dict[tuple[str, str], list[int]] = {}
            for child in children:
                if child["child_kind"] == "unittest-subtest":
                    occurrence_groups.setdefault(
                        (child["parent_baseline_id"], child["declared_site_id"]), []
                    ).append(child["occurrence_ordinal"])
            if any(sorted(ordinals) != list(range(len(ordinals))) for ordinals in occurrence_groups.values()):
                raise InventoryV2ContractError("subtest occurrences must be contiguous and zero-based per parent/site")
            subtest_count = sum(
                child["child_kind"] == "unittest-subtest" for child in children
            )
            if legacy["historical_subtest_call_count"] != subtest_count:
                raise InventoryV2ContractError(
                    "resolved unittest historical subtest count must equal recovered subtest children"
                )
        else:
            if outputs or len(parents) != 5:
                raise InventoryV2ContractError("doctest recovery requires exactly five parent containers")
            if any(
                child["child_kind"] != "doctest"
                or child["parent_baseline_id"] not in set(parents)
                for child in children
            ):
                raise InventoryV2ContractError("doctest recovery contains a foreign child")
            ordinal_groups: dict[str, list[int]] = {}
            for child in children:
                ordinal_groups.setdefault(child["parent_baseline_id"], []).append(
                    child["occurrence_ordinal"]
                )
            if set(ordinal_groups) != set(parents) or any(
                sorted(ordinals) != list(range(len(ordinals)))
                for ordinals in ordinal_groups.values()
            ):
                raise InventoryV2ContractError("doctest ordinals must cover all parents contiguously from zero")
            if (
                audit["raw_count"] != len(children)
                or legacy["historical_raw_count"] != len(children)
            ):
                raise InventoryV2ContractError(
                    "resolved doctest recovery raw counts must equal every recovered occurrence"
                )
        _require_sha256_field(resolution["recapture_receipt_sha256"], "recapture_receipt_sha256")
    else:
        raise InventoryV2ContractError("invalid recovery state")


def validate_inventory_recovery_authority_v1(value: Any) -> None:
    _require_object(value, {"format_id", "frozen_source_authority", "records", "schema_version", "self_hash", "semantic_sha256"}, "InventoryRecoveryAuthorityV1")
    canonical_jcs(value)
    _require_schema_version(value["schema_version"], 1, "InventoryRecoveryAuthorityV1")
    if value["format_id"] != "kd4.inventory-recovery-authority.v1":
        raise InventoryV2ContractError("invalid recovery authority format")
    frozen = _require_object(value["frozen_source_authority"], {"baseline_commit", "repository_identity_sha256", "source_tree_sha256"}, "FrozenSourceAuthorityV1")
    _require_nonempty_nfc(frozen["baseline_commit"], "baseline_commit")
    _require_sha256_field(frozen["repository_identity_sha256"], "repository_identity_sha256")
    _require_sha256_field(frozen["source_tree_sha256"], "source_tree_sha256")
    records = value["records"]
    if not isinstance(records, list) or len(records) != 2:
        raise InventoryV2ContractError("recovery authority requires exactly two records")
    for record in records:
        _validate_recovery_record_v1(record)
        pending = record["pending_requirement"]
        if pending is not None and pending["baseline_commit"] != frozen["baseline_commit"]:
            raise InventoryV2ContractError(
                "recovery record and frozen-source baseline commits disagree"
            )
    if [record["kind"] for record in records] != ["doctest", "unittest"] or records[0]["recovery_id"] >= records[1]["recovery_id"]:
        raise InventoryV2ContractError("recovery records must be sorted doctest then unittest")
    if records[0]["gap_id"] == records[1]["gap_id"]:
        raise InventoryV2ContractError("recovery records must bind distinct gaps")
    semantic_projection = {key: value[key] for key in ("format_id", "frozen_source_authority", "records", "schema_version")}
    if value["semantic_sha256"] != proof_hash("kd4.inventory-recovery-authority.semantic.v1", semantic_projection):
        raise InventoryV2ContractError("recovery authority semantic hash mismatch")
    self_projection = dict(semantic_projection)
    self_projection["semantic_sha256"] = value["semantic_sha256"]
    if value["self_hash"] != proof_hash("kd4.inventory-recovery-authority.self.v1", self_projection):
        raise InventoryV2ContractError("recovery authority self hash mismatch")


def encode_selection_request_v1(value: Any) -> str:
    validate_selection_request_v1(value)
    raw = canonical_jcs(value)
    if len(raw) > MAX_SELECTION_REQUEST_BYTES:
        raise InventoryV2ContractError("SelectionRequestV1 exceeds the size limit")
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def decode_selection_request_v1(token: str) -> Any:
    if (
        not isinstance(token, str)
        or not token
        or len(token) > MAX_SELECTION_REQUEST_TOKEN_BYTES
        or _BASE64URL_RE.fullmatch(token) is None
        or "=" in token
    ):
        raise InventoryV2ContractError("invalid unpadded base64url selection token")
    try:
        raw = base64.urlsafe_b64decode(token + "=" * ((4 - len(token) % 4) % 4))
    except (ValueError, binascii.Error) as error:
        raise InventoryV2ContractError("invalid selection token") from error
    if len(raw) > MAX_SELECTION_REQUEST_BYTES:
        raise InventoryV2ContractError("SelectionRequestV1 exceeds the size limit")
    value = parse_canonical_jcs(raw)
    validate_selection_request_v1(value)
    if encode_selection_request_v1(value) != token:
        raise InventoryV2ContractError("selection token is not canonical")
    return value


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Validate canonical KD4 Inventory V2 contract artifacts"
    )
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument(
        "--validate-doctest-recapture",
        type=Path,
        help="validate one canonical typed doctest recapture packet",
    )
    group.add_argument(
        "--validate-unittest-recapture",
        type=Path,
        help="validate one canonical typed unittest recapture packet",
    )
    args = parser.parse_args(argv)
    artifact_path = (
        args.validate_unittest_recapture
        if args.validate_unittest_recapture is not None
        else args.validate_doctest_recapture
    )
    try:
        raw = artifact_path.read_bytes()
        value = parse_canonical_jcs(raw)
        if args.validate_unittest_recapture is not None:
            validate_unittest_recapture_packet_v1(value)
            result = {
                "artifact_sha256": hashlib.sha256(raw).hexdigest(),
                "parent_record_count": value["total_counts"]["parent_record_count"],
                "result": "valid-unittest-recapture",
                "subtest_occurrence_count": value["total_counts"]["subtest_occurrence_count"],
            }
        else:
            validate_doctest_recapture_packet_v1(value)
            result = {
                "artifact_sha256": hashlib.sha256(raw).hexdigest(),
                "raw_occurrence_count": value["raw_occurrence_count"],
                "result": "valid-doctest-recapture",
            }
    except (OSError, InventoryV2ContractError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        print(f"inventory-v2 validation failed: {exc}", file=sys.stderr)
        return 1
    print(
        json.dumps(
            result,
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


__all__ = [
    "ActiveHostApplicabilityIssuerV1",
    "InventoryV2ContractError",
    "INVENTORY_V2_SCHEMA_IDS",
    "DOCTEST_RECAPTURE_PACKAGE_SPECS",
    "DOCTEST_RECAPTURE_PARENT_TARGETS",
    "UNITTEST_RECAPTURE_FORMAT_ID",
    "canonical_jcs",
    "decode_selection_request_v1",
    "doctest_recovered_child_sources_v1",
    "unittest_recovered_child_sources_v1",
    "unittest_subtest_manifests_v1",
    "encode_selection_request_v1",
    "parse_canonical_jcs",
    "proof_hash",
    "inventory_declaration_id_v2",
    "inventory_declaration_obligation_id_v2",
    "frozen_baseline_obligation_id_v2",
    "require_identifier",
    "require_nfc",
    "require_sha256",
    "require_strict_repository_path",
    "validate_selection_request_v1",
    "validate_executable_identity_v1",
    "validate_active_host_applicability_authority_v1",
    "validate_active_rust_compiled_listing_authority_v1",
    "validate_applicability_result_v1",
    "validate_cargo_build_context_observation_v1",
    "validate_cargo_target_context_spec_v1",
    "validate_doctest_recapture_packet_v1",
    "validate_unittest_recapture_packet_v1",
    "validate_executable_inventory_entry_v2",
    "validate_execution_input_contract_v1",
    "validate_frozen_test_inventory_v2",
    "validate_inventory_ledger_predecessor_closure",
    "validate_inventory_declaration_v2",
    "validate_inventory_authority_ref_v1",
    "validate_inventory_recovery_authority_v1",
    "validate_predecessor_artifact_reconciliation_v1",
    "validate_recovery_transition_receipt_v1",
    "validate_schema_resource_set_v1",
    "validate_canonical_parameter_projection_v1",
    "validate_intended_execution_projection_v1",
    "validate_path_spec_v1",
    "validate_platform_applicability_v1",
    "validate_resolved_executable_entry_v1",
    "validate_resolved_input_leaves_v1",
    "validate_runner_selector_v1",
    "validate_rust_cfg_atom_v1",
    "validate_rust_cfg_expression_v1",
    "validate_rust_cfg_invocation_receipt_v1",
    "validate_rust_cfg_predicate_v1",
    "validate_selection_v1",
    "validate_target_applicability_projection_v1",
    "validate_test_replacement_ledger_v2",
    "validate_trusted_defect_receipt_v1",
    "validate_validation_receipt_projection_v1",
]


if __name__ == "__main__":
    raise SystemExit(main())
