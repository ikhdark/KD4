from __future__ import annotations

import base64
import copy
import json
from pathlib import Path
import unittest

from scripts.completion_proof_canonical import canonical_jcs
from scripts.focused_replacement_approval_receipt import FORMAT_ID
from scripts.focused_replacement_approval_receipt import RECEIPT_DIGEST_FIELDS
from scripts.focused_replacement_approval_receipt import RECEIPT_FIELDS
from scripts.focused_replacement_approval_receipt import RECEIPT_HASH_DOMAIN
from scripts.focused_replacement_approval_receipt import TRUSTED_CURRENT_CONTEXT_FIELDS
from scripts.focused_replacement_approval_receipt import (
    FocusedReplacementApprovalReceiptError,
)
from scripts.focused_replacement_approval_receipt import (
    focused_replacement_approval_receipt_digest_projection_v1,
)
from scripts.focused_replacement_approval_receipt import (
    focused_replacement_approval_receipt_digest_v1,
)
from scripts.focused_replacement_approval_receipt import (
    parse_focused_replacement_approval_receipt_v1,
)
from scripts.focused_replacement_approval_receipt import (
    validate_against_trusted_current_context_v1,
)
from scripts.focused_replacement_approval_receipt import (
    validate_focused_replacement_approval_receipt_v1,
)


VECTORS_PATH = (
    Path(__file__).resolve().parent
    / "fixtures"
    / "focused_replacement_approval_receipt_v1_vectors.json"
)


def _trusted_context(receipt: dict[str, object]) -> dict[str, object]:
    return {field: receipt[field] for field in TRUSTED_CURRENT_CONTEXT_FIELDS}


