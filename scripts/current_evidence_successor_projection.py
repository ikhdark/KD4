#!/usr/bin/env python3
"""Project historical replacement successors onto one fresh inventory attempt.

The frozen V1 ledger owns the historical edges.  A successor enters the live
catalog only when the current inventory and the fresh Python collection reports
provide its current identity and a complete typed selector.  Known edges which
cannot be projected remain visible in ``unresolved_projection``; they are never
silently discarded and do not prevent the focused current-evidence validation
from executing.
"""

from __future__ import annotations

from collections import defaultdict
import json
from pathlib import Path, PurePosixPath
import tomllib
import unicodedata
from typing import Any, Mapping, NoReturn, Sequence

try:
    from scripts.completion_proof_inventory_v2 import (
        InventoryV2ContractError,
        canonical_jcs,
        proof_hash,
        validate_cargo_target_context_spec_v1,
        validate_frozen_test_inventory_v2,
        validate_resolved_executable_entry_v1,
        validate_test_replacement_ledger_v2,
    )
except ImportError:  # pragma: no cover - direct script execution
    from completion_proof_inventory_v2 import (  # type: ignore[no-redef]
        InventoryV2ContractError,
        canonical_jcs,
        proof_hash,
        validate_cargo_target_context_spec_v1,
        validate_frozen_test_inventory_v2,
        validate_resolved_executable_entry_v1,
        validate_test_replacement_ledger_v2,
    )


V1_INVENTORY_PATH = ".codex/validation/frozen-test-inventory-v1.json"
V1_LEDGER_PATH = ".codex/validation/test-replacements-v1.json"
V2_INVENTORY_PATH = ".codex/validation/frozen-test-inventory-v2.json"
V2_LEDGER_PATH = ".codex/validation/test-replacements-v2.json"

_FRAMEWORK_ROUTE = {
    "rust-nextest": ("test-route.rust-nextest.v1", "codex-rust-tests"),
    "rust-doctest": ("test-route.rust-doctest.v1", "codex-rust-doctests"),
    "python-unittest": ("test-route.python-unittest.v1", "maintenance.root-unittest"),
    "python-pytest": ("test-route.python-pytest.v1", "sdk.python.pytest"),
    "javascript-jest": ("test-route.javascript-jest.v1", "sdk.typescript.jest"),
    "argument-comment-lint-native": (
        "test-route.argument-comment-lint-native.v1",
        "tools.argument-comment-lint.native",
    ),
    "windows-sandbox-smoke": (
        "test-route.windows-sandbox-smoke-native.v1",
        "windows-sandbox-smoke",
    ),
}


class CurrentSuccessorProjectionError(ValueError):
    """Raised when projection authority or fresh evidence is malformed."""


def _fail(message: str) -> NoReturn:
    raise CurrentSuccessorProjectionError(message)


