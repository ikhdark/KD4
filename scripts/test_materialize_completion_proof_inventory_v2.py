from __future__ import annotations

from collections import Counter
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts.completion_proof_inventory_v2 import ActiveHostApplicabilityIssuerV1
from scripts.completion_proof_inventory_v2 import FROZEN_V1_INVENTORY_RAW_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_LEDGER_RAW_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_PATHS
from scripts.completion_proof_inventory_v2 import canonical_jcs
from scripts.completion_proof_inventory_v2 import derive_frozen_v1_historical_replacement_graph_v1
from scripts.completion_proof_inventory_v2 import proof_hash
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
from scripts.materialize_completion_proof_inventory_v2 import POST_BASELINE_CURRENT_SPECS
from scripts.materialize_completion_proof_inventory_v2 import V1_INVENTORY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V1_LEDGER_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_DOCTEST_RECAPTURE_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_INVENTORY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_LEDGER_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_RECOVERY_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_RECOVERY_TRANSITION_RECEIPTS_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_UNITTEST_RECAPTURE_PATH
from scripts.materialize_completion_proof_inventory_v2 import V2_UNITTEST_SOURCE_EXCEPTIONS_PATH
from scripts.materialize_completion_proof_inventory_v2 import UNITTEST_RECAPTURE_UNAVAILABLE
from scripts.materialize_completion_proof_inventory_v2 import MaterializationError
from scripts.materialize_completion_proof_inventory_v2 import _jest_selector_index
from scripts.materialize_completion_proof_inventory_v2 import _manifest_feature_contexts
from scripts.materialize_completion_proof_inventory_v2 import _manifest_index
from scripts.materialize_completion_proof_inventory_v2 import _run_jest_observer
from scripts.materialize_completion_proof_inventory_v2 import _write_or_check
from scripts.materialize_completion_proof_inventory_v2 import build_materialized_bundle
from scripts.materialize_completion_proof_inventory_v2 import validate_recovery_source_anchors
from scripts.test_completion_proof_inventory_v2 import _validate_schema_instance


REPO_ROOT = Path(__file__).resolve().parents[1]
MATERIALIZER = REPO_ROOT / "scripts/materialize_completion_proof_inventory_v2.py"
APPROVED_UNITTEST_RECAPTURE_PACKET = REPO_ROOT / V2_UNITTEST_RECAPTURE_PATH


def _valid_jest_observation() -> dict[str, object]:
    return {
        "ancestor_titles": ["AbortSignal support"],
        "column": 5,
        "config_path": "sdk/typescript/jest.config.cjs",
        "file_path": "sdk/typescript/tests/abort.test.ts",
        "full_title": (
            "AbortSignal support aborts run() when signal is aborted during execution"
        ),
        "line": 82,
        "registration_ordinal": 0,
    }


def _valid_jest_report() -> dict[str, object]:
    return {
        "complete": True,
        "num_failed_tests": 0,
        "num_passed_tests": 0,
        "num_pending_tests": 1,
        "num_total_tests": 1,
        "observations": [_valid_jest_observation()],
        "schema_version": 1,
    }


