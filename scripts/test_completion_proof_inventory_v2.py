from __future__ import annotations

import base64
import hashlib
import hmac
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
import unicodedata

from scripts.completion_proof_inventory_v2 import InventoryV2ContractError
from scripts.completion_proof_inventory_v2 import ActiveHostApplicabilityIssuerV1
from scripts.completion_proof_inventory_v2 import FROZEN_V1_BASELINE_ASSOCIATIONS_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_BASELINE_IDS_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_INVENTORY_RAW_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_INVENTORY_SEMANTIC_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_LEDGER_RAW_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_WORKSPACE_FINGERPRINT
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_IDS
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_PATHS
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_RAW_SHA256S
from scripts.completion_proof_inventory_v2 import canonical_jcs
from scripts.completion_proof_inventory_v2 import decode_selection_request_v1
from scripts.completion_proof_inventory_v2 import doctest_recovered_child_sources_v1
from scripts.completion_proof_inventory_v2 import encode_selection_request_v1
from scripts.completion_proof_inventory_v2 import proof_hash
from scripts.completion_proof_inventory_v2 import inventory_declaration_id_v2
from scripts.completion_proof_inventory_v2 import inventory_declaration_obligation_id_v2
from scripts.completion_proof_inventory_v2 import frozen_baseline_obligation_id_v2
from scripts.completion_proof_inventory_v2 import require_strict_repository_path
from scripts.completion_proof_inventory_v2 import validate_executable_identity_v1
from scripts.completion_proof_inventory_v2 import validate_applicability_result_v1
from scripts.completion_proof_inventory_v2 import validate_active_host_applicability_authority_v1
from scripts.completion_proof_inventory_v2 import validate_canonical_parameter_projection_v1
from scripts.completion_proof_inventory_v2 import validate_cargo_build_context_observation_v1
from scripts.completion_proof_inventory_v2 import validate_cargo_target_context_spec_v1
from scripts.completion_proof_inventory_v2 import validate_frozen_test_inventory_v2
from scripts.completion_proof_inventory_v2 import validate_inventory_ledger_predecessor_closure
from scripts.completion_proof_inventory_v2 import validate_inventory_recovery_authority_v1
from scripts.completion_proof_inventory_v2 import validate_inventory_declaration_v2
from scripts.completion_proof_inventory_v2 import validate_predecessor_artifact_reconciliation_v1
from scripts.completion_proof_inventory_v2 import validate_provenance_receipt_v1
from scripts.completion_proof_inventory_v2 import validate_recovery_transition_receipt_v1
from scripts.completion_proof_inventory_v2 import validate_schema_resource_set_v1
from scripts.completion_proof_inventory_v2 import validate_intended_execution_projection_v1
from scripts.completion_proof_inventory_v2 import validate_path_spec_v1
from scripts.completion_proof_inventory_v2 import validate_resolved_input_leaves_v1
from scripts.completion_proof_inventory_v2 import validate_runner_selector_v1
from scripts.completion_proof_inventory_v2 import validate_rust_cfg_expression_v1
from scripts.completion_proof_inventory_v2 import validate_selection_v1
from scripts.completion_proof_inventory_v2 import validate_selection_request_v1
from scripts.completion_proof_inventory_v2 import validate_target_applicability_projection_v1
from scripts.completion_proof_inventory_v2 import validate_test_replacement_ledger_v2
from scripts.completion_proof_inventory_v2 import validate_trusted_defect_receipt_v1
from scripts.completion_proof_inventory_v2 import validate_unittest_recapture_packet_v1
from scripts.completion_proof_inventory_v2 import validate_validation_receipt_projection_v1


REPO_ROOT = Path(__file__).resolve().parents[1]
VECTORS_PATH = REPO_ROOT / "scripts/fixtures/completion_proof_v2_cross_language_vectors.json"


def _unittest_recapture_fail_closed_packet_v1() -> dict[str, object]:
    """Return the smallest closed packet that reaches the unavailable authority."""

    return {
        "artifacts": {},
        "attempt_id": "fail-closed-contract-test",
        "baseline_commit": "60bb133fa0a4f25e83851ab16d8c462e5f42ff95",
        "format_id": "kd4.unittest-recapture.v1",
        "frozen_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "network_isolation": {},
        "output_bindings": [],
        "parent_manifests": [],
        "parent_records": [],
        "parent_results": [],
        "python_identity": {},
        "receipt_sha256": "0" * 64,
        "repository_identity_sha256": (
            "f386e4786f3a61829ecdd61e764fa9d65eddbd08f2902745c6480bce448573cc"
        ),
        "schema_version": 1,
        "source_audit": {},
        "source_isolation": {},
        "source_site_manifest": [],
        "source_tree_sha256": (
            "654591dd1ddda7a77312172ec7c70e60e80990590c7c74a7c3b08a445279d90e"
        ),
        "subtest_occurrences": [],
        "total_counts": {},
        "worker_identity": {},
    }