def _pairs_no_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            _fail(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _load_json(path: Path, label: str) -> Any:
    try:
        value = json.loads(
            path.read_text(encoding="utf-8"), object_pairs_hook=_pairs_no_duplicates
        )
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        _fail(f"cannot read {label} {path}: {error}")
    try:
        canonical_jcs(value)
    except (InventoryV2ContractError, ValueError) as error:
        _fail(f"{label} is not canonical I-JSON data: {error}")
    return value


def _nfc_string(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        _fail(f"{label} must be a nonempty string")
    if unicodedata.normalize("NFC", value) != value:
        _fail(f"{label} must be NFC")
    return value


def _repository_path(value: Any, label: str) -> str:
    text = _nfc_string(value, label)
    path = PurePosixPath(text)
    if (
        "\\" in text
        or ":" in text
        or path.is_absolute()
        or not path.parts
        or any(part in {"", ".", ".."} for part in path.parts)
        or path.as_posix() != text
    ):
        _fail(f"{label} must be a normalized repository-relative path")
    return text


def _inventory_rows(
    current_inventory: Sequence[Mapping[str, object]], repo_root: Path
) -> dict[str, dict[str, Any]]:
    expected_fields = {
        "baseline_id",
        "framework",
        "native_id",
        "source",
        "ignored",
        "platforms",
    }
    rows: dict[str, dict[str, Any]] = {}
    for index, raw in enumerate(current_inventory):
        row = dict(raw)
        if set(row) != expected_fields:
            _fail(f"current_inventory[{index}] has invalid fields")
        test_id = _nfc_string(row["baseline_id"], f"current_inventory[{index}].baseline_id")
        framework = _nfc_string(row["framework"], f"current_inventory[{index}].framework")
        native_id = _nfc_string(row["native_id"], f"current_inventory[{index}].native_id")
        source = _repository_path(row["source"], f"current_inventory[{index}].source")
        if framework not in _FRAMEWORK_ROUTE:
            _fail(f"current_inventory[{index}] has unknown framework {framework!r}")
        if not isinstance(row["ignored"], bool):
            _fail(f"current_inventory[{index}].ignored must be boolean")
        platforms = row["platforms"]
        if (
            not isinstance(platforms, list)
            or not platforms
            or any(platform not in {"darwin", "linux", "windows"} for platform in platforms)
            or platforms != sorted(set(platforms))
        ):
            _fail(f"current_inventory[{index}].platforms is invalid")
        if test_id in rows:
            _fail(f"current inventory repeats test ID {test_id!r}")
        if not (repo_root / Path(*PurePosixPath(source).parts)).exists():
            _fail(f"current inventory source does not exist: {source}")
        rows[test_id] = {
            "baseline_id": test_id,
            "framework": framework,
            "native_id": native_id,
            "source": source,
            "ignored": row["ignored"],
            "platforms": list(platforms),
        }
    return rows


def _predecessor_rows(value: Any) -> tuple[dict[str, dict[str, Any]], str]:
    if not isinstance(value, dict) or not isinstance(value.get("tests"), list):
        _fail("frozen V1 inventory has no test rows")
    semantic_hash = _nfc_string(value.get("inventory_hash"), "frozen V1 inventory hash")
    rows: dict[str, dict[str, Any]] = {}
    for index, raw in enumerate(value["tests"]):
        if not isinstance(raw, dict):
            _fail(f"frozen V1 inventory row {index} is not an object")
        baseline_id = _nfc_string(raw.get("baseline_id"), f"frozen V1 row {index} baseline_id")
        if baseline_id in rows:
            _fail(f"frozen V1 inventory repeats baseline ID {baseline_id!r}")
        rows[baseline_id] = raw
    return rows, semantic_hash


def _historical_graph(
    value: Any, expected_inventory_hash: str
) -> tuple[dict[str, list[str]], dict[str, dict[str, Any]]]:
    if not isinstance(value, dict) or not isinstance(value.get("rows"), list):
        _fail("frozen V1 replacement ledger has no row array")
    if value.get("frozen_inventory_hash") != expected_inventory_hash:
        _fail("frozen V1 replacement ledger does not bind the configured inventory")
    owners: dict[str, set[str]] = defaultdict(set)
    replacement_rows: dict[str, dict[str, Any]] = {}
    for index, raw in enumerate(value["rows"]):
        if not isinstance(raw, dict) or raw.get("resolution") != "replacement":
            continue
        baseline_id = _nfc_string(raw.get("baseline_id"), f"replacement row {index} baseline_id")
        replacements = raw.get("replacement_ids")
        if (
            not isinstance(replacements, list)
            or not replacements
            or any(not isinstance(item, str) or not item for item in replacements)
            or len(replacements) != len(set(replacements))
        ):
            _fail(f"replacement row {baseline_id!r} has invalid replacement IDs")
        if baseline_id in replacement_rows:
            _fail(f"replacement ledger repeats baseline ID {baseline_id!r}")
        replacement_rows[baseline_id] = raw
        for successor_id in sorted(replacements):
            _nfc_string(successor_id, f"replacement row {baseline_id!r} successor")
            owners[successor_id].add(baseline_id)
    return (
        {successor_id: sorted(baselines) for successor_id, baselines in sorted(owners.items())},
        replacement_rows,
    )


def _validate_v2_edge_mirror(
    value: Any,
    owners: Mapping[str, Sequence[str]],
    replacement_rows: Mapping[str, Mapping[str, Any]],
) -> None:
    try:
        validate_test_replacement_ledger_v2(value)
    except InventoryV2ContractError as error:
        _fail(f"current V2 replacement ledger is invalid: {error}")
    rows_by_baseline = {
        row["baseline_id"]: row
        for row in value["rows"]
        if row["baseline_id"] is not None
    }
    successors_by_baseline: dict[str, list[str]] = defaultdict(list)
    for successor_id, baseline_ids in owners.items():
        for baseline_id in baseline_ids:
            successors_by_baseline[baseline_id].append(successor_id)
    for baseline_id in successors_by_baseline:
        successors_by_baseline[baseline_id].sort()
    mirrored_baselines = {
        baseline_id
        for baseline_id, row in rows_by_baseline.items()
        if row.get("disposition", {}).get("kind") == "replacement"
        and row.get("disposition", {}).get("contract", {}).get("legacy_replacement_hint")
        is not None
    }
    if mirrored_baselines != set(successors_by_baseline):
        _fail("current V2 ledger does not mirror the complete historical replacement baseline set")
    for baseline_id, replacement_ids in successors_by_baseline.items():
        predecessor_row_hash = proof_hash(
            "kd4.frozen-v1-replacement-ledger-row.v1",
            dict(replacement_rows[baseline_id]),
        )
        expected_hint = {
            "predecessor_row_sha256": predecessor_row_hash,
            "replacement_ids": replacement_ids,
        }
        expected_edge_ids = sorted(
            "replacement-edge-v2."
            + proof_hash(
                "kd4.legacy-replacement-edge.v1",
                {
                    "baseline_id": baseline_id,
                    "predecessor_row_sha256": predecessor_row_hash,
                    "replacement_id": successor_id,
                },
            )
            for successor_id in replacement_ids
        )
        disposition = rows_by_baseline[baseline_id]["disposition"]
        if (
            disposition["contract"]["legacy_replacement_hint"] != expected_hint
            or disposition["edge_ids"] != expected_edge_ids
        ):
            _fail(f"current V2 ledger rewired historical replacement baseline {baseline_id!r}")


def _fresh_python_collections(
    unittest_path: Path, pytest_path: Path
) -> tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]]:
    unittest = _load_json(unittest_path, "fresh unittest collection report")
    pytest = _load_json(pytest_path, "fresh pytest collection report")
    if (
        not isinstance(unittest, dict)
        or unittest.get("schema_version") != 2
        or unittest.get("report_type") != "CompletionProofUnittestCollectionV2"
        or unittest.get("framework") != "python-unittest"
        or unittest.get("classification") != "discovered"
        or not isinstance(unittest.get("tests"), list)
    ):
        _fail("fresh unittest collection report has an invalid envelope")
    if (
        not isinstance(pytest, dict)
        or pytest.get("schema_version") != 1
        or pytest.get("framework") != "python-pytest"
        or pytest.get("classification") != "discovered"
        or not isinstance(pytest.get("tests"), list)
    ):
        _fail("fresh pytest collection report has an invalid envelope")
    unittest_by_id: dict[str, dict[str, Any]] = {}
    for index, raw in enumerate(unittest["tests"]):
        if not isinstance(raw, dict):
            _fail(f"fresh unittest collection row {index} is not an object")
        test_id = _nfc_string(raw.get("id"), f"fresh unittest row {index} ID")
        source = _repository_path(raw.get("source_path"), f"fresh unittest row {index} source")
        sites = raw.get("declared_subtest_sites")
        if not isinstance(sites, list):
            _fail(f"fresh unittest row {index} has no declared subtest sites")
        normalized_sites: list[dict[str, Any]] = []
        for site_index, site in enumerate(sites):
            if not isinstance(site, dict) or set(site) != {"path", "line", "column"}:
                _fail(f"fresh unittest row {index} subtest site {site_index} is invalid")
            if site["path"] != source or type(site["line"]) is not int or type(site["column"]) is not int:
                _fail(f"fresh unittest row {index} subtest site {site_index} is inconsistent")
            if site["line"] <= 0 or site["column"] <= 0:
                _fail(f"fresh unittest row {index} subtest site {site_index} is invalid")
            normalized_sites.append(dict(site))
        site_keys = [
            (str(site["path"]), int(site["line"]), int(site["column"]))
            for site in normalized_sites
        ]
        if site_keys != sorted(set(site_keys)):
            _fail(f"fresh unittest row {index} subtest sites are not sorted and unique")
        if test_id in unittest_by_id:
            _fail(f"fresh unittest collection repeats {test_id!r}")
        unittest_by_id[test_id] = {
            "source_path": source,
            "declared_subtest_sites": normalized_sites,
        }
    pytest_by_id: dict[str, dict[str, Any]] = {}
    for index, raw in enumerate(pytest["tests"]):
        if not isinstance(raw, dict) or set(raw) != {"id", "skip_markers"}:
            _fail(f"fresh pytest collection row {index} is invalid")
        test_id = _nfc_string(raw["id"], f"fresh pytest row {index} ID")
        if not isinstance(raw["skip_markers"], list):
            _fail(f"fresh pytest row {index} skip markers are invalid")
        if test_id in pytest_by_id:
            _fail(f"fresh pytest collection repeats {test_id!r}")
        pytest_by_id[test_id] = dict(raw)
    return unittest_by_id, pytest_by_id