class InventoryV2MaterializationTests(unittest.TestCase):
    def test_jest_selector_index_observes_real_sdk_config_without_running_tests(
        self,
    ) -> None:
        selectors = _jest_selector_index(REPO_ROOT)

        self.assertEqual(len(selectors), 45)
        self.assertEqual(
            selectors[
                "sdk/typescript/tests/abort.test.ts::AbortSignal support aborts run() "
                "when signal is aborted during execution"
            ],
            {"kind": "javascript-jest", **_valid_jest_observation()},
        )

    def test_jest_observer_uses_real_config_and_temporary_circus_extensions(
        self,
    ) -> None:
        observed_command: list[str] = []

        def complete_observation(
            command: list[str], **kwargs: object
        ) -> subprocess.CompletedProcess[str]:
            observed_command.extend(command)
            environment_path = Path(command[command.index("--env") + 1])
            reporter_path = Path(command[-1])
            self.assertTrue(environment_path.is_file())
            self.assertTrue(reporter_path.is_file())
            environment_source = environment_path.read_text(encoding="utf-8")
            self.assertIn('event.name === "test_fn_start"', environment_source)
            self.assertIn('event.name === "hook_start"', environment_source)
            reporter_path.with_name("report.json").write_text(
                json.dumps(_valid_jest_report()),
                encoding="utf-8",
            )
            self.assertEqual(kwargs["cwd"], (REPO_ROOT / "sdk/typescript").resolve())
            self.assertEqual(kwargs["encoding"], "utf-8")
            self.assertEqual(kwargs["errors"], "replace")
            self.assertEqual(kwargs["timeout"], 180)
            environment = kwargs["env"]
            self.assertIsInstance(environment, dict)
            assert isinstance(environment, dict)
            self.assertEqual(environment["CI"], "1")
            self.assertEqual(environment["RUN_REAL_CODEX_TESTS"], "0")
            return subprocess.CompletedProcess(command, 0, "", "")

        with mock.patch(
            "scripts.materialize_completion_proof_inventory_v2.subprocess.run",
            side_effect=complete_observation,
        ):
            selectors = _jest_selector_index(REPO_ROOT)

        self.assertEqual(
            observed_command[:4],
            [
                "node",
                str(REPO_ROOT / "node_modules/jest/bin/jest.js"),
                "--config",
                str(REPO_ROOT / "sdk/typescript/jest.config.cjs"),
            ],
        )
        self.assertIn("--runInBand", observed_command)
        self.assertIn("--no-cache", observed_command)
        self.assertIn("default", observed_command)
        self.assertEqual(
            selectors,
            {
                "sdk/typescript/tests/abort.test.ts::AbortSignal support aborts run() "
                "when signal is aborted during execution": {
                    "kind": "javascript-jest",
                    **_valid_jest_observation(),
                }
            },
        )

    def test_jest_selector_index_rejects_nonfinal_or_malformed_observations(
        self,
    ) -> None:
        cases: list[tuple[str, dict[str, object], str]] = []

        missing_report_field = _valid_jest_report()
        del missing_report_field["complete"]
        cases.append(("missing report field", missing_report_field, "fields mismatch"))

        extra_report_field = _valid_jest_report()
        extra_report_field["extra"] = True
        cases.append(("extra report field", extra_report_field, "fields mismatch"))

        nonfinal = _valid_jest_report()
        nonfinal["complete"] = False
        cases.append(("nonfinal", nonfinal, "report is not finalized"))

        boolean_schema_version = _valid_jest_report()
        boolean_schema_version["schema_version"] = True
        cases.append(
            (
                "boolean schema version",
                boolean_schema_version,
                "report is not finalized",
            )
        )

        float_schema_version = _valid_jest_report()
        float_schema_version["schema_version"] = 1.0
        cases.append(
            ("float schema version", float_schema_version, "report is not finalized")
        )

        string_count = _valid_jest_report()
        string_count["num_failed_tests"] = "0"
        cases.append(("string count", string_count, "num_failed_tests is invalid"))

        boolean_count = _valid_jest_report()
        boolean_count["num_total_tests"] = True
        cases.append(("boolean count", boolean_count, "num_total_tests is invalid"))

        float_count = _valid_jest_report()
        float_count["num_passed_tests"] = 0.0
        cases.append(("float count", float_count, "num_passed_tests is invalid"))

        negative_count = _valid_jest_report()
        negative_count["num_pending_tests"] = -1
        cases.append(("negative count", negative_count, "num_pending_tests is invalid"))

        zero_tests = _valid_jest_report()
        zero_tests["num_pending_tests"] = 0
        zero_tests["num_total_tests"] = 0
        zero_tests["observations"] = []
        cases.append(("zero tests", zero_tests, "discovered no tests"))

        missing_field = _valid_jest_report()
        del missing_field["observations"][0]["column"]  # type: ignore[index]
        cases.append(("missing field", missing_field, "fields mismatch"))

        extra_field = _valid_jest_report()
        extra_field["observations"][0]["extra"] = True  # type: ignore[index]
        cases.append(("extra field", extra_field, "fields mismatch"))

        invalid_line = _valid_jest_report()
        invalid_line["observations"][0]["line"] = True  # type: ignore[index]
        cases.append(("invalid line", invalid_line, "violates RunnerSelectorV1"))

        wrong_config = _valid_jest_report()
        wrong_config["observations"][0][  # type: ignore[index]
            "config_path"
        ] = "jest.config.cjs"
        cases.append(("wrong config", wrong_config, "unexpected config_path"))

        outside_repo = _valid_jest_report()
        outside_repo["observations"][0][  # type: ignore[index]
            "file_path"
        ] = "../outside.test.ts"
        cases.append(
            ("outside repository", outside_repo, "violates RunnerSelectorV1")
        )

        nonexistent_source = _valid_jest_report()
        nonexistent_source["observations"][0][  # type: ignore[index]
            "file_path"
        ] = "sdk/typescript/tests/missing.test.ts"
        cases.append(
            ("nonexistent source", nonexistent_source, "source does not exist")
        )

        count_mismatch = _valid_jest_report()
        count_mismatch["num_total_tests"] = 2
        count_mismatch["num_pending_tests"] = 2
        cases.append(("count mismatch", count_mismatch, "count mismatch"))

        executed_test = _valid_jest_report()
        executed_test["num_pending_tests"] = 0
        executed_test["num_passed_tests"] = 1
        cases.append(("executed test", executed_test, "executed a test body or hook"))

        duplicate = _valid_jest_report()
        duplicate["observations"] = [
            _valid_jest_observation(),
            _valid_jest_observation(),
        ]
        duplicate["num_total_tests"] = 2
        duplicate["num_pending_tests"] = 2
        cases.append(("duplicate", duplicate, "duplicate Jest selector"))

        ambiguous = _valid_jest_report()
        second_observation = _valid_jest_observation()
        second_observation["registration_ordinal"] = 1
        ambiguous["observations"] = [_valid_jest_observation(), second_observation]
        ambiguous["num_total_tests"] = 2
        ambiguous["num_pending_tests"] = 2
        cases.append(("ambiguous", ambiguous, "ambiguous Jest selector"))

        for name, report, expected_error in cases:
            with self.subTest(name=name), mock.patch(
                "scripts.materialize_completion_proof_inventory_v2._run_jest_observer",
                return_value=report,
            ), self.assertRaisesRegex(MaterializationError, expected_error):
                _jest_selector_index(REPO_ROOT)

    def test_jest_observer_process_failures_are_closed_and_bounded(self) -> None:
        diagnostic = "diagnostic-tail"
        with mock.patch(
            "scripts.materialize_completion_proof_inventory_v2.subprocess.run",
            return_value=subprocess.CompletedProcess(
                [], 2, "", f"{'x' * 8_000}{diagnostic}"
            ),
        ), self.assertRaises(MaterializationError) as failure:
            _run_jest_observer(REPO_ROOT)
        self.assertIn("exit code 2", str(failure.exception))
        self.assertIn(diagnostic, str(failure.exception))
        self.assertLess(len(str(failure.exception)), 4_200)

        timeout_diagnostic = "timeout-tail"
        with mock.patch(
            "scripts.materialize_completion_proof_inventory_v2.subprocess.run",
            side_effect=subprocess.TimeoutExpired(
                ["node", "jest"],
                180,
                stderr=f"{'y' * 8_000}{timeout_diagnostic}",
            ),
        ), self.assertRaises(MaterializationError) as timeout_failure:
            _run_jest_observer(REPO_ROOT)
        self.assertIn("timed out after 180 seconds", str(timeout_failure.exception))
        self.assertIn(timeout_diagnostic, str(timeout_failure.exception))
        self.assertLess(len(str(timeout_failure.exception)), 4_200)

        with mock.patch(
            "scripts.materialize_completion_proof_inventory_v2.subprocess.run",
            return_value=subprocess.CompletedProcess([], 0, "", ""),
        ), self.assertRaisesRegex(MaterializationError, "without a finalized report"):
            _run_jest_observer(REPO_ROOT)

    def test_jest_observer_rejects_invalid_json_and_nonobject_reports(self) -> None:
        cases = (
            ("invalid JSON", "{", "not valid JSON"),
            ("nonobject JSON", "[]", "must be an object"),
        )
        for name, report_content, expected_error in cases:
            def write_report(
                command: list[str], **_kwargs: object
            ) -> subprocess.CompletedProcess[str]:
                Path(command[-1]).with_name("report.json").write_text(
                    report_content,
                    encoding="utf-8",
                )
                return subprocess.CompletedProcess(command, 0, "", "")

            with self.subTest(name=name), mock.patch(
                "scripts.materialize_completion_proof_inventory_v2.subprocess.run",
                side_effect=write_report,
            ), self.assertRaisesRegex(MaterializationError, expected_error):
                _run_jest_observer(REPO_ROOT)

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
                {
                    "current": 1,
                    "exception": 244,
                    "replacement": 644,
                    "unresolved": 14_659,
                },
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

    def test_real_cli_rejects_nonregular_output_before_changing_bundle(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="kd4-inventory-v2-nonregular-output-"
        ) as temp:
            output_root = Path(temp)
            sentinels = {
                V2_INVENTORY_PATH: b"preserve inventory bytes",
                V2_RECOVERY_TRANSITION_RECEIPTS_PATH: b"preserve receipt bytes",
                V2_LEDGER_PATH: b"preserve ledger bytes",
                ".codex/config.toml": b"preserve unrelated config bytes",
            }
            for relative, raw in sentinels.items():
                destination = output_root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(raw)
            recovery_directory = output_root / V2_RECOVERY_PATH
            recovery_directory.mkdir(parents=True)
            recovery_sentinel = recovery_directory / "sentinel.bin"
            recovery_sentinel.write_bytes(b"preserve directory contents")

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

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(
                hashlib.sha256(
                    (output_root / V2_INVENTORY_PATH).read_bytes()
                ).hexdigest(),
                hashlib.sha256(sentinels[V2_INVENTORY_PATH]).hexdigest(),
                V2_INVENTORY_PATH,
            )
            for relative, raw in sentinels.items():
                self.assertTrue((output_root / relative).read_bytes() == raw, relative)
            self.assertTrue(recovery_directory.is_dir())
            self.assertTrue(
                recovery_sentinel.read_bytes() == b"preserve directory contents"
            )
            self.assertEqual(result.stdout, "")
            self.assertIn(Path(V2_RECOVERY_PATH).name, result.stderr)

    def test_real_cli_staging_failure_preserves_existing_bundle(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="kd4-inventory-v2-staging-failure-"
        ) as temp:
            output_root = Path(temp)
            sentinels = {
                V2_INVENTORY_PATH: b"preserve inventory bytes",
                V2_RECOVERY_PATH: b"preserve recovery bytes",
                V2_RECOVERY_TRANSITION_RECEIPTS_PATH: b"preserve receipt bytes",
                V2_LEDGER_PATH: b"preserve ledger bytes",
                ".codex/config.toml": b"preserve unrelated config bytes",
            }
            for relative, raw in sentinels.items():
                destination = output_root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(raw)

            def tree_paths() -> list[tuple[str, bool]]:
                return [
                    (path.relative_to(output_root).as_posix(), path.is_dir())
                    for path in sorted(output_root.rglob("*"))
                ]

            original_paths = tree_paths()
            wrapper = """
import errno
import runpy
import sys
from unittest import mock

materializer = sys.argv[1]
sys.argv = [materializer, *sys.argv[2:]]
with mock.patch(
    "os.fsync",
    side_effect=OSError(errno.ENOSPC, "simulated staging disk full"),
):
    runpy.run_path(materializer, run_name="__main__")
"""
            result = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    wrapper,
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

            self.assertNotEqual(result.returncode, 0)
            for relative, raw in sentinels.items():
                self.assertTrue((output_root / relative).read_bytes() == raw, relative)
            self.assertEqual(tree_paths(), original_paths)
            self.assertEqual(result.stdout, "")
            self.assertIn("simulated staging disk full", result.stderr)

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
                    "inventory_declarations": 15_548,
                    "ledger_rows": 15_548,
                    "post_baseline_current_declarations": 1,
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
                    "python-unittest": 910,
                    "rust-doctest": 5,
                    "rust-nextest": 14_329,
                    "windows-sandbox-smoke-native": 46,
                },
            )
            self.assertEqual(
                summary["disposition_counts"],
                {
                    "current": 1,
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
            predecessor_ledger_raw = (REPO_ROOT / V1_LEDGER_PATH).read_bytes()
            predecessor_ledger = json.loads(predecessor_ledger_raw)
            historical_graph = derive_frozen_v1_historical_replacement_graph_v1(
                predecessor_ledger
            )
            self.assertEqual(
                (
                    len(historical_graph["baseline_ids"]),
                    len(historical_graph["edges"]),
                    len(historical_graph["successor_ids"]),
                    len(historical_graph["components"]),
                ),
                (644, 685, 572, 531),
            )
            self.assertEqual(
                proof_hash(
                    "kd4.frozen-v1-historical-replacement-graph.v1",
                    historical_graph,
                ),
                FROZEN_V1_HISTORICAL_REPLACEMENT_GRAPH_SHA256,
            )
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
                    V2_UNITTEST_SOURCE_EXCEPTIONS_PATH: hashlib.sha256(
                        (REPO_ROOT / V2_UNITTEST_SOURCE_EXCEPTIONS_PATH).read_bytes()
                    ).hexdigest(),
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
                None,
                predecessor_ledger_raw,
            )
            self._assert_frozen_parent_dispositions(
                ledger, recovery, predecessor_ledger
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
            declarations_by_baseline_id = {
                item["baseline_id"]: item
                for item in declarations
                if item["kind"] == "frozen-baseline"
            }
            contracts_by_sha256 = {
                contract["contract_sha256"]: contract
                for contract in inventory["execution_input_contracts"]
            }
            argument_comment_lint_contract_rows = sorted(
                (
                    {
                        "baseline_id": row["baseline_id"],
                        "consumed": contracts_by_sha256[
                            declarations_by_baseline_id[row["baseline_id"]]["entry"][
                                "execution_input_contract_sha256"
                            ]
                        ]["consumed"],
                        "source": row["source"],
                    }
                    for row in frozen_inventory["tests"]
                    if row["source"]
                    in {
                        "tools/argument-comment-lint/ui/comment_mismatch.rs",
                        "tools/argument-comment-lint/ui/multiple_method_arguments.rs",
                        "tools/argument-comment-lint/ui/uncommented_literal.rs",
                    }
                ),
                key=lambda row: row["baseline_id"],
            )
            self.assertEqual(
                argument_comment_lint_contract_rows,
                [
                    {
                        "baseline_id": (
                            "hidden-at-freeze-v1::argument-comment-lint::dylint-ui::"
                            "comment_mismatch"
                        ),
                        "consumed": [
                            {
                                "kind": "exact",
                                "path": (
                                    "tools/argument-comment-lint/ui/"
                                    "comment_mismatch.stderr"
                                ),
                            }
                        ],
                        "source": "tools/argument-comment-lint/ui/comment_mismatch.rs",
                    },
                    {
                        "baseline_id": (
                            "hidden-at-freeze-v1::argument-comment-lint::dylint-ui::"
                            "multiple_method_arguments"
                        ),
                        "consumed": [
                            {
                                "kind": "exact",
                                "path": (
                                    "tools/argument-comment-lint/ui/"
                                    "multiple_method_arguments.stderr"
                                ),
                            }
                        ],
                        "source": (
                            "tools/argument-comment-lint/ui/"
                            "multiple_method_arguments.rs"
                        ),
                    },
                    {
                        "baseline_id": (
                            "hidden-at-freeze-v1::argument-comment-lint::dylint-ui::"
                            "uncommented_literal"
                        ),
                        "consumed": [
                            {
                                "kind": "exact",
                                "path": (
                                    "tools/argument-comment-lint/ui/"
                                    "uncommented_literal.stderr"
                                ),
                            }
                        ],
                        "source": (
                            "tools/argument-comment-lint/ui/uncommented_literal.rs"
                        ),
                    },
                ],
            )
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
            post_baseline_current = [
                item for item in declarations if item["kind"] == "post-baseline-current"
            ]
            self.assertEqual(len(post_baseline_current), 1)
            current_declaration = post_baseline_current[0]
            current_spec = POST_BASELINE_CURRENT_SPECS[0]
            self.assertEqual(
                current_declaration["entry"]["executable_identity"]["test_id"],
                current_spec["canonical_id"],
            )
            self.assertEqual(
                current_declaration["source_provenance"]["kind"],
                "source-declaration",
            )
            current_entry = current_declaration["entry"]
            self.assertIsNone(current_entry["cargo_target_context_spec_sha256"])
            self.assertEqual(
                (current_entry["test_route_id"], current_entry["validation_id"]),
                ("test-route.python-unittest.v1", "maintenance.root-unittest"),
            )
            self.assertEqual(
                current_entry["platform_applicability"],
                {"kind": "host-set", "required_hosts": ["darwin", "linux", "windows"]},
            )
            expected_manifest_sha256 = proof_hash(
                "kd4.python-unittest-subtest-manifest.source-declared.v1",
                {
                    "declared_subtests": [],
                    "parent_test_id": current_spec["native_id"],
                    "source_path": current_spec["source_path"],
                },
            )
            self.assertEqual(
                current_entry["runner_selector"],
                {
                    "kind": "python-unittest",
                    "parent_test_id": current_spec["native_id"],
                    "selection_unit": "parent-with-all-declared-subtests",
                    "subtest_manifest_sha256": expected_manifest_sha256,
                },
            )
            self.assertEqual(
                contracts_by_sha256[current_entry["execution_input_contract_sha256"]],
                {
                    "consumed": [],
                    "contract_id": (
                        "execution-input-contract-v1."
                        + current_entry["execution_input_contract_sha256"]
                    ),
                    "contract_sha256": current_entry["execution_input_contract_sha256"],
                    "owned": [{"kind": "exact", "path": current_spec["source_path"]}],
                    "schema_version": 1,
                },
            )
            current_rows = [
                row
                for row in ledger["rows"]
                if row["obligation_id"] == current_declaration["obligation_id"]
            ]
            self.assertEqual(
                current_rows,
                [
                    {
                        "baseline_id": None,
                        "disposition": {
                            "inventory_entry_semantic_sha256": proof_hash(
                                "kd4.executable-inventory-entry.v2", current_entry
                            ),
                            "kind": "current",
                        },
                        "obligation_id": current_declaration["obligation_id"],
                    }
                ],
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
                        "current": 1,
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
                    V2_UNITTEST_SOURCE_EXCEPTIONS_PATH: (REPO_ROOT / V2_UNITTEST_SOURCE_EXCEPTIONS_PATH).read_bytes(),
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

    def test_real_cli_materializes_authenticated_unittest_recovery_bundle(
        self,
    ) -> None:
        unittest_recapture_raw = APPROVED_UNITTEST_RECAPTURE_PACKET.read_bytes()
        unittest_recapture = json.loads(unittest_recapture_raw)
        doctest_recapture_raw = (REPO_ROOT / V2_DOCTEST_RECAPTURE_PATH).read_bytes()
        predecessor_ledger_raw = (REPO_ROOT / V1_LEDGER_PATH).read_bytes()
        predecessor_ledger = json.loads(predecessor_ledger_raw)

        self.assertEqual(unittest_recapture_raw, canonical_jcs(unittest_recapture))
        self.assertEqual(
            unittest_recapture["total_counts"],
            {
                "method_body_child_count": 859,
                "parent_record_count": 893,
                "recovered_child_count": 1_228,
                "selected_parent_count": 859,
                "started_parent_count": 859,
                "subtest_occurrence_count": 369,
                "terminal_parent_count": 859,
            },
        )
        self.assertTrue(
            all(
                result["terminal_result"] == "passed"
                for result in unittest_recapture["parent_results"]
            )
        )
        source_exception = unittest_recapture["source_provenance_exception"]
        self.assertEqual(len(source_exception["baseline_ids"]), 5)
        self.assertEqual(
            len(
                source_exception["historical_execution_extension"]["baseline_ids"]
            ),
            29,
        )

        with tempfile.TemporaryDirectory(
            prefix="kd4-inventory-v2-unittest-recovery-"
        ) as temp:
            stage = Path(temp)
            output_root = stage / "output"
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
                    "--unittest-recapture-packet",
                    str(APPROVED_UNITTEST_RECAPTURE_PACKET),
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
                summary["mode"],
                "unittest-and-doctest-recovery-materialization",
            )
            self.assertEqual(summary["executed_test_count"], 0)
            self.assertEqual(
                summary["counts"],
                {
                    "cargo_target_contexts": 182,
                    "frozen_baseline_declarations": 15_544,
                    "inventory_declarations": 15_548,
                    "ledger_rows": 16_776,
                    "post_baseline_current_declarations": 1,
                    "recovery_records": 2,
                    "recovery_transition_receipts": 2,
                    "source_only_declarations": 3,
                },
            )
            self.assertEqual(
                summary["disposition_counts"],
                {
                    "current": 1,
                    "exception": 244,
                    "recovered-container": 5,
                    "replacement": 644,
                    "unresolved": 15_882,
                },
            )

            raw_documents = {
                path: (output_root / path).read_bytes()
                for path in (
                    V2_INVENTORY_PATH,
                    V2_RECOVERY_PATH,
                    V2_RECOVERY_TRANSITION_RECEIPTS_PATH,
                    V2_DOCTEST_RECAPTURE_PATH,
                    V2_UNITTEST_RECAPTURE_PATH,
                    V2_UNITTEST_SOURCE_EXCEPTIONS_PATH,
                    V2_LEDGER_PATH,
                )
            }
            self.assertEqual(
                summary["artifact_sha256"],
                {
                    path: hashlib.sha256(raw).hexdigest()
                    for path, raw in raw_documents.items()
                },
            )
            self.assertEqual(
                raw_documents[V2_UNITTEST_RECAPTURE_PATH], unittest_recapture_raw
            )
            self.assertEqual(
                raw_documents[V2_DOCTEST_RECAPTURE_PATH], doctest_recapture_raw
            )
            for path, raw in raw_documents.items():
                self.assertEqual(raw, canonical_jcs(json.loads(raw)), path)

            inventory = json.loads(raw_documents[V2_INVENTORY_PATH])
            recovery = json.loads(raw_documents[V2_RECOVERY_PATH])
            transition_receipts = json.loads(
                raw_documents[V2_RECOVERY_TRANSITION_RECEIPTS_PATH]
            )
            ledger = json.loads(raw_documents[V2_LEDGER_PATH])
            validate_frozen_test_inventory_v2(inventory)
            validate_inventory_recovery_authority_v1(recovery)
            validate_test_replacement_ledger_v2(ledger)
            self.assertEqual(
                [(record["kind"], record["state"]) for record in recovery["records"]],
                [("doctest", "resolved"), ("unittest", "resolved")],
            )
            self.assertEqual(len(transition_receipts), 2)
            receipts_by_hash = {}
            for receipt in transition_receipts:
                validate_recovery_transition_receipt_v1(receipt)
                receipts_by_hash[receipt["receipt_sha256"]] = receipt
            for record in recovery["records"]:
                receipt = receipts_by_hash[record["transition_receipt_sha256"]]
                self.assertEqual(
                    receipt["recapture_receipt_sha256"],
                    record["resolution"]["recapture_receipt_sha256"],
                )

            issuer = ActiveHostApplicabilityIssuerV1(
                "39bc53cc-ec47-4ea4-a940-b9a874779c30",
                hashlib.sha256(
                    b"kd4.inventory-v2-a2.dormant-validation-only.v1"
                ).digest(),
            )
            validate_inventory_ledger_predecessor_closure(
                inventory,
                ledger,
                raw_documents[V2_RECOVERY_PATH],
                transition_receipts,
                issuer,
                doctest_recapture_raw,
                unittest_recapture_raw,
                predecessor_ledger_raw,
            )
            self._assert_frozen_parent_dispositions(
                ledger, recovery, predecessor_ledger
            )

            declarations_by_baseline_id = {
                declaration["baseline_id"]: declaration
                for declaration in inventory["declaration_universe"]
                if declaration["kind"] == "frozen-baseline"
            }
            authenticated_manifests = {
                manifest["parent_baseline_id"]: manifest["manifest_sha256"]
                for manifest in unittest_recapture["parent_manifests"]
            }
            self.assertEqual(len(authenticated_manifests), 859)
            for baseline_id, manifest_sha256 in authenticated_manifests.items():
                selector = declarations_by_baseline_id[baseline_id]["entry"][
                    "runner_selector"
                ]
                self.assertEqual(
                    selector["subtest_manifest_sha256"], manifest_sha256
                )
                self.assertEqual(
                    declarations_by_baseline_id[baseline_id]["entry"][
                        "runner_selector_sha256"
                    ],
                    proof_hash("kd4.runner-selector.v1", selector),
                )

            unittest_record = next(
                record for record in recovery["records"] if record["kind"] == "unittest"
            )
            unittest_receipt = receipts_by_hash[
                unittest_record["transition_receipt_sha256"]
            ]
            self.assertEqual(
                len(unittest_record["resolution"]["child_sources"]), 1_228
            )
            self.assertEqual(len(unittest_receipt["child_obligation_ids"]), 1_228)
            ledger_by_obligation_id = {
                row["obligation_id"]: row for row in ledger["rows"]
            }
            for obligation_id in unittest_receipt["child_obligation_ids"]:
                child_row = ledger_by_obligation_id[obligation_id]
                self.assertIsNone(child_row["baseline_id"])
                self.assertEqual(child_row["disposition"], {"kind": "unresolved"})

            def output_snapshot() -> dict[str, str]:
                return {
                    path.relative_to(output_root).as_posix(): hashlib.sha256(
                        path.read_bytes()
                    ).hexdigest()
                    for path in sorted(output_root.rglob("*"))
                    if path.is_file()
                }

            original_output = output_snapshot()
            unittest_recapture["parent_manifests"][0]["manifest_sha256"] = "0" * 64
            wrong_packet = stage / "wrong-unittest-recapture.json"
            wrong_packet.write_bytes(canonical_jcs(unittest_recapture))
            rejected = subprocess.run(
                [
                    sys.executable,
                    str(MATERIALIZER),
                    "--repo-root",
                    str(REPO_ROOT),
                    "--output-root",
                    str(output_root),
                    "--doctest-recapture-packet",
                    str(REPO_ROOT / V2_DOCTEST_RECAPTURE_PATH),
                    "--unittest-recapture-packet",
                    str(wrong_packet),
                    "--write",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertNotEqual(rejected.returncode, 0)
            self.assertEqual(rejected.stdout, "")
            self.assertIn("unittest recapture artifact is invalid", rejected.stderr)
            self.assertEqual(output_snapshot(), original_output)

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
            self.assertIn("unittest recapture artifact is invalid", explicit.stderr)
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
            self.assertIn("unittest recapture artifact is invalid JSON", saved.stderr)
            self.assertEqual(snapshot(), original_tree)
            self.assertEqual(
                saved_packet.read_bytes(), b"saved packet must not be consumed"
            )

            with self.assertRaises(MaterializationError) as failure:
                build_materialized_bundle(REPO_ROOT, unittest_recapture_raw=b"{}")
            self.assertIn("unittest recapture artifact is invalid", str(failure.exception))
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

    def _assert_frozen_parent_dispositions(
        self,
        ledger: dict[str, object],
        recovery: dict[str, object],
        predecessor_ledger: dict[str, object],
    ) -> None:
        ledger_rows = ledger["rows"]
        predecessor_rows = predecessor_ledger["rows"]
        assert isinstance(ledger_rows, list)
        assert isinstance(predecessor_rows, list)
        rows_by_baseline_id = {
            row["baseline_id"]: row
            for row in ledger_rows
            if row["baseline_id"] is not None
        }
        expected_baseline_ids = {row["baseline_id"] for row in predecessor_rows}
        self.assertEqual(set(rows_by_baseline_id), expected_baseline_ids)

        recovery_records = recovery["records"]
        assert isinstance(recovery_records, list)
        doctest_record = next(
            record for record in recovery_records if record["kind"] == "doctest"
        )
        recovered_doctest_parents = (
            {
                child["parent_baseline_id"]
                for child in doctest_record["resolution"]["child_sources"]
            }
            if doctest_record["state"] == "resolved"
            else set()
        )
        for predecessor_row in predecessor_rows:
            baseline_id = predecessor_row["baseline_id"]
            disposition = rows_by_baseline_id[baseline_id]["disposition"]
            expected_kind = (
                "recovered-container"
                if baseline_id in recovered_doctest_parents
                else predecessor_row["resolution"]
            )
            self.assertEqual(disposition["kind"], expected_kind, baseline_id)
            if expected_kind == "replacement":
                self.assertEqual(
                    disposition["contract"]["legacy_replacement_hint"][
                        "replacement_ids"
                    ],
                    sorted(predecessor_row["replacement_ids"]),
                    baseline_id,
                )
            elif expected_kind == "exception":
                self.assertEqual(
                    disposition["exception"]["tag"],
                    predecessor_row["provenance"]["kind"],
                    baseline_id,
                )

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