def _full_scale_integration_fixture_v2() -> dict[str, object]:
    """Build a temporary, non-canonical 15,547-row bundle for both validators."""
    doctest_recapture_raw = (
        REPO_ROOT
        / ".codex/validation/frozen-test-inventory-v2-doctest-recapture.json"
    ).read_bytes()
    doctest_recapture = json.loads(doctest_recapture_raw)
    assert canonical_jcs(doctest_recapture) == doctest_recapture_raw
    frozen_inventory = json.loads(
        (REPO_ROOT / ".codex/validation/frozen-test-inventory-v1.json").read_text(
            encoding="utf-8"
        )
    )
    frozen_ledger = json.loads(
        (REPO_ROOT / ".codex/validation/test-replacements-v1.json").read_text(
            encoding="utf-8"
        )
    )
    predecessor_entries = {
        row["baseline_id"]: row for row in frozen_inventory["tests"]
    }
    baseline_ids = sorted(predecessor_entries)
    assert baseline_ids == sorted(row["baseline_id"] for row in frozen_ledger["rows"])
    assert len(baseline_ids) == 15_544
    associations = [
        {
            "baseline_id": baseline_id,
            "predecessor_entry_sha256": proof_hash(
                "kd4.frozen-v1-inventory-entry.v1",
                predecessor_entries[baseline_id],
            ),
        }
        for baseline_id in baseline_ids
    ]
    reconciliation = {
        "frozen_baseline_associations": associations,
        "frozen_baseline_associations_sha256": FROZEN_V1_BASELINE_ASSOCIATIONS_SHA256,
        "frozen_baseline_ids": baseline_ids,
        "frozen_baseline_ids_sha256": FROZEN_V1_BASELINE_IDS_SHA256,
        "frozen_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "frozen_inventory_semantic_sha256": FROZEN_V1_INVENTORY_SEMANTIC_SHA256,
        "frozen_ledger_raw_sha256": FROZEN_V1_LEDGER_RAW_SHA256,
        "schema_version": 1,
    }
    reconciliation["projection_sha256"] = proof_hash(
        "kd4.predecessor-artifact-reconciliation.v1", reconciliation
    )

    def execution_input_contract(path: str) -> dict[str, object]:
        projection = {
            "consumed": [],
            "owned": [{"kind": "exact", "path": path}],
            "schema_version": 1,
        }
        digest = proof_hash("kd4.execution-input-contract.v1", projection)
        return {
            "consumed": [],
            "contract_id": f"execution-input-contract-v1.{digest}",
            "contract_sha256": digest,
            "owned": projection["owned"],
            "schema_version": 1,
        }

    contracts = sorted(
        [
            execution_input_contract("SOURCEMAP.md"),
            execution_input_contract("docs/README.md"),
        ],
        key=canonical_jcs,
    )
    action_routes = sorted(
        [
            {
                "action_id": "documentation.markdown",
                "execution_input_contract_sha256": contracts[0]["contract_sha256"],
                "validation_id": "documentation.markdown",
            },
            {
                "action_id": "maintenance.source-map",
                "execution_input_contract_sha256": contracts[1]["contract_sha256"],
                "validation_id": "maintenance.source-map",
            },
        ],
        key=canonical_jcs,
    )
    route_kinds = (
        "argument-comment-lint-native",
        "javascript-jest",
        "python-pytest",
        "python-unittest",
        "rust-doctest",
        "rust-nextest",
        "windows-sandbox-smoke-native",
    )
    routes = [
        {
            "route_id": f"test-route.{kind}.v1",
            "runner_kind": kind,
            "validation_id": f"validate.{kind}",
        }
        for kind in route_kinds
    ]
    def cargo_target_context(
        package_manifest_path: str,
        package_name: str,
        target_kind: str,
        target_name: str,
        target_source_path: str,
    ) -> dict[str, object]:
        projection = {
            "cargo_profile": "test",
            "feature_selection": {"additional_features": [], "kind": "default"},
            "package_manifest_path": package_manifest_path,
            "package_name": package_name,
            "schema_version": 1,
            "target_kind": target_kind,
            "target_name": target_name,
            "target_source_path": target_source_path,
            "workspace_manifest_path": "codex-rs/Cargo.toml",
        }
        return {
            **projection,
            "context_sha256": proof_hash(
                "kd4.cargo-target-context-spec.v1", projection
            ),
        }

    cargo_target_contexts = [
        cargo_target_context(
            "codex-rs/http-client/Cargo.toml",
            "codex-http-client",
            "lib",
            "codex_http_client",
            "codex-rs/http-client/src/lib.rs",
        ),
        cargo_target_context(
            "codex-rs/core/Cargo.toml",
            "codex-core",
            "lib",
            "codex_core",
            "codex-rs/core/src/lib.rs",
        ),
        cargo_target_context(
            "codex-rs/core/Cargo.toml",
            "codex-core",
            "bench",
            "turn_latency",
            "codex-rs/core/benches/turn_latency.rs",
        ),
    ]
    cargo_target_contexts.sort(key=lambda context: context["context_sha256"])
    context_by_target = {
        (context["package_name"], context["target_name"]): context
        for context in cargo_target_contexts
    }
    applicability = {
        "kind": "host-set",
        "required_hosts": ["darwin", "linux", "windows"],
    }
    applicability_sha256 = proof_hash(
        "kd4.platform-applicability.v1", applicability
    )
    declarations = []
    for baseline_id, association in zip(baseline_ids, associations):
        identity = {
            "kind": "test",
            "route_id": "test-route.python-pytest.v1",
            "test_id": baseline_id,
            "validation_id": "validate.python-pytest",
        }
        selector = {"kind": "python-pytest", "node_id": baseline_id}
        entry = {
            "cargo_target_context_spec_sha256": None,
            "executable_identity": identity,
            "executable_identity_sha256": proof_hash(
                "kd4.executable-identity.v1", identity
            ),
            "execution_input_contract_sha256": contracts[0]["contract_sha256"],
            "platform_applicability": applicability,
            "platform_applicability_sha256": applicability_sha256,
            "runner_selector": selector,
            "runner_selector_sha256": proof_hash("kd4.runner-selector.v1", selector),
            "test_route_id": "test-route.python-pytest.v1",
            "validation_id": "validate.python-pytest",
        }
        declarations.append(
            {
                "baseline_id": baseline_id,
                "entry": entry,
                "kind": "frozen-baseline",
                "predecessor_entry_sha256": association[
                    "predecessor_entry_sha256"
                ],
            }
        )
    source_only_specs = [
        {
            "canonical_id": (
                "rust-nextest::codex-http-client::codex_http_client$"
                "outbound_proxy::tests::"
                "unsupported_platform_system_proxy_falls_back_explicitly"
            ),
            "harness_test_name": (
                "outbound_proxy::tests::"
                "unsupported_platform_system_proxy_falls_back_explicitly"
            ),
            "line": 328,
            "nextest_binary_id": "codex-http-client::codex_http_client",
            "package_name": "codex-http-client",
            "required_hosts": ["linux"],
            "source_path": "codex-rs/http-client/src/outbound_proxy_tests.rs",
            "target_name": "codex_http_client",
        },
        {
            "canonical_id": (
                "rust-nextest::codex-core::codex_core$"
                "tools::command_output_artifact::hardening_tests::"
                "read_rejects_uuid_named_symlink_outside_thread_directory"
            ),
            "harness_test_name": (
                "tools::command_output_artifact::hardening_tests::"
                "read_rejects_uuid_named_symlink_outside_thread_directory"
            ),
            "line": 5985,
            "nextest_binary_id": "codex-core::codex_core",
            "package_name": "codex-core",
            "required_hosts": ["darwin", "linux"],
            "source_path": "codex-rs/core/src/tools/command_output_artifact.rs",
            "target_name": "codex_core",
        },
        {
            "canonical_id": (
                "rust-nextest::codex-core::turn_latency_bench$"
                "turn_latency::tests::"
                "ab_worker_tree_cleanup_process_group_survives_root_exit"
            ),
            "harness_test_name": (
                "turn_latency::tests::"
                "ab_worker_tree_cleanup_process_group_survives_root_exit"
            ),
            "line": 6010,
            "nextest_binary_id": "codex-core::turn_latency_bench",
            "package_name": "codex-core",
            "required_hosts": ["darwin", "linux"],
            "source_path": "codex-rs/core/benches/turn_latency/tests.rs",
            "target_name": "turn_latency",
        },
    ]
    for spec in source_only_specs:
        context = context_by_target[(spec["package_name"], spec["target_name"])]
        identity = {
            "kind": "test",
            "route_id": "test-route.rust-nextest.v1",
            "test_id": spec["canonical_id"],
            "validation_id": "validate.rust-nextest",
        }
        selector = {
            "cargo_target_context_spec_sha256": context["context_sha256"],
            "harness_test_name": spec["harness_test_name"],
            "kind": "rust-nextest",
            "nextest_binary_id": spec["nextest_binary_id"],
        }
        source_applicability = {
            "kind": "host-set",
            "required_hosts": spec["required_hosts"],
        }
        entry = {
            "cargo_target_context_spec_sha256": context["context_sha256"],
            "executable_identity": identity,
            "executable_identity_sha256": proof_hash(
                "kd4.executable-identity.v1", identity
            ),
            "execution_input_contract_sha256": contracts[0]["contract_sha256"],
            "platform_applicability": source_applicability,
            "platform_applicability_sha256": proof_hash(
                "kd4.platform-applicability.v1", source_applicability
            ),
            "runner_selector": selector,
            "runner_selector_sha256": proof_hash(
                "kd4.runner-selector.v1", selector
            ),
            "test_route_id": "test-route.rust-nextest.v1",
            "validation_id": "validate.rust-nextest",
        }
        evidence = {
            "confirmation_state": (
                "deterministically-reconstructed-pending-off-host-compiled-confirmation"
            ),
            "line": spec["line"],
            "native_id": spec["canonical_id"].removeprefix("rust-nextest::"),
            "source_path": spec["source_path"],
        }
        provenance_projection = {
            "evidence_paths": [spec["source_path"]],
            "evidence_sha256": proof_hash(
                "kd4.source-only-declaration-evidence.v1", evidence
            ),
            "kind": "platform-pending",
            "schema_version": 1,
        }
        source_provenance = {
            **provenance_projection,
            "receipt_sha256": proof_hash(
                "kd4.provenance-receipt.v1", provenance_projection
            ),
        }
        declaration = {
            "entry": entry,
            "kind": "missing-baseline",
            "source_provenance": source_provenance,
        }
        declaration["declaration_id"] = inventory_declaration_id_v2(
            declaration["kind"], entry, source_provenance
        )
        declaration["obligation_id"] = inventory_declaration_obligation_id_v2(
            declaration["kind"], entry, source_provenance
        )
        declarations.append(declaration)
    declarations.sort(key=canonical_jcs)

    parents = [f"parent-{index:03d}" for index in range(893)]
    outputs = [
        {"output_sha256": "f" * 64, "parent_id": parent} for parent in parents
    ]
    sites = [
        {
            "column": 18,
            "line": 868,
            "parent_id": "FilteringArgumentPolicyTest.test_package_and_target_overrides_are_rejected_through_cli",
            "path": "scripts/test_rust_test_runner.py",
        },
        {
            "column": 18,
            "line": 875,
            "parent_id": "FilteringArgumentPolicyTest.test_no_tests_override_is_rejected_through_cli",
            "path": "scripts/test_rust_test_runner.py",
        },
        {
            "column": 18,
            "line": 913,
            "parent_id": "GenericRecipeGuardTest.test_every_codex_core_package_spelling_is_rejected_through_cli",
            "path": "scripts/test_rust_test_runner.py",
        },
        {
            "column": 22,
            "line": 1688,
            "parent_id": "TargetDirectoryPropagationTest.test_relative_codex_rs_target_dir_is_rejected_before_cargo_through_cli",
            "path": "scripts/test_rust_test_runner.py",
        },
        {
            "column": 18,
            "line": 1704,
            "parent_id": "TargetDirectoryPropagationTest.test_effective_environment_target_dir_is_validated_before_cargo_through_cli",
            "path": "scripts/test_rust_test_runner.py",
        },
    ]
    frozen_source_authority = {
        "baseline_commit": doctest_recapture["baseline_commit"],
        "repository_identity_sha256": doctest_recapture[
            "repository_identity_sha256"
        ],
        "source_tree_sha256": doctest_recapture["source_tree_sha256"],
    }
    recovery_parents = sorted(
        row["parent_baseline_id"] for row in doctest_recapture["parent_counts"]
    )
    recovered_children = doctest_recovered_child_sources_v1(doctest_recapture)
    recovered_child_ids = sorted(
        "inventory-v2-recovered."
        + proof_hash("kd4.recovered-child-identity.v1", child)
        for child in recovered_children
    )
    transition_receipt = {
        "authority_before_semantic_sha256": "3" * 64,
        "child_obligation_ids": recovered_child_ids,
        "frozen_source_authority_sha256": proof_hash(
            "kd4.frozen-source-authority.v1", frozen_source_authority
        ),
        "parent_container_ids": recovery_parents,
        "recapture_receipt_sha256": doctest_recapture["receipt_sha256"],
        "schema_version": 1,
    }
    transition_receipt["receipt_sha256"] = proof_hash(
        "kd4.recovery-transition-receipt.v1", transition_receipt
    )
    recovery = {
        "format_id": "kd4.inventory-recovery-authority.v1",
        "frozen_source_authority": frozen_source_authority,
        "records": [
            {
                "current_audit": {
                    "declared_count": 5,
                    "kind": "doctest",
                    "raw_count": doctest_recapture["raw_occurrence_count"],
                    "unique_count": 5,
                },
                "gap_id": "gap.doctest-raw-versus-unique",
                "kind": "doctest",
                "legacy_evidence": {
                    "frozen_unique_count": 5,
                    "historical_raw_count": doctest_recapture[
                        "raw_occurrence_count"
                    ],
                    "kind": "doctest",
                },
                "pending_requirement": None,
                "recovery_id": "a.doctest",
                "resolution": {
                    "child_sources": recovered_children,
                    "parent_container_ids": recovery_parents,
                    "parent_recapture_outputs": [],
                    "recapture_receipt_sha256": doctest_recapture[
                        "receipt_sha256"
                    ],
                },
                "state": "resolved",
                "transition_receipt_sha256": transition_receipt["receipt_sha256"],
            },
            {
                "current_audit": {
                    "executable_ast_call_count": 62,
                    "excluded_embedded_fixture_count": 1,
                    "kind": "unittest",
                    "runner_site_observations": sites,
                    "text_call_count": 63,
                },
                "gap_id": "gap.unittest",
                "kind": "unittest",
                "legacy_evidence": {
                    "frozen_parent_count": 909,
                    "historical_subtest_call_count": None,
                    "kind": "unittest",
                },
                "pending_requirement": {
                    "baseline_commit": frozen_source_authority["baseline_commit"],
                    "expected_parent_output_sha256s": outputs,
                    "kind": "unittest",
                    "reasons": ["pending"],
                    "required_parent_count": 893,
                    "required_parent_ids": parents,
                },
                "recovery_id": "b.unittest",
                "resolution": None,
                "state": "pending",
                "transition_receipt_sha256": None,
            },
        ],
        "schema_version": 1,
    }
    pending_records = json.loads(json.dumps(recovery["records"]))
    pending_doctest = pending_records[0]
    pending_doctest["current_audit"]["raw_count"] = None
    pending_doctest["legacy_evidence"]["historical_raw_count"] = None
    pending_doctest["pending_requirement"] = {
        "baseline_commit": frozen_source_authority["baseline_commit"],
        "kind": "doctest",
        "reasons": [
            "historical-raw-count-unknown",
            "off-host-recapture-required",
        ],
        "required_package_targets": [
            "codex-core::lib::codex_core",
            "codex-rollout::lib::codex_rollout",
            "codex-state::lib::codex_state",
            "codex-tui::lib::codex_tui",
        ],
    }
    pending_doctest["resolution"] = None
    pending_doctest["state"] = "pending"
    pending_doctest["transition_receipt_sha256"] = None
    transition_receipt["authority_before_semantic_sha256"] = proof_hash(
        "kd4.inventory-recovery-authority.semantic.v1",
        {
            "format_id": recovery["format_id"],
            "frozen_source_authority": frozen_source_authority,
            "records": pending_records,
            "schema_version": 1,
        },
    )
    transition_receipt["receipt_sha256"] = proof_hash(
        "kd4.recovery-transition-receipt.v1",
        {
            key: value
            for key, value in transition_receipt.items()
            if key != "receipt_sha256"
        },
    )
    recovery["records"][0]["transition_receipt_sha256"] = transition_receipt[
        "receipt_sha256"
    ]
    recovery["semantic_sha256"] = proof_hash(
        "kd4.inventory-recovery-authority.semantic.v1", recovery
    )
    recovery_self_projection = {
        key: recovery[key]
        for key in (
            "format_id",
            "frozen_source_authority",
            "records",
            "schema_version",
            "semantic_sha256",
        )
    }
    recovery["self_hash"] = proof_hash(
        "kd4.inventory-recovery-authority.self.v1", recovery_self_projection
    )
    recovery_raw_sha256 = hashlib.sha256(canonical_jcs(recovery)).hexdigest()
    schema_resources = [
        {"path": path, "raw_sha256": raw_sha256, "schema_id": schema_id}
        for path, raw_sha256, schema_id in zip(
            INVENTORY_V2_SCHEMA_PATHS,
            INVENTORY_V2_SCHEMA_RAW_SHA256S,
            INVENTORY_V2_SCHEMA_IDS,
        )
    ]
    inventory = {
        "action_routes": action_routes,
        "authority": {
            "format_id": "kd4-frozen-test-inventory-v2",
            "raw_sha256": hashlib.sha256(
                b"kd4.inventory-v2.full-scale-integration-fixture.v1"
            ).hexdigest(),
            "schema_sha256": INVENTORY_V2_SCHEMA_RAW_SHA256S[1],
            "semantic_sha256": "0" * 64,
            "self_hash": "0" * 64,
        },
        "cargo_target_context_specs": cargo_target_contexts,
        "declaration_universe": declarations,
        "execution_input_contracts": contracts,
        "format_id": "kd4-frozen-test-inventory-v2",
        "predecessor": {
            "inventory_hash": FROZEN_V1_INVENTORY_SEMANTIC_SHA256,
            "raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
            "recorded_baseline_workspace_fingerprint": FROZEN_V1_WORKSPACE_FINGERPRINT,
            "test_count": 15_544,
        },
        "predecessor_reconciliation": reconciliation,
        "recovery_authority": {
            "path": ".codex/validation/frozen-test-inventory-v2-recoveries.json",
            "raw_sha256": recovery_raw_sha256,
            "semantic_sha256": recovery["semantic_sha256"],
            "self_hash": recovery["self_hash"],
        },
        "routes": routes,
        "schema_resources": schema_resources,
        "schema_resources_sha256": proof_hash(
            "kd4.inventory-v2-schema-resource-set.v1", schema_resources
        ),
        "schema_version": 2,
    }
    inventory_semantic_projection = {
        key: inventory[key]
        for key in (
            "action_routes",
            "cargo_target_context_specs",
            "declaration_universe",
            "execution_input_contracts",
            "format_id",
            "predecessor",
            "predecessor_reconciliation",
            "recovery_authority",
            "routes",
            "schema_resources",
            "schema_resources_sha256",
            "schema_version",
        )
    }
    inventory["authority"]["semantic_sha256"] = proof_hash(
        "kd4.frozen-test-inventory-v2.semantic", inventory_semantic_projection
    )
    inventory_authority_projection = {
        key: inventory["authority"][key]
        for key in ("format_id", "raw_sha256", "schema_sha256", "semantic_sha256")
    }
    inventory["authority"]["self_hash"] = proof_hash(
        "kd4.frozen-test-inventory-v2.authority.self.v1",
        inventory_authority_projection,
    )
    applicability_issuer = ActiveHostApplicabilityIssuerV1(
        "12345678-1234-1234-1234-123456789abc", b"K" * 32
    )
    active_host_authority = applicability_issuer.issue(
        inventory,
        "windows",
        base64.b64encode(b"N" * 32).decode("ascii").rstrip("="),
    )
    recovered_children_by_parent = {
        parent: sorted(
            "inventory-v2-recovered."
            + proof_hash("kd4.recovered-child-identity.v1", child)
            for child in recovered_children
            if child["parent_baseline_id"] == parent
        )
        for parent in recovery_parents
    }
    ledger_rows = []
    for declaration in declarations:
        baseline_id = declaration.get("baseline_id")
        if baseline_id is None:
            exception_projection = {
                "active_host_authority": active_host_authority,
                "provenance_receipt": declaration["source_provenance"],
                "tag": "platform-pending",
            }
            exception = {
                **exception_projection,
                "kind": "accepted",
                "receipt_sha256": proof_hash(
                    "kd4.accepted-exception-receipt.v1", exception_projection
                ),
            }
            ledger_rows.append(
                {
                    "baseline_id": None,
                    "disposition": {"exception": exception, "kind": "exception"},
                    "obligation_id": declaration["obligation_id"],
                }
            )
            continue
        disposition = (
            {
                "child_obligation_ids": recovered_children_by_parent[baseline_id],
                "kind": "recovered-container",
                "transition_receipt_sha256": transition_receipt["receipt_sha256"],
            }
            if baseline_id in recovered_children_by_parent
            else {"kind": "unresolved"}
        )
        ledger_rows.append(
            {
                "baseline_id": baseline_id,
                "disposition": disposition,
                "obligation_id": frozen_baseline_obligation_id_v2(declaration),
            }
        )
    ledger_rows.sort(key=canonical_jcs)
    ledger = {
        "format_id": "kd4.test-replacement-ledger.v2",
        "inventory_authority": {
            "path": ".codex/validation/frozen-test-inventory-v2.json",
            "raw_sha256": inventory["authority"]["raw_sha256"],
            "semantic_sha256": inventory["authority"]["semantic_sha256"],
            "self_hash": inventory["authority"]["self_hash"],
        },
        "rows": ledger_rows,
        "schema_version": 2,
        "trusted_defect_receipts": None,
    }
    ledger["semantic_sha256"] = proof_hash(
        "kd4.test-replacement-ledger.v2.semantic", ledger
    )
    ledger_self_projection = {
        key: ledger[key]
        for key in (
            "format_id",
            "inventory_authority",
            "rows",
            "schema_version",
            "semantic_sha256",
            "trusted_defect_receipts",
        )
    }
    ledger["self_hash"] = proof_hash(
        "kd4.test-replacement-ledger.v2.self", ledger_self_projection
    )
    documents = {"inventory": inventory, "ledger": ledger, "recovery": recovery}
    return {
        "artifact_sha256": {
            name: hashlib.sha256(canonical_jcs(document)).hexdigest()
            for name, document in documents.items()
        },
        "counts": {
            "inventory_declarations": len(declarations),
            "ledger_rows": len(ledger["rows"]),
            "recovery_records": len(recovery["records"]),
        },
        **documents,
        "doctest_recapture": doctest_recapture,
        "transition_receipts": [transition_receipt],
    }