def _path_spec_contains_source(spec: Mapping[str, Any], source_path: str) -> int:
    if spec.get("kind") == "exact" and isinstance(spec.get("path"), str):
        owned = str(spec["path"])
        if source_path == owned or source_path.startswith(owned + "/"):
            return len(PurePosixPath(owned).parts)
    if (
        spec.get("kind") == "glob"
        and isinstance(spec.get("root"), str)
        and isinstance(spec.get("pattern"), str)
    ):
        root = str(spec["root"])
        if source_path == root or source_path.startswith(root + "/"):
            relative = source_path[len(root) :].lstrip("/")
            if relative and PurePosixPath(relative).match(str(spec["pattern"])):
                return len(PurePosixPath(root).parts)
    return -1


def _authoritative_execution_contract(
    source_path: str, contracts: Sequence[Mapping[str, Any]]
) -> dict[str, Any] | None:
    candidates: list[tuple[int, bytes, dict[str, Any]]] = []
    for raw in contracts:
        contract = dict(raw)
        score = max(
            (
                _path_spec_contains_source(spec, source_path)
                for spec in contract.get("owned", [])
                if isinstance(spec, dict)
            ),
            default=-1,
        )
        if score >= 0:
            candidates.append((score, canonical_jcs(contract), contract))
    if not candidates:
        return None
    best_score = max(score for score, _, _ in candidates)
    best = [(encoded, contract) for score, encoded, contract in candidates if score == best_score]
    unique = {encoded: contract for encoded, contract in best}
    if len(unique) != 1:
        return None
    return next(iter(unique.values()))


