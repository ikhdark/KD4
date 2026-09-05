from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
import uuid
from pathlib import Path

try:
    from scripts.current_evidence_successor_projection import (
        build_current_successor_projection_v1,
    )
    from scripts import test_completion_proof as completion_fixture
except ModuleNotFoundError:  # Direct `python scripts/test_...py` execution.
    from current_evidence_successor_projection import (
        build_current_successor_projection_v1,
    )
    import test_completion_proof as completion_fixture


REPO_ROOT = Path(__file__).resolve().parents[1]
RUNNER = REPO_ROOT / "scripts" / "completion_proof.py"
CURRENT_EVIDENCE_ID = "inventory.current-evidence"
PYTHON_VALIDATION_IDS = (
    "maintenance.root-unittest",
    "sdk.python.pytest",
)
EVIDENCE_MEMBER_NAMES = (
    "catalog",
    "process",
    "unittest_collect",
    "unittest_exec",
    "pytest_collect",
    "pytest_exec",
)


def _uuid7() -> uuid.UUID:
    value = (int(time.time() * 1000) & ((1 << 48) - 1)) << 80
    value |= uuid.uuid4().int & ((1 << 80) - 1)
    value = (value & ~(0xF << 76)) | (7 << 76)
    value = (value & ~(0x3 << 62)) | (0x2 << 62)
    return uuid.UUID(int=value)


def _frame_members(frame: bytes) -> dict[str, bytes]:
    if len(frame) < 28 or frame[:8] != b"KD4EVID1":
        raise AssertionError("fixture did not capture a KD4EVID1 frame")
    if frame[8:16] != b"\x01\x00\x00\x06\x00\x00\x00\x00":
        raise AssertionError("focused evidence header is not the closed v1 header")
    manifest_length = int.from_bytes(frame[16:20], "big")
    payload_length = int.from_bytes(frame[20:28], "big")
    if len(frame) != 28 + manifest_length + payload_length:
        raise AssertionError("focused evidence frame length is inconsistent")
    manifest_bytes = frame[28 : 28 + manifest_length]
    manifest = json.loads(manifest_bytes)
    if completion_fixture._canonical_json(manifest) != manifest_bytes:
        raise AssertionError("focused evidence manifest is not canonical JSON")
    members = manifest.get("members")
    if manifest.get("schema_version") != 1 or not isinstance(members, list):
        raise AssertionError("focused evidence manifest envelope is invalid")
    names = tuple(item.get("name") for item in members)
    if names != EVIDENCE_MEMBER_NAMES:
        raise AssertionError(f"unexpected focused evidence member order: {names!r}")
    payload = frame[28 + manifest_length :]
    offset = 0
    decoded: dict[str, bytes] = {}
    for item in members:
        length = item.get("length")
        if type(length) is not int or length <= 0:
            raise AssertionError("focused evidence member length is not positive")
        value = payload[offset : offset + length]
        offset += length
        if hashlib.sha256(value).hexdigest() != item.get("sha256"):
            raise AssertionError(f"focused evidence member {item.get('name')} hash differs")
        decoded[str(item["name"])] = value
    if offset != len(payload):
        raise AssertionError("focused evidence members do not consume the payload")
    return decoded


