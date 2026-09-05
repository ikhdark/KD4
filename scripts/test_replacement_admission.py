from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from scripts import replacement_admission as admission
from scripts.completion_proof_inventory_v2 import canonical_jcs, proof_hash


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
CLI = REPOSITORY_ROOT / "scripts" / "replacement_admission.py"
SCHEMA = REPOSITORY_ROOT / ".codex/validation/replacement-admissions-v1.schema.json"
FOCUSED_APPROVAL_VECTORS = (
    REPOSITORY_ROOT
    / "scripts/fixtures/focused_replacement_approval_receipt_v1_vectors.json"
)


def _write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def _stage2_register_bytes(entries: list[str]) -> bytes:
    return (
        "KD4_STAGE2_INCORRECT_BEHAVIORS_V1\n" + "\n".join(entries) + "\n"
    ).encode("utf-8")


def _provenance(actor_kind: str, text: str) -> dict[str, object]:
    return {
        "actor_kind": actor_kind,
        "source_kind": "conversation",
        "source_locator": "task:test-fixture",
        "exact_text": text,
        "text_sha256": admission.content_digest(
            "kd4.replacement-admission.provenance-text.v1", text
        ),
    }


def _reviewer(text: str) -> dict[str, object]:
    result = _provenance("root-reviewer", text)
    result["reviewer_id"] = "root:test-reviewer"
    result["verdict"] = "approved"
    return result


def _focused_replacement_approval_receipt() -> dict[str, object]:
    vectors = json.loads(FOCUSED_APPROVAL_VECTORS.read_text(encoding="utf-8"))
    return copy.deepcopy(vectors["valid_vectors"][0]["receipt"])


def _historical_scope_reviews(
    plan: dict[str, object],
) -> list[dict[str, object]]:
    return sorted(
        [
            {
                "review_scope_id": scope["review_scope_id"],
                "review_scope_sha256": scope["review_scope_sha256"],
                "disposition": admission.HISTORICAL_SCOPE_REVIEW_DISPOSITION,
            }
            for scope in plan["review_scopes"]
        ],
        key=lambda review: review["review_scope_id"],
    )