def _refresh_full_scale_ledger_hashes(ledger: dict[str, object]) -> None:
    ledger.pop("semantic_sha256", None)
    ledger.pop("self_hash", None)
    ledger["semantic_sha256"] = proof_hash(
        "kd4.test-replacement-ledger.v2.semantic", ledger
    )
    ledger["self_hash"] = proof_hash(
        "kd4.test-replacement-ledger.v2.self", ledger
    )


def _validate_schema_instance(
    instance: object,
    schema: dict[str, object],
    resources: dict[str, dict[str, object]],
    current_name: str,
) -> None:
    reference = schema.get("$ref")
    if isinstance(reference, str):
        resource_name, _, fragment = reference.partition("#")
        target_name = resource_name or current_name
        target: object = resources[target_name]
        if fragment:
            for escaped in fragment.removeprefix("/").split("/"):
                part = escaped.replace("~1", "/").replace("~0", "~")
                target = target[int(part)] if isinstance(target, list) else target[part]
        _validate_schema_instance(instance, target, resources, target_name)
        return
    variants = schema.get("oneOf")
    if isinstance(variants, list):
        matches = 0
        for variant in variants:
            try:
                _validate_schema_instance(instance, variant, resources, current_name)
            except AssertionError:
                continue
            matches += 1
        assert matches == 1, f"expected exactly one schema variant, got {matches}: {instance!r}"
        return
    if "const" in schema:
        assert instance == schema["const"]
    if "enum" in schema:
        assert instance in schema["enum"]
    expected_type = schema.get("type")
    if expected_type == "null":
        assert instance is None
    elif expected_type == "boolean":
        assert isinstance(instance, bool)
    elif expected_type == "integer":
        assert isinstance(instance, int) and not isinstance(instance, bool)
        if "minimum" in schema:
            assert instance >= schema["minimum"]
        if "maximum" in schema:
            assert instance <= schema["maximum"]
    elif expected_type == "string":
        assert isinstance(instance, str)
        assert len(instance) >= schema.get("minLength", 0)
        if "pattern" in schema:
            assert re.search(schema["pattern"], instance) is not None
        if schema.get("format") == "kd4-nfc-string":
            assert unicodedata.normalize("NFC", instance) == instance
    elif expected_type == "array":
        assert isinstance(instance, list)
        assert len(instance) >= schema.get("minItems", 0)
        if "maxItems" in schema:
            assert len(instance) <= schema["maxItems"]
        if schema.get("uniqueItems"):
            assert len({canonical_jcs(item) for item in instance}) == len(instance)
        item_schema = schema.get("items")
        if isinstance(item_schema, dict):
            for item in instance:
                _validate_schema_instance(item, item_schema, resources, current_name)
    elif expected_type == "object":
        assert isinstance(instance, dict)
        properties = schema.get("properties", {})
        required = schema.get("required", [])
        assert set(required) <= set(instance)
        if schema.get("additionalProperties") is False:
            assert set(instance) <= set(properties)
        for key, item in instance.items():
            if key in properties:
                _validate_schema_instance(item, properties[key], resources, current_name)