class CurrentEvidenceCliTest(unittest.TestCase):
    """Real Python-wrapper coverage composed over the established CLI fixture."""

    def setUp(self) -> None:
        self.fixture = completion_fixture.CompletionProofCliTest(methodName="runTest")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.repository = self.fixture.repository
        self.base = self.fixture.base
        self.unit_marker = self.base / "real-unittest-executions.txt"
        self.pytest_marker = self.base / "real-pytest-executions.txt"
        self.unit_fail = self.base / "fail-unittest"
        self.pytest_skip = self.base / "skip-pytest"
        self._install_bounded_python_workspace()
        self._write_current_evidence_config()

    def _copy(self, relative: str) -> None:
        source = REPO_ROOT / relative
        destination = self.repository / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)

    def _install_bounded_python_workspace(self) -> None:
        for relative in (
            "scripts/completion_proof_canonical.py",
            "scripts/completion_proof_inventory_v2.py",
            "scripts/completion_proof_unittest.py",
            "scripts/current_evidence_successor_projection.py",
            "scripts/completion_proof_pytest.py",
            "scripts/focused_live_successor_catalog.py",
            "scripts/pyproject.toml",
            "scripts/uv.lock",
            "sdk/python/pyproject.toml",
            "sdk/python/uv.lock",
            "sdk/python/README.md",
            "codex-rs/windows-sandbox-rs/sandbox_smoketests.py",
            "tools/argument-comment-lint/native_test_runner.py",
            "tools/argument-comment-lint/src/lib.rs",
        ):
            self._copy(relative)
        shutil.copytree(
            REPO_ROOT / "sdk" / "python" / "src",
            self.repository / "sdk" / "python" / "src",
            dirs_exist_ok=True,
        )
        (self.repository / ".gitignore").write_text(
            ".pytest_cache/\n.venv/\n__pycache__/\n",
            encoding="utf-8",
        )
        (self.repository / "scripts" / "root_maintenance.py").write_text(
            textwrap.dedent(
                """\
                def python_unittest_targets():
                    return ["scripts.test_runtime_path"]
                """
            ),
            encoding="utf-8",
        )
        rust_source = self.repository / "codex-rs" / "fixture" / "src" / "lib.rs"
        rust_source.parent.mkdir(parents=True)
        rust_source.write_text("/// Fixture.\n", encoding="utf-8")
        jest_test = (
            self.repository
            / "sdk"
            / "typescript"
            / "tests"
            / "runtime_path.test.ts"
        )
        jest_test.parent.mkdir(parents=True)
        jest_test.write_text('test("inventory path", () => {});\n', encoding="utf-8")
        self._write_unittest_source()
        self._write_pytest_source()

    def _write_unittest_source(self, *, include_test: bool = True) -> None:
        if include_test:
            body = textwrap.dedent(
                f"""\
                import pathlib
                import unittest
                import uuid

                MARKER = pathlib.Path({str(self.unit_marker)!r})
                FAIL = pathlib.Path({str(self.unit_fail)!r})

                class RuntimePathTest(unittest.TestCase):
                    def test_runs(self):
                        with MARKER.open("a", encoding="utf-8") as output:
                            output.write(str(uuid.uuid4()) + "\\n")
                        if FAIL.exists():
                            self.fail("fixture unittest failure")
                """
            )
        else:
            body = textwrap.dedent(
                f"""\
                import pathlib

                pathlib.Path({str(self.unit_marker)!r}).write_text(
                    "imported-only\\n", encoding="utf-8"
                )
                """
            )
        (self.repository / "scripts" / "test_runtime_path.py").write_text(
            body, encoding="utf-8"
        )

    def _write_pytest_source(self) -> None:
        test_file = self.repository / "sdk" / "python" / "tests" / "test_runtime_path.py"
        test_file.parent.mkdir(parents=True, exist_ok=True)
        test_file.write_text(
            textwrap.dedent(
                f"""\
                import pathlib
                import uuid

                import pytest

                MARKER = pathlib.Path({str(self.pytest_marker)!r})
                SKIP = pathlib.Path({str(self.pytest_skip)!r})

                @pytest.mark.skipif(SKIP.exists(), reason="fixture pre-result skip")
                def test_runs():
                    with MARKER.open("a", encoding="utf-8") as output:
                        output.write(str(uuid.uuid4()) + "\\n")
                """
            ),
            encoding="utf-8",
        )

    def _write_current_evidence_config(self) -> None:
        self.fixture._write_config(mode="pass", use_testing_inventory=False)
        ledger = json.loads(self.fixture.ledger.read_text(encoding="utf-8"))
        ledger["rows"] = [
            {
                "baseline_id": "fixture-command::old-behavior",
                "resolution": "unresolved",
            }
        ]
        self.fixture._write_json(self.fixture.ledger, ledger)
        with self.fixture.config.open("a", encoding="utf-8") as stream:
            stream.write(
                textwrap.dedent(
                    """\

                    [focused_inventory_evidence]
                    validation_ids = ["maintenance.root-unittest", "sdk.python.pytest"]

                    [[validation]]
                    id = "maintenance.root-unittest"
                    runner = "python-unittest"
                    owned_paths = ["scripts/test_runtime_path.py"]
                    consumed_paths = [
                        "scripts/test_runtime_path.py",
                        "scripts/completion_proof_unittest.py",
                        "scripts/root_maintenance.py",
                        "scripts/pyproject.toml",
                        "scripts/uv.lock",
                    ]
                    timeout_seconds = 60

                    [[validation]]
                    id = "sdk.python.pytest"
                    runner = "python-pytest"
                    owned_paths = ["sdk/python/tests/**"]
                    consumed_paths = [
                        "sdk/python/tests/**",
                        "scripts/completion_proof_pytest.py",
                        "sdk/python/pyproject.toml",
                        "sdk/python/uv.lock",
                    ]
                    timeout_seconds = 60
                    """
                )
            )

    def _write_shared_unittest_replacement_history(self) -> tuple[list[str], str]:
        baseline_ids = [
            "python-unittest::scripts.test_runtime_path.RuntimePathTest.test_old",
            "fixture-command::still-unresolved",
        ]
        successor_id = (
            "python-unittest::scripts.test_runtime_path.RuntimePathTest.test_runs"
        )
        baseline_rows = [
            {
                "baseline_id": baseline_ids[0],
                "framework": "python-unittest",
                "native_id": "scripts.test_runtime_path.RuntimePathTest.test_old",
                "source": "scripts/test_runtime_path.py",
                "ignored": False,
                "platforms": ["windows"],
            },
            {
                "baseline_id": baseline_ids[1],
                "framework": "fixture-command",
                "native_id": "still-unresolved",
                "source": "validator.py",
                "ignored": False,
                "platforms": ["windows"],
            },
        ]
        digest = completion_fixture._inventory_hash(baseline_rows)
        self.fixture._write_json(
            self.fixture.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": "windows",
                "inventory_hash": digest,
                "tests": baseline_rows,
            },
        )
        self.fixture._write_json(
            self.fixture.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": baseline_ids[0],
                        "resolution": "replacement",
                        "replacement_ids": [successor_id],
                        "preserved_behavior": "the bounded unittest runtime path executes",
                        "product_path": (
                            "scripts/test_runtime_path.py via maintenance.root-unittest"
                        ),
                        "validation_id": "maintenance.root-unittest",
                    },
                    {"baseline_id": baseline_ids[1], "resolution": "unresolved"},
                ],
                "overrides": [],
            },
        )
        old_digest = completion_fixture._inventory_hash(self.fixture.baseline_rows)
        config = self.fixture.config.read_text(encoding="utf-8")
        self.assertIn(old_digest, config)
        self.fixture.config.write_text(
            config.replace(old_digest, digest), encoding="utf-8"
        )
        return baseline_ids, successor_id

    def _fixture_patch(self, capture: Path, *, ack_mode: str = "accept") -> str:
        fake_inventory = self.base / "fake-rust-inventory.py"
        fake_inventory.write_text(
            textwrap.dedent(
                f"""\
                import json
                import pathlib
                import sys

                if sys.argv[1] == "inventory.rust-nextest":
                    print(json.dumps({{
                        "test-count": 1,
                        "rust-suites": {{
                            "fixture": {{
                                "package-name": "fixture-package",
                                "binary-name": "fixture-test",
                                "cwd": {str(self.repository / 'codex-rs' / 'fixture')!r},
                                "testcases": {{"runtime_path": {{"ignored": False}}}},
                            }}
                        }},
                    }}))
                elif sys.argv[1] == "inventory.rust-doctest":
                    print("fixture/src/lib.rs - fixture (line 1): test")
                elif sys.argv[1] == "inventory.tools.argument-comment-lint.native":
                    print(json.dumps({{
                        "schema_version": 1,
                        "report_type": "ArgumentCommentLintNativeTestInventoryV1",
                        "count": 1,
                        "tests": [{{
                            "id": "argument-comment-lint::rust-lib::fixture",
                            "kind": "rust-lib",
                            "cargo_target": ["--lib"],
                            "native_id": "fixture",
                            "ui_case": None,
                            "doctest_item": None,
                            "doctest_ordinal": None,
                        }}],
                    }}))
                elif sys.argv[1] == "inventory.windows.sandbox-smoke":
                    print(json.dumps({{
                        "schema_version": 1,
                        "report_type": "WindowsSandboxSmokeCaseListV1",
                        "validation_id": "windows-sandbox-smoke",
                        "host_platform": "windows",
                        "cases": [
                            {{
                                "id": (
                                    "python-script-case::windows-sandbox-smoke::"
                                    f"fixture-{{index}}"
                                ),
                                "name": f"fixture {{index}}",
                            }}
                            for index in range(46)
                        ],
                    }}))
                else:
                    raise SystemExit("unknown fake inventory role")
                """
            ),
            encoding="utf-8",
        )
        return textwrap.dedent(
            f"""\
            import hashlib as _fixture_hashlib
            import json as _fixture_json
            import pathlib as _fixture_pathlib

            _fixture_capture = _fixture_pathlib.Path({str(capture)!r})
            _fixture_attestation = _fixture_capture.with_suffix(".attestation.json")
            _fixture_scripts_env = _fixture_pathlib.Path(
                {str(self.base / 'uv-scripts-environment')!r}
            )
            _fixture_sdk_env = _fixture_pathlib.Path(
                {str(self.base / 'uv-sdk-environment')!r}
            )
            _fixture_report = _fixture_pathlib.Path(
                os.environ["CODEX_COMPLETION_PROOF_REPORT"]
            )
            _fixture_endpoint = "current-evidence-fixture-channel"
            _fixture_ack_mode = {ack_mode!r}
            _fixture_original_open = open
            _fixture_original_run_process = run_process

            class _FixtureRawEvidenceChannel:
                def __init__(self):
                    self.attestation = b""
                    self.frame = bytearray()
                    self.attestation_complete = False
                    self.ack = None
                    self.ack_offset = 0

                def write(self, value):
                    value = bytes(value)
                    if not self.attestation_complete:
                        self.attestation += value
                    else:
                        self.frame.extend(value)
                    return len(value)

                def readline(self, _limit=-1):
                    self.attestation_complete = True
                    return b"ok\\n"

                def read(self, count=-1):
                    if self.ack is None:
                        digest = _fixture_hashlib.sha256(bytes(self.frame)).digest()
                        attempt = _fixture_json.loads(
                            _fixture_report.read_text(encoding="utf-8")
                        )
                        accepted = _fixture_ack_mode == "accept" or (
                            _fixture_ack_mode == "auto"
                            and attempt["attempt_classification"] == "confirmed_pass"
                        )
                        if accepted:
                            self.ack = (
                                b"KD4EVACK" + bytes((1, 0)) + (0).to_bytes(2, "big")
                                + (0).to_bytes(4, "big") + digest
                            )
                        elif _fixture_ack_mode in {"auto", "reject"}:
                            self.ack = (
                                b"KD4EVACK" + bytes((1, 0)) + (1).to_bytes(2, "big")
                                + (0).to_bytes(4, "big") + digest
                            )
                        else:
                            self.ack = b"short"
                    if count < 0:
                        count = len(self.ack) - self.ack_offset
                    value = self.ack[self.ack_offset:self.ack_offset + count]
                    self.ack_offset += len(value)
                    return value

                def settimeout(self, _timeout):
                    return None

                def close(self):
                    if self.attestation:
                        _fixture_attestation.write_bytes(self.attestation)
                    if self.frame:
                        _fixture_capture.write_bytes(bytes(self.frame))

            _fixture_channel = _FixtureRawEvidenceChannel()

            def _fixture_open(path, mode="r", *args, **kwargs):
                if str(path) == _fixture_endpoint and mode == "r+b":
                    return _fixture_channel
                return _fixture_original_open(path, mode, *args, **kwargs)

            def _fixture_run_process(**kwargs):
                if kwargs["validation_id"] in {{
                    "inventory.root-unittest", "maintenance.root-unittest"
                }}:
                    kwargs["env"] = {{
                        **kwargs["env"],
                        "UV_PROJECT_ENVIRONMENT": str(_fixture_scripts_env),
                    }}
                elif kwargs["validation_id"] in {{
                    "inventory.sdk-python-pytest", "sdk.python.pytest"
                }}:
                    kwargs["env"] = {{
                        **kwargs["env"],
                        "UV_PROJECT_ENVIRONMENT": str(_fixture_sdk_env),
                    }}
                if kwargs["validation_id"] in {{
                    "inventory.rust-nextest",
                    "inventory.rust-doctest",
                    "inventory.tools.argument-comment-lint.native",
                    "inventory.windows.sandbox-smoke",
                }}:
                    kwargs["command"] = [
                        sys.executable,
                        {str(fake_inventory)!r},
                        kwargs["validation_id"],
                    ]
                    kwargs["cwd"] = repo_root if "repo_root" in globals() else Path.cwd()
                return _fixture_original_run_process(**kwargs)

            open = _fixture_open
            run_process = _fixture_run_process
            """
        )

    def _run_current_evidence(
        self,
        *,
        ack_mode: str = "auto",
        start_fingerprint: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object], Path]:
        report_path = self.base / f"current-evidence-report-{uuid.uuid4()}.json"
        capture_path = self.base / f"current-evidence-frame-{uuid.uuid4()}.bin"
        patch_source = self._fixture_patch(capture_path, ack_mode=ack_mode)
        env = self.fixture._base_env()
        env.update(
            {
                "CODEX_COMPLETION_PROOF_NONCE": uuid.uuid4().hex * 2,
                "CODEX_COMPLETION_PROOF_REPORT": str(report_path),
                "CODEX_COMPLETION_PROOF_ATTEMPT_ID": str(_uuid7()),
                "CODEX_COMPLETION_PROOF_PARENT_PID": str(os.getpid()),
                "CODEX_COMPLETION_PROOF_REPOSITORY": str(self.repository.resolve()),
                "CODEX_COMPLETION_PROOF_START_FINGERPRINT": (
                    start_fingerprint or self.fixture._fingerprint()
                ),
                "CODEX_COMPLETION_PROOF_MUTATION_EPOCH": "7",
                "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256": "a" * 64,
                "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT": (
                    "current-evidence-fixture-channel"
                ),
            }
        )
        command = [
            sys.executable,
            "-c",
            (
                "import runpy,sys; module=runpy.run_path(sys.argv[1]); "
                "runner_globals=module['_unittest_main'].__globals__; "
                f"exec({patch_source!r}, runner_globals); "
                "raise SystemExit(module['_unittest_main'](sys.argv[2:]))"
            ),
            str(RUNNER),
            "--config",
            str(self.fixture.config),
            "focused",
            CURRENT_EVIDENCE_ID,
        ]
        result = subprocess.run(
            command,
            cwd=self.repository,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            timeout=180,
            check=False,
        )
        report = self.fixture._load_report(report_path)
        return result, report, capture_path

    def _assert_component(
        self, report: dict[str, object], validation_id: str, outcome: str
    ) -> dict[str, object]:
        validation = next(
            item for item in report["validations"] if item["id"] == validation_id
        )
        self.assertEqual(validation["classification"], outcome)
        self.assertEqual(validation["intended_count"], 1)
        self.assertEqual(validation["selected_count"], 1)
        self.assertEqual(validation["executed_count"], 1)
        return validation

    def test_unresolved_inventory_runs_both_real_wrappers_fresh_twice(self) -> None:
        first_result, first, first_frame = self._run_current_evidence()
        second_result, second, second_frame = self._run_current_evidence()

        self.assertEqual(first_result.returncode, 0, first_result.stderr)
        self.assertEqual(second_result.returncode, 0, second_result.stderr)
        for report in (first, second):
            self.assertEqual(report["report_type"], "FocusedValidationAttemptReportV2")
            self.assertEqual(report["focused_validation_id"], CURRENT_EVIDENCE_ID)
            self.assertEqual(
                report["exact_command"],
                "just completion-focused inventory.current-evidence",
            )
            self.assertEqual(report["attempt_classification"], "confirmed_pass")
            self.assertEqual(
                [item["id"] for item in report["validations"]],
                list(PYTHON_VALIDATION_IDS),
            )
            self.assertEqual(
                [item["validation_id"] for item in report["child_processes"]],
                list(PYTHON_VALIDATION_IDS),
            )
            self.assertNotEqual(report["report_type"], "CompletionProofAttemptReportV2")
            self.assertEqual(report["exceptions"], [])
            self.assertEqual(report["overrides"], [])
            for validation_id in PYTHON_VALIDATION_IDS:
                validation = self._assert_component(
                    report, validation_id, "confirmed_pass"
                )
                self.assertEqual(len(validation["outcomes"]), 1)
                self.assertEqual(
                    validation["outcomes"][0]["id"],
                    validation["executed_ids"][0],
                )
                self.assertEqual(validation["outcomes"][0]["outcome"], "passed")

        first_members = _frame_members(first_frame.read_bytes())
        second_members = _frame_members(second_frame.read_bytes())
        first_catalog = json.loads(first_members["catalog"])
        second_catalog = json.loads(second_members["catalog"])
        first_attestation = json.loads(
            first_frame.with_suffix(".attestation.json").read_text(encoding="utf-8")
        )
        self.assertEqual(
            set(first_attestation),
            {"schema_version", "attempt_id", "nonce", "process_id", "entrypoint_path"},
        )
        self.assertEqual(first_attestation["schema_version"], 1)
        self.assertEqual(first_attestation["attempt_id"], first_catalog["attempt_id"])
        self.assertEqual(first_attestation["nonce"], first["nonce"])
        self.assertEqual(
            first_attestation["process_id"], first["runner_process_identity"]["pid"]
        )
        self.assertEqual(
            first_attestation["entrypoint_path"],
            first["runner_process_identity"]["entrypoint_identity"]["resolved_path"],
        )
        self.assertEqual(first_catalog["focused_validation_id"], CURRENT_EVIDENCE_ID)
        self.assertEqual(first_catalog["replacement_baseline_row_count"], 0)
        self.assertEqual(first_catalog["distinct_successor_count"], 0)
        self.assertEqual(first_catalog["resolved_successor_entries"], [])
        self.assertGreater(first_catalog["current_inventory_count"], 2)
        self.assertNotEqual(first_catalog["attempt_id"], second_catalog["attempt_id"])
        self.assertEqual(len(json.loads(first_members["process"])), 6)
        self.assertEqual(len(json.loads(second_members["process"])), 6)
        unittest_collection = json.loads(first_members["unittest_collect"])
        unittest_exec = json.loads(first_members["unittest_exec"])
        native_id = unittest_collection["tests"][0]["id"]
        catalog_row = next(
            row
            for row in first_catalog["current_inventory"]
            if row["baseline_id"] == f"python-unittest::{native_id}"
        )
        self.assertEqual(
            unittest_collection["tests"][0]["source_path"],
            "scripts/test_runtime_path.py",
        )
        self.assertEqual(catalog_row["source"], "scripts/test_runtime_path.py")
        self.assertEqual(
            unittest_exec["proof_execution_id"],
            first["validations"][0]["execution_id"],
        )
        self.assertEqual(len(self.unit_marker.read_text(encoding="utf-8").splitlines()), 2)
        self.assertEqual(len(self.pytest_marker.read_text(encoding="utf-8").splitlines()), 2)
        for validation_id in PYTHON_VALIDATION_IDS:
            first_validation = next(
                item for item in first["validations"] if item["id"] == validation_id
            )
            second_validation = next(
                item for item in second["validations"] if item["id"] == validation_id
            )
            self.assertNotEqual(
                first_validation["execution_id"], second_validation["execution_id"]
            )

    def test_zero_build_only_unittest_is_pre_result_and_runs_no_test(self) -> None:
        self._write_unittest_source(include_test=False)

        result, report, capture = self._run_current_evidence()

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertNotIn("FOCUSED VALIDATION PASSED", result.stdout)
        self.assertTrue(self.unit_marker.is_file(), "collection imported the module")
        self.assertEqual(
            self.unit_marker.read_text(encoding="utf-8"), "imported-only\n"
        )
        self.assertFalse(self.pytest_marker.exists())
        self.assertFalse(capture.exists())

    def test_known_shared_replacement_projection_is_not_erased(self) -> None:
        baseline_ids, successor_id = self._write_shared_unittest_replacement_history()

        result, report, frame = self._run_current_evidence()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        members = _frame_members(frame.read_bytes())
        catalog = json.loads(members["catalog"])
        self.assertEqual(catalog["replacement_baseline_row_count"], 0)
        self.assertEqual(catalog["distinct_successor_count"], 0)
        self.assertEqual(catalog["resolved_successor_entries"], [])
        self.assertIn("unresolved-baselines=1", result.stdout)
        self.assertIn("unresolved-edges=1", result.stdout)
        self.assertIn("unresolved-successors=1", result.stdout)
        unittest_collection = self.base / "retained-unittest-collection.json"
        pytest_collection = self.base / "retained-pytest-collection.json"
        unittest_collection.write_bytes(members["unittest_collect"])
        pytest_collection.write_bytes(members["pytest_collect"])
        projection = build_current_successor_projection_v1(
            repo_root=self.repository,
            current_inventory=catalog["current_inventory"],
            unittest_collection_report=unittest_collection,
            pytest_collection_report=pytest_collection,
            frozen_inventory_path=self.fixture.frozen_inventory,
            replacement_ledger_path=self.fixture.ledger,
        )
        unresolved = projection["unresolved_projection"]
        self.assertEqual(unresolved["historical_replacement_baseline_row_count"], 1)
        self.assertEqual(unresolved["historical_replacement_edge_count"], 1)
        self.assertEqual(unresolved["historical_distinct_successor_count"], 1)
        self.assertEqual(
            unresolved["unresolved_successors"],
            [
                {
                    "successor_id": successor_id,
                    "baseline_ids": baseline_ids[:1],
                    "reason": "missing-current-v2-inventory-authority",
                }
            ],
        )
        self.assertEqual(len(self.unit_marker.read_text(encoding="utf-8").splitlines()), 1)
        self.assertEqual(
            len(self.pytest_marker.read_text(encoding="utf-8").splitlines()), 1
        )

    def test_confirmed_failure_survives_later_pytest_pre_result(self) -> None:
        self.unit_fail.write_text("fail\n", encoding="utf-8")
        self.pytest_skip.write_text("skip\n", encoding="utf-8")

        result, report, _ = self._run_current_evidence()

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertEqual(
            [item["id"] for item in report["validations"]],
            list(PYTHON_VALIDATION_IDS),
        )
        unittest_report = report["validations"][0]
        pytest_report = report["validations"][1]
        self.assertEqual(
            unittest_report["classification"], "confirmed_validation_failure"
        )
        self.assertEqual(
            unittest_report["confirmed_failure_ids"],
            ["scripts.test_runtime_path.RuntimePathTest.test_runs"],
        )
        self.assertEqual(unittest_report["executed_count"], 1)
        self.assertEqual(pytest_report["classification"], "pre_result_error")
        self.assertEqual(pytest_report["executed_count"], 0)
        self.assertEqual(len(self.unit_marker.read_text(encoding="utf-8").splitlines()), 1)
        self.assertFalse(self.pytest_marker.exists())

    def test_unchanged_failure_executes_fresh_and_never_becomes_success(self) -> None:
        self.unit_fail.write_text("fail\n", encoding="utf-8")

        first_result, first, first_frame = self._run_current_evidence()
        second_result, second, second_frame = self._run_current_evidence()

        self.assertEqual(first_result.returncode, 2, first_result.stderr)
        self.assertEqual(second_result.returncode, 2, second_result.stderr)
        for report, frame in ((first, first_frame), (second, second_frame)):
            self.assertEqual(report["attempt_classification"], "pre_result_error")
            self.assertIn("acknowledgement", report["fatal_error"])
            failed = self._assert_component(
                report, "maintenance.root-unittest", "confirmed_validation_failure"
            )
            self.assertEqual(failed["outcomes"][0]["outcome"], "failed")
            self._assert_component(report, "sdk.python.pytest", "confirmed_pass")
            _frame_members(frame.read_bytes())
        self.assertEqual(len(self.unit_marker.read_text(encoding="utf-8").splitlines()), 2)
        self.assertEqual(
            len(self.pytest_marker.read_text(encoding="utf-8").splitlines()), 2
        )
        self.assertNotEqual(
            first["validations"][0]["execution_id"],
            second["validations"][0]["execution_id"],
        )

    def test_cli_rejects_open_focused_configuration_shapes(self) -> None:
        valid_config = self.fixture.config.read_text(encoding="utf-8")
        valid_fingerprint = self.fixture._fingerprint()
        exact_table = textwrap.dedent(
            """
            [focused_inventory_evidence]
            validation_ids = ["maintenance.root-unittest", "sdk.python.pytest"]
            """
        )
        self.assertIn(exact_table, valid_config)
        forbidden_validation = textwrap.dedent(
            """

            [[validation]]
            id = "inventory.current-evidence"
            runner = "typed-validation"
            validation_type = "fixture-command"
            command = ["python", "validator.py"]
            validation_failure_exit_codes = [1]
            owned_paths = ["validator.py"]
            consumed_paths = ["validator.py"]
            timeout_seconds = 30
            """
        )
        cases = {
            "reordered": (
                valid_config.replace(
                    exact_table,
                    exact_table.replace(
                        '["maintenance.root-unittest", "sdk.python.pytest"]',
                        '["sdk.python.pytest", "maintenance.root-unittest"]',
                    ),
                ),
                "must contain only the exact ordered validation_ids",
            ),
            "missing": (
                valid_config.replace(exact_table, ""),
                "inventory.current-evidence is not declared by policy",
            ),
            "additional": (
                valid_config.replace(
                    exact_table,
                    exact_table.replace(
                        '"sdk.python.pytest"]', '"sdk.python.pytest", "extra"]'
                    ),
                ),
                "must contain only the exact ordered validation_ids",
            ),
            "required-validation": (
                valid_config + forbidden_validation,
                "cannot be declared as a required validation",
            ),
        }

        for name, (config, diagnostic) in cases.items():
            with self.subTest(name=name):
                self.fixture.config.write_text(config, encoding="utf-8")
                result, report, capture = self._run_current_evidence(
                    start_fingerprint=valid_fingerprint
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(report["attempt_classification"], "pre_result_error")
                self.assertIn(diagnostic, report["fatal_error"])
                self.assertFalse(capture.exists())
                self.assertFalse(self.unit_marker.exists())
                self.assertFalse(self.pytest_marker.exists())

    def test_rejected_authenticated_frame_cannot_become_success(self) -> None:
        result, report, capture = self._run_current_evidence(ack_mode="reject")

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn("acknowledgement", report["fatal_error"])
        self.assertTrue(capture.is_file())
        _frame_members(capture.read_bytes())
        self.assertEqual(len(self.unit_marker.read_text(encoding="utf-8").splitlines()), 1)
        self.assertEqual(len(self.pytest_marker.read_text(encoding="utf-8").splitlines()), 1)
        self.assertNotIn("FOCUSED VALIDATION PASSED", result.stdout)


class FocusedJustLauncherTest(unittest.TestCase):
    def test_actual_recipe_imports_runner_without_pythonpath(self) -> None:
        just = shutil.which("just")
        self.assertIsNotNone(just, "the repository Just recipe requires just")
        assert just is not None
        protected_paths = (
            RUNNER,
            REPO_ROOT / "justfile",
            REPO_ROOT / ".codex" / "validation" / "completion-proof.toml",
            REPO_ROOT / ".codex" / "validation" / "frozen-test-inventory-v1.json",
            REPO_ROOT / ".codex" / "validation" / "test-replacements-v1.json",
        )
        before = {path: path.read_bytes() for path in protected_paths}

        with tempfile.TemporaryDirectory(prefix="kd4-focused-just-launcher-") as name:
            fixture_root = Path(name)
            sentinel = fixture_root / "sentinel.txt"
            sentinel.write_text("bounded fixture\n", encoding="utf-8")
            report_path = fixture_root / "forbidden-report.json"
            env = os.environ.copy()
            for variable in tuple(env):
                normalized = variable.upper()
                if normalized == "PYTHONPATH" or normalized.startswith(
                    "CODEX_COMPLETION_PROOF_"
                ):
                    env.pop(variable)
            env["PYTHONDONTWRITEBYTECODE"] = "1"
            env["CODEX_COMPLETION_PROOF_REPORT"] = str(report_path)

            result = subprocess.run(
                [
                    just,
                    "--justfile",
                    str(REPO_ROOT / "justfile"),
                    "completion-focused",
                    CURRENT_EVIDENCE_ID,
                ],
                cwd=fixture_root,
                env=env,
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                timeout=60,
                check=False,
            )

            self.assertEqual(sentinel.read_text(encoding="utf-8"), "bounded fixture\n")
            self.assertEqual(list(fixture_root.iterdir()), [sentinel])
            self.assertFalse(report_path.exists())

        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 2, output)
        self.assertIn("missing runtime completion-proof input(s):", output)
        self.assertIn(
            "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT", output
        )
        self.assertNotIn("ModuleNotFoundError", output)
        self.assertNotIn("No module named", output)
        self.assertNotIn("just completion-proof", output)
        self.assertNotIn("COMPLETION PROOF PASSED", output)
        self.assertNotIn("FOCUSED VALIDATION PASSED", output)
        self.assertEqual(
            {path: path.read_bytes() for path in protected_paths},
            before,
            "the rejected launcher must not mutate repository proof inputs",
        )


if __name__ == "__main__":
    unittest.main()