def _cargo_context_is_current(
    repo_root: Path, row: Mapping[str, Any], context: Mapping[str, Any]
) -> bool:
    try:
        manifest_path = repo_root / Path(
            *PurePosixPath(context["package_manifest_path"]).parts
        )
        target_path = repo_root / Path(
            *PurePosixPath(context["target_source_path"]).parts
        )
        workspace_path = repo_root / Path(
            *PurePosixPath(context["workspace_manifest_path"]).parts
        )
        source_path = repo_root / Path(*PurePosixPath(row["source"]).parts)
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        source_path.resolve().relative_to(manifest_path.parent.resolve())
    except (KeyError, OSError, UnicodeError, ValueError, tomllib.TOMLDecodeError):
        return False
    if not target_path.is_file() or not workspace_path.is_file():
        return False
    if manifest.get("package", {}).get("name") != context["package_name"]:
        return False
    selection = context["feature_selection"]
    selected_features = (
        selection.get("additional_features", [])
        if selection.get("kind") == "default"
        else selection.get("features", [])
    )
    declared_features = manifest.get("features", {})
    return all(
        (
            feature.split("/", 1)[1]
            if feature.startswith(str(context["package_name"]) + "/")
            else feature
        )
        in declared_features
        for feature in selected_features
    )


def _selector_and_context(
    repo_root: Path,
    row: Mapping[str, Any],
    unittest_by_id: Mapping[str, Mapping[str, Any]],
    pytest_by_id: Mapping[str, Mapping[str, Any]],
    contexts: Sequence[Mapping[str, Any]],
    doctest_ordinal: int,
) -> tuple[dict[str, Any] | None, dict[str, Any] | None, str | None]:
    framework = row["framework"]
    native_id = row["native_id"]
    source = row["source"]
    if framework == "python-unittest":
        observation = unittest_by_id.get(native_id)
        if observation is None:
            return None, None, "missing-fresh-unittest-collection-observation"
        if observation["source_path"] != source:
            return None, None, "fresh-unittest-source-mismatch"
        manifest = {
            "declared_subtests": observation["declared_subtest_sites"],
            "parent_test_id": native_id,
            "source_path": source,
        }
        return (
            {
                "kind": "python-unittest",
                "parent_test_id": native_id,
                "selection_unit": "parent-with-all-declared-subtests",
                "subtest_manifest_sha256": proof_hash(
                    "kd4.python-unittest-subtest-manifest.source-declared.v1", manifest
                ),
            },
            None,
            None,
        )
    if framework == "python-pytest":
        if native_id not in pytest_by_id:
            return None, None, "missing-fresh-pytest-collection-observation"
        expected_source = "sdk/python/" + native_id.split("::", 1)[0]
        if expected_source != source:
            return None, None, "fresh-pytest-source-mismatch"
        return {"kind": "python-pytest", "node_id": native_id}, None, None
    if framework == "rust-nextest":
        try:
            package_name, rest = native_id.split("::", 1)
            binary_name, harness_name = rest.split("$", 1)
        except ValueError:
            return None, None, "invalid-fresh-rust-nextest-native-id"
        matches = [
            dict(context)
            for context in contexts
            if context["package_name"] == package_name
            and context["target_name"] == binary_name
        ]
        if len(matches) != 1:
            return None, None, "missing-or-ambiguous-current-cargo-target-context"
        context = matches[0]
        if not _cargo_context_is_current(repo_root, row, context):
            return None, None, "stale-current-cargo-target-context"
        return (
            {
                "cargo_target_context_spec_sha256": context["context_sha256"],
                "harness_test_name": harness_name,
                "kind": "rust-nextest",
                "nextest_binary_id": f"{package_name}::{binary_name}",
            },
            context,
            None,
        )
    if framework == "rust-doctest":
        matches = [
            dict(context)
            for context in contexts
            if context["target_source_path"] == source
            and context["target_kind"] in {"lib", "proc-macro"}
        ]
        if len(matches) != 1 or " - " not in native_id or " (line " not in native_id:
            return None, None, "missing-or-ambiguous-current-doctest-context"
        context = matches[0]
        if not _cargo_context_is_current(repo_root, row, context):
            return None, None, "stale-current-doctest-context"
        item_path = native_id.split(" - ", 1)[1].rsplit(" (line ", 1)[0]
        return (
            {
                "cargo_target_context_spec_sha256": context["context_sha256"],
                "declaration_ordinal": doctest_ordinal,
                "harness_test_name": native_id,
                "item_path": item_path,
                "kind": "rust-doctest",
                "source_path": source,
            },
            context,
            None,
        )
    if framework == "argument-comment-lint-native":
        return {"case_id": native_id, "kind": framework}, None, None
    if framework == "windows-sandbox-smoke":
        return {"case_id": native_id, "kind": "windows-sandbox-smoke-native"}, None, None
    return None, None, f"no-fresh-selector-observation-for-{framework}"


