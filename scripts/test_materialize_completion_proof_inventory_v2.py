from __future__ import annotations

from collections import Counter
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.completion_proof_inventory_v2 import ActiveHostApplicabilityIssuerV1
from scripts.completion_proof_inventory_v2 import FROZEN_V1_INVENTORY_RAW_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_LEDGER_RAW_SHA256
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_PATHS
from scripts.completion_proof_inventory_v2 import canonical_jcs
from scripts.completion_proof_inventory_v2 import validate_frozen_test_inventory_v2
from scripts.completion_proof_inventory_v2 import validate_inventory_ledger_predecessor_closure
from scripts.completion_proof_inventory_v2 import validate_inventory_recovery_authority_v1
from scripts.completion_proof_inventory_v2 import validate_recovery_transition_receipt_v1
from scripts.completion_proof_inventory_v2 import validate_test_replacement_ledger_v2
from scripts.materialize_completion_proof_inventory_v2 import EXPECTED_DECLARATION_COUNT
from scripts.materialize_completion_proof_inventory_v2 import (
    EXPECTED_UNITTEST_HIDDEN_REPLACEMENT_COUNT,
)
from scripts.materialize_completion_proof_inventory_v2 import (
    EXPECTED_UNITTEST_LEDGER_IDENTITY_COUNT,
)
from scripts.materialize_completion_proof_inventory_v2 import (
    EXPECTED_UNITTEST_RECAPTURE_PARENT_COUNT,
)
from scripts.materialize_completion_proof_inventory_v2 import SOURCE_ONLY_SPECS
from scripts.materialize_completion_proof_inventory_v2 import V1_INVENTORY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V1_LEDGER_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_DOCTEST_RECAPTURE_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_INVENTORY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_LEDGER_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_RECOVERY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_RECOVERY_TRANSITION_RECEIPTS_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_UNITTEST_RECAPTURE_PATH
from scripts.materialize_completion_proof_inventory_v2 import UNITTEST_RECAPTURE_UNAVAILABLE
from scripts.materialize_completion_proof_inventory_v2 import MaterializationError
from scripts.materialize_completion_proof_inventory_v2 import _manifest_feature_contexts
from scripts.materialize_completion_proof_inventory_v2 import _manifest_index
from scripts.materialize_completion_proof_inventory_v2 import _write_or_check
from scripts.materialize_completion_proof_inventory_v2 import build_materialized_bundle
from scripts.materialize_completion_proof_inventory_v2 import validate_recovery_source_anchors
from scripts.test_completion_proof_inventory_v2 import _validate_schema_instance


REPO_ROOT = Path(__file__).resolve().parents[1]
MATERIALIZER = REPO_ROOT / "scripts/materialize_completion_proof_inventory_v2.py"