class InventoryV2SharedContractTests(unittest.TestCase):
    def test_checked_in_v1_unittest_partition_is_893_executable_and_16_hidden(self) -> None:
        frozen_inventory = json.loads(
            (
                REPO_ROOT
                / ".codex/validation/frozen-test-inventory-v1.json"
            ).read_text(encoding="utf-8")
        )
        predecessor_ledger = json.loads(
            (
                REPO_ROOT / ".codex/validation/test-replacements-v1.json"
            ).read_text(encoding="utf-8")
        )
        unittest_entries = sorted(
            (
                entry
                for entry in frozen_inventory["tests"]
                if entry["framework"] == "python-unittest"
            ),
            key=lambda entry: entry["baseline_id"],
        )
        hidden_ids = sorted(
            entry["baseline_id"]
            for entry in unittest_entries
            if entry["baseline_id"].startswith(
                "hidden-at-freeze-v1::python-unittest::"
            )
        )
        hidden_set = set(hidden_ids)
        executable_entries = [
            entry
            for entry in unittest_entries
            if entry["baseline_id"] not in hidden_set
        ]
        executable_records = [
            {
                "baseline_id": entry["baseline_id"],
                "native_id": entry["native_id"],
                "predecessor_entry_sha256": proof_hash(
                    "kd4.frozen-v1-inventory-entry.v1", entry
                ),
            }
            for entry in executable_entries
        ]
        rows_by_id = {
            row["baseline_id"]: row for row in predecessor_ledger["rows"]
        }
        replacement_ids = sorted(
            entry["baseline_id"]
            for entry in unittest_entries
            if rows_by_id[entry["baseline_id"]]["resolution"] == "replacement"
        )
        executable_replacement_ids = sorted(
            baseline_id
            for baseline_id in replacement_ids
            if baseline_id not in hidden_set
        )
        unresolved_ids = sorted(
            entry["baseline_id"]
            for entry in unittest_entries
            if rows_by_id[entry["baseline_id"]]["resolution"] == "unresolved"
        )

        self.assertEqual(len(unittest_entries), 909)
        self.assertEqual(len(executable_records), 893)
        self.assertEqual(len(hidden_ids), 16)
        self.assertEqual(
            proof_hash(
                "kd4.unittest-recapture-parent-record-set.v1",
                executable_records,
            ),
            "a46a941721c872655dcb1c4ca55c070b9f48008a451d0df283f2d69957c2dd07",
        )
        self.assertEqual(
            proof_hash("kd4.unittest-hidden-ledger-parent-ids.v1", hidden_ids),
            "936330f9e9a23c8d628f651a1ed31b3f4ea836a06cf152a6acf09cff898ebc40",
        )
        self.assertEqual(len(replacement_ids), 536)
        self.assertEqual(
            proof_hash("kd4.unittest-parent-replacement-ids.v1", replacement_ids),
            "59202883ef32488ae9a488345b2401400794d961e5d539c58fd6364e86f25fe1",
        )
        self.assertEqual(len(executable_replacement_ids), 520)
        self.assertEqual(
            proof_hash(
                "kd4.unittest-parent-replacement-ids.v1",
                executable_replacement_ids,
            ),
            "208d6735c1413d94c503a453331889fe5709b8eca540cf9c13993c42f9bb8cf7",
        )
        self.assertEqual(len(unresolved_ids), 373)
        self.assertEqual(
            proof_hash("kd4.unittest-parent-unresolved-ids.v1", unresolved_ids),
            "d1e66a89d1a943b60f6516bc9550102306919d6ae673bee4027595a3df8036f7",
        )
        self.assertTrue(hidden_set.issubset(replacement_ids))
        for entry in unittest_entries:
            if entry["baseline_id"] not in hidden_set:
                continue
            self.assertEqual(
                rows_by_id[entry["baseline_id"]]["replacement_ids"],
                ["python-unittest::" + entry["native_id"]],
            )

    def test_unittest_recapture_public_validator_and_cli_fail_closed(self) -> None:
        packet = _unittest_recapture_fail_closed_packet_v1()
        with self.assertRaisesRegex(
            InventoryV2ContractError,
            "freeze-overlay source authority is unavailable",
        ):
            validate_unittest_recapture_packet_v1(packet)

        tampered = json.loads(json.dumps(packet))
        tampered["baseline_commit"] = "0" * 40
        with self.assertRaisesRegex(
            InventoryV2ContractError, "frozen authority mismatch"
        ):
            validate_unittest_recapture_packet_v1(tampered)

        with tempfile.TemporaryDirectory() as temporary_name:
            packet_path = Path(temporary_name) / "unittest-recapture.json"
            packet_path.write_bytes(canonical_jcs(packet))
            completed = subprocess.run(
                [
                    sys.executable,
                    str(REPO_ROOT / "scripts/completion_proof_inventory_v2.py"),
                    "--validate-unittest-recapture",
                    str(packet_path),
                ],
                cwd=REPO_ROOT,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(completed.stdout, b"")
        self.assertIn(
            b"unittest recapture freeze-overlay source authority is unavailable",
            completed.stderr,
        )

    def test_repository_path_parameter_projection_rejects_noncanonical_paths(self) -> None:
        validate_canonical_parameter_projection_v1(
            {"kind": "repository-path", "value": "scripts/test_example.py"}
        )
        for path in (
            "/scripts/test_example.py",
            "scripts\\test_example.py",
            "scripts/../test_example.py",
            "C:/scripts/test_example.py",
            "//server/share/test_example.py",
            "scripts/e\u0301.py",
        ):
            with self.subTest(path=path), self.assertRaises(
                InventoryV2ContractError
            ):
                validate_canonical_parameter_projection_v1(
                    {"kind": "repository-path", "value": path}
                )

    def test_full_scale_fixture_validates_with_cross_language_hash_anchors(self) -> None:
        bundle = _full_scale_integration_fixture_v2()
        doctest_recapture_raw = canonical_jcs(bundle["doctest_recapture"])
        self.assertEqual(
            doctest_recapture_raw,
            (
                REPO_ROOT
                / ".codex/validation/frozen-test-inventory-v2-doctest-recapture.json"
            ).read_bytes(),
        )
        doctest_record = next(
            record
            for record in bundle["recovery"]["records"]
            if record["kind"] == "doctest"
        )
        self.assertEqual(
            doctest_record["resolution"]["child_sources"],
            doctest_recovered_child_sources_v1(bundle["doctest_recapture"]),
        )
        self.assertEqual(
            bundle["counts"],
            {
                "inventory_declarations": 15_547,
                "ledger_rows": 15_547,
                "recovery_records": 2,
            },
        )
        self.assertEqual(
            bundle["artifact_sha256"],
            {
                "inventory": "e0ddc09c36fe346e511256a6fd1011f3892606e3150e2c045ccae14cdfb6807a",
                "ledger": "651f98d60ca3e0c6755dfecf87c46e251494ad6f05397aa502ca1a4c4a000eb6",
                "recovery": "2d6e7ee1aa26ce1094f93f38152f63fc54e6db7c569a8e7acf51a709896a1712",
            },
        )
        self.assertEqual(
            bundle["inventory"]["authority"]["semantic_sha256"],
            "46b2bde1834a893f7d9851a92cb10e3ac2b2348555288a873f14941040d79288",
        )
        self.assertEqual(
            bundle["ledger"]["semantic_sha256"],
            "134ed74a5ac1ca9c4d3c5080ec441439b1081895f31f180214f2432dee490db8",
        )
        self.assertEqual(
            bundle["recovery"]["semantic_sha256"],
            "34b80e4f41d380d6e9c83e3182a80f7d35250b3284973dee75dcb67802a6dde0",
        )
        validate_inventory_recovery_authority_v1(bundle["recovery"])
        validate_frozen_test_inventory_v2(bundle["inventory"])
        validate_test_replacement_ledger_v2(bundle["ledger"])
        issuer = ActiveHostApplicabilityIssuerV1(
            "12345678-1234-1234-1234-123456789abc", b"K" * 32
        )
        authority = issuer.issue(
            bundle["inventory"], "windows", base64.b64encode(b"N" * 32).decode().rstrip("=")
        )
        issuer.validate_complete_authority(authority, bundle["inventory"])
        incomplete_authority = json.loads(json.dumps(authority))
        incomplete_authority["body"]["target_applicability_projection"]["entries"].pop()
        incomplete_body = incomplete_authority["body"]
        incomplete_body["target_applicability_projection_sha256"] = proof_hash(
            "kd4.target-applicability-projection.v1",
            incomplete_body["target_applicability_projection"],
        )
        incomplete_authority["authority_sha256"] = proof_hash(
            "kd4.active-host-applicability-authority.v1", incomplete_body
        )
        authentication_payload = {
            key: incomplete_authority[key]
            for key in ("authority_sha256", "body", "key_id", "schema_version")
        }
        authentication_bytes = (
            b"kd4.active-host-applicability-authority.authentication.v1\0"
            + canonical_jcs(authentication_payload)
        )
        incomplete_authority["authentication_tag"] = base64.b64encode(
            hmac.new(b"K" * 32, authentication_bytes, hashlib.sha256).digest()
        ).decode().rstrip("=")
        validate_active_host_applicability_authority_v1(
            incomplete_authority, b"K" * 32
        )
        with self.assertRaisesRegex(
            InventoryV2ContractError, "issuer-derived complete inventory projection"
        ):
            issuer.validate_complete_authority(
                incomplete_authority, bundle["inventory"]
            )
        recovery_raw = canonical_jcs(bundle["recovery"])
        with self.assertRaisesRegex(InventoryV2ContractError, "exact canonical JSON bytes"):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], bundle["ledger"], recovery_raw + b"\n",
                bundle["transition_receipts"], issuer, doctest_recapture_raw
            )
        validate_inventory_ledger_predecessor_closure(
            bundle["inventory"], bundle["ledger"], recovery_raw,
            bundle["transition_receipts"], issuer, doctest_recapture_raw
        )
        predecessor_ledger_raw = (
            REPO_ROOT / ".codex/validation/test-replacements-v1.json"
        ).read_bytes()
        with self.assertRaisesRegex(
            InventoryV2ContractError,
            "freeze-overlay source authority is unavailable",
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"],
                bundle["ledger"],
                recovery_raw,
                bundle["transition_receipts"],
                issuer,
                doctest_recapture_raw,
                canonical_jcs(_unittest_recapture_fail_closed_packet_v1()),
                predecessor_ledger_raw,
            )

        tampered_receipt = json.loads(json.dumps(bundle["doctest_recapture"]))
        tampered_receipt["receipt_sha256"] = "0" * 64
        with self.assertRaisesRegex(
            InventoryV2ContractError, "doctest recapture receipt hash mismatch"
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], bundle["ledger"], recovery_raw,
                bundle["transition_receipts"], issuer,
                canonical_jcs(tampered_receipt),
            )

        tampered_children = json.loads(json.dumps(bundle["doctest_recapture"]))
        first_run = tampered_children["runs"][0]
        first_occurrence = first_run["raw_occurrences"][0]
        duplicate_occurrence = json.loads(json.dumps(first_occurrence))
        duplicate_occurrence["global_ordinal"] = 1
        duplicate_occurrence["parent_ordinal"] = 1
        first_run["raw_occurrences"].append(duplicate_occurrence)
        first_run["selected_count"] += 1
        stdout = base64.b64decode(first_run["stdout_base64"])
        stdout += (first_occurrence["raw_listing_line"] + "\n").encode("utf-8")
        first_run["stdout_base64"] = base64.b64encode(stdout).decode("ascii")
        first_run["stdout_sha256"] = hashlib.sha256(stdout).hexdigest()
        for run in tampered_children["runs"][1:]:
            for occurrence in run["raw_occurrences"]:
                occurrence["global_ordinal"] += 1
        matching_parent_count = next(
            row
            for row in tampered_children["parent_counts"]
            if row["parent_baseline_id"] == first_occurrence["parent_baseline_id"]
        )
        matching_parent_count["raw_count"] += 1
        tampered_children["raw_occurrence_count"] += 1
        tampered_children.pop("receipt_sha256")
        tampered_children["receipt_sha256"] = proof_hash(
            "kd4.doctest-recapture-receipt.v1", tampered_children
        )
        self.assertEqual(len(doctest_recovered_child_sources_v1(tampered_children)), 6)
        with self.assertRaisesRegex(
            InventoryV2ContractError,
            "doctest recovery does not exactly materialize its typed recapture packet",
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], bundle["ledger"], recovery_raw,
                bundle["transition_receipts"], issuer,
                canonical_jcs(tampered_children),
            )

        forged = json.loads(json.dumps(bundle["ledger"]))
        forged["rows"][0]["obligation_id"] = "inventory-obligation-v2.forged"
        forged["rows"].sort(key=canonical_jcs)
        _refresh_full_scale_ledger_hashes(forged)
        validate_test_replacement_ledger_v2(forged)
        with self.assertRaisesRegex(
            InventoryV2ContractError, "exactly cover every inventory declaration"
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], forged, recovery_raw,
                bundle["transition_receipts"], issuer, doctest_recapture_raw
            )
        del forged

        source_rows = [
            row for row in bundle["ledger"]["rows"]
            if row["baseline_id"] is None
        ]
        self.assertEqual(len(source_rows), 3)

        omitted = json.loads(json.dumps(bundle["ledger"]))
        omitted["rows"] = [
            row for row in omitted["rows"]
            if row["obligation_id"] != source_rows[0]["obligation_id"]
        ]
        _refresh_full_scale_ledger_hashes(omitted)
        validate_test_replacement_ledger_v2(omitted)
        with self.assertRaisesRegex(
            InventoryV2ContractError, "exactly cover every inventory declaration"
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], omitted, recovery_raw,
                bundle["transition_receipts"], issuer, doctest_recapture_raw
            )
        del omitted

        duplicate = json.loads(json.dumps(bundle["ledger"]))
        duplicate_row = json.loads(json.dumps(source_rows[0]))
        duplicate_row["disposition"] = {"kind": "unresolved"}
        duplicate["rows"].append(duplicate_row)
        duplicate["rows"].sort(key=canonical_jcs)
        _refresh_full_scale_ledger_hashes(duplicate)
        validate_test_replacement_ledger_v2(duplicate)
        with self.assertRaisesRegex(
            InventoryV2ContractError, "duplicate obligation rows"
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], duplicate, recovery_raw,
                bundle["transition_receipts"], issuer, doctest_recapture_raw
            )
        del duplicate

        unknown = json.loads(json.dumps(bundle["ledger"]))
        unknown_row = next(
            row for row in unknown["rows"] if row["baseline_id"] is None
        )
        unknown_row["obligation_id"] = (
            "inventory-obligation-v2.missing-declaration." + "f" * 64
        )
        unknown["rows"].sort(key=canonical_jcs)
        _refresh_full_scale_ledger_hashes(unknown)
        validate_test_replacement_ledger_v2(unknown)
        with self.assertRaisesRegex(
            InventoryV2ContractError, "exactly cover every inventory declaration"
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], unknown, recovery_raw,
                bundle["transition_receipts"], issuer, doctest_recapture_raw
            )
        del unknown

        substituted = json.loads(json.dumps(bundle["ledger"]))
        substituted_rows = [
            row for row in substituted["rows"] if row["baseline_id"] is None
        ]
        substituted_exception = substituted_rows[0]["disposition"]["exception"]
        substituted_exception["provenance_receipt"] = substituted_rows[1][
            "disposition"
        ]["exception"]["provenance_receipt"]
        substituted_exception["receipt_sha256"] = proof_hash(
            "kd4.accepted-exception-receipt.v1",
            {
                key: substituted_exception[key]
                for key in (
                    "active_host_authority", "provenance_receipt", "tag"
                )
            },
        )
        substituted["rows"].sort(key=canonical_jcs)
        _refresh_full_scale_ledger_hashes(substituted)
        validate_test_replacement_ledger_v2(substituted)
        with self.assertRaisesRegex(
            InventoryV2ContractError, "exact nonbaseline declaration provenance"
        ):
            validate_inventory_ledger_predecessor_closure(
                bundle["inventory"], substituted, recovery_raw,
                bundle["transition_receipts"], issuer, doctest_recapture_raw
            )

    def test_inventory_v2_schema_and_shared_type_hash_vectors_match_rust(self) -> None:
        raw_vectors = VECTORS_PATH.read_bytes()
        vectors = json.loads(raw_vectors.decode("utf-8"))
        self.assertEqual(canonical_jcs(vectors), raw_vectors)
        self.assertEqual(vectors["schema_version"], 1)

        for vector in vectors["canonical_parameter_negative_vectors"]:
            value = json.loads(
                base64.urlsafe_b64decode(
                    vector["value_json_base64url"]
                    + "=" * (-len(vector["value_json_base64url"]) % 4)
                ).decode("utf-8")
            )
            with self.assertRaises(InventoryV2ContractError, msg=value):
                validate_canonical_parameter_projection_v1(value)

        for vector in vectors["proof_hash_vectors"]:
            value = json.loads(vector["canonical_json"])
            self.assertEqual(
                canonical_jcs(value).decode("utf-8"), vector["canonical_json"]
            )
            self.assertEqual(proof_hash(vector["domain"], value), vector["sha256"])

        self.assertEqual(len(vectors["executable_identity_vectors"]), 8)
        for identity in vectors["executable_identity_vectors"]:
            validate_executable_identity_v1(identity)
        self.assertEqual(len(vectors["runner_selector_vectors"]), 8)
        for selector in vectors["runner_selector_vectors"]:
            validate_runner_selector_v1(selector)
        for provenance in vectors["provenance_receipt_vectors"]:
            validate_provenance_receipt_v1(provenance)
        self.assertEqual(len(vectors["inventory_declaration_vectors"]), 1)
        for vector in vectors["inventory_declaration_vectors"]:
            validate_inventory_declaration_v2(vector["frozen_declaration"])
            self.assertEqual(
                frozen_baseline_obligation_id_v2(vector["frozen_declaration"]),
                vector["frozen_obligation_id"],
            )
            validate_inventory_declaration_v2(vector["missing_declaration"])
            with self.assertRaises(InventoryV2ContractError):
                validate_inventory_declaration_v2(vector["entry_tamper"])
            with self.assertRaises(InventoryV2ContractError):
                validate_inventory_declaration_v2(vector["provenance_tamper"])
        for selection in vectors["selection_contract_vectors"]:
            validate_selection_v1(selection)
            substituted = json.loads(json.dumps(selection))
            substituted["resolved_entries"][0]["inventory_entry_semantic_sha256"] = (
                substituted["resolved_entries"][0]["inventory_entry"][
                    "platform_applicability_sha256"
                ]
            )
            substituted["resolved_entries_sha256"] = proof_hash(
                "kd4.resolved-executable-entry-set.v1",
                substituted["resolved_entries"],
            )
            with self.assertRaises(InventoryV2ContractError):
                validate_selection_v1(substituted)
        for receipt in vectors["validation_receipt_vectors"]:
            validate_validation_receipt_projection_v1(receipt)
        self.assertEqual(len(vectors["replacement_ledger_vectors"]), 3)
        for ledger in vectors["replacement_ledger_vectors"]:
            validate_test_replacement_ledger_v2(ledger)

        def refresh_ledger_hashes(ledger: dict[str, object]) -> None:
            ledger.pop("semantic_sha256", None)
            ledger.pop("self_hash", None)
            ledger["semantic_sha256"] = proof_hash(
                "kd4.test-replacement-ledger.v2.semantic", ledger
            )
            ledger["self_hash"] = proof_hash(
                "kd4.test-replacement-ledger.v2.self", ledger
            )

        pending_with_review = json.loads(
            json.dumps(vectors["replacement_ledger_vectors"][0])
        )
        pending_with_review["rows"][0]["disposition"][
            "stage2_incorrect_behavior_ids"
        ] = []
        refresh_ledger_hashes(pending_with_review)
        with self.assertRaises(InventoryV2ContractError):
            validate_test_replacement_ledger_v2(pending_with_review)
        accepted_without_review = json.loads(
            json.dumps(vectors["replacement_ledger_vectors"][1])
        )
        accepted_without_review["rows"][0]["disposition"][
            "stage2_incorrect_behavior_ids"
        ] = None
        refresh_ledger_hashes(accepted_without_review)
        with self.assertRaises(InventoryV2ContractError):
            validate_test_replacement_ledger_v2(accepted_without_review)
        stage2_states = [
            ledger["rows"][0]["disposition"]["stage2_incorrect_behavior_ids"]
            for ledger in vectors["replacement_ledger_vectors"]
        ]
        self.assertIsNone(stage2_states[0])
        self.assertEqual(stage2_states[1], [])
        self.assertTrue(stage2_states[2])
        absent_stage2 = json.loads(
            json.dumps(vectors["replacement_ledger_vectors"][0])
        )
        del absent_stage2["rows"][0]["disposition"][
            "stage2_incorrect_behavior_ids"
        ]
        refresh_ledger_hashes(absent_stage2)
        with self.assertRaises(InventoryV2ContractError):
            validate_test_replacement_ledger_v2(absent_stage2)

        for vector in vectors["selection_request_vectors"]["valid"]:
            request = vector["request"]
            validate_selection_request_v1(request)
            self.assertEqual(encode_selection_request_v1(request), vector["token"])
            self.assertEqual(decode_selection_request_v1(vector["token"]), request)
            self.assertEqual(
                proof_hash("kd4.selection-request.v1", request),
                vector["request_sha256"],
            )
            reversed_request = dict(request)
            reversed_request["selectors"] = list(reversed(request["selectors"]))
            with self.assertRaises(InventoryV2ContractError):
                validate_selection_request_v1(reversed_request)
        for token in vectors["selection_request_vectors"]["invalid_tokens"]:
            with self.assertRaises(InventoryV2ContractError, msg=token):
                decode_selection_request_v1(token)

        for value in vectors["repository_path_vectors"]["accepted"]:
            self.assertEqual(require_strict_repository_path(value), value)
        for value in vectors["repository_path_vectors"]["rejected"]:
            with self.assertRaises(InventoryV2ContractError, msg=value):
                require_strict_repository_path(value)
        for encoded in vectors["repository_path_vectors"]["rejected_utf8_base64url"]:
            value = base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4)).decode()
            with self.assertRaises(InventoryV2ContractError, msg=encoded):
                require_strict_repository_path(value)

        for relative, expected_schema_id in zip(
            vectors["schema_files"], INVENTORY_V2_SCHEMA_IDS
        ):
            raw = (REPO_ROOT / relative).read_bytes()
            schema = json.loads(raw.decode("utf-8"))
            self.assertEqual(canonical_jcs(schema), raw, relative)
            self.assertEqual(
                schema["$schema"], "https://json-schema.org/draft/2020-12/schema"
            )
            self.assertEqual(schema["$id"], expected_schema_id)
            self._assert_closed_object_schemas(schema, relative)
            self._assert_freeform_strings_require_nfc(schema, relative)

        resources = {
            Path(relative).name: json.loads((REPO_ROOT / relative).read_text(encoding="utf-8"))
            for relative in vectors["schema_files"]
        }
        shared = resources["inventory-shared-types-v1.schema.json"]
        shared_instances = {
            "applicability_result": [
                vector["applicability_result"]
                for vector in vectors["rust_cfg_contract_vectors"]
            ],
            "cargo_build_context_observation": [
                vector["cargo_build_context_observation"]
                for vector in vectors["rust_cfg_contract_vectors"]
            ],
            "platform_applicability": [
                vector["platform_applicability"]
                for vector in vectors["rust_cfg_contract_vectors"]
            ],
            "provenance_receipt": vectors["provenance_receipt_vectors"],
            "resolved_executable_entry": [
                entry
                for selection in vectors["selection_contract_vectors"]
                for entry in selection["resolved_entries"]
            ],
            "runner_selector": vectors["runner_selector_vectors"],
            "rust_cfg_expression": [
                vector["rust_cfg_expression"]
                for vector in vectors["rust_cfg_contract_vectors"]
            ],
            "selection_v1": vectors["selection_contract_vectors"],
            "validation_receipt_projection": vectors["validation_receipt_vectors"],
        }
        for definition, instances in shared_instances.items():
            for instance in instances:
                _validate_schema_instance(
                    instance,
                    shared["$defs"][definition],
                    resources,
                    "inventory-shared-types-v1.schema.json",
                )
        inventory_schema = resources["frozen-test-inventory-v2.schema.json"]
        for selection in vectors["selection_contract_vectors"]:
            for entry in selection["resolved_entries"]:
                _validate_schema_instance(
                    entry["inventory_entry"],
                    inventory_schema["$defs"]["entry"],
                    resources,
                    "frozen-test-inventory-v2.schema.json",
                )
        request_schema = resources["selection-request-v1.schema.json"]
        for vector in vectors["selection_request_vectors"]["valid"]:
            _validate_schema_instance(
                vector["request"],
                request_schema,
                resources,
                "selection-request-v1.schema.json",
            )
        replacement_schema = resources["test-replacements-v2.schema.json"]
        for ledger in vectors["replacement_ledger_vectors"]:
            _validate_schema_instance(
                ledger,
                replacement_schema,
                resources,
                "test-replacements-v2.schema.json",
            )

        stale_resolved = json.loads(json.dumps(vectors["selection_contract_vectors"][0]["resolved_entries"][0]))
        stale_resolved["identity"] = stale_resolved["inventory_entry"]["executable_identity"]
        with self.assertRaises(AssertionError):
            _validate_schema_instance(
                stale_resolved,
                shared["$defs"]["resolved_executable_entry"],
                resources,
                "inventory-shared-types-v1.schema.json",
            )

    def test_rust_cfg_contract_vector_matches_hashes_and_runtime_shapes(self) -> None:
        vectors = json.loads(VECTORS_PATH.read_text(encoding="utf-8"))
        self.assertEqual(len(vectors["rust_cfg_contract_vectors"]), 1)
        vector = vectors["rust_cfg_contract_vectors"][0]
        validate_rust_cfg_expression_v1(vector["rust_cfg_expression"])
        validate_cargo_target_context_spec_v1(vector["cargo_target_context_spec"])
        validate_cargo_build_context_observation_v1(
            vector["cargo_build_context_observation"]
        )
        validate_applicability_result_v1(vector["applicability_result"])

        self.assertEqual(
            proof_hash("kd4.platform-applicability.v1", vector["platform_applicability"]),
            vector["platform_applicability_sha256"],
        )
        expected = {
            "expression": "bb95b883cd1daf9719c8372ac27de2bafefd45eb4fa3e4de0511adea041bd1c0",
            "platform": "20e6f016ac32b95f57cf7419701a50d3b402ed29752766127ab80c1606516cc1",
            "context": "9b8e8a4de5c02e2e3e7a40065af40116a0bb9a47daa16e98e08fa0f7a0014b83",
            "atoms": "6183583acc60c0f6a45bfd669a7a51ab191a2c1640207c66f0d403a7c5f660f7",
            "observation": "81d363246007f9c60440edaf4168013020a2d8d270a1e77bf892748713aa0a9e",
            "applicability": "f0baa352cae67719db1d3a34f9974ad4c2cc4a89b16dabf326c495f5681d5f5f",
        }
        self.assertEqual(vector["rust_cfg_expression"]["semantic_sha256"], expected["expression"])
        self.assertEqual(vector["platform_applicability_sha256"], expected["platform"])
        self.assertEqual(vector["cargo_target_context_spec"]["context_sha256"], expected["context"])
        self.assertEqual(vector["cargo_build_context_observation"]["actual_cfg_atoms_sha256"], expected["atoms"])
        self.assertEqual(vector["cargo_build_context_observation"]["observation_sha256"], expected["observation"])
        self.assertEqual(vector["applicability_result"]["result_sha256"], expected["applicability"])

        for field, validator in (
            ("rust_cfg_expression", validate_rust_cfg_expression_v1),
            ("cargo_target_context_spec", validate_cargo_target_context_spec_v1),
            ("cargo_build_context_observation", validate_cargo_build_context_observation_v1),
            ("applicability_result", validate_applicability_result_v1),
        ):
            mutated = json.loads(json.dumps(vector[field]))
            digest_field = {
                "rust_cfg_expression": "semantic_sha256",
                "cargo_target_context_spec": "context_sha256",
                "cargo_build_context_observation": "observation_sha256",
                "applicability_result": "result_sha256",
            }[field]
            mutated[digest_field] = "0" * 64
            with self.assertRaises(InventoryV2ContractError, msg=field):
                validator(mutated)

        bad_nullability = json.loads(json.dumps(vector["applicability_result"]))
        bad_nullability["cargo_target_context_spec_sha256"] = None
        bad_nullability["result_sha256"] = proof_hash(
            "kd4.applicability-result.v1",
            {key: value for key, value in bad_nullability.items() if key != "result_sha256"},
        )
        with self.assertRaises(InventoryV2ContractError):
            validate_applicability_result_v1(bad_nullability)

    def test_integer_fields_reject_bool_and_tokens_reject_ambiguous_forms(self) -> None:
        for version in (True, False):
            with self.assertRaises(InventoryV2ContractError):
                validate_selection_request_v1(
                    {
                        "schema_version": version,
                        "selectors": [{"kind": "test", "test_id": "x"}],
                    }
                )
        for field in ("line", "column", "registration_ordinal"):
            selector = {
                "ancestor_titles": ["suite"],
                "column": 1,
                "config_path": "sdk/typescript/jest.config.cjs",
                "full_title": "suite works",
                "file_path": "sdk/typescript/test/example.test.ts",
                "kind": "javascript-jest",
                "line": 1,
                "registration_ordinal": 1,
            }
            selector[field] = True
            with self.assertRaises(InventoryV2ContractError, msg=field):
                validate_runner_selector_v1(selector)

    def test_json_schemas_expose_the_same_closed_selection_and_receipt_contract(self) -> None:
        shared = json.loads(
            (REPO_ROOT / ".codex/validation/inventory-shared-types-v1.schema.json").read_text(
                encoding="utf-8"
            )
        )["$defs"]
        replacements = json.loads(
            (REPO_ROOT / ".codex/validation/test-replacements-v2.schema.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(len(shared["executable_identity"]["oneOf"]), 2)
        self.assertEqual(
            set(shared["executable_identity"]["oneOf"][0]["required"]),
            {"kind", "route_id", "test_id", "validation_id"},
        )
        self.assertEqual(
            set(shared["executable_identity"]["oneOf"][1]["required"]),
            {"action_id", "kind", "validation_id"},
        )
        self.assertEqual(
            [variant["properties"]["kind"]["const"] for variant in shared["runner_selector"]["oneOf"]],
            [
                "rust-nextest",
                "rust-doctest",
                "python-unittest",
                "python-pytest",
                "javascript-jest",
                "argument-comment-lint-native",
                "windows-sandbox-smoke-native",
                "non-test-action",
            ],
        )
        self.assertEqual(
            set(shared["intended_execution_projection"]["required"]),
            {
                "attempt_id",
                "executable_identities",
                "intended_count",
                "inventory_authority",
                "selection_v1_sha256",
                "validation_id",
            },
        )
        self.assertIn(
            "intended_execution_projection",
            shared["validation_receipt_projection"]["required"],
        )
        self.assertIn("baseline_ids", shared["trusted_defect_receipt"]["required"])
        self.assertIn(
            "contract_input_state_sha256", shared["resolved_input_leaves"]["required"]
        )
        self.assertNotIn(
            "platform_applicability",
            shared["target_applicability_projection_entry"]["properties"],
        )
        self.assertEqual(
            set(replacements["$defs"]["accepted_contract"]["required"]),
            {
                "accepted_receipt_sha256",
                "candidate",
                "contract_sources_sha256",
                "product_behavior_obligation_sha256",
                "resolved_entry_set_sha256",
                "runtime_path_sha256",
                "selection_v1_sha256",
                "trusted_defect_receipt_sha256s",
            },
        )
        self.assertIn("trusted_defect_receipts", replacements["required"])

        resources = {
            path.name: json.loads(path.read_text(encoding="utf-8"))
            for path in (REPO_ROOT / ".codex/validation").glob("*.schema.json")
        }

        def assert_refs_resolve(value: object, current: dict[str, object]) -> None:
            if isinstance(value, dict):
                reference = value.get("$ref")
                if isinstance(reference, str):
                    resource_name, _, fragment = reference.partition("#")
                    target = resources[resource_name] if resource_name else current
                    if fragment:
                        for escaped_part in fragment.removeprefix("/").split("/"):
                            part = escaped_part.replace("~1", "/").replace("~0", "~")
                            if isinstance(target, dict):
                                self.assertIn(part, target, reference)
                                target = target[part]
                            else:
                                self.assertIsInstance(target, list, reference)
                                self.assertRegex(part, r"^(0|[1-9][0-9]*)$", reference)
                                index = int(part)
                                self.assertLess(index, len(target), reference)
                                target = target[index]
                for nested in value.values():
                    assert_refs_resolve(nested, current)
            elif isinstance(value, list):
                for nested in value:
                    assert_refs_resolve(nested, current)

        for resource in resources.values():
            assert_refs_resolve(resource, resource)

    def test_globs_and_new_envelopes_are_closed_and_hash_bound(self) -> None:
        validate_path_spec_v1({"kind": "glob", "pattern": "codex-rs/**/tests/*.rs", "root": "codex-rs"})
        for pattern in ("/absolute/**", "a//b", "a/./b", "a/../b", "a\\b", "C:/**"):
            with self.assertRaises(InventoryV2ContractError, msg=pattern):
                validate_path_spec_v1({"kind": "glob", "pattern": pattern, "root": "codex-rs"})

        leaf_projection = {
            "execution_input_contract_sha256": "1" * 64,
            "leaves": [{"path": "scripts/example.py", "provenance": "tracked", "raw_sha256": "2" * 64}],
            "matched_path_set_sha256": "3" * 64,
            "raw_sha256": "4" * 64,
            "schema_version": 1,
            "semantic_inputs_sha256": "5" * 64,
            "workspace_fingerprint": "6" * 64,
        }
        resolved = dict(leaf_projection)
        contract_projection = {
            key: leaf_projection[key]
            for key in (
                "execution_input_contract_sha256",
                "leaves",
                "matched_path_set_sha256",
                "semantic_inputs_sha256",
            )
        }
        resolved["contract_input_state_sha256"] = proof_hash(
            "kd4.contract-input-state.v1", contract_projection
        )
        leaf_projection["contract_input_state_sha256"] = resolved["contract_input_state_sha256"]
        resolved["semantic_sha256"] = proof_hash("kd4.resolved-input-leaves.semantic.v1", leaf_projection)
        self_projection = dict(leaf_projection)
        self_projection["semantic_sha256"] = resolved["semantic_sha256"]
        resolved["self_hash"] = proof_hash("kd4.resolved-input-leaves.self.v1", self_projection)
        validate_resolved_input_leaves_v1(resolved)
        bad = dict(resolved, raw_sha256="7" * 64)
        with self.assertRaises(InventoryV2ContractError):
            validate_resolved_input_leaves_v1(bad)

    def test_applicability_selection_and_receipt_validators_reject_bool_epochs(self) -> None:
        parameters = {
            "kind": "mapping",
            "entries": [
                {
                    "key": {"kind": "string", "value": "case"},
                    "value": {
                        "kind": "tuple",
                        "items": [
                            {"kind": "bytes", "base64url": "AP8"},
                            {"kind": "integer", "value": 7},
                        ],
                    },
                }
            ],
        }
        validate_canonical_parameter_projection_v1(parameters)
        for bad_parameters in (
            {"kind": "bytes", "base64url": "AP8="},
            {"kind": "string", "value": "e\u0301"},
            {"kind": "set", "items": [{"kind": "integer", "value": 2}, {"kind": "integer", "value": 1}]},
            {"kind": "custom", "value": "repr-is-forbidden"},
        ):
            with self.assertRaises(InventoryV2ContractError):
                validate_canonical_parameter_projection_v1(bad_parameters)

        identity = {"action_id": "documentation.markdown", "kind": "action", "validation_id": "documentation.markdown"}
        selector = {"action_id": "documentation.markdown", "kind": "non-test-action"}
        platform = {"kind": "host-set", "required_hosts": ["darwin", "linux", "windows"]}

        def resolved_entry(
            current_identity: dict[str, object],
            current_selector: dict[str, object],
            contract_sha256: str,
        ) -> dict[str, object]:
            inventory_entry = {
                "cargo_target_context_spec_sha256": None,
                "executable_identity": current_identity,
                "executable_identity_sha256": proof_hash(
                    "kd4.executable-identity.v1", current_identity
                ),
                "execution_input_contract_sha256": contract_sha256,
                "platform_applicability": platform,
                "platform_applicability_sha256": proof_hash(
                    "kd4.platform-applicability.v1", platform
                ),
                "runner_selector": current_selector,
                "runner_selector_sha256": proof_hash(
                    "kd4.runner-selector.v1", current_selector
                ),
                "test_route_id": None,
                "validation_id": current_identity["validation_id"],
            }
            return {
                "inventory_entry": inventory_entry,
                "inventory_entry_semantic_sha256": proof_hash(
                    "kd4.executable-inventory-entry.v2", inventory_entry
                ),
            }

        entry = resolved_entry(identity, selector, "1" * 64)
        authority = {"path": ".codex/validation/frozen-test-inventory-v2.json", "raw_sha256": "4" * 64, "self_hash": "5" * 64, "semantic_sha256": "6" * 64}
        selection = {
            "activated_policy_sha256": "7" * 64,
            "host": "windows",
            "intended_count": 1,
            "inventory_authority": authority,
            "repository_identity_sha256": "8" * 64,
            "request_sha256": "9" * 64,
            "resolved_entries": [entry],
            "resolved_entries_sha256": proof_hash("kd4.resolved-executable-entry-set.v1", [entry]),
            "schema_version": 1,
            "target_applicability_sha256": "a" * 64,
            "validation_execution_contract_sha256": "b" * 64,
            "validation_id": "documentation.markdown",
        }
        validate_selection_v1(selection)
        with self.assertRaises(InventoryV2ContractError):
            validate_selection_v1(dict(selection, intended_count=True))

        second_identity = {
            "action_id": "maintenance.source-map",
            "kind": "action",
            "validation_id": "documentation.markdown",
        }
        second_selector = {
            "action_id": "maintenance.source-map",
            "kind": "non-test-action",
        }
        second_entry = resolved_entry(
            second_identity,
            second_selector,
            "0" * 64,
        )
        identity_sorted_entries = [entry, second_entry]
        identity_sorted_entries.sort(
            key=lambda item: canonical_jcs(
                item["inventory_entry"]["executable_identity"]
            )
        )
        two_entry_selection = dict(
            selection,
            intended_count=2,
            resolved_entries=identity_sorted_entries,
            resolved_entries_sha256=proof_hash(
                "kd4.resolved-executable-entry-set.v1", identity_sorted_entries
            ),
        )
        validate_selection_v1(two_entry_selection)
        descending_entries = list(reversed(identity_sorted_entries))
        with self.assertRaises(InventoryV2ContractError):
            validate_selection_v1(
                dict(
                    two_entry_selection,
                    resolved_entries=descending_entries,
                    resolved_entries_sha256=proof_hash(
                        "kd4.resolved-executable-entry-set.v1", descending_entries
                    ),
                )
            )
        duplicate_entries = [entry, json.loads(json.dumps(entry))]
        with self.assertRaises(InventoryV2ContractError):
            validate_selection_v1(
                dict(
                    two_entry_selection,
                    resolved_entries=duplicate_entries,
                    resolved_entries_sha256=proof_hash(
                        "kd4.resolved-executable-entry-set.v1", duplicate_entries
                    ),
                )
            )

        applicability = {"kind": "host-set", "required_hosts": ["windows"]}
        applicability_result = {
            "cargo_build_context_observation_sha256": None,
            "cargo_target_context_spec_sha256": None,
            "executable_identity_sha256": proof_hash("kd4.executable-identity.v1", identity),
            "host": "windows",
            "platform_applicability_sha256": proof_hash("kd4.platform-applicability.v1", applicability),
            "result_sha256": "0" * 64,
            "rust_cfg_expression_semantic_sha256": None,
            "schema_version": 1,
            "verdict": "applicable",
        }
        applicability_result["result_sha256"] = proof_hash(
            "kd4.applicability-result.v1",
            {
                key: value
                for key, value in applicability_result.items()
                if key != "result_sha256"
            },
        )
        projection = {
            "entries": [{
                "applicability_result": applicability_result,
                "identity": identity,
                "identity_sha256": proof_hash("kd4.executable-identity.v1", identity),
                "platform_applicability_sha256": proof_hash("kd4.platform-applicability.v1", applicability),
            }],
            "host": "windows",
            "inventory_authority": authority,
            "schema_version": 1,
        }
        validate_target_applicability_projection_v1(projection)
        authority_key = b"K" * 32
        authority_body = {
            "authority_nonce": base64.b64encode(b"N" * 32).decode("ascii").rstrip("="),
            "inventory_authority": authority,
            "schema_version": 1,
            "target_applicability_projection": projection,
            "target_applicability_projection_sha256": proof_hash(
                "kd4.target-applicability-projection.v1", projection
            ),
        }
        active_host_authority = {
            "authority_sha256": proof_hash(
                "kd4.active-host-applicability-authority.v1", authority_body
            ),
            "body": authority_body,
            "key_id": "12345678-1234-4234-8234-123456789abc",
            "schema_version": 1,
        }
        authentication_projection = dict(active_host_authority)
        active_host_authority["authentication_tag"] = base64.b64encode(
            hmac.new(
                authority_key,
                b"kd4.active-host-applicability-authority.authentication.v1\0"
                + canonical_jcs(authentication_projection),
                hashlib.sha256,
            ).digest()
        ).decode("ascii").rstrip("=")
        validate_active_host_applicability_authority_v1(
            active_host_authority, authority_key
        )
        with self.assertRaises(InventoryV2ContractError):
            validate_active_host_applicability_authority_v1(
                active_host_authority, b"W" * 32
            )
        substituted_authority = json.loads(json.dumps(active_host_authority))
        substituted_authority["body"]["target_applicability_projection"]["host"] = "linux"
        with self.assertRaises(InventoryV2ContractError):
            validate_active_host_applicability_authority_v1(
                substituted_authority, authority_key
            )

        exception_provenance = {
            "evidence_paths": ["scripts/completion_proof_inventory_v2.py"],
            "evidence_sha256": "c" * 64,
            "kind": "generated",
            "receipt_sha256": "0" * 64,
            "schema_version": 1,
        }
        exception_provenance["receipt_sha256"] = proof_hash(
            "kd4.provenance-receipt.v1",
            {
                key: value
                for key, value in exception_provenance.items()
                if key != "receipt_sha256"
            },
        )
        exception = {
            "active_host_authority": active_host_authority,
            "kind": "accepted",
            "provenance_receipt": exception_provenance,
            "receipt_sha256": "0" * 64,
            "tag": "generated",
        }
        exception["receipt_sha256"] = proof_hash(
            "kd4.accepted-exception-receipt.v1",
            {
                key: value
                for key, value in exception.items()
                if key not in {"kind", "receipt_sha256"}
            },
        )
        exception_ledger = {
            "format_id": "kd4.test-replacement-ledger.v2",
            "inventory_authority": authority,
            "rows": [{
                "baseline_id": "baseline.one",
                "disposition": {"exception": exception, "kind": "exception"},
                "obligation_id": "obligation.one",
            }],
            "schema_version": 2,
            "trusted_defect_receipts": None,
        }
        exception_ledger["semantic_sha256"] = proof_hash(
            "kd4.test-replacement-ledger.v2.semantic", exception_ledger
        )
        exception_ledger["self_hash"] = proof_hash(
            "kd4.test-replacement-ledger.v2.self", exception_ledger
        )
        validate_test_replacement_ledger_v2(exception_ledger)
        stale_exception_receipt = json.loads(json.dumps(exception_ledger))
        stale_exception_receipt["rows"][0]["disposition"]["exception"][
            "active_host_authority"
        ]["authentication_tag"] = base64.b64encode(b"M" * 32).decode("ascii").rstrip("=")
        stale_exception_receipt.pop("semantic_sha256")
        stale_exception_receipt.pop("self_hash")
        stale_exception_receipt["semantic_sha256"] = proof_hash(
            "kd4.test-replacement-ledger.v2.semantic", stale_exception_receipt
        )
        stale_exception_receipt["self_hash"] = proof_hash(
            "kd4.test-replacement-ledger.v2.self", stale_exception_receipt
        )
        with self.assertRaises(InventoryV2ContractError):
            validate_test_replacement_ledger_v2(stale_exception_receipt)

        schema_resources = {
            path.name: json.loads(path.read_text(encoding="utf-8"))
            for path in (REPO_ROOT / ".codex/validation").glob("*.schema.json")
        }
        _validate_schema_instance(
            active_host_authority,
            schema_resources["inventory-shared-types-v1.schema.json"]["$defs"][
                "active_host_applicability_authority"
            ],
            schema_resources,
            "inventory-shared-types-v1.schema.json",
        )

        provenance = {
            "evidence_paths": ["scripts/completion_proof_inventory_v2.py"],
            "evidence_sha256": "c" * 64,
            "kind": "source-declaration",
            "receipt_sha256": "0" * 64,
            "schema_version": 1,
        }
        provenance["receipt_sha256"] = proof_hash(
            "kd4.provenance-receipt.v1",
            {key: value for key, value in provenance.items() if key != "receipt_sha256"},
        )
        declaration = {
            "declaration_id": inventory_declaration_id_v2(
                "post-baseline-current", entry["inventory_entry"], provenance
            ),
            "entry": entry["inventory_entry"],
            "kind": "post-baseline-current",
            "obligation_id": inventory_declaration_obligation_id_v2(
                "post-baseline-current", entry["inventory_entry"], provenance
            ),
            "source_provenance": provenance,
        }
        validate_inventory_declaration_v2(declaration)
        stale_entry_id = json.loads(json.dumps(declaration))
        stale_entry_id["entry"]["execution_input_contract_sha256"] = "d" * 64
        with self.assertRaises(InventoryV2ContractError):
            validate_inventory_declaration_v2(stale_entry_id)
        stale_provenance_id = json.loads(json.dumps(declaration))
        stale_provenance_id["source_provenance"]["evidence_sha256"] = "e" * 64
        stale_provenance_id["source_provenance"]["receipt_sha256"] = proof_hash(
            "kd4.provenance-receipt.v1",
            {
                key: value
                for key, value in stale_provenance_id["source_provenance"].items()
                if key != "receipt_sha256"
            },
        )
        with self.assertRaises(InventoryV2ContractError):
            validate_inventory_declaration_v2(stale_provenance_id)

        receipt = {
            "baseline_ids": ["baseline.one"],
            "baseline_obligation_ids": ["baseline.one"],
            "defect_id": "defect.one",
            "failure": {"attempt_id": "attempt.failure", "classification": "confirmed-validation-failure", "execution_ids": ["execution.failure"], "focused_projection_sha256": "1" * 64, "mutation_epoch": 3, "workspace_fingerprint": "2" * 64},
            "incorrect_behavior": "old behavior",
            "mutation": {"changed_input_leaf_ids": ["src.one"], "classification": "relevant-non-test-product-runtime", "from_epoch": 3, "input_contract_set_sha256": "3" * 64, "production_delta_sha256": "4" * 64, "to_epoch": 4},
            "pass": {"attempt_id": "attempt.pass", "classification": "confirmed-pass", "execution_ids": ["execution.pass"], "focused_projection_sha256": "5" * 64, "mutation_epoch": 4, "workspace_fingerprint": "6" * 64},
            "replacement_edge_ids": ["edge.one"],
            "resolved_entry_set_sha256": "7" * 64,
            "schema_version": 1,
            "selection_v1_sha256": "8" * 64,
        }
        receipt["receipt_sha256"] = proof_hash("kd4.trusted-defect-receipt.v1", receipt)
        validate_trusted_defect_receipt_v1(receipt)
        bad_receipt = json.loads(json.dumps(receipt))
        bad_receipt["pass"]["mutation_epoch"] = True
        with self.assertRaises(InventoryV2ContractError):
            validate_trusted_defect_receipt_v1(bad_receipt)

    def test_validation_receipt_binds_intended_selection_and_preserves_partial_failure(self) -> None:
        authority = {"path": ".codex/validation/frozen-test-inventory-v2.json", "raw_sha256": "1" * 64, "self_hash": "2" * 64, "semantic_sha256": "3" * 64}
        identities = [
            {"action_id": "action.one", "kind": "action", "validation_id": "validation.one"},
            {"action_id": "action.two", "kind": "action", "validation_id": "validation.one"},
        ]
        identities.sort(key=canonical_jcs)
        intended = {
            "attempt_id": "attempt.one",
            "executable_identities": identities,
            "intended_count": 2,
            "inventory_authority": authority,
            "selection_v1_sha256": "4" * 64,
            "validation_id": "validation.one",
        }
        validate_intended_execution_projection_v1(intended)
        receipt = {
            "attempt_id": "attempt.one",
            "classification": "confirmed-validation-failure",
            "executed_count": 1,
            "intended_execution_projection": intended,
            "intended_execution_projection_sha256": proof_hash("kd4.intended-execution-projection.v1", intended),
            "mismatch_codes": ["runner-interrupted-after-failure"],
            "outcomes": [{"execution_id": "execution.one", "identity": identities[0], "outcome": "failed"}],
            "schema_version": 1,
            "selected_count": 2,
            "started_count": 1,
            "terminal_count": 1,
            "validation_id": "validation.one",
        }
        validate_validation_receipt_projection_v1(receipt)
        bad_pass = dict(receipt, classification="confirmed-pass", mismatch_codes=[])
        with self.assertRaises(InventoryV2ContractError):
            validate_validation_receipt_projection_v1(bad_pass)
        bad_projection = json.loads(json.dumps(receipt))
        bad_projection["intended_execution_projection"]["attempt_id"] = "attempt.replayed"
        with self.assertRaises(InventoryV2ContractError):
            validate_validation_receipt_projection_v1(bad_projection)

        zero_pre_result = dict(
            receipt,
            classification="pre-result-error",
            executed_count=0,
            mismatch_codes=["runner-launch-error"],
            outcomes=[],
            selected_count=0,
            started_count=0,
            terminal_count=0,
        )
        validate_validation_receipt_projection_v1(zero_pre_result)
        partial_pre_result = dict(
            receipt,
            classification="pre-result-error",
            executed_count=1,
            mismatch_codes=["runner-interrupted-after-pass"],
            outcomes=[
                {
                    "execution_id": "execution.one",
                    "identity": identities[0],
                    "outcome": "passed",
                }
            ],
            selected_count=2,
            started_count=2,
            terminal_count=1,
        )
        validate_validation_receipt_projection_v1(partial_pre_result)
        failed_pre_result = json.loads(json.dumps(partial_pre_result))
        failed_pre_result["outcomes"][0]["outcome"] = "failed"
        with self.assertRaises(InventoryV2ContractError):
            validate_validation_receipt_projection_v1(failed_pre_result)
        complete_pre_result = dict(
            partial_pre_result,
            executed_count=2,
            outcomes=[
                {
                    "execution_id": f"execution.{index}",
                    "identity": identity,
                    "outcome": "passed",
                }
                for index, identity in enumerate(identities)
            ],
            terminal_count=2,
        )
        with self.assertRaises(InventoryV2ContractError):
            validate_validation_receipt_projection_v1(complete_pre_result)

    def test_ledger_defect_receipts_close_over_accepted_rows_and_selection(self) -> None:
        identity = {"kind": "test", "route_id": "test-route.python-pytest.v1", "test_id": "tests/test_example.py::test_fixed", "validation_id": "sdk.python.pytest"}
        selector = {"kind": "python-pytest", "node_id": "tests/test_example.py::test_fixed"}
        candidate = {
            "candidate_receipt_sha256": "1" * 64,
            "executable_identity": identity,
            "executable_identity_sha256": proof_hash("kd4.executable-identity.v1", identity),
            "execution_input_contract_sha256": "2" * 64,
            "platform_applicability_sha256": "3" * 64,
            "replacement_id": "replacement.one",
            "runner_selector": selector,
            "runner_selector_sha256": proof_hash("kd4.runner-selector.v1", selector),
            "test_route_id": "test-route.python-pytest.v1",
            "validation_id": "sdk.python.pytest",
        }
        receipt = {
            "baseline_ids": ["baseline.one"],
            "baseline_obligation_ids": ["obligation.one"],
            "defect_id": "defect.one",
            "failure": {"attempt_id": "attempt.failure", "classification": "confirmed-validation-failure", "execution_ids": ["execution.failure"], "focused_projection_sha256": "4" * 64, "mutation_epoch": 3, "workspace_fingerprint": "5" * 64},
            "incorrect_behavior": "The old product behavior failed through its runtime path.",
            "mutation": {"changed_input_leaf_ids": ["src/product.rs"], "classification": "relevant-non-test-product-runtime", "from_epoch": 3, "input_contract_set_sha256": "6" * 64, "production_delta_sha256": "7" * 64, "to_epoch": 4},
            "pass": {"attempt_id": "attempt.pass", "classification": "confirmed-pass", "execution_ids": ["execution.pass"], "focused_projection_sha256": "8" * 64, "mutation_epoch": 4, "workspace_fingerprint": "9" * 64},
            "replacement_edge_ids": ["edge.one"],
            "resolved_entry_set_sha256": "a" * 64,
            "schema_version": 1,
            "selection_v1_sha256": "b" * 64,
        }
        receipt["receipt_sha256"] = proof_hash("kd4.trusted-defect-receipt.v1", receipt)
        accepted = {
            "accepted_receipt_sha256": "c" * 64,
            "candidate": candidate,
            "contract_sources_sha256": "d" * 64,
            "product_behavior_obligation_sha256": "e" * 64,
            "resolved_entry_set_sha256": receipt["resolved_entry_set_sha256"],
            "runtime_path_sha256": "f" * 64,
            "selection_v1_sha256": receipt["selection_v1_sha256"],
            "trusted_defect_receipt_sha256s": [receipt["receipt_sha256"]],
        }
        row = {
            "baseline_id": "baseline.one",
            "disposition": {"contract": {"accepted": accepted, "candidate": None, "legacy_replacement_hint": {"predecessor_row_sha256": "0" * 64, "replacement_ids": ["replacement.one"]}, "state": "accepted"}, "edge_ids": ["edge.one"], "kind": "replacement", "stage2_incorrect_behavior_ids": ["defect.one"]},
            "obligation_id": "obligation.one",
        }
        authority = {"path": ".codex/validation/frozen-test-inventory-v2.json", "raw_sha256": "1" * 64, "self_hash": "2" * 64, "semantic_sha256": "3" * 64}
        ledger = {"format_id": "kd4.test-replacement-ledger.v2", "inventory_authority": authority, "rows": [row], "schema_version": 2, "trusted_defect_receipts": [receipt]}
        ledger["semantic_sha256"] = proof_hash("kd4.test-replacement-ledger.v2.semantic", ledger)
        ledger["self_hash"] = proof_hash("kd4.test-replacement-ledger.v2.self", ledger)
        validate_test_replacement_ledger_v2(ledger)
        for mutate in (
            lambda value: value["rows"][0]["disposition"].update(stage2_incorrect_behavior_ids=[]),
            lambda value: value["trusted_defect_receipts"][0].update(baseline_ids=["baseline.other"]),
            lambda value: value["rows"][0]["disposition"]["contract"]["accepted"].update(selection_v1_sha256="0" * 64),
        ):
            bad = json.loads(json.dumps(ledger))
            mutate(bad)
            with self.assertRaises(InventoryV2ContractError):
                validate_test_replacement_ledger_v2(bad)

    def test_inventory_ledger_and_recovery_validators_bind_complete_contracts(self) -> None:
        resources = []
        for relative, schema_id in zip((
            ".codex/validation/frozen-test-inventory-v2-recoveries.schema.json",
            ".codex/validation/frozen-test-inventory-v2.schema.json",
            ".codex/validation/inventory-shared-types-v1.schema.json",
            ".codex/validation/selection-request-v1.schema.json",
            ".codex/validation/test-replacements-v2.schema.json",
        ), INVENTORY_V2_SCHEMA_IDS):
            resources.append(
                {
                    "path": relative,
                    "raw_sha256": hashlib.sha256((REPO_ROOT / relative).read_bytes()).hexdigest(),
                    "schema_id": json.loads(
                        (REPO_ROOT / relative).read_text(encoding="utf-8")
                    )["$id"],
                }
            )
            self.assertEqual(resources[-1]["schema_id"], schema_id)
        resource_set_sha256 = proof_hash(
            "kd4.inventory-v2-schema-resource-set.v1", resources
        )
        validate_schema_resource_set_v1(resources, resource_set_sha256)
        for mutate in (
            lambda value: value.reverse(),
            lambda value: value[0].update(raw_sha256="0" * 64),
            lambda value: value[0].update(path="schemas/substitute.json"),
            lambda value: value[0].update(schema_id="kd4://validation/substitute.schema.json"),
        ):
            invalid_resources = json.loads(json.dumps(resources))
            mutate(invalid_resources)
            with self.assertRaises(InventoryV2ContractError):
                validate_schema_resource_set_v1(invalid_resources, resource_set_sha256)

        frozen_inventory = json.loads(
            (REPO_ROOT / ".codex/validation/frozen-test-inventory-v1.json").read_text(
                encoding="utf-8"
            )
        )
        frozen_ledger = json.loads(
            (REPO_ROOT / ".codex/validation/test-replacements-v1.json").read_text(
                encoding="utf-8"
            )
        )
        inventory_ids = sorted(row["baseline_id"] for row in frozen_inventory["tests"])
        ledger_ids = sorted(row["baseline_id"] for row in frozen_ledger["rows"])
        self.assertEqual(inventory_ids, ledger_ids)
        predecessor_entries = {
            row["baseline_id"]: row for row in frozen_inventory["tests"]
        }
        associations = [
            {
                "baseline_id": baseline_id,
                "predecessor_entry_sha256": proof_hash(
                    "kd4.frozen-v1-inventory-entry.v1",
                    predecessor_entries[baseline_id],
                ),
            }
            for baseline_id in inventory_ids
        ]
        predecessor = {
            "frozen_baseline_associations": associations,
            "frozen_baseline_associations_sha256": proof_hash(
                "kd4.frozen-baseline-association-set.v1", associations
            ),
            "frozen_baseline_ids": inventory_ids,
            "frozen_baseline_ids_sha256": "9100d0fe0dd4c270a1216b6d3ec6f6c39b7f93278cf68235f500edae17d25eb1",
            "frozen_inventory_raw_sha256": "df230a7683f0f31f1aae4d3f7644af39cec67b09fadf8f3f1e6c60729d18196a",
            "frozen_inventory_semantic_sha256": "a2fb8c0b806853b6375d92cfa6daf985ea35a5c4d4ecd49cf1d6da6f23359152",
            "frozen_ledger_raw_sha256": "210acb8428be83b9c6acd021bde4e44905e738d557ca89db60cb1271f65da46b",
            "schema_version": 1,
        }
        predecessor["projection_sha256"] = proof_hash(
            "kd4.predecessor-artifact-reconciliation.v1", predecessor
        )
        validate_predecessor_artifact_reconciliation_v1(predecessor)
        substituted = json.loads(json.dumps(predecessor))
        association_rows = substituted["frozen_baseline_associations"]
        association_rows[0]["predecessor_entry_sha256"], association_rows[1]["predecessor_entry_sha256"] = (
            association_rows[1]["predecessor_entry_sha256"],
            association_rows[0]["predecessor_entry_sha256"],
        )
        substituted["frozen_baseline_associations_sha256"] = proof_hash(
            "kd4.frozen-baseline-association-set.v1", association_rows
        )
        substituted["projection_sha256"] = proof_hash(
            "kd4.predecessor-artifact-reconciliation.v1",
            {
                key: value
                for key, value in substituted.items()
                if key != "projection_sha256"
            },
        )
        with self.assertRaises(InventoryV2ContractError):
            validate_predecessor_artifact_reconciliation_v1(substituted)
        missing = json.loads(json.dumps(predecessor))
        missing["frozen_baseline_ids"].pop()
        with self.assertRaises(InventoryV2ContractError):
            validate_predecessor_artifact_reconciliation_v1(missing)

        authority = {"path": ".codex/validation/frozen-test-inventory-v2.json", "raw_sha256": "1" * 64, "self_hash": "2" * 64, "semantic_sha256": "3" * 64}
        ledger = {"format_id": "kd4.test-replacement-ledger.v2", "inventory_authority": authority, "rows": [], "schema_version": 2, "trusted_defect_receipts": None}
        ledger["semantic_sha256"] = proof_hash("kd4.test-replacement-ledger.v2.semantic", ledger)
        ledger["self_hash"] = proof_hash("kd4.test-replacement-ledger.v2.self", ledger)
        validate_test_replacement_ledger_v2(ledger)

        parents = [f"parent-{index:03d}" for index in range(893)]
        outputs = [{"output_sha256": "f" * 64, "parent_id": parent} for parent in parents]
        sites = [
            {"column": 18, "line": 868, "parent_id": "FilteringArgumentPolicyTest.test_package_and_target_overrides_are_rejected_through_cli", "path": "scripts/test_rust_test_runner.py"},
            {"column": 18, "line": 875, "parent_id": "FilteringArgumentPolicyTest.test_no_tests_override_is_rejected_through_cli", "path": "scripts/test_rust_test_runner.py"},
            {"column": 18, "line": 913, "parent_id": "GenericRecipeGuardTest.test_every_codex_core_package_spelling_is_rejected_through_cli", "path": "scripts/test_rust_test_runner.py"},
            {"column": 22, "line": 1688, "parent_id": "TargetDirectoryPropagationTest.test_relative_codex_rs_target_dir_is_rejected_before_cargo_through_cli", "path": "scripts/test_rust_test_runner.py"},
            {"column": 18, "line": 1704, "parent_id": "TargetDirectoryPropagationTest.test_effective_environment_target_dir_is_validated_before_cargo_through_cli", "path": "scripts/test_rust_test_runner.py"},
        ]
        records = [
            {"current_audit": {"declared_count": 5, "kind": "doctest", "raw_count": None, "unique_count": 5}, "gap_id": "gap.doctest", "kind": "doctest", "legacy_evidence": {"frozen_unique_count": 5, "historical_raw_count": None, "kind": "doctest"}, "pending_requirement": {"baseline_commit": "baseline", "kind": "doctest", "reasons": ["pending"], "required_package_targets": ["codex-core"]}, "recovery_id": "a.doctest", "resolution": None, "state": "pending", "transition_receipt_sha256": None},
            {"current_audit": {"executable_ast_call_count": 62, "excluded_embedded_fixture_count": 1, "kind": "unittest", "runner_site_observations": sites, "text_call_count": 63}, "gap_id": "gap.unittest", "kind": "unittest", "legacy_evidence": {"frozen_parent_count": 909, "historical_subtest_call_count": None, "kind": "unittest"}, "pending_requirement": {"baseline_commit": "baseline", "expected_parent_output_sha256s": outputs, "kind": "unittest", "reasons": ["pending"], "required_parent_count": 893, "required_parent_ids": parents}, "recovery_id": "b.unittest", "resolution": None, "state": "pending", "transition_receipt_sha256": None},
        ]
        recovery = {"format_id": "kd4.inventory-recovery-authority.v1", "frozen_source_authority": {"baseline_commit": "baseline", "repository_identity_sha256": "1" * 64, "source_tree_sha256": "2" * 64}, "records": records, "schema_version": 1}
        recovery["semantic_sha256"] = proof_hash("kd4.inventory-recovery-authority.semantic.v1", recovery)
        recovery["self_hash"] = proof_hash("kd4.inventory-recovery-authority.self.v1", recovery)
        validate_inventory_recovery_authority_v1(recovery)
        transition = {
            "authority_before_semantic_sha256": "1" * 64,
            "child_obligation_ids": ["inventory-v2-recovered.child"],
            "frozen_source_authority_sha256": "2" * 64,
            "parent_container_ids": ["parent-000"],
            "recapture_receipt_sha256": "3" * 64,
            "schema_version": 1,
        }
        transition["receipt_sha256"] = proof_hash(
            "kd4.recovery-transition-receipt.v1", transition
        )
        validate_recovery_transition_receipt_v1(transition)
        bad_transition = dict(transition, child_obligation_ids=["e\u0301"])
        with self.assertRaises(InventoryV2ContractError):
            validate_recovery_transition_receipt_v1(bad_transition)
        recovery["records"][1]["current_audit"]["runner_site_observations"][0]["line"] = True
        with self.assertRaises(InventoryV2ContractError):
            validate_inventory_recovery_authority_v1(recovery)

    def _assert_closed_object_schemas(self, value: object, location: str) -> None:
        if isinstance(value, dict):
            if value.get("type") == "object":
                self.assertIs(value.get("additionalProperties"), False, location)
            for key, nested in value.items():
                self._assert_closed_object_schemas(nested, f"{location}/{key}")
        elif isinstance(value, list):
            for index, nested in enumerate(value):
                self._assert_closed_object_schemas(nested, f"{location}/{index}")

    def _assert_freeform_strings_require_nfc(self, value: object, location: str) -> None:
        if isinstance(value, dict):
            if value.get("type") == "string" and not any(
                key in value for key in ("const", "enum", "pattern")
            ):
                self.assertIn(
                    value.get("format"),
                    {"kd4-nfc-string", "kd4-repository-path"},
                    location,
                )
            for key, nested in value.items():
                self._assert_freeform_strings_require_nfc(nested, f"{location}/{key}")
        elif isinstance(value, list):
            for index, nested in enumerate(value):
                self._assert_freeform_strings_require_nfc(nested, f"{location}/{index}")


if __name__ == "__main__":
    unittest.main()