class FixtureBuilder:
    def __init__(self) -> None:
        inventory = json.loads(
            (REPOSITORY_ROOT / admission.INVENTORY_PATH).read_text(encoding="utf-8")
        )
        ledger = json.loads(
            (REPOSITORY_ROOT / admission.LEDGER_PATH).read_text(encoding="utf-8")
        )
        current_ledger = json.loads(
            (REPOSITORY_ROOT / admission.V2_LEDGER_PATH).read_text(encoding="utf-8")
        )
        self.inventory = {row["baseline_id"]: row for row in inventory["tests"]}
        self.ledger = {row["baseline_id"]: row for row in ledger["rows"]}
        self.current_ledger = {
            row["baseline_id"]: row
            for row in current_ledger["rows"]
            if row["baseline_id"] is not None
        }
        self.baseline_ids = sorted(
            baseline_id
            for baseline_id in self.inventory
            if self.current_ledger[baseline_id]["disposition"]["kind"]
            == "unresolved"
        )[:3]
        self.identities: dict[str, dict[str, object]] = {}

    @staticmethod
    def _fixed_digest(label: str) -> str:
        return admission.content_digest("kd4.replacement-admission.test-fixture.v1", label)

    def identity(self, replacement_id: str, validation_id: str) -> dict[str, object]:
        identity = {
            "test_id": replacement_id,
            "framework": "python-unittest",
            "native_id": replacement_id,
            "source_path": "scripts/test_replacement_admission.py",
            "test_route_id": f"route:{replacement_id}",
            "validation_id": validation_id,
            "runner_selector_sha256": self._fixed_digest(f"selector:{replacement_id}"),
            "executable_identity_sha256": self._fixed_digest(f"executable:{replacement_id}"),
            "execution_input_contract_sha256": self._fixed_digest(f"inputs:{replacement_id}"),
            "platform_applicability_sha256": self._fixed_digest(f"platform:{replacement_id}"),
        }
        self.identities[replacement_id] = identity
        return identity

    def baseline_binding(self, baseline_id: str) -> dict[str, object]:
        return {
            "baseline_id": baseline_id,
            "inventory_entry_sha256": admission.content_digest(
                "kd4.replacement-admission.baseline-inventory-entry.v1",
                self.inventory[baseline_id],
            ),
            "ledger_row_sha256": admission.content_digest(
                "kd4.replacement-admission.baseline-ledger-row.v1",
                self.ledger[baseline_id],
            ),
            "obligation_id": self.current_ledger[baseline_id]["obligation_id"],
        }

    def baseline_id_for_disposition(self, kind: str) -> str:
        return next(
            baseline_id
            for baseline_id in sorted(self.inventory)
            if self.current_ledger[baseline_id]["disposition"]["kind"] == kind
        )

    def successor(self, replacement_id: str, validation_id: str) -> dict[str, object]:
        identity = self.identity(replacement_id, validation_id)
        source_path = "scripts/test_replacement_admission.py"
        contract_path = "scripts/replacement_admission.py"
        source_files = [
            {
                "path": source_path,
                "raw_sha256": admission.file_sha256(REPOSITORY_ROOT / source_path),
            }
        ]
        contract_sources = [
            {
                "path": contract_path,
                "raw_sha256": admission.file_sha256(REPOSITORY_ROOT / contract_path),
            }
        ]
        runtime_path = ["Python CLI", "replacement admission check", replacement_id]
        runtime_path.sort()
        return {
            "replacement_id": replacement_id,
            "current_identity": identity,
            "current_identity_sha256": admission.content_digest(
                "kd4.replacement-admission.successor-identity.v1", identity
            ),
            "source_files": source_files,
            "runtime_path": runtime_path,
            "runtime_path_sha256": admission.content_digest(
                "kd4.replacement-admission.runtime-path.v1", runtime_path
            ),
            "contract_sources": contract_sources,
            "contract_sources_sha256": admission.content_digest(
                "kd4.replacement-admission.contract-sources.v1", contract_sources
            ),
            "validation_contract_sha256": admission.content_digest(
                "kd4.replacement-admission.validation-contract.v1",
                {
                    "validation_id": identity["validation_id"],
                    "runner_selector_sha256": identity["runner_selector_sha256"],
                    "execution_input_contract_sha256": identity[
                        "execution_input_contract_sha256"
                    ],
                },
            ),
        }

    @staticmethod
    def edge(
        baseline_id: str,
        replacement_id: str,
        role: str = "primary",
    ) -> dict[str, object]:
        behavior = f"{replacement_id} preserves {baseline_id} through a real CLI path."
        return {
            "baseline_id": baseline_id,
            "replacement_id": replacement_id,
            "role": role,
            "preserved_behavior": behavior,
            "behavior_sha256": admission.content_digest(
                "kd4.replacement-admission.behavior.v1", behavior
            ),
        }

    def admission(
        self,
        baseline_ids: list[str],
        replacement_ids: list[str],
        edges: list[dict[str, object]],
        *,
        accepted: bool = False,
    ) -> dict[str, object]:
        baselines = [self.baseline_binding(item) for item in sorted(baseline_ids)]
        successors = [
            self.successor(item, f"validation:{item}")
            for item in sorted(replacement_ids)
        ]
        edges.sort(key=lambda item: (item["baseline_id"], item["replacement_id"]))
        mapping_payload = {
            "baseline_bindings": baselines,
            "successors": successors,
            "edges": edges,
        }
        mapping_hash = admission.content_digest(
            "kd4.replacement-admission.mapping.v1", mapping_payload
        )
        result = {
            "admission_id": f"replacement-admission-v1.{mapping_hash}",
            "state": "accepted" if accepted else "focused-candidate",
            "approval_receipt_id": "pending",
            **mapping_payload,
            "mapping_sha256": mapping_hash,
            "product_behavior_obligation_sha256": admission.content_digest(
                "kd4.replacement-admission.product-behavior-obligation.v1",
                {
                    "obligation_ids": sorted(item["obligation_id"] for item in baselines),
                    "behavior_hashes": sorted(item["behavior_sha256"] for item in edges),
                },
            ),
            "candidate_receipt_sha256": "pending",
            "acceptance": None,
        }
        return result

    @staticmethod
    def approval(admission_ids: list[str]) -> dict[str, object]:
        admission_ids = sorted(admission_ids)
        result = {
            "admission_ids": admission_ids,
            "scope_sha256": admission.content_digest(
                "kd4.replacement-admission.approval-scope.v1", admission_ids
            ),
            "current_user": _provenance(
                "current-user", "Approve this exact replacement mapping scope."
            ),
            "reviewer": _reviewer("Approved after exact mapping review."),
        }
        receipt_hash = admission.content_digest(
            "kd4.replacement-admission.approval-receipt.v1", result
        )
        return {
            "approval_receipt_id": f"replacement-approval-v1.{receipt_hash}",
            **result,
            "receipt_sha256": receipt_hash,
        }

    @staticmethod
    def attach_approval(item: dict[str, object], approval: dict[str, object]) -> None:
        item["approval_receipt_id"] = approval["approval_receipt_id"]
        candidate_payload = {
            key: value
            for key, value in item.items()
            if key not in {"candidate_receipt_sha256", "acceptance"}
        }
        candidate_payload["approval_receipt_sha256"] = approval["receipt_sha256"]
        item["candidate_receipt_sha256"] = admission.content_digest(
            "kd4.replacement-admission.candidate-receipt.v1", candidate_payload
        )

    def trusted_defect_receipt(
        self,
        item: dict[str, object],
        behavior_id: str,
        incorrect_behavior: str,
    ) -> dict[str, object]:
        baseline_ids = sorted(
            binding["baseline_id"] for binding in item["baseline_bindings"]
        )
        obligation_ids = sorted(
            binding["obligation_id"] for binding in item["baseline_bindings"]
        )
        edge_ids = sorted(
            "replacement-edge-v2."
            + admission.content_digest(
                "kd4.replacement-admission.test-edge.v1",
                {
                    "baseline_id": edge["baseline_id"],
                    "replacement_id": edge["replacement_id"],
                },
            )
            for edge in item["edges"]
        )
        receipt = {
            "baseline_ids": baseline_ids,
            "baseline_obligation_ids": obligation_ids,
            "defect_id": behavior_id,
            "failure": {
                "attempt_id": f"failure:{behavior_id}",
                "classification": "confirmed-validation-failure",
                "execution_ids": [f"failure-execution:{behavior_id}"],
                "focused_projection_sha256": self._fixed_digest(
                    f"failure-projection:{behavior_id}"
                ),
                "mutation_epoch": 3,
                "workspace_fingerprint": self._fixed_digest(
                    f"failure-workspace:{behavior_id}"
                ),
            },
            "incorrect_behavior": incorrect_behavior,
            "mutation": {
                "changed_input_leaf_ids": ["scripts/replacement_admission.py"],
                "classification": "relevant-non-test-product-runtime",
                "from_epoch": 3,
                "input_contract_set_sha256": self._fixed_digest(
                    f"input-contract:{behavior_id}"
                ),
                "production_delta_sha256": self._fixed_digest(
                    f"production-delta:{behavior_id}"
                ),
                "to_epoch": 4,
            },
            "pass": {
                "attempt_id": f"pass:{behavior_id}",
                "classification": "confirmed-pass",
                "execution_ids": [f"pass-execution:{behavior_id}"],
                "focused_projection_sha256": self._fixed_digest(
                    f"pass-projection:{behavior_id}"
                ),
                "mutation_epoch": 4,
                "workspace_fingerprint": self._fixed_digest(
                    f"pass-workspace:{behavior_id}"
                ),
            },
            "replacement_edge_ids": edge_ids,
            "resolved_entry_set_sha256": self._fixed_digest(
                f"resolved-entries:{behavior_id}"
            ),
            "schema_version": 1,
            "selection_v1_sha256": self._fixed_digest(f"selection:{behavior_id}"),
        }
        receipt["receipt_sha256"] = proof_hash(
            "kd4.trusted-defect-receipt.v1", receipt
        )
        return receipt

    def accept(
        self,
        item: dict[str, object],
        *,
        defect_receipts: list[dict[str, object]] | None = None,
        register_raw_sha256: str | None = None,
    ) -> None:
        focused = []
        for successor in item["successors"]:
            replacement_id = successor["replacement_id"]
            fingerprint = self._fixed_digest(f"workspace:{replacement_id}")
            receipt = {
                "admission_id": item["admission_id"],
                "replacement_id": replacement_id,
                "report_path": f"private-reports/{replacement_id}.json",
                "report_sha256": self._fixed_digest(f"report:{replacement_id}"),
                "attempt_id": f"attempt:{replacement_id}",
                "private_nonce_sha256": self._fixed_digest(f"nonce:{replacement_id}"),
                "exact_command": f"python -m unittest {replacement_id}",
                "repository_identity_sha256": self._fixed_digest("repository"),
                "host_identity_sha256": self._fixed_digest("host"),
                "parent_process_sha256": self._fixed_digest("parent"),
                "runner_identity_sha256": self._fixed_digest("runner"),
                "child_runner_identities_sha256": self._fixed_digest(
                    f"child:{replacement_id}"
                ),
                "validation_id": successor["current_identity"]["validation_id"],
                "intended_count": 1,
                "selected_count": 1,
                "executed_count": 1,
                "executed_validation_ids": [replacement_id],
                "classification": "confirmed-pass",
                "starting_workspace_fingerprint": fingerprint,
                "ending_workspace_fingerprint": fingerprint,
                "mutation_epoch": f"epoch:{replacement_id}",
                "selection_sha256": self._fixed_digest(f"selection:{replacement_id}"),
                "resolved_inputs_sha256": self._fixed_digest(f"resolved:{replacement_id}"),
                "validation_execution_id": f"execution:{replacement_id}",
                "eligibility_receipt_sha256": self._fixed_digest(
                    f"eligibility:{replacement_id}"
                ),
            }
            receipt["receipt_sha256"] = admission.content_digest(
                "kd4.replacement-admission.focused-validation-receipt.v1", receipt
            )
            focused.append(receipt)
        defect_receipts = defect_receipts or []
        stage2 = {
            "disposition": (
                "incorrect-behavior-fixed"
                if defect_receipts
                else "reviewed-no-incorrect-behavior"
            ),
            "register_path": admission.STAGE2_PATH,
            "register_raw_sha256": (
                register_raw_sha256
                if register_raw_sha256 is not None
                else admission.file_sha256(REPOSITORY_ROOT / admission.STAGE2_PATH)
            ),
            "behavior_ids": sorted(
                receipt["defect_id"] for receipt in defect_receipts
            ),
            "defect_receipt_sha256s": sorted(
                receipt["receipt_sha256"] for receipt in defect_receipts
            ),
        }
        accepted_payload = {
            "admission_id": item["admission_id"],
            "candidate_receipt_sha256": item["candidate_receipt_sha256"],
            "focused_validation_receipts": focused,
            "stage2": stage2,
        }
        item["acceptance"] = {
            "focused_validation_receipts": focused,
            "stage2": stage2,
            "accepted_receipt_sha256": admission.content_digest(
                "kd4.replacement-admission.accepted-receipt.v1", accepted_payload
            ),
        }

    @staticmethod
    def manifest(
        admissions: list[dict[str, object]], approvals: list[dict[str, object]]
    ) -> dict[str, object]:
        result = {
            "format_id": admission.FORMAT_ID,
            "schema_version": 1,
            "predecessor_authority": {
                "inventory_path": admission.INVENTORY_PATH,
                "inventory_raw_sha256": admission.FROZEN_INVENTORY_RAW_SHA256,
                "inventory_semantic_sha256": admission.FROZEN_INVENTORY_SEMANTIC_SHA256,
                "ledger_path": admission.LEDGER_PATH,
                "ledger_raw_sha256": admission.FROZEN_LEDGER_RAW_SHA256,
                "baseline_ids_sha256": admission.FROZEN_BASELINE_IDS_SHA256,
            },
            "approval_receipts": sorted(
                approvals, key=lambda item: item["approval_receipt_id"]
            ),
            "admissions": sorted(admissions, key=lambda item: item["admission_id"]),
        }
        result["semantic_sha256"] = admission.content_digest(
            "kd4.replacement-admission.manifest-semantic.v1", result
        )
        result["self_hash"] = admission.content_digest(
            "kd4.replacement-admission.manifest-self.v1", result
        )
        return result

    def catalog(self) -> dict[str, object]:
        return {
            "format_id": admission.SUCCESSOR_CATALOG_FORMAT_ID,
            "schema_version": 1,
            "successors": [self.identities[key] for key in sorted(self.identities)],
        }


