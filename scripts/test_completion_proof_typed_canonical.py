from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import platform
import subprocess
import sys
import tempfile
import textwrap
import unittest
import uuid
from pathlib import Path
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[1]
RUNNER = REPO_ROOT / "scripts" / "completion_proof.py"
MODULE_NAME = "_kd4_completion_proof_typed_canonical_under_test"
MODULE_SPEC = importlib.util.spec_from_file_location(MODULE_NAME, RUNNER)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError(f"cannot import completion-proof runner from {RUNNER}")
COMPLETION_PROOF = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_NAME] = COMPLETION_PROOF
MODULE_SPEC.loader.exec_module(COMPLETION_PROOF)


def _canonical_json(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def _inventory_hash(rows: list[dict[str, object]]) -> str:
    normalized = [
        {
            "baseline_id": str(row["baseline_id"]),
            "framework": str(row["framework"]),
            "native_id": str(row["native_id"]),
            "source": str(row["source"]),
            "ignored": bool(row.get("ignored", False)),
            "platforms": sorted(str(value) for value in row.get("platforms", [])),
        }
        for row in rows
    ]
    normalized.sort(key=lambda row: row["baseline_id"])
    return hashlib.sha256(
        _canonical_json({"schema_version": 1, "tests": normalized})
    ).hexdigest()


class CanonicalTypedJournalIntegrationTest(unittest.TestCase):
    def test_canonical_attempt_consumes_real_broker_journal(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="completion-proof-typed-canonical-"
        ) as temp_name:
            base = Path(temp_name)
            repository = base / "repository"
            validation_dir = repository / ".codex" / "validation"
            validation_dir.mkdir(parents=True)
            marker = base / "validator-observations.ndjson"
            validator = repository / "validator.py"
            validator.write_text(
                textwrap.dedent(
                    """\
                    import json
                    import os
                    import pathlib
                    import sys
                    import uuid

                    with pathlib.Path(sys.argv[1]).open(
                        "a", encoding="utf-8", newline="\\n"
                    ) as stream:
                        stream.write(
                            json.dumps(
                                {
                                    "arguments": sys.argv[2:],
                                    "execution_id": str(uuid.uuid4()),
                                    "pid": os.getpid(),
                                    "proof_environment": sorted(
                                        name
                                        for name in os.environ
                                        if name.casefold().startswith(
                                            "codex_completion_proof_"
                                        )
                                    ),
                                }
                            )
                            + "\\n"
                        )
                    """
                ),
                encoding="utf-8",
            )

            baseline_rows = [
                {
                    "baseline_id": "fixture-command::old-runtime-path",
                    "framework": "fixture-command",
                    "native_id": "old-runtime-path",
                    "source": "validator.py",
                    "ignored": False,
                    "platforms": [platform.system().casefold()],
                }
            ]
            current_rows = [
                {
                    "baseline_id": "fixture-command::typed-canonical-runtime-path",
                    "framework": "fixture-command",
                    "native_id": "typed-canonical-runtime-path",
                    "source": "validator.py",
                    "ignored": False,
                    "platforms": [platform.system().casefold()],
                }
            ]
            inventory_digest = _inventory_hash(baseline_rows)
            frozen_inventory = validation_dir / "frozen.json"
            replacement_ledger = validation_dir / "ledger.json"
            current_inventory = validation_dir / "current.json"
            config = validation_dir / "completion-proof.toml"
            frozen_inventory.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "baseline_commit": "fixture",
                        "baseline_workspace_fingerprint": "fixture",
                        "host_platform": platform.system().casefold(),
                        "inventory_hash": inventory_digest,
                        "tests": baseline_rows,
                    },
                    indent=2,
                    sort_keys=True,
                )
                + "\n",
                encoding="utf-8",
            )
            replacement_ledger.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "frozen_inventory_hash": inventory_digest,
                        "rows": [
                            {
                                "baseline_id": "fixture-command::old-runtime-path",
                                "resolution": "replacement",
                                "replacement_ids": [
                                    "fixture-command::typed-canonical-runtime-path"
                                ],
                                "preserved_behavior": (
                                    "canonical dispatch launches the typed validator"
                                ),
                                "product_path": "completion-proof canonical dispatch",
                                "validation_id": "fixture.typed-canonical",
                            }
                        ],
                        "overrides": [],
                    },
                    indent=2,
                    sort_keys=True,
                )
                + "\n",
                encoding="utf-8",
            )
            current_inventory.write_text(
                json.dumps(
                    {"schema_version": 1, "tests": current_rows},
                    indent=2,
                    sort_keys=True,
                )
                + "\n",
                encoding="utf-8",
            )
            command = [
                sys.executable,
                str(validator),
                str(marker),
                "canonical-argument",
            ]
            config.write_text(
                textwrap.dedent(
                    f"""\
                    schema_version = 2
                    policy_id = "fixture.typed-canonical"
                    frozen_inventory_hash = {json.dumps(inventory_digest)}
                    canonical_command = "just completion-proof"
                    focused_command = "just completion-focused {{validation_id}}"
                    documentation_command = "just source-map-check"
                    repository_root = {json.dumps(repository.as_posix())}
                    frozen_inventory = ".codex/validation/frozen.json"
                    replacement_ledger = ".codex/validation/ledger.json"
                    testing_current_inventory = ".codex/validation/current.json"
                    host_platform = {json.dumps(platform.system().casefold())}

                    [[validation]]
                    id = "fixture.typed-canonical"
                    runner = "typed-validation"
                    validation_type = "fixture-command"
                    command = {json.dumps(command)}
                    validation_failure_exit_codes = [1]
                    owned_paths = ["validator.py"]
                    consumed_paths = ["validator.py"]
                    timeout_seconds = 30
                    """
                ),
                encoding="utf-8",
            )

            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=repository,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "config", "user.email", "typed-canonical@example.invalid"],
                cwd=repository,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "config", "user.name", "Typed Canonical Test"],
                cwd=repository,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "add", "."],
                cwd=repository,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "commit", "--quiet", "-m", "fixture"],
                cwd=repository,
                check=True,
                capture_output=True,
            )

            start_fingerprint = COMPLETION_PROOF.workspace_fingerprint(repository)

            def runtime_for(report_path: Path) -> dict[str, str]:
                return {
                    "CODEX_COMPLETION_PROOF_NONCE": uuid.uuid4().hex * 2,
                    "CODEX_COMPLETION_PROOF_REPORT": str(report_path),
                    "CODEX_COMPLETION_PROOF_ATTEMPT_ID": str(uuid.uuid4()),
                    "CODEX_COMPLETION_PROOF_PARENT_PID": str(os.getpid()),
                    "CODEX_COMPLETION_PROOF_REPOSITORY": str(repository.resolve()),
                    "CODEX_COMPLETION_PROOF_START_FINGERPRINT": start_fingerprint,
                    "CODEX_COMPLETION_PROOF_MUTATION_EPOCH": "7",
                    "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256": "a" * 64,
                    "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT": (
                        "fixture-attestation-endpoint"
                    ),
                }

            def run_canonical(
                report_path: Path,
                *,
                broker_exchange: object | None = None,
            ) -> tuple[int, str, str]:
                stdout = io.StringIO()
                stderr = io.StringIO()
                patches = [
                    mock.patch.object(
                        COMPLETION_PROOF,
                        "_runtime_inputs",
                        return_value=runtime_for(report_path),
                    ),
                    mock.patch.object(
                        COMPLETION_PROOF,
                        "_attest_runner_process",
                        return_value=None,
                    ),
                ]
                if broker_exchange is not None:
                    patches.append(
                        mock.patch.object(
                            COMPLETION_PROOF,
                            "_typed_validation_broker_exchange",
                            new=broker_exchange,
                        )
                    )
                with (
                    contextlib.ExitStack() as stack,
                    contextlib.redirect_stdout(stdout),
                    contextlib.redirect_stderr(stderr),
                ):
                    for patcher in patches:
                        stack.enter_context(patcher)
                    returncode = COMPLETION_PROOF._unittest_main(
                        ["--config", str(config), "run"]
                    )
                return returncode, stdout.getvalue(), stderr.getvalue()

            report_path = base / "canonical-pass-report.json"
            returncode, stdout, stderr = run_canonical(report_path)

            self.assertEqual(returncode, 0, stderr)
            self.assertIn("COMPLETION PROOF PASSED", stdout)

            report = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual(report["exact_command"], "just completion-proof")
            self.assertEqual(report["attempt_classification"], "confirmed_pass")
            self.assertEqual(len(report["validations"]), 1)
            validation = report["validations"][0]
            self.assertEqual(validation["id"], "fixture.typed-canonical")
            self.assertEqual(validation["classification"], "confirmed_pass")
            self.assertEqual(
                validation["executed_ids"], ["fixture.typed-canonical"]
            )
            self.assertEqual(len(report["child_processes"]), 1)
            broker = report["child_processes"][0]
            self.assertEqual(broker["validation_id"], "fixture.typed-canonical")
            self.assertGreater(broker["pid"], 0)
            self.assertEqual(broker["exit_code"], 0)
            self.assertTrue(broker["executable"])
            self.assertTrue(
                broker["launch_target_identity"]["resolved_path"]
            )
            self.assertTrue(
                broker["launch_target_identity"]["sha256_before"]
            )

            real_broker_exchange = COMPLETION_PROOF._typed_validation_broker_exchange

            def empty_journal_after_real_broker(**kwargs: object) -> object:
                result = real_broker_exchange(**kwargs)
                precommit = kwargs["precommit"]
                self.assertIsInstance(
                    precommit,
                    COMPLETION_PROOF._TypedValidationLaunchPrecommit,
                )
                precommit.journal_path.write_bytes(b"")
                return result

            rejected_report_path = base / "canonical-empty-journal-report.json"
            rejected_code, rejected_stdout, rejected_stderr = run_canonical(
                rejected_report_path,
                broker_exchange=empty_journal_after_real_broker,
            )
            self.assertEqual(rejected_code, 2, rejected_stderr)
            self.assertNotIn("COMPLETION PROOF PASSED", rejected_stdout)

            rejected_report = json.loads(
                rejected_report_path.read_text(encoding="utf-8")
            )
            self.assertEqual(
                rejected_report["attempt_classification"], "pre_result_error"
            )
            self.assertEqual(len(rejected_report["validations"]), 1)
            rejected_validation = rejected_report["validations"][0]
            self.assertEqual(
                rejected_validation["classification"], "pre_result_error"
            )
            self.assertEqual(rejected_validation["selected_ids"], [])
            self.assertEqual(rejected_validation["executed_ids"], [])
            self.assertIn("journal is empty", rejected_validation["diagnostic"])
            self.assertEqual(len(rejected_report["child_processes"]), 1)
            rejected_broker = rejected_report["child_processes"][0]
            self.assertGreater(rejected_broker["pid"], 0)
            self.assertEqual(rejected_broker["exit_code"], 0)
            self.assertTrue(rejected_broker["executable"])
            self.assertTrue(
                rejected_broker["launch_target_identity"]["resolved_path"]
            )
            self.assertTrue(
                rejected_broker["launch_target_identity"]["sha256_before"]
            )

            observations = [
                json.loads(line)
                for line in marker.read_text(encoding="utf-8").splitlines()
            ]
            self.assertEqual(len(observations), 2)
            self.assertEqual(
                len({item["execution_id"] for item in observations}), 2
            )
            self.assertTrue(
                all(
                    item["arguments"] == ["canonical-argument"]
                    and item["proof_environment"] == []
                    for item in observations
                )
            )
            self.assertNotEqual(observations[0]["pid"], broker["pid"])
            self.assertNotEqual(observations[1]["pid"], rejected_broker["pid"])


if __name__ == "__main__":
    unittest.main()
