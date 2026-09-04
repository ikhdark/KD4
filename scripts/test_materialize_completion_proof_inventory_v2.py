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
from scripts.completion_proof_inventory_v2 import validate_test_replacement_ledger_v2
from scripts.materialize_completion_proof_inventory_v2 import EXPECTED_DECLARATION_COUNT
from scripts.materialize_completion_proof_inventory_v2 import SOURCE_ONLY_SPECS
from scripts.materialize_completion_proof_inventory_v2 import V1_INVENTORY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V1_LEDGER_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_INVENTORY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_LEDGER_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_RECOVERY_PATH
from scripts.materialize_completion_proof_inventory_v2 import MaterializationError
from scripts.materialize_completion_proof_inventory_v2 import _manifest_feature_contexts
from scripts.materialize_completion_proof_inventory_v2 import _manifest_index
from scripts.materialize_completion_proof_inventory_v2 import _write_or_check
from scripts.materialize_completion_proof_inventory_v2 import validate_recovery_source_anchors
from scripts.test_completion_proof_inventory_v2 import _validate_schema_instance


REPO_ROOT = Path(__file__).resolve().parents[1]
MATERIALIZER = REPO_ROOT / "scripts/materialize_completion_proof_inventory_v2.py"


class InventoryV2MaterializationTests(unittest.TestCase):
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
            self.assertEqual(
                (summary["intended_count"], summary["selected_count"], summary["executed_count"]),
                (EXPECTED_DECLARATION_COUNT,) * 3,
            )
            self.assertEqual(
                summary["counts"],
                {
                    "cargo_target_contexts": 182,
                    "frozen_baseline_declarations": 15_544,
                    "inventory_declarations": 15_547,
                    "ledger_rows": 15_547,
                    "recovery_records": 2,
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
                {"exception": 244, "replacement": 644, "unresolved": 14_659},
            )

            inventory_raw = (output_root / V2_INVENTORY_PATH).read_bytes()
            recovery_raw = (output_root / V2_RECOVERY_PATH).read_bytes()
            ledger_raw = (output_root / V2_LEDGER_PATH).read_bytes()
            inventory = json.loads(inventory_raw)
            recovery = json.loads(recovery_raw)
            ledger = json.loads(ledger_raw)
            self.assertEqual(inventory_raw, canonical_jcs(inventory))
            self.assertEqual(recovery_raw, canonical_jcs(recovery))
            self.assertEqual(ledger_raw, canonical_jcs(ledger))
            self.assertEqual(
                summary["artifact_sha256"],
                {
                    V2_INVENTORY_PATH: hashlib.sha256(inventory_raw).hexdigest(),
                    V2_RECOVERY_PATH: hashlib.sha256(recovery_raw).hexdigest(),
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
            validate_inventory_ledger_predecessor_closure(
                inventory, ledger, recovery_raw, [], issuer
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
            self.assertEqual([record["state"] for record in recovery["records"]], ["pending", "pending"])
            self.assertEqual(
                Counter(row["disposition"]["kind"] for row in ledger["rows"]),
                Counter({"unresolved": 14_659, "replacement": 644, "exception": 244}),
            )

            materialized_bundle = {
                "raw_documents": {
                    V2_INVENTORY_PATH: inventory_raw,
                    V2_RECOVERY_PATH: recovery_raw,
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

        after_v1_hashes = {
            V1_INVENTORY_PATH: hashlib.sha256((REPO_ROOT / V1_INVENTORY_PATH).read_bytes()).hexdigest(),
            V1_LEDGER_PATH: hashlib.sha256((REPO_ROOT / V1_LEDGER_PATH).read_bytes()).hexdigest(),
        }
        self.assertEqual(after_v1_hashes, before_v1_hashes)

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