class FocusedReplacementApprovalReceiptV1Tests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.vectors = json.loads(VECTORS_PATH.read_text(encoding="utf-8"))
        cls.ordinary_receipt = copy.deepcopy(
            next(
                vector["receipt"]
                for vector in cls.vectors["valid_vectors"]
                if vector["case"] == "ordinary-confirmed-pass"
            )
        )

    def test_shared_valid_vectors_parse_and_bind_through_public_contract(self) -> None:
        self.assertEqual(
            self.vectors["receipt_format_id"],
            FORMAT_ID,
        )
        self.assertEqual(self.vectors["hash_domain"], RECEIPT_HASH_DOMAIN)
        for vector in self.vectors["valid_vectors"]:
            with self.subTest(case=vector["case"]):
                receipt = vector["receipt"]
                parsed = parse_focused_replacement_approval_receipt_v1(
                    canonical_jcs(receipt)
                )
                self.assertEqual(parsed, receipt)
                self.assertEqual(set(parsed), set(RECEIPT_FIELDS))
                self.assertEqual(
                    list(
                        focused_replacement_approval_receipt_digest_projection_v1(
                            parsed
                        )
                    ),
                    list(RECEIPT_DIGEST_FIELDS),
                )
                self.assertEqual(
                    focused_replacement_approval_receipt_digest_v1(parsed),
                    vector["expected_receipt_sha256"],
                )
                validate_against_trusted_current_context_v1(
                    parsed, _trusted_context(parsed)
                )
                canonical_digest_input = vector.get("canonical_digest_input")
                if canonical_digest_input is not None:
                    self.assertEqual(
                        canonical_jcs(
                            focused_replacement_approval_receipt_digest_projection_v1(
                                parsed
                            )
                        ).decode("utf-8"),
                        canonical_digest_input,
                    )

    def test_shared_invalid_vectors_fail_closed_through_public_contract(self) -> None:
        for vector in self.vectors["invalid_vectors"]:
            with self.subTest(case=vector["case"]):
                with self.assertRaises(FocusedReplacementApprovalReceiptError):
                    if vector["kind"] == "raw-json-bytes":
                        encoded = vector["raw_json_base64url"]
                        padding = "=" * (-len(encoded) % 4)
                        parse_focused_replacement_approval_receipt_v1(
                            base64.urlsafe_b64decode(encoded + padding)
                        )
                    elif vector["kind"] == "receipt":
                        raw = json.dumps(
                            vector["receipt"],
                            ensure_ascii=False,
                            separators=(",", ":"),
                            sort_keys=True,
                        ).encode("utf-8")
                        parse_focused_replacement_approval_receipt_v1(
                            raw
                        )
                    elif vector["kind"] == "value":
                        validate_focused_replacement_approval_receipt_v1(
                            vector["value"]
                        )
                    elif vector["kind"] == "trusted-current-context":
                        validate_against_trusted_current_context_v1(
                            vector["receipt"],
                            vector["trusted_current_context"],
                        )
                    else:
                        self.fail(f"unknown invalid vector kind {vector['kind']!r}")

    def test_parser_rejects_duplicate_keys_and_nonfinite_constants(self) -> None:
        for raw in (
            b'{"format_id":"first","format_id":"second"}',
            b'{"mutation_epoch":NaN}',
            b'{"mutation_epoch":Infinity}',
            b'{"mutation_epoch":-Infinity}',
        ):
            with self.subTest(raw=raw):
                with self.assertRaises(FocusedReplacementApprovalReceiptError):
                    parse_focused_replacement_approval_receipt_v1(raw)

    def test_closed_wire_shape_and_scalar_contracts_fail_closed(self) -> None:
        mutations: list[tuple[str, object]] = [
            ("schema_version", True),
            ("attempt_id", "01890f45-7e9a-4cc3-98c4-dc0c0c07398f"),
            ("focused_validation_id", "bad identifier"),
            ("classification", "passed"),
            ("frozen_inventory_hash", "A" * 64),
            ("policy_id", "bad/policy"),
            ("mutation_epoch", True),
            ("mutation_epoch", -1),
            ("mutation_epoch", 2**53),
        ]
        for field, value in mutations:
            with self.subTest(field=field, value=value):
                receipt = copy.deepcopy(self.ordinary_receipt)
                receipt[field] = value
                with self.assertRaises(FocusedReplacementApprovalReceiptError):
                    validate_focused_replacement_approval_receipt_v1(receipt)

        missing = copy.deepcopy(self.ordinary_receipt)
        missing.pop("classification")
        with self.assertRaises(FocusedReplacementApprovalReceiptError):
            validate_focused_replacement_approval_receipt_v1(missing)

        extra = copy.deepcopy(self.ordinary_receipt)
        extra["approval_text"] = "looks approved"
        with self.assertRaises(FocusedReplacementApprovalReceiptError):
            validate_focused_replacement_approval_receipt_v1(extra)

    def test_self_consistent_digest_is_not_current_context_authority(self) -> None:
        trusted = _trusted_context(self.ordinary_receipt)
        forged = copy.deepcopy(self.ordinary_receipt)
        forged["workspace_fingerprint"] = "6" * 64
        forged["receipt_sha256"] = focused_replacement_approval_receipt_digest_v1(
            forged
        )

        # The wire is internally consistent, but it is not authoritative for
        # the independently trusted current workspace.
        validate_focused_replacement_approval_receipt_v1(forged)
        with self.assertRaisesRegex(
            FocusedReplacementApprovalReceiptError,
            "independently trusted current context",
        ):
            validate_against_trusted_current_context_v1(forged, trusted)

    def test_semantic_validator_binds_every_trusted_current_context_field(self) -> None:
        receipt = self.ordinary_receipt
        trusted = _trusted_context(receipt)
        validate_against_trusted_current_context_v1(receipt, trusted)

        replacements: dict[str, object] = {
            "format_id": "kd4.other-receipt.v1",
            "schema_version": 2,
            "attempt_id": "01890f45-7e9a-7cc3-88c4-dc0c0c07398f",
            "focused_validation_id": "inventory.other-validation",
            "classification": "confirmed-failure",
            "frozen_inventory_hash": "6" * 64,
            "focused_inventory_catalog_semantic_sha256": "7" * 64,
            "inventory_discovery_processes_sha256": "8" * 64,
            "policy_id": "other-policy",
            "policy_runner_bundle_sha256": "9" * 64,
            "workspace_fingerprint": "a" * 64,
            "mutation_epoch": 8,
        }
        self.assertEqual(set(replacements), set(TRUSTED_CURRENT_CONTEXT_FIELDS))
        for field, replacement in replacements.items():
            with self.subTest(field=field):
                stale = dict(trusted)
                stale[field] = replacement
                with self.assertRaisesRegex(
                    FocusedReplacementApprovalReceiptError,
                    field,
                ):
                    validate_against_trusted_current_context_v1(receipt, stale)

        missing = dict(trusted)
        missing.pop("policy_id")
        with self.assertRaises(FocusedReplacementApprovalReceiptError):
            validate_against_trusted_current_context_v1(receipt, missing)

        extra = dict(trusted)
        extra["receipt_sha256"] = receipt["receipt_sha256"]
        with self.assertRaises(FocusedReplacementApprovalReceiptError):
            validate_against_trusted_current_context_v1(receipt, extra)


if __name__ == "__main__":
    unittest.main()