class ReplacementAdmissionCliTests(unittest.TestCase):
    maxDiff = None

    def run_cli(
        self,
        manifest: dict[str, object] | None,
        catalog: dict[str, object] | None = None,
        *,
        materialize: bool = False,
        stage2_entries: list[str] | None = None,
        trusted_defect_receipts: list[dict[str, object]] | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object] | None]:
        with tempfile.TemporaryDirectory() as temporary:
            temp = Path(temporary)
            repository_root = REPOSITORY_ROOT
            if stage2_entries is not None:
                repository_root = temp / "repository"
                for relative in (
                    admission.INVENTORY_PATH,
                    admission.LEDGER_PATH,
                    admission.V2_LEDGER_PATH,
                    "scripts/replacement_admission.py",
                    "scripts/test_replacement_admission.py",
                ):
                    destination = repository_root / relative
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(REPOSITORY_ROOT / relative, destination)
                register_path = repository_root / admission.STAGE2_PATH
                register_path.parent.mkdir(parents=True, exist_ok=True)
                register_path.write_bytes(_stage2_register_bytes(stage2_entries))
            manifest_path = temp / "manifest.json"
            if manifest is None:
                manifest_path = repository_root / ".codex/validation/replacement-admissions-v1.json"
            else:
                _write_json(manifest_path, manifest)
            command = [
                sys.executable,
                str(CLI),
                "materialize" if materialize else "check",
                "--repository-root",
                str(repository_root),
                "--manifest",
                str(manifest_path),
            ]
            if catalog is not None:
                catalog_path = temp / "catalog.json"
                _write_json(catalog_path, catalog)
                command.extend(["--successor-inventory", str(catalog_path)])
            if trusted_defect_receipts is not None:
                receipts_path = temp / "trusted-defect-receipts.json"
                _write_json(receipts_path, trusted_defect_receipts)
                command.extend(["--trusted-defect-receipts", str(receipts_path)])
            output_path = temp / "projection.json"
            if materialize:
                command.extend(["--output", str(output_path)])
            result = subprocess.run(
                command,
                cwd=REPOSITORY_ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            projection = (
                json.loads(output_path.read_text(encoding="utf-8"))
                if output_path.exists()
                else None
            )
            return result, projection

    def run_historical_acceptance_cli(
        self,
        review_plan: dict[str, object],
        focused_receipt: dict[str, object],
        scope_reviews: list[dict[str, object]],
        *,
        materialize: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            temp = Path(temporary)
            review_plan_path = temp / "historical-review-plan.json"
            focused_receipt_path = temp / "focused-approval-receipt.json"
            scope_reviews_path = temp / "scope-reviews.json"
            _write_json(review_plan_path, review_plan)
            _write_json(focused_receipt_path, focused_receipt)
            _write_json(scope_reviews_path, scope_reviews)
            return subprocess.run(
                [
                    sys.executable,
                    str(CLI),
                    (
                        "materialize-historical-acceptance"
                        if materialize
                        else "check-historical-acceptance"
                    ),
                    "--repository-root",
                    str(REPOSITORY_ROOT),
                    "--review-plan",
                    str(review_plan_path),
                    "--focused-approval-receipt",
                    str(focused_receipt_path),
                    "--scope-reviews",
                    str(scope_reviews_path),
                ],
                cwd=REPOSITORY_ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )

    def test_real_cli_accepts_empty_repository_manifest(self) -> None:
        result, projection = self.run_cli(None)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("replacement-admissions: ok admissions=0", result.stdout)
        self.assertIsNone(projection)

        materialize_result, materialized_projection = self.run_cli(
            None, materialize=True
        )
        self.assertEqual(
            materialize_result.returncode, 0, materialize_result.stdout
        )
        self.assertEqual(materialized_projection["mappings"], [])

    def test_historical_replacement_review_plan_closes_exact_frozen_components(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "historical-review.json"
            result = subprocess.run(
                [
                    sys.executable,
                    str(CLI),
                    "review-historical",
                    "--repository-root",
                    str(REPOSITORY_ROOT),
                    "--output",
                    str(output),
                ],
                cwd=REPOSITORY_ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertIn(
                "replacement-admissions: ok historical-review-scopes=531",
                result.stdout,
            )
            plan = json.loads(output.read_text(encoding="utf-8"))

        self.assertEqual(plan["baseline_count"], 644)
        self.assertEqual(plan["edge_count"], 685)
        self.assertEqual(plan["successor_count"], 572)
        self.assertEqual(plan["review_scope_count"], 531)
        self.assertEqual(len(plan["review_scopes"]), 531)

        predecessor = json.loads(
            (REPOSITORY_ROOT / admission.LEDGER_PATH).read_text(encoding="utf-8")
        )
        current = json.loads(
            (REPOSITORY_ROOT / admission.V2_LEDGER_PATH).read_text(encoding="utf-8")
        )
        admission.validate_historical_replacement_review_plan_v1(
            plan, predecessor, current
        )

        mutations: list[tuple[str, dict[str, object], dict[str, object]]] = []

        def rehash_review_plan(candidate: dict[str, object]) -> None:
            candidate["review_scopes"].sort(
                key=lambda scope: scope["baseline_ids"]
            )
            for scope in candidate["review_scopes"]:
                component = {
                    "baseline_ids": scope["baseline_ids"],
                    "edges": scope["edges"],
                    "successor_ids": scope["successor_ids"],
                }
                scope_hash = proof_hash(
                    "kd4.historical-replacement-review-scope.v1", component
                )
                scope["review_scope_sha256"] = scope_hash
                scope["review_scope_id"] = (
                    f"historical-replacement-review-v1.{scope_hash}"
                )
            candidate["baseline_count"] = len(
                {
                    baseline_id
                    for scope in candidate["review_scopes"]
                    for baseline_id in scope["baseline_ids"]
                }
            )
            candidate["edge_count"] = sum(
                len(scope["edges"])
                for scope in candidate["review_scopes"]
            )
            candidate["successor_count"] = len(
                {
                    successor_id
                    for scope in candidate["review_scopes"]
                    for successor_id in scope["successor_ids"]
                }
            )
            candidate["review_scope_count"] = len(candidate["review_scopes"])
            candidate.pop("review_plan_sha256", None)
            candidate["review_plan_sha256"] = proof_hash(
                "kd4.historical-replacement-review-plan.v1", candidate
            )

        def rehash_ledger(candidate: dict[str, object]) -> None:
            candidate["rows"].sort(key=canonical_jcs)
            semantic_projection = {
                key: candidate[key]
                for key in (
                    "format_id",
                    "inventory_authority",
                    "rows",
                    "schema_version",
                    "trusted_defect_receipts",
                )
            }
            candidate["semantic_sha256"] = proof_hash(
                "kd4.test-replacement-ledger.v2.semantic", semantic_projection
            )
            candidate["self_hash"] = proof_hash(
                "kd4.test-replacement-ledger.v2.self",
                {
                    **semantic_projection,
                    "semantic_sha256": candidate["semantic_sha256"],
                },
            )

        missing_edge = copy.deepcopy(plan)
        missing_edge["review_scopes"][0]["edges"].pop()
        rehash_review_plan(missing_edge)
        mutations.append(("missing edge", missing_edge, current))

        same_count_rewire = copy.deepcopy(plan)
        singleton_scopes = [
            scope
            for scope in same_count_rewire["review_scopes"]
            if len(scope["edges"]) == 1
            and len(scope["successor_ids"]) == 1
        ][:2]
        first_successor = singleton_scopes[0]["successor_ids"][0]
        second_successor = singleton_scopes[1]["successor_ids"][0]
        singleton_scopes[0]["successor_ids"] = [second_successor]
        singleton_scopes[0]["edges"][0]["replacement_id"] = second_successor
        singleton_scopes[1]["successor_ids"] = [first_successor]
        singleton_scopes[1]["edges"][0]["replacement_id"] = first_successor
        rehash_review_plan(same_count_rewire)
        self.assertEqual(
            (
                same_count_rewire["baseline_count"],
                same_count_rewire["edge_count"],
                same_count_rewire["successor_count"],
                same_count_rewire["review_scope_count"],
            ),
            (644, 685, 572, 531),
        )
        mutations.append(("same-count rewire", same_count_rewire, current))

        shared_scope_index = next(
            index
            for index, scope in enumerate(plan["review_scopes"])
            if len(scope["baseline_ids"]) > 1
        )
        shared_successor_split = copy.deepcopy(plan)
        shared_scope = shared_successor_split["review_scopes"].pop(
            shared_scope_index
        )
        left_baseline = shared_scope["baseline_ids"][0]
        left_edges = [
            edge
            for edge in shared_scope["edges"]
            if edge["baseline_id"] == left_baseline
        ]
        right_edges = [
            edge
            for edge in shared_scope["edges"]
            if edge["baseline_id"] != left_baseline
        ]
        shared_successor_split["review_scopes"].extend(
            [
                {
                    **shared_scope,
                    "baseline_ids": [left_baseline],
                    "edges": left_edges,
                    "successor_ids": sorted(
                        {edge["replacement_id"] for edge in left_edges}
                    ),
                },
                {
                    **shared_scope,
                    "baseline_ids": shared_scope["baseline_ids"][1:],
                    "edges": right_edges,
                    "successor_ids": sorted(
                        {edge["replacement_id"] for edge in right_edges}
                    ),
                },
            ]
        )
        mutations.append(
            ("shared-successor split", shared_successor_split, current)
        )
        rehash_review_plan(shared_successor_split)

        unrelated_merge = copy.deepcopy(plan)
        left = unrelated_merge["review_scopes"].pop(0)
        right = unrelated_merge["review_scopes"].pop(0)
        unrelated_merge["review_scopes"].insert(
            0,
            {
                **left,
                "baseline_ids": sorted(left["baseline_ids"] + right["baseline_ids"]),
                "edges": sorted(
                    left["edges"] + right["edges"],
                    key=lambda edge: (edge["baseline_id"], edge["replacement_id"]),
                ),
                "successor_ids": sorted(
                    set(left["successor_ids"] + right["successor_ids"])
                ),
            },
        )
        rehash_review_plan(unrelated_merge)
        mutations.append(("unrelated-component merge", unrelated_merge, current))

        cartesian_expansion = copy.deepcopy(plan)
        expansion_scope = next(
            scope
            for scope in cartesian_expansion["review_scopes"]
            if len(scope["baseline_ids"]) > 1
            and len(scope["successor_ids"]) > 1
            and len(scope["edges"])
            < len(scope["baseline_ids"]) * len(scope["successor_ids"])
        )
        existing_pairs = {
            (edge["baseline_id"], edge["replacement_id"])
            for edge in expansion_scope["edges"]
        }
        missing_pair = next(
            (baseline_id, successor_id)
            for baseline_id in expansion_scope["baseline_ids"]
            for successor_id in expansion_scope["successor_ids"]
            if (baseline_id, successor_id) not in existing_pairs
        )
        expansion_scope["edges"].append(
            {"baseline_id": missing_pair[0], "replacement_id": missing_pair[1]}
        )
        expansion_scope["edges"].sort(
            key=lambda edge: (edge["baseline_id"], edge["replacement_id"])
        )
        rehash_review_plan(cartesian_expansion)
        mutations.append(("Cartesian expansion", cartesian_expansion, current))

        historical_reset = copy.deepcopy(current)
        historical_baseline = plan["review_scopes"][0]["baseline_ids"][0]
        historical_row = next(
            row
            for row in historical_reset["rows"]
            if row["baseline_id"] == historical_baseline
        )
        unresolved_disposition = next(
            copy.deepcopy(row["disposition"])
            for row in historical_reset["rows"]
            if row.get("disposition", {}).get("kind") == "unresolved"
        )
        historical_row["disposition"] = unresolved_disposition
        rehash_ledger(historical_reset)
        mutations.append(("historical reset", plan, historical_reset))

        duplicate_historical = copy.deepcopy(current)
        conflicting_row = next(
            copy.deepcopy(row)
            for row in duplicate_historical["rows"]
            if row.get("disposition", {}).get("kind") == "recovered-container"
        )
        conflicting_row["baseline_id"] = historical_baseline
        duplicate_historical["rows"].append(conflicting_row)
        rehash_ledger(duplicate_historical)
        mutations.append(
            ("duplicate historical baseline", plan, duplicate_historical)
        )

        for label, candidate, candidate_current in mutations:
            with self.subTest(label=label):
                with self.assertRaises(admission.AdmissionError):
                    admission.validate_historical_replacement_review_plan_v1(
                        candidate, predecessor, candidate_current
                    )

        with tempfile.TemporaryDirectory() as temporary:
            repository = Path(temporary) / "repository"
            for relative, value in (
                (admission.LEDGER_PATH, predecessor),
                (admission.V2_LEDGER_PATH, duplicate_historical),
            ):
                destination = repository / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                if relative == admission.LEDGER_PATH:
                    shutil.copyfile(REPOSITORY_ROOT / relative, destination)
                else:
                    _write_json(destination, value)
            output = Path(temporary) / "duplicate-review.json"
            duplicate_result = subprocess.run(
                [
                    sys.executable,
                    str(CLI),
                    "review-historical",
                    "--repository-root",
                    str(repository),
                    "--output",
                    str(output),
                ],
                cwd=REPOSITORY_ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
        self.assertNotEqual(duplicate_result.returncode, 0)
        self.assertIn(
            "replacement ledger repeats a baseline ID", duplicate_result.stdout
        )

    def test_historical_acceptance_proposal_cli_and_schema_close_exact_review_set(
        self,
    ) -> None:
        plan = admission.compile_historical_replacement_review_plan(
            REPOSITORY_ROOT
        )
        receipt = _focused_replacement_approval_receipt()
        reviews = _historical_scope_reviews(plan)

        result = self.run_historical_acceptance_cli(plan, receipt, reviews)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "baselines=644 edges=685 successors=572 scopes=531",
            result.stdout,
        )
        self.assertIn("authority=structural-only", result.stdout)

        proposal = admission.build_historical_replacement_acceptance_proposal_v1(
            plan, receipt, reviews
        )
        self.assertEqual(
            proposal,
            admission.build_historical_replacement_acceptance_proposal_v1(
                plan, receipt, reviews
            ),
        )
        self.assertEqual(
            proposal,
            admission.validate_historical_replacement_acceptance_proposal_v1(
                proposal, REPOSITORY_ROOT, receipt
            ),
        )
        self.assertIsNone(proposal["activation_authority"])
        self.assertEqual(len(proposal["scope_reviews"]), 531)
        self.assertEqual(
            proposal["focused_replacement_approval_receipt_ref"][
                "receipt_sha256"
            ],
            receipt["receipt_sha256"],
        )
        schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
        proposal_schema = {
            "$schema": schema["$schema"],
            "$defs": schema["$defs"],
            "$ref": "#/$defs/historicalAcceptanceProposal",
        }
        proposal_contract = schema["$defs"]["historicalAcceptanceProposal"]
        self.assertEqual(
            set(proposal_contract["required"]), set(proposal)
        )
        self.assertFalse(proposal_contract["additionalProperties"])
        self.assertEqual(
            proposal_contract["properties"]["format_id"]["const"],
            admission.HISTORICAL_ACCEPTANCE_PROPOSAL_FORMAT_ID,
        )
        self.assertEqual(
            proposal_contract["properties"]["scope_reviews"]["minItems"],
            531,
        )
        self.assertEqual(
            proposal_contract["properties"]["activation_authority"]["type"],
            "null",
        )
        if importlib.util.find_spec("jsonschema") is not None:
            import jsonschema

            jsonschema.Draft202012Validator(proposal_schema).validate(proposal)

    def test_historical_acceptance_cli_rejects_scope_and_plan_drift(self) -> None:
        plan = admission.compile_historical_replacement_review_plan(
            REPOSITORY_ROOT
        )
        receipt = _focused_replacement_approval_receipt()
        reviews = _historical_scope_reviews(plan)

        missing = copy.deepcopy(reviews[:-1])
        extra = copy.deepcopy(reviews)
        extra.append(
            {
                "review_scope_id": "historical-replacement-review-v1." + "f" * 64,
                "review_scope_sha256": "f" * 64,
                "disposition": admission.HISTORICAL_SCOPE_REVIEW_DISPOSITION,
            }
        )
        extra.sort(key=lambda review: review["review_scope_id"])
        rewired = copy.deepcopy(reviews)
        rewired[0]["review_scope_sha256"] = rewired[1]["review_scope_sha256"]
        cases = (
            ("missing scope", plan, missing, "do not exactly close"),
            ("extra scope", plan, extra, "do not exactly close"),
            ("rewired scope", plan, rewired, "ID does not match its scope hash"),
        )
        for label, candidate_plan, candidate_reviews, expected in cases:
            with self.subTest(label=label):
                result = self.run_historical_acceptance_cli(
                    candidate_plan, receipt, candidate_reviews
                )
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(expected, result.stdout)

        wrong_plan_hash = copy.deepcopy(plan)
        wrong_plan_hash["review_plan_sha256"] = "0" * 64
        result = self.run_historical_acceptance_cli(
            wrong_plan_hash, receipt, reviews
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("review plan or review-plan hash", result.stdout)

    def test_historical_acceptance_rejects_rehashed_and_hash_spoofed_scopes(
        self,
    ) -> None:
        plan = admission.compile_historical_replacement_review_plan(
            REPOSITORY_ROOT
        )
        receipt = _focused_replacement_approval_receipt()

        fully_rehashed = copy.deepcopy(plan)
        singleton_scopes = [
            scope
            for scope in fully_rehashed["review_scopes"]
            if len(scope["edges"]) == 1
            and len(scope["successor_ids"]) == 1
        ][:2]
        first_successor = singleton_scopes[0]["successor_ids"][0]
        second_successor = singleton_scopes[1]["successor_ids"][0]
        singleton_scopes[0]["successor_ids"] = [second_successor]
        singleton_scopes[0]["edges"][0]["replacement_id"] = second_successor
        singleton_scopes[1]["successor_ids"] = [first_successor]
        singleton_scopes[1]["edges"][0]["replacement_id"] = first_successor
        for scope in singleton_scopes:
            component = {
                "baseline_ids": scope["baseline_ids"],
                "edges": scope["edges"],
                "successor_ids": scope["successor_ids"],
            }
            scope_hash = proof_hash(
                "kd4.historical-replacement-review-scope.v1", component
            )
            scope["review_scope_sha256"] = scope_hash
            scope["review_scope_id"] = (
                f"historical-replacement-review-v1.{scope_hash}"
            )
        fully_rehashed["review_plan_sha256"] = proof_hash(
            "kd4.historical-replacement-review-plan.v1",
            {
                key: value
                for key, value in fully_rehashed.items()
                if key != "review_plan_sha256"
            },
        )

        copied_good_hash = copy.deepcopy(plan)
        copied_scope = next(
            scope
            for scope in copied_good_hash["review_scopes"]
            if len(scope["edges"]) == 1
            and len(scope["successor_ids"]) == 1
        )
        copied_scope["successor_ids"] = ["successor:altered-but-unhashed"]
        copied_scope["edges"][0]["replacement_id"] = (
            "successor:altered-but-unhashed"
        )

        for label, candidate, direct_error in (
            (
                "fully rehashed altered scope",
                fully_rehashed,
                "do not match the exact frozen V1 graph",
            ),
            (
                "copied-good-hash altered scope",
                copied_good_hash,
                "review_scope_sha256 mismatch",
            ),
        ):
            with self.subTest(label=label, boundary="direct API"):
                with self.assertRaisesRegex(
                    admission.AdmissionError, direct_error
                ):
                    admission.build_historical_replacement_acceptance_proposal_v1(
                        candidate,
                        receipt,
                        _historical_scope_reviews(candidate),
                    )
            with self.subTest(label=label, boundary="CLI"):
                result = self.run_historical_acceptance_cli(
                    candidate,
                    receipt,
                    _historical_scope_reviews(candidate),
                )
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("review plan or review-plan hash", result.stdout)

    def test_historical_acceptance_rejects_old_receipt_and_materialization(
        self,
    ) -> None:
        plan = admission.compile_historical_replacement_review_plan(
            REPOSITORY_ROOT
        )
        reviews = _historical_scope_reviews(plan)
        fixture = FixtureBuilder()
        baseline_id = fixture.baseline_ids[0]
        item = fixture.admission(
            [baseline_id],
            ["successor:old-focused-receipt"],
            [fixture.edge(baseline_id, "successor:old-focused-receipt")],
            accepted=True,
        )
        approval = fixture.approval([item["admission_id"]])
        fixture.attach_approval(item, approval)
        fixture.accept(item)
        old_receipt = item["acceptance"]["focused_validation_receipts"][0]

        old_result = self.run_historical_acceptance_cli(
            plan, old_receipt, reviews
        )
        self.assertNotEqual(old_result.returncode, 0, old_result.stdout)
        self.assertIn(
            "focused replacement approval receipt is invalid", old_result.stdout
        )

        materialize_result = self.run_historical_acceptance_cli(
            plan,
            _focused_replacement_approval_receipt(),
            reviews,
            materialize=True,
        )
        self.assertNotEqual(
            materialize_result.returncode, 0, materialize_result.stdout
        )
        self.assertIn(
            "requires a trusted in-process Core capability",
            materialize_result.stdout,
        )

    def test_real_cli_structurally_checks_unresolved_one_to_many_and_many_to_one_candidates(self) -> None:
        fixture = FixtureBuilder()
        first, second, third = fixture.baseline_ids
        one_to_many = fixture.admission(
            [first],
            ["successor:cli-primary", "successor:native-fallback"],
            [
                fixture.edge(first, "successor:cli-primary"),
                fixture.edge(first, "successor:native-fallback", "supplementary"),
            ],
        )
        many_to_one = fixture.admission(
            [second, third],
            ["successor:consolidated"],
            [
                fixture.edge(second, "successor:consolidated"),
                fixture.edge(third, "successor:consolidated"),
            ],
        )
        approvals = []
        for item in (one_to_many, many_to_one):
            receipt = fixture.approval([item["admission_id"]])
            fixture.attach_approval(item, receipt)
            approvals.append(receipt)
        manifest = fixture.manifest([one_to_many, many_to_one], approvals)
        result, projection = self.run_cli(manifest, fixture.catalog())
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("replacement-admissions: ok admissions=2", result.stdout)
        self.assertIsNone(projection)

    def test_real_cli_rejects_nonempty_materialization_without_runtime_authority(self) -> None:
        fixture = FixtureBuilder()
        baseline_id = fixture.baseline_ids[0]
        item = fixture.admission(
            [baseline_id],
            ["successor:authority-gated"],
            [fixture.edge(baseline_id, "successor:authority-gated")],
        )
        receipt = fixture.approval([item["admission_id"]])
        fixture.attach_approval(item, receipt)
        manifest = fixture.manifest([item], [receipt])

        result, projection = self.run_cli(
            manifest, fixture.catalog(), materialize=True
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("requires trusted in-process authority", result.stdout)
        self.assertIsNone(projection)

    def test_real_cli_rejects_non_unresolved_v2_baselines(self) -> None:
        for kind in ("replacement", "exception", "recovered-container"):
            with self.subTest(kind=kind):
                fixture = FixtureBuilder()
                baseline_id = fixture.baseline_id_for_disposition(kind)
                replacement_id = f"successor:reject-{kind}"
                item = fixture.admission(
                    [baseline_id],
                    [replacement_id],
                    [fixture.edge(baseline_id, replacement_id)],
                )
                receipt = fixture.approval([item["admission_id"]])
                fixture.attach_approval(item, receipt)
                manifest = fixture.manifest([item], [receipt])

                result, projection = self.run_cli(manifest, fixture.catalog())
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(
                    f"has current V2 disposition {kind}; only unresolved baselines are admissible",
                    result.stdout,
                )
                self.assertIsNone(projection)

    def test_real_cli_rejects_forged_v2_obligation(self) -> None:
        fixture = FixtureBuilder()
        baseline_id = fixture.baseline_ids[0]
        fixture.current_ledger[baseline_id]["obligation_id"] = "forged-obligation"
        item = fixture.admission(
            [baseline_id],
            ["successor:forged-obligation"],
            [fixture.edge(baseline_id, "successor:forged-obligation")],
        )
        receipt = fixture.approval([item["admission_id"]])
        fixture.attach_approval(item, receipt)
        manifest = fixture.manifest([item], [receipt])

        result, projection = self.run_cli(manifest, fixture.catalog())
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "obligation_id does not match current V2 ledger row", result.stdout
        )
        self.assertIsNone(projection)

    def test_real_cli_binds_stage2_register_text_to_trusted_receipt_and_unresolved_admission(self) -> None:
        fixture = FixtureBuilder()
        baseline_id = fixture.baseline_ids[0]
        behavior_id = "stage2-incorrect-behavior-v1.real-cli-binding"
        description = "The replacement CLI accepted a stale behavior description."
        entries = [f"{behavior_id}\t{description}"]
        register_hash = admission.sha256_bytes(_stage2_register_bytes(entries))
        item = fixture.admission(
            [baseline_id],
            ["successor:stage2-binding"],
            [fixture.edge(baseline_id, "successor:stage2-binding")],
            accepted=True,
        )
        approval = fixture.approval([item["admission_id"]])
        fixture.attach_approval(item, approval)
        receipt = fixture.trusted_defect_receipt(
            item, behavior_id, description
        )
        fixture.accept(
            item,
            defect_receipts=[receipt],
            register_raw_sha256=register_hash,
        )
        manifest = fixture.manifest([item], [approval])

        result, projection = self.run_cli(
            manifest,
            fixture.catalog(),
            stage2_entries=entries,
            trusted_defect_receipts=[receipt],
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("replacement-admissions: ok admissions=1", result.stdout)
        self.assertIsNone(projection)

        missing_receipt_result, _ = self.run_cli(
            manifest,
            fixture.catalog(),
            stage2_entries=entries,
        )
        self.assertNotEqual(
            missing_receipt_result.returncode, 0, missing_receipt_result.stdout
        )
        self.assertIn(
            "is missing from --trusted-defect-receipts",
            missing_receipt_result.stdout,
        )

    def test_real_cli_rejects_unreferenced_trusted_receipt_for_sentinel_no_defect(self) -> None:
        fixture = FixtureBuilder()
        baseline_id = fixture.baseline_ids[0]
        item = fixture.admission(
            [baseline_id],
            ["successor:no-defect-sentinel"],
            [fixture.edge(baseline_id, "successor:no-defect-sentinel")],
            accepted=True,
        )
        approval = fixture.approval([item["admission_id"]])
        fixture.attach_approval(item, approval)
        fixture.accept(item)
        receipt = fixture.trusted_defect_receipt(
            item,
            "stage2-incorrect-behavior-v1.unreferenced",
            "This valid receipt is not referenced by the no-defect admission.",
        )
        manifest = fixture.manifest([item], [approval])

        result, projection = self.run_cli(
            manifest,
            fixture.catalog(),
            trusted_defect_receipts=[receipt],
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "trusted defect receipt collection does not close over accepted "
            "Stage2 incorrect behaviors",
            result.stdout,
        )
        self.assertIn("missing=[]", result.stdout)
        self.assertIn(receipt["receipt_sha256"], result.stdout)
        self.assertIsNone(projection)

    def test_real_cli_rejects_duplicate_stage2_behavior_ids_with_different_text(self) -> None:
        fixture = FixtureBuilder()
        baseline_id = fixture.baseline_ids[0]
        behavior_id = "stage2-incorrect-behavior-v1.duplicate-id"
        description = "First description."
        entries = [
            f"{behavior_id}\t{description}",
            f"{behavior_id}\tSecond description.",
        ]
        register_hash = admission.sha256_bytes(_stage2_register_bytes(entries))
        item = fixture.admission(
            [baseline_id],
            ["successor:duplicate-id"],
            [fixture.edge(baseline_id, "successor:duplicate-id")],
            accepted=True,
        )
        approval = fixture.approval([item["admission_id"]])
        fixture.attach_approval(item, approval)
        receipt = fixture.trusted_defect_receipt(item, behavior_id, description)
        fixture.accept(
            item,
            defect_receipts=[receipt],
            register_raw_sha256=register_hash,
        )
        manifest = fixture.manifest([item], [approval])

        result, _ = self.run_cli(
            manifest,
            fixture.catalog(),
            stage2_entries=entries,
            trusted_defect_receipts=[receipt],
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("duplicate Stage2 incorrect-behavior ID", result.stdout)

    def test_real_cli_rejects_stage2_description_or_admission_binding_mismatch(self) -> None:
        fixture = FixtureBuilder()
        first, second = fixture.baseline_ids[:2]
        behavior_id = "stage2-incorrect-behavior-v1.exact-binding"
        description = "The replacement preserved the wrong product behavior."
        item = fixture.admission(
            [first],
            ["successor:exact-binding"],
            [fixture.edge(first, "successor:exact-binding")],
            accepted=True,
        )
        approval = fixture.approval([item["admission_id"]])
        fixture.attach_approval(item, approval)

        cases: list[tuple[str, str, dict[str, object], str]] = []
        text_receipt = fixture.trusted_defect_receipt(item, behavior_id, description)
        cases.append(
            (
                "description",
                "A different description was logged.",
                text_receipt,
                "description does not exactly match trusted receipt",
            )
        )
        baseline_receipt = fixture.trusted_defect_receipt(
            item, behavior_id, description
        )
        baseline_receipt["baseline_ids"] = [second]
        baseline_receipt["baseline_obligation_ids"] = [
            fixture.current_ledger[second]["obligation_id"]
        ]
        baseline_receipt["receipt_sha256"] = proof_hash(
            "kd4.trusted-defect-receipt.v1",
            {
                key: value
                for key, value in baseline_receipt.items()
                if key != "receipt_sha256"
            },
        )
        cases.append(
            (
                "baseline",
                description,
                baseline_receipt,
                "does not exactly bind the admission baseline IDs",
            )
        )

        for label, logged_description, receipt, expected in cases:
            with self.subTest(label=label):
                entries = [f"{behavior_id}\t{logged_description}"]
                register_hash = admission.sha256_bytes(
                    _stage2_register_bytes(entries)
                )
                case_item = copy.deepcopy(item)
                fixture.accept(
                    case_item,
                    defect_receipts=[receipt],
                    register_raw_sha256=register_hash,
                )
                manifest = fixture.manifest([case_item], [approval])
                result, _ = self.run_cli(
                    manifest,
                    fixture.catalog(),
                    stage2_entries=entries,
                    trusted_defect_receipts=[receipt],
                )
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(expected, result.stdout)

    def test_real_cli_rejects_missing_extra_stale_copied_and_replayed_admissions(self) -> None:
        fixture = FixtureBuilder()
        first, second, third = fixture.baseline_ids
        left = fixture.admission(
            [first], ["successor:left"], [fixture.edge(first, "successor:left")], accepted=True
        )
        right = fixture.admission(
            [second, third],
            ["successor:right"],
            [
                fixture.edge(second, "successor:right"),
                fixture.edge(third, "successor:right"),
            ],
            accepted=True,
        )
        approvals = []
        for item in (left, right):
            receipt = fixture.approval([item["admission_id"]])
            fixture.attach_approval(item, receipt)
            fixture.accept(item)
            approvals.append(receipt)
        good = fixture.manifest([left, right], approvals)
        good_catalog = fixture.catalog()
        result, _ = self.run_cli(good, good_catalog)
        self.assertEqual(result.returncode, 0, result.stdout)

        cases: list[tuple[str, dict[str, object], dict[str, object], str]] = []

        missing_baseline = copy.deepcopy(good)
        missing_baseline["admissions"][0]["baseline_bindings"][0][
            "baseline_id"
        ] = "missing-baseline"
        cases.append(("missing baseline", missing_baseline, good_catalog, "missing baseline"))

        extra_scope = copy.deepcopy(good)
        extra_scope["approval_receipts"][0]["admission_ids"].append(
            "replacement-admission-v1." + "f" * 64
        )
        extra_scope["approval_receipts"][0]["admission_ids"].sort()
        extra_scope["approval_receipts"][0]["scope_sha256"] = admission.content_digest(
            "kd4.replacement-admission.approval-scope.v1",
            extra_scope["approval_receipts"][0]["admission_ids"],
        )
        approval_payload = {
            key: value
            for key, value in extra_scope["approval_receipts"][0].items()
            if key not in {"approval_receipt_id", "receipt_sha256"}
        }
        extra_scope["approval_receipts"][0]["receipt_sha256"] = admission.content_digest(
            "kd4.replacement-admission.approval-receipt.v1", approval_payload
        )
        extra_scope["approval_receipts"][0]["approval_receipt_id"] = (
            "replacement-approval-v1."
            + extra_scope["approval_receipts"][0]["receipt_sha256"]
        )
        cases.append(("extra approval scope", extra_scope, good_catalog, "exact scope-bound approval"))

        stale_source = copy.deepcopy(good)
        stale_source["admissions"][0]["successors"][0]["source_files"][0][
            "raw_sha256"
        ] = "0" * 64
        cases.append(("stale source", stale_source, good_catalog, "file hash is stale"))

        missing_successor_catalog = copy.deepcopy(good_catalog)
        missing_successor_catalog["successors"] = missing_successor_catalog[
            "successors"
        ][1:]
        cases.append(("missing successor", good, missing_successor_catalog, "missing or differs"))

        copied_approval = copy.deepcopy(good)
        copied_approval["admissions"][1]["approval_receipt_id"] = copied_approval[
            "admissions"
        ][0]["approval_receipt_id"]
        cases.append(("copied approval", copied_approval, good_catalog, "exact scope-bound approval"))

        replayed = copy.deepcopy(good)
        replayed["admissions"][0]["acceptance"]["focused_validation_receipts"].append(
            copy.deepcopy(
                replayed["admissions"][0]["acceptance"][
                    "focused_validation_receipts"
                ][0]
            )
        )
        cases.append(("replayed receipt", replayed, good_catalog, "replays"))

        for label, manifest, catalog, expected in cases:
            with self.subTest(label=label):
                result, _ = self.run_cli(manifest, catalog)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(expected, result.stdout)


if __name__ == "__main__":
    unittest.main()