class InventoryV2MaterializationTests(unittest.TestCase):
    def test_real_cli_materializes_dormant_bundle_without_recovery_packet(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-inventory-v2-dormant-") as temp:
            output_root = Path(temp)
            result = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(REPO_ROOT),
                    "--output-root",
                    str(output_root),
                    "--without-doctest-recapture-packet",
                    "--without-unittest-recapture-packet",
                    "--write",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=180,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            summary = json.loads(result.stdout)
            self.assertEqual(summary["operation"], "write")
            self.assertEqual(summary["mode"], "dormant-materialization")
            self.assertEqual(
                summary["materialized_declaration_count"], EXPECTED_DECLARATION_COUNT
            )
            self.assertEqual(summary["executed_test_count"], 0)
            self.assertEqual(summary["counts"]["recovery_transition_receipts"], 0)
            self.assertEqual(
                summary["disposition_counts"],
                {"exception": 244, "replacement": 644, "unresolved": 14_659},
            )
            transition_receipts_path = output_root / V2_RECOVERY_TRANSITION_RECEIPTS_PATH
            self.assertEqual(transition_receipts_path.read_bytes(), b"[]")
            self.assertEqual(
                summary["artifact_sha256"][V2_RECOVERY_TRANSITION_RECEIPTS_PATH],
                hashlib.sha256(b"[]").hexdigest(),
            )
            self.assertFalse((output_root / V2_DOCTEST_RECAPTURE_PATH).exists())
            recovery = json.loads((output_root / V2_RECOVERY_PATH).read_bytes())
            self.assertEqual(
                [record["state"] for record in recovery["records"]],
                ["pending", "pending"],
            )
            self._assert_unittest_recovery_partition(output_root)

    def test_real_cli_materializes_exact_full_repository_bundle(self) -> None:
        before_v1_hashes = {
            V1_INVENTORY_PATH: hashlib.sha256((REPO_ROOT / V1_INVENTORY_PATH).read_bytes()).hexdigest(),
            V1_LEDGER_PATH: hashlib.sha256((REPO_ROOT / V1_LEDGER_PATH).read_bytes()).hexdigest(),
        }
        self.assertEqual(before_v1_hashes[V1_INVENTORY_PATH], FROZEN_V1_INVENTORY_RAW_SHA256)
        self.assertEqual(before_v1_hashes[V1_LEDGER_PATH], FROZEN_V1_LEDGER_RAW_SHA256)

        with tempfile.TemporaryDirectory(prefix="kd4-inventory-v2-a2-") as temp:
            output_root = Path(temp)
            result = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(REPO_ROOT),
                    "--output-root",
                    str(output_root),
                    "--doctest-recapture-packet",
                    str(REPO_ROOT / V2_DOCTEST_RECAPTURE_PATH),
                    "--without-unittest-recapture-packet",
                    "--write",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=180,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            summary = json.loads(result.stdout)
            self.assertEqual(summary["operation"], "write")
            self.assertEqual(summary["mode"], "doctest-recovery-materialization")
            self.assertEqual(
                summary["materialized_declaration_count"], EXPECTED_DECLARATION_COUNT
            )
            self.assertEqual(summary["executed_test_count"], 0)
            self.assertNotIn("intended_count", summary)
            self.assertNotIn("selected_count", summary)
            self.assertNotIn("executed_count", summary)
            self.assertEqual(
                summary["counts"],
                {
                    "cargo_target_contexts": 182,
                    "frozen_baseline_declarations": 15_544,
                    "inventory_declarations": 15_547,
                    "ledger_rows": 15_547,
                    "recovery_records": 2,
                    "recovery_transition_receipts": 1,
                    "source_only_declarations": 3,
                },
            )
            self.assertEqual(
                summary["framework_counts"],
                {
                    "argument-comment-lint-native": 21,
                    "javascript-jest": 45,
                    "python-pytest": 192,
                    "python-unittest": 909,
                    "rust-doctest": 5,
                    "rust-nextest": 14_329,
                    "windows-sandbox-smoke-native": 46,
                },
            )
            self.assertEqual(
                summary["disposition_counts"],
                {
                    "exception": 244,
                    "recovered-container": 5,
                    "replacement": 644,
                    "unresolved": 14_654,
                },
            )

            inventory_raw = (output_root / V2_INVENTORY_PATH).read_bytes()
            recovery_raw = (output_root / V2_RECOVERY_PATH).read_bytes()
            transition_receipts_raw = (
                output_root / V2_RECOVERY_TRANSITION_RECEIPTS_PATH
            ).read_bytes()
            doctest_recapture_raw = (output_root / V2_DOCTEST_RECAPTURE_PATH).read_bytes()
            ledger_raw = (output_root / V2_LEDGER_PATH).read_bytes()
            inventory = json.loads(inventory_raw)
            recovery = json.loads(recovery_raw)
            transition_receipts = json.loads(transition_receipts_raw)
            doctest_recapture = json.loads(doctest_recapture_raw)
            ledger = json.loads(ledger_raw)
            self.assertEqual(inventory_raw, canonical_jcs(inventory))
            self.assertEqual(recovery_raw, canonical_jcs(recovery))
            self.assertEqual(transition_receipts_raw, canonical_jcs(transition_receipts))
            self.assertEqual(doctest_recapture_raw, canonical_jcs(doctest_recapture))
            self.assertEqual(ledger_raw, canonical_jcs(ledger))
            self.assertEqual(
                summary["artifact_sha256"],
                {
                    V2_INVENTORY_PATH: hashlib.sha256(inventory_raw).hexdigest(),
                    V2_RECOVERY_PATH: hashlib.sha256(recovery_raw).hexdigest(),
                    V2_RECOVERY_TRANSITION_RECEIPTS_PATH: hashlib.sha256(
                        transition_receipts_raw
                    ).hexdigest(),
                    V2_DOCTEST_RECAPTURE_PATH: hashlib.sha256(
                        doctest_recapture_raw
                    ).hexdigest(),
                    V2_LEDGER_PATH: hashlib.sha256(ledger_raw).hexdigest(),
                },
            )

            validate_frozen_test_inventory_v2(inventory)
            validate_inventory_recovery_authority_v1(recovery)
            frozen_inventory = json.loads((REPO_ROOT / V1_INVENTORY_PATH).read_bytes())
            validate_recovery_source_anchors(recovery, frozen_inventory, REPO_ROOT)
            tampered_recovery = json.loads(recovery_raw)
            tampered_recovery["records"][1]["current_audit"]["runner_site_observations"][0][
                "line"
            ] += 1
            with self.assertRaisesRegex(MaterializationError, "source anchor does not resolve"):
                validate_recovery_source_anchors(tampered_recovery, frozen_inventory, REPO_ROOT)
            validate_test_replacement_ledger_v2(ledger)
            issuer = ActiveHostApplicabilityIssuerV1(
                "39bc53cc-ec47-4ea4-a940-b9a874779c30",
                hashlib.sha256(b"kd4.inventory-v2-a2.dormant-validation-only.v1").digest(),
            )
            doctest_record = recovery["records"][0]
            self.assertEqual(len(transition_receipts), 1)
            transition_receipt = transition_receipts[0]
            validate_recovery_transition_receipt_v1(transition_receipt)
            self.assertEqual(
                transition_receipt["receipt_sha256"],
                doctest_record["transition_receipt_sha256"],
            )
            validate_inventory_ledger_predecessor_closure(
                inventory,
                ledger,
                recovery_raw,
                [transition_receipt],
                issuer,
                doctest_recapture_raw,
            )

            schema_resources = {
                Path(path).name: json.loads((REPO_ROOT / path).read_bytes())
                for path in INVENTORY_V2_SCHEMA_PATHS
            }
            for name, instance in (
                ("frozen-test-inventory-v2.schema.json", inventory),
                ("frozen-test-inventory-v2-recoveries.schema.json", recovery),
                ("test-replacements-v2.schema.json", ledger),
            ):
                _validate_schema_instance(instance, schema_resources[name], schema_resources, name)

            declarations = inventory["declaration_universe"]
            self.assertEqual(len(declarations), EXPECTED_DECLARATION_COUNT)
            contexts = {
                (
                    context["package_name"],
                    context["target_kind"],
                    context["target_name"],
                ): context
                for context in inventory["cargo_target_context_specs"]
            }
            test_store_features = {
                "additional_features": ["codex-core/completion-proof-test-store"],
                "kind": "default",
            }
            for identity in (
                ("codex-core", "lib", "codex_core"),
                ("codex-core", "test", "core_thread_state"),
                ("codex-cli", "test", "completion_proof"),
                ("codex-app-server", "test", "all"),
                ("codex-mcp-server", "test", "all"),
            ):
                self.assertEqual(
                    contexts[identity]["feature_selection"], test_store_features
                )
            self.assertEqual(
                contexts[("codex-cli", "test", "debug_models")]["feature_selection"],
                {"additional_features": [], "kind": "default"},
            )
            source_only = [item for item in declarations if item["kind"] == "missing-baseline"]
            self.assertEqual(
                {item["entry"]["executable_identity"]["test_id"] for item in source_only},
                {spec["canonical_id"] for spec in SOURCE_ONLY_SPECS},
            )
            self.assertTrue(
                all(
                    item["source_provenance"]["kind"] == "platform-pending"
                    for item in source_only
                )
            )
            self.assertEqual(
                [record["state"] for record in recovery["records"]],
                ["resolved", "pending"],
            )
            self._assert_unittest_recovery_partition(output_root)
            self.assertEqual(
                Counter(row["disposition"]["kind"] for row in ledger["rows"]),
                Counter(
                    {
                        "unresolved": 14_654,
                        "replacement": 644,
                        "exception": 244,
                        "recovered-container": 5,
                    }
                ),
            )

            materialized_bundle = {
                "raw_documents": {
                    V2_INVENTORY_PATH: inventory_raw,
                    V2_RECOVERY_PATH: recovery_raw,
                    V2_RECOVERY_TRANSITION_RECEIPTS_PATH: transition_receipts_raw,
                    V2_DOCTEST_RECAPTURE_PATH: doctest_recapture_raw,
                    V2_LEDGER_PATH: ledger_raw,
                }
            }
            _write_or_check(materialized_bundle, output_root, write=False)
            (output_root / V2_LEDGER_PATH).write_bytes(ledger_raw + b" ")
            with self.assertRaisesRegex(MaterializationError, "test-replacements-v2.json"):
                _write_or_check(materialized_bundle, output_root, write=False)
            (output_root / V2_LEDGER_PATH).write_bytes(ledger_raw)

            def run_check() -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    [
                        sys.executable,
                        str(MATERIALIZER),
                        "--repo-root",
                        str(REPO_ROOT),
                        "--output-root",
                        str(output_root),
                        "--doctest-recapture-packet",
                        str(REPO_ROOT / V2_DOCTEST_RECAPTURE_PATH),
                        "--without-unittest-recapture-packet",
                        "--check",
                    ],
                    cwd=REPO_ROOT,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=180,
                )

            checked = run_check()
            self.assertEqual(checked.returncode, 0, checked.stderr)
            self.assertEqual(json.loads(checked.stdout)["operation"], "check")

            (output_root / V2_RECOVERY_PATH).unlink()
            missing = run_check()
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn(V2_RECOVERY_PATH, missing.stderr)
            (output_root / V2_RECOVERY_PATH).write_bytes(recovery_raw)

            (output_root / V2_RECOVERY_TRANSITION_RECEIPTS_PATH).unlink()
            missing_receipts = run_check()
            self.assertNotEqual(missing_receipts.returncode, 0)
            self.assertIn(V2_RECOVERY_TRANSITION_RECEIPTS_PATH, missing_receipts.stderr)
            (output_root / V2_RECOVERY_TRANSITION_RECEIPTS_PATH).write_bytes(
                transition_receipts_raw
            )

            extra_path = output_root / ".codex/validation/frozen-test-inventory-v2-unexpected.json"
            extra_path.write_bytes(b"{}")
            extra = run_check()
            self.assertNotEqual(extra.returncode, 0)
            self.assertIn("unexpected owned V2 artifacts", extra.stderr)
            self.assertIn(extra_path.name, extra.stderr)
            extra_path.unlink()

            (output_root / V2_LEDGER_PATH).write_bytes(inventory_raw)
            substituted = run_check()
            self.assertNotEqual(substituted.returncode, 0)
            self.assertIn(V2_LEDGER_PATH, substituted.stderr)
            (output_root / V2_LEDGER_PATH).write_bytes(ledger_raw)

            final_check = run_check()
            self.assertEqual(final_check.returncode, 0, final_check.stderr)

            missing_packet_path = output_root / "explicitly-missing-doctest-recapture.json"
            missing_packet_output = output_root / "missing-packet-output"
            explicitly_missing_packet = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(REPO_ROOT),
                    "--output-root",
                    str(missing_packet_output),
                    "--doctest-recapture-packet",
                    str(missing_packet_path),
                    "--without-unittest-recapture-packet",
                    "--write",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=180,
            )
            self.assertNotEqual(explicitly_missing_packet.returncode, 0)
            self.assertIn(
                "explicit --doctest-recapture-packet does not exist or is not a file",
                explicitly_missing_packet.stderr,
            )
            self.assertIn(str(missing_packet_path), explicitly_missing_packet.stderr)
            self.assertFalse(missing_packet_output.exists())

        after_v1_hashes = {
            V1_INVENTORY_PATH: hashlib.sha256((REPO_ROOT / V1_INVENTORY_PATH).read_bytes()).hexdigest(),
            V1_LEDGER_PATH: hashlib.sha256((REPO_ROOT / V1_LEDGER_PATH).read_bytes()).hexdigest(),
        }
        self.assertEqual(after_v1_hashes, before_v1_hashes)

    def test_real_cli_unittest_paths_fail_closed_before_writing(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-inventory-v2-unittest-closed-") as temp:
            stage = Path(temp)
            output_root = stage / "output"
            destination = output_root / V2_UNITTEST_RECAPTURE_PATH
            destination.parent.mkdir(parents=True)
            original = b"existing authenticated packet must survive"
            destination.write_bytes(original)
            (output_root / "unrelated-sentinel.bin").write_bytes(b"preserve every byte")

            def snapshot() -> dict[str, bytes]:
                return {
                    path.relative_to(output_root).as_posix(): path.read_bytes()
                    for path in sorted(output_root.rglob("*"))
                    if path.is_file()
                }

            original_tree = snapshot()

            recapture = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(REPO_ROOT),
                    "--output-root",
                    str(output_root),
                    "--without-doctest-recapture-packet",
                    "--recapture-unittests",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertNotEqual(recapture.returncode, 0)
            self.assertIn(UNITTEST_RECAPTURE_UNAVAILABLE, recapture.stderr)
            self.assertEqual(snapshot(), original_tree)

            missing_packet = stage / "missing-unittest-packet.json"
            missing_output = stage / "missing-output"
            missing = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(REPO_ROOT),
                    "--output-root",
                    str(missing_output),
                    "--without-doctest-recapture-packet",
                    "--unittest-recapture-packet",
                    str(missing_packet),
                    "--write",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn(
                "explicit --unittest-recapture-packet does not exist or is not a file",
                missing.stderr,
            )
            self.assertFalse(missing_output.exists())
            self.assertEqual(snapshot(), original_tree)

            explicit_packet = stage / "explicit-unittest-packet.json"
            explicit_packet.write_bytes(b"{}")
            explicit = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(REPO_ROOT),
                    "--output-root",
                    str(output_root),
                    "--without-doctest-recapture-packet",
                    "--unittest-recapture-packet",
                    str(explicit_packet),
                    "--write",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertNotEqual(explicit.returncode, 0)
            self.assertIn(UNITTEST_RECAPTURE_UNAVAILABLE, explicit.stderr)
            self.assertEqual(snapshot(), original_tree)

            saved_repo = stage / "saved-repo"
            saved_packet = saved_repo / V2_UNITTEST_RECAPTURE_PATH
            saved_packet.parent.mkdir(parents=True)
            saved_packet.write_bytes(b"saved packet must not be consumed")
            saved = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(saved_repo),
                    "--output-root",
                    str(output_root),
                    "--write",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertNotEqual(saved.returncode, 0)
            self.assertIn(UNITTEST_RECAPTURE_UNAVAILABLE, saved.stderr)
            self.assertEqual(snapshot(), original_tree)
            self.assertEqual(
                saved_packet.read_bytes(), b"saved packet must not be consumed"
            )

            with self.assertRaises(MaterializationError) as failure:
                build_materialized_bundle(REPO_ROOT, unittest_recapture_raw=b"{}")
            self.assertEqual(str(failure.exception), UNITTEST_RECAPTURE_UNAVAILABLE)
            self.assertEqual(snapshot(), original_tree)

    def test_manifest_feature_contexts_reject_missing_cargo_target(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-inventory-v2-manifest-missing-") as temp:
            repo_root = Path(temp)
            self._write_fixture_package(repo_root)
            self._write_rust_test_manifest(
                repo_root,
                """
[targets.missing]
package = "fixture"
test = "missing"
helpers = []
""",
            )
            with self.assertRaisesRegex(
                MaterializationError,
                "target 'missing' mapped to 0 Cargo targets",
            ):
                _manifest_feature_contexts(repo_root, _manifest_index(repo_root))

    def test_manifest_feature_contexts_reject_ambiguous_feature_selection(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-inventory-v2-manifest-ambiguous-") as temp:
            repo_root = Path(temp)
            self._write_fixture_package(repo_root)
            self._write_rust_test_manifest(
                repo_root,
                """
[targets.plain]
package = "fixture"
lib = true
helpers = []

[targets.featured]
package = "fixture"
lib = true
features = ["codex-core/completion-proof-test-store"]
helpers = []
""",
            )
            with self.assertRaisesRegex(
                MaterializationError,
                "ambiguous feature selections for Cargo target fixture::lib/fixture",
            ):
                _manifest_feature_contexts(repo_root, _manifest_index(repo_root))

    @staticmethod
    def _assert_unittest_recovery_partition(output_root: Path) -> None:
        inventory = json.loads((output_root / V2_INVENTORY_PATH).read_bytes())
        recovery = json.loads((output_root / V2_RECOVERY_PATH).read_bytes())
        ledger = json.loads((output_root / V2_LEDGER_PATH).read_bytes())
        unittest_ids = sorted(
            declaration["baseline_id"]
            for declaration in inventory["declaration_universe"]
            if declaration["kind"] == "frozen-baseline"
            and declaration["entry"]["runner_selector"]["kind"] == "python-unittest"
        )
        hidden_prefix = "hidden-at-freeze-v1::python-unittest::"
        hidden_replacement_ids = sorted(
            baseline_id
            for baseline_id in unittest_ids
            if baseline_id.startswith(hidden_prefix)
        )
        recapture_parent_ids = [
            baseline_id
            for baseline_id in unittest_ids
            if not baseline_id.startswith(hidden_prefix)
        ]
        if len(unittest_ids) != EXPECTED_UNITTEST_LEDGER_IDENTITY_COUNT:
            raise AssertionError(
                f"unexpected unittest ledger identity count: {len(unittest_ids)}"
            )
        if len(recapture_parent_ids) != EXPECTED_UNITTEST_RECAPTURE_PARENT_COUNT:
            raise AssertionError(
                f"unexpected unittest recapture-parent count: {len(recapture_parent_ids)}"
            )
        if len(hidden_replacement_ids) != EXPECTED_UNITTEST_HIDDEN_REPLACEMENT_COUNT:
            raise AssertionError(
                "unexpected hidden unittest replacement count: "
                f"{len(hidden_replacement_ids)}"
            )

        unittest_record = next(
            record for record in recovery["records"] if record["kind"] == "unittest"
        )
        pending = unittest_record["pending_requirement"]
        expected_output_parent_ids = sorted(
            item["parent_id"] for item in pending["expected_parent_output_sha256s"]
        )
        if pending["required_parent_count"] != EXPECTED_UNITTEST_RECAPTURE_PARENT_COUNT:
            raise AssertionError("unittest pending requirement has the wrong parent count")
        if pending["required_parent_ids"] != recapture_parent_ids:
            raise AssertionError("unittest pending requirement includes the wrong parents")
        if expected_output_parent_ids != recapture_parent_ids:
            raise AssertionError("unittest output hashes include the wrong parents")
        if unittest_record["resolution"] is not None:
            raise AssertionError("unavailable unittest recapture unexpectedly created children")
        if set(hidden_replacement_ids) & set(pending["required_parent_ids"]):
            raise AssertionError("hidden unittest replacements became recapture parents")

        ledger_by_baseline_id = {row["baseline_id"]: row for row in ledger["rows"]}
        for baseline_id in hidden_replacement_ids:
            disposition = ledger_by_baseline_id[baseline_id]["disposition"]
            if disposition["kind"] != "replacement":
                raise AssertionError(
                    f"hidden unittest identity is not a replacement: {baseline_id}"
                )
            replacement_ids = disposition["contract"]["legacy_replacement_hint"][
                "replacement_ids"
            ]
            expected_replacement_id = "python-unittest::" + baseline_id.removeprefix(
                hidden_prefix
            )
            if replacement_ids != [expected_replacement_id]:
                raise AssertionError(
                    f"hidden unittest identity has the wrong replacement: {baseline_id}"
                )

    @staticmethod
    def _write_fixture_package(repo_root: Path) -> None:
        package_root = repo_root / "codex-rs/fixture"
        (package_root / "src").mkdir(parents=True)
        (package_root / "src/lib.rs").write_text("pub fn fixture() {}\n", encoding="utf-8")
        (package_root / "Cargo.toml").write_text(
            """
[package]
name = "fixture"
version = "0.0.0"
edition = "2021"
""".lstrip(),
            encoding="utf-8",
        )

    @staticmethod
    def _write_rust_test_manifest(repo_root: Path, targets: str) -> None:
        path = repo_root / "codex-rs/.config/kd4-rust-tests.toml"
        path.parent.mkdir(parents=True)
        path.write_text(
            (
                "version = 1\n\n"
                "[helpers]\n\n"
                f"{targets.strip()}\n\n"
                "[gates.fixture]\n"
                "description = \"fixture\"\n\n"
                "[[gates.fixture.steps]]\n"
                "target = \"" + ("missing" if "targets.missing" in targets else "plain") + "\"\n"
                "tests = [\"fixture\"]\n"
            ),
            encoding="utf-8",
        )


if __name__ == "__main__":
    unittest.main()
