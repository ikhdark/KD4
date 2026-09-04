from __future__ import annotations

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts import replacement_admission as admission


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
CLI = REPOSITORY_ROOT / "scripts" / "replacement_admission.py"


def _write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")


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


class FixtureBuilder:
    def __init__(self) -> None:
        inventory = json.loads(
            (REPOSITORY_ROOT / admission.INVENTORY_PATH).read_text(encoding="utf-8")
        )
        ledger = json.loads(
            (REPOSITORY_ROOT / admission.LEDGER_PATH).read_text(encoding="utf-8")
        )
        self.inventory = {row["baseline_id"]: row for row in inventory["tests"]}
        self.ledger = {row["baseline_id"]: row for row in ledger["rows"]}
        self.baseline_ids = sorted(self.inventory)[:3]
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
            "obligation_id": f"obligation:{baseline_id}",
        }

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

    def accept(self, item: dict[str, object]) -> None:
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
        stage2 = {
            "disposition": "reviewed-no-incorrect-behavior",
            "register_path": admission.STAGE2_PATH,
            "register_raw_sha256": admission.file_sha256(
                REPOSITORY_ROOT / admission.STAGE2_PATH
            ),
            "behavior_ids": [],
            "defect_receipt_sha256s": [],
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
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object] | None]:
        with tempfile.TemporaryDirectory() as temporary:
            temp = Path(temporary)
            manifest_path = temp / "manifest.json"
            if manifest is None:
                manifest_path = REPOSITORY_ROOT / ".codex/validation/replacement-admissions-v1.json"
            else:
                _write_json(manifest_path, manifest)
            command = [
                sys.executable,
                str(CLI),
                "materialize" if materialize else "check",
                "--repository-root",
                str(REPOSITORY_ROOT),
                "--manifest",
                str(manifest_path),
            ]
            if catalog is not None:
                catalog_path = temp / "catalog.json"
                _write_json(catalog_path, catalog)
                command.extend(["--successor-inventory", str(catalog_path)])
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

    def test_real_cli_accepts_empty_repository_manifest(self) -> None:
        result, projection = self.run_cli(None)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("replacement-admissions: ok admissions=0", result.stdout)
        self.assertIsNone(projection)

    def test_real_cli_accepts_one_to_many_and_many_to_one_candidates(self) -> None:
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
        result, projection = self.run_cli(
            manifest, fixture.catalog(), materialize=True
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIsNotNone(projection)
        self.assertEqual(len(projection["mappings"]), 3)
        self.assertEqual(
            len(
                next(
                    row
                    for row in projection["mappings"]
                    if row["baseline_id"] == first
                )["successors"]
            ),
            2,
        )
        repeated_result, repeated_projection = self.run_cli(
            manifest, fixture.catalog(), materialize=True
        )
        self.assertEqual(repeated_result.returncode, 0, repeated_result.stdout)
        self.assertEqual(repeated_projection, projection)

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