def build_current_successor_projection_v1(
    *,
    repo_root: Path,
    current_inventory: Sequence[Mapping[str, object]],
    unittest_collection_report: Path,
    pytest_collection_report: Path,
    frozen_inventory_path: Path | None = None,
    replacement_ledger_path: Path | None = None,
) -> dict[str, Any]:
    """Build resolved catalog inputs plus complete non-blocking unresolved metadata."""

    repo_root = Path(repo_root).resolve()
    frozen_inventory_path = frozen_inventory_path or repo_root / V1_INVENTORY_PATH
    replacement_ledger_path = replacement_ledger_path or repo_root / V1_LEDGER_PATH
    if not frozen_inventory_path.is_absolute():
        frozen_inventory_path = repo_root / frozen_inventory_path
    if not replacement_ledger_path.is_absolute():
        replacement_ledger_path = repo_root / replacement_ledger_path

    current_by_id = _inventory_rows(current_inventory, repo_root)
    predecessor_by_id, predecessor_hash = _predecessor_rows(
        _load_json(frozen_inventory_path, "frozen V1 inventory")
    )
    owners, replacement_rows = _historical_graph(
        _load_json(replacement_ledger_path, "frozen V1 replacement ledger"),
        predecessor_hash,
    )
    historical_baselines = sorted(
        {baseline_id for baseline_ids in owners.values() for baseline_id in baseline_ids}
    )
    missing_predecessors = sorted(set(historical_baselines) - set(predecessor_by_id))
    if missing_predecessors:
        _fail(f"historical replacement baselines are absent from V1 inventory: {missing_predecessors[:20]}")

    unittest_by_id, pytest_by_id = _fresh_python_collections(
        Path(unittest_collection_report), Path(pytest_collection_report)
    )
    contexts: list[dict[str, Any]] = []
    authoritative_contracts: list[dict[str, Any]] = []
    authority_reason: str | None = None
    if owners:
        v2_inventory_path = repo_root / V2_INVENTORY_PATH
        v2_ledger_path = repo_root / V2_LEDGER_PATH
        if not v2_inventory_path.is_file():
            authority_reason = "missing-current-v2-inventory-authority"
        elif not v2_ledger_path.is_file():
            authority_reason = "missing-current-v2-ledger-authority"
        else:
            v2_inventory = _load_json(v2_inventory_path, "current V2 inventory")
            v2_ledger = _load_json(v2_ledger_path, "current V2 replacement ledger")
            try:
                validate_frozen_test_inventory_v2(v2_inventory)
            except InventoryV2ContractError as error:
                _fail(f"current V2 inventory is invalid: {error}")
            _validate_v2_edge_mirror(v2_ledger, owners, replacement_rows)
            contexts = [dict(context) for context in v2_inventory["cargo_target_context_specs"]]
            authoritative_contracts = [
                dict(contract) for contract in v2_inventory["execution_input_contracts"]
            ]
            for context in contexts:
                try:
                    validate_cargo_target_context_spec_v1(context)
                except InventoryV2ContractError as error:
                    _fail(f"current Cargo target context is invalid: {error}")

    resolved_entries: list[dict[str, Any]] = []
    contracts_by_hash: dict[str, dict[str, Any]] = {}
    contexts_by_hash: dict[str, dict[str, Any]] = {}
    successor_catalog_rows: list[dict[str, Any]] = []
    successor_owner_map: list[dict[str, Any]] = []
    unresolved_successors: list[dict[str, Any]] = []
    doctest_counts: dict[tuple[str, str], int] = defaultdict(int)
    doctest_ordinal_by_test_id: dict[str, int] = {}
    for test_id, row in sorted(current_by_id.items()):
        if row["framework"] != "rust-doctest":
            continue
        native_id = row["native_id"]
        if " - " not in native_id or " (line " not in native_id:
            continue
        item_path = native_id.split(" - ", 1)[1].rsplit(" (line ", 1)[0]
        ordinal_key = (row["source"], item_path)
        doctest_ordinal_by_test_id[test_id] = doctest_counts[ordinal_key]
        doctest_counts[ordinal_key] += 1

    for successor_id, baseline_ids in owners.items():
        reason = authority_reason
        owner_frameworks = {
            predecessor_by_id[baseline_id].get("framework") for baseline_id in baseline_ids
        }
        if len(owner_frameworks) != 1 or not all(
            isinstance(framework, str) and framework for framework in owner_frameworks
        ):
            reason = reason or "historical-owner-framework-conflict"
        current = current_by_id.get(successor_id)
        if current is None:
            reason = reason or "missing-current-inventory-successor"
        elif current["framework"] not in owner_frameworks:
            reason = reason or "current-successor-framework-mismatch"

        selector: dict[str, Any] | None = None
        context: dict[str, Any] | None = None
        contract: dict[str, Any] | None = None
        if reason is None and current is not None:
            selector, context, reason = _selector_and_context(
                repo_root,
                current,
                unittest_by_id,
                pytest_by_id,
                contexts,
                doctest_ordinal_by_test_id.get(successor_id, 0),
            )
            if reason is None and selector is not None:
                contract = _authoritative_execution_contract(
                    current["source"], authoritative_contracts
                )
                if contract is None:
                    reason = "missing-or-ambiguous-authoritative-execution-input-contract"
        if reason is not None or current is None or selector is None or contract is None:
            unresolved_successors.append(
                {
                    "successor_id": successor_id,
                    "baseline_ids": list(baseline_ids),
                    "reason": reason or "unresolved-current-successor",
                }
            )
            continue

        route_id, validation_id = _FRAMEWORK_ROUTE[current["framework"]]
        identity = {
            "kind": "test",
            "route_id": route_id,
            "test_id": successor_id,
            "validation_id": validation_id,
        }
        applicability = {
            "kind": "host-set",
            "required_hosts": list(current["platforms"]),
        }
        entry = {
            "cargo_target_context_spec_sha256": (
                context["context_sha256"] if context is not None else None
            ),
            "executable_identity": identity,
            "executable_identity_sha256": proof_hash(
                "kd4.executable-identity.v1", identity
            ),
            "execution_input_contract_sha256": contract["contract_sha256"],
            "platform_applicability": applicability,
            "platform_applicability_sha256": proof_hash(
                "kd4.platform-applicability.v1", applicability
            ),
            "runner_selector": selector,
            "runner_selector_sha256": proof_hash("kd4.runner-selector.v1", selector),
            "test_route_id": route_id,
            "validation_id": validation_id,
        }
        resolved = {
            "inventory_entry": entry,
            "inventory_entry_semantic_sha256": proof_hash(
                "kd4.executable-inventory-entry.v2", entry
            ),
        }
        try:
            validate_resolved_executable_entry_v1(resolved)
        except InventoryV2ContractError as error:
            _fail(f"derived successor entry {successor_id!r} is invalid: {error}")
        contracts_by_hash[contract["contract_sha256"]] = contract
        if context is not None:
            contexts_by_hash[context["context_sha256"]] = context
        resolved_entries.append(resolved)
        successor_owner_map.append(
            {"successor_id": successor_id, "baseline_ids": list(baseline_ids)}
        )
        successor_catalog_rows.append(
            {
                "test_id": successor_id,
                "framework": current["framework"],
                "native_id": current["native_id"],
                "source_path": current["source"],
                "test_route_id": route_id,
                "validation_id": validation_id,
                "runner_selector_sha256": entry["runner_selector_sha256"],
                "executable_identity_sha256": entry["executable_identity_sha256"],
                "execution_input_contract_sha256": contract["contract_sha256"],
                "platform_applicability_sha256": entry["platform_applicability_sha256"],
            }
        )

    resolved_entries.sort(key=canonical_jcs)
    successor_owner_map.sort(key=lambda row: row["successor_id"])
    successor_catalog_rows.sort(key=lambda row: row["test_id"])
    resolved_baselines = sorted(
        {baseline_id for row in successor_owner_map for baseline_id in row["baseline_ids"]}
    )
    unresolved_baselines = sorted(set(historical_baselines) - set(resolved_baselines))
    resolved_edge_count = sum(len(row["baseline_ids"]) for row in successor_owner_map)
    unresolved_edge_count = sum(len(row["baseline_ids"]) for row in unresolved_successors)
    historical_edge_count = sum(len(baseline_ids) for baseline_ids in owners.values())
    if resolved_edge_count + unresolved_edge_count != historical_edge_count:
        _fail("resolved and unresolved projections do not account for every historical edge")

    unresolved_projection = {
        "schema_version": 1,
        "historical_replacement_baseline_row_count": len(historical_baselines),
        "historical_replacement_edge_count": historical_edge_count,
        "historical_distinct_successor_count": len(owners),
        "resolved_replacement_baseline_row_count": len(resolved_baselines),
        "resolved_replacement_edge_count": resolved_edge_count,
        "resolved_successor_count": len(successor_owner_map),
        "unresolved_replacement_baseline_row_count": len(unresolved_baselines),
        "unresolved_replacement_edge_count": unresolved_edge_count,
        "unresolved_successor_count": len(unresolved_successors),
        "unresolved_baseline_ids": unresolved_baselines,
        "unresolved_successors": unresolved_successors,
    }
    return {
        "replacement_baseline_row_count": len(resolved_baselines),
        "replacement_successor_catalog": {
            "format_id": "kd4.replacement-successor-catalog.v1",
            "schema_version": 1,
            "successors": successor_catalog_rows,
        },
        "successor_owner_map": successor_owner_map,
        "resolved_successor_entries": resolved_entries,
        "execution_input_contracts": sorted(contracts_by_hash.values(), key=canonical_jcs),
        "cargo_target_context_specs": sorted(
            contexts_by_hash.values(), key=lambda row: row["context_sha256"]
        ),
        "unresolved_projection": unresolved_projection,
    }


__all__ = [
    "CurrentSuccessorProjectionError",
    "build_current_successor_projection_v1",
]
