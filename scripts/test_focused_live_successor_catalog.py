from __future__ import annotations

import base64
import copy
import hashlib
import json
import os
import sys
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path
from typing import Any
from unittest.mock import patch

from scripts.completion_proof_canonical import canonical_jcs, proof_hash
from scripts.focused_live_successor_catalog import (
    FocusedLiveSuccessorCatalogError,
    build_focused_live_successor_catalog_v1,
    parse_focused_live_successor_catalog_v1,
    validate_focused_live_successor_catalog_semantics_v1,
    validate_focused_live_successor_catalog_wire_v1,
    validate_inventory_discovery_process_authority_v1,
    validate_inventory_discovery_process_set_v1,
)
import scripts.focused_live_successor_catalog as catalog_module


ROOT = Path(__file__).resolve().parents[1]
SCHEMA_PATH = ROOT / ".codex/validation/focused-live-successor-catalog-v1.schema.json"
VECTORS_PATH = ROOT / "scripts/fixtures/focused_live_successor_catalog_v1_vectors.json"
ATTEMPT_ID = "01890f47-5e7a-7cc1-98b7-9abc01234567"


def file_hash(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


@contextmanager
def captured_file_state(processes: Any) -> Any:
    hashes: dict[str, str] = {}
    identities: dict[str, dict[str, str]] = {}
    try:
        for process in processes:
            child = process["child_process"]
            hashes[child["executable"]] = child["launch_target_identity"]["sha256_before"]
            output = process["output"]
            if output["kind"] == "report-file":
                hashes[output["report_path"]] = output["report_sha256"]
                identities[output["report_path"]] = output["report_identity"]
    except (KeyError, TypeError):
        # Malformed negative vectors must reach the public API, where the
        # contract error is translated consistently.
        pass
    with (
        patch.object(catalog_module, "_file_sha256", side_effect=lambda path, _label: hashes[path]),
        patch.object(
            catalog_module,
            "_current_windows_file_identity",
            side_effect=lambda path, _label: identities[path],
        ),
    ):
        yield


class FocusedLiveSuccessorCatalogTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.schema = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
        if not VECTORS_PATH.is_file():
            raise AssertionError(f"required cross-language vector packet is missing: {VECTORS_PATH}")
        cls.vectors = json.loads(VECTORS_PATH.read_text(encoding="utf-8"))

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.temp_path = Path(self.temp.name).resolve()
        self.executable = str(Path(sys.executable).resolve())
        self.cwd = str(self.temp_path)
        self.executable_hash = file_hash(Path(self.executable))
        self.report_paths = {
            "inventory.root-unittest": self.temp_path / "unittest-report.json",
            "inventory.sdk-python-pytest": self.temp_path / "pytest-report.json",
        }
        for role, path in self.report_paths.items():
            path.write_bytes(canonical_jcs({"role": role, "status": "ok"}))
        self.attempt_bounds = {
            "attempt_id": ATTEMPT_ID,
            "runner_pid": os.getpid(),
            "started_at": "10",
            "reconciliation_started_at": "80",
            "ended_at": "100",
        }
        self.processes, self.invocation_authority = self._process_inputs()
        self.inputs = self._builder_inputs()

    def _process_inputs(self) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        roles = (
            "inventory.rust-nextest",
            "inventory.rust-doctest",
            "inventory.root-unittest",
            "inventory.sdk-python-pytest",
            "inventory.tools.argument-comment-lint.native",
            "inventory.windows.sandbox-smoke",
        )
        processes: list[dict[str, Any]] = []
        authority: list[dict[str, Any]] = []
        for index, role in enumerate(roles):
            is_report = role in self.report_paths
            report_path = str(self.report_paths[role]) if is_report else None
            argv = (
                [self.executable, role, "--output", report_path]
                if report_path is not None
                else [self.executable, role]
            )
            output: dict[str, Any]
            if is_report:
                path = self.report_paths[role]
                output = {
                    "kind": "report-file",
                    "report_path": str(path),
                    "report_identity": catalog_module._current_windows_file_identity(
                        str(path), "test report"
                    ),
                    "report_sha256": file_hash(path),
                }
            else:
                output = {"kind": "stdout", "stdout_sha256": hashlib.sha256(role.encode()).hexdigest()}
            processes.append(
                {
                    "role": role,
                    "child_process": {
                        "validation_id": role,
                        "execution_id": f"00000000-0000-4000-8000-{index + 1:012x}",
                        "pid": 1000 + index,
                        "executable": self.executable,
                        "launch_target_identity": {
                            "requested": self.executable,
                            "resolved_path": self.executable,
                            "sha256_before": self.executable_hash,
                            "sha256_after": self.executable_hash,
                        },
                        "args_hash": hashlib.sha256(canonical_jcs(argv)).hexdigest(),
                        "started_at": str(20 + index * 2),
                        "ended_at": str(21 + index * 2),
                        "exit_code": 0,
                    },
                    "argv": argv,
                    "cwd": self.cwd,
                    "output": output,
                }
            )
            authority.append(
                {
                    "role": role,
                    "executable": self.executable,
                    "argv": argv,
                    "cwd": self.cwd,
                    "output_kind": output["kind"],
                    "report_path": report_path,
                }
            )
        return processes, {"expected_processes": authority}

    def _builder_inputs(self) -> dict[str, Any]:
        test_id = "javascript-jest::sdk/typescript/example.test.ts::suite example"
        contract_projection = {
            "consumed": [{"kind": "exact", "path": "sdk/typescript/example.test.ts"}],
            "owned": [],
            "schema_version": 1,
        }
        contract_hash = proof_hash("kd4.execution-input-contract.v1", contract_projection)
        contract = {
            "contract_id": f"execution-input-contract-v1.{contract_hash}",
            "contract_sha256": contract_hash,
            **contract_projection,
        }
        identity = {
            "kind": "test",
            "route_id": "test-route.javascript-jest.v1",
            "test_id": test_id,
            "validation_id": "sdk.typescript.jest",
        }
        runner = {
            "ancestor_titles": ["suite"],
            "column": 1,
            "config_path": "sdk/typescript/jest.config.js",
            "file_path": "sdk/typescript/example.test.ts",
            "full_title": "suite example",
            "kind": "javascript-jest",
            "line": 1,
            "registration_ordinal": 0,
        }
        applicability = {"kind": "host-set", "required_hosts": ["windows"]}
        entry = {
            "cargo_target_context_spec_sha256": None,
            "executable_identity": identity,
            "executable_identity_sha256": proof_hash("kd4.executable-identity.v1", identity),
            "execution_input_contract_sha256": contract_hash,
            "platform_applicability": applicability,
            "platform_applicability_sha256": proof_hash("kd4.platform-applicability.v1", applicability),
            "runner_selector": runner,
            "runner_selector_sha256": proof_hash("kd4.runner-selector.v1", runner),
            "test_route_id": identity["route_id"],
            "validation_id": identity["validation_id"],
        }
        resolved = {
            "inventory_entry": entry,
            "inventory_entry_semantic_sha256": proof_hash("kd4.executable-inventory-entry.v2", entry),
        }
        successor = {
            "test_id": test_id,
            "framework": "javascript-jest",
            "native_id": test_id,
            "source_path": "sdk/typescript/example.test.ts",
            "test_route_id": entry["test_route_id"],
            "validation_id": entry["validation_id"],
            "runner_selector_sha256": entry["runner_selector_sha256"],
            "executable_identity_sha256": entry["executable_identity_sha256"],
            "execution_input_contract_sha256": entry["execution_input_contract_sha256"],
            "platform_applicability_sha256": entry["platform_applicability_sha256"],
        }
        return {
            "attempt_id": ATTEMPT_ID,
            "focused_validation_id": "inventory.current-evidence",
            "frozen_inventory_hash": "1" * 64,
            "start_fingerprint": "2" * 64,
            "start_mutation_epoch": 7,
            "replacement_baseline_row_count": 1,
            "current_inventory": [
                {
                    "baseline_id": test_id,
                    "framework": "javascript-jest",
                    "native_id": test_id,
                    "source": "sdk/typescript/example.test.ts",
                    "ignored": False,
                    "platforms": ["windows"],
                }
            ],
            "resolved_successor_entries": [resolved],
            "execution_input_contracts": [contract],
            "cargo_target_context_specs": [],
            "replacement_successor_catalog": {
                "format_id": "kd4.replacement-successor-catalog.v1",
                "schema_version": 1,
                "successors": [successor],
            },
            "successor_owner_map": [
                {"successor_id": test_id, "baseline_ids": ["legacy.javascript-jest.example"]}
            ],
            "inventory_discovery_processes": self.processes,
            "invocation_authority": self.invocation_authority,
            "attempt_bounds": self.attempt_bounds,
            "jest_observation": {
                "observation_id": "inventory.sdk.typescript.jest",
                "execution_id": "00000000-0000-4000-8000-000000000099",
                "runner_pid": os.getpid(),
                "started_at": "40",
                "ended_at": "50",
                "discovered_count": 1,
                "discovered_test_ids_sha256": proof_hash(
                    "kd4.in-process-jest-discovered-id-set.v1", [test_id]
                ),
            },
            "jest_discovered_ids": [test_id],
        }

    def _semantic_kwargs(self) -> dict[str, Any]:
        keys = {
            "successor_owner_map",
            "inventory_discovery_processes",
            "invocation_authority",
            "attempt_bounds",
            "current_inventory",
            "resolved_successor_entries",
            "execution_input_contracts",
            "cargo_target_context_specs",
            "replacement_successor_catalog",
            "jest_observation",
            "jest_discovered_ids",
        }
        result = {key: self.inputs[key] for key in keys}
        result.update(
            expected_replacement_baseline_row_count=self.inputs["replacement_baseline_row_count"],
            expected_frozen_inventory_hash=self.inputs["frozen_inventory_hash"],
            expected_start_fingerprint=self.inputs["start_fingerprint"],
            expected_start_mutation_epoch=self.inputs["start_mutation_epoch"],
        )
        return result

    @staticmethod
    def _vector_semantic_kwargs(vector: dict[str, Any]) -> dict[str, Any]:
        keys = (
            "successor_owner_map",
            "inventory_discovery_processes",
            "invocation_authority",
            "attempt_bounds",
            "current_inventory",
            "resolved_successor_entries",
            "execution_input_contracts",
            "cargo_target_context_specs",
            "replacement_successor_catalog",
            "jest_observation",
            "jest_discovered_ids",
            "expected_replacement_baseline_row_count",
            "expected_frozen_inventory_hash",
            "expected_start_fingerprint",
            "expected_start_mutation_epoch",
        )
        return {key: vector[key] for key in keys}

    @classmethod
    def _vector_builder_inputs(cls, vector: dict[str, Any]) -> dict[str, Any]:
        catalog = vector["catalog"]
        semantic = cls._vector_semantic_kwargs(vector)
        return {
            "attempt_id": catalog["attempt_id"],
            "focused_validation_id": catalog["focused_validation_id"],
            "frozen_inventory_hash": semantic["expected_frozen_inventory_hash"],
            "start_fingerprint": semantic["expected_start_fingerprint"],
            "start_mutation_epoch": semantic["expected_start_mutation_epoch"],
            "replacement_baseline_row_count": semantic["expected_replacement_baseline_row_count"],
            **{
                key: semantic[key]
                for key in (
                    "current_inventory",
                    "resolved_successor_entries",
                    "execution_input_contracts",
                    "cargo_target_context_specs",
                    "replacement_successor_catalog",
                    "successor_owner_map",
                    "inventory_discovery_processes",
                    "invocation_authority",
                    "attempt_bounds",
                    "jest_observation",
                    "jest_discovered_ids",
                )
            },
        }

    def _invoke_invalid_vector(self, vector: dict[str, Any]) -> None:
        kind = vector["kind"]
        api = vector.get("api")
        fixed_routes = {
            "raw-json-bytes": "parse_focused_live_successor_catalog_v1",
            "catalog": "validate_focused_live_successor_catalog_wire_v1",
            "process-set": "validate_inventory_discovery_process_set_v1",
            "process-authority": "validate_inventory_discovery_process_authority_v1",
            "semantic-catalog": "validate_focused_live_successor_catalog_semantics_v1",
        }
        if kind in fixed_routes:
            expected_api = fixed_routes[kind]
            if api is not None and api != expected_api:
                self.fail(f"invalid vector {vector['case']} has mismatched kind/API route")
            api = expected_api
        elif kind != "value":
            self.fail(f"unknown invalid vector kind: {kind!r}")

        known_apis = {
            "parse_focused_live_successor_catalog_v1",
            "validate_focused_live_successor_catalog_wire_v1",
            "validate_inventory_discovery_process_set_v1",
            "validate_inventory_discovery_process_authority_v1",
            "build_focused_live_successor_catalog_v1",
            "validate_focused_live_successor_catalog_semantics_v1",
        }
        if api not in known_apis:
            self.fail(f"unknown invalid vector API: {api!r}")

        base = self.vectors["valid_vectors"][0]
        if api == "parse_focused_live_successor_catalog_v1":
            if "raw_json_base64url" in vector:
                encoded = vector["raw_json_base64url"]
                raw: Any = base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4))
            else:
                raw = vector["value"]
            parse_focused_live_successor_catalog_v1(raw)
            return
        if api == "validate_focused_live_successor_catalog_wire_v1":
            validate_focused_live_successor_catalog_wire_v1(vector["value"])
            return
        if api == "validate_inventory_discovery_process_set_v1":
            validate_inventory_discovery_process_set_v1(vector["value"])
            return
        if api == "validate_inventory_discovery_process_authority_v1":
            payload = vector["value"]
            if not isinstance(payload, dict) or set(payload) != {
                "processes",
                "invocation_authority",
                "attempt_bounds",
            }:
                self.fail(
                    f"process-authority invalid vector {vector['case']} must carry "
                    "exactly processes, invocation_authority, and attempt_bounds"
                )
            processes = payload["processes"]
            with captured_file_state(processes):
                validate_inventory_discovery_process_authority_v1(
                    processes,
                    invocation_authority=payload["invocation_authority"],
                    attempt_bounds=payload["attempt_bounds"],
                )
            return
        if api == "build_focused_live_successor_catalog_v1":
            inputs = vector.get("builder_inputs", vector.get("value"))
            processes = (
                inputs.get("inventory_discovery_processes", [])
                if isinstance(inputs, dict)
                else []
            )
            with captured_file_state(processes):
                if isinstance(inputs, dict):
                    build_focused_live_successor_catalog_v1(**inputs)
                else:
                    build_focused_live_successor_catalog_v1(inputs)
            return

        payload = vector["value"]
        if not isinstance(payload, dict) or "catalog" not in payload:
            self.fail(
                f"semantic invalid vector {vector['case']} must carry a full trusted envelope"
            )
        semantic_source = payload
        catalog = payload["catalog"]
        processes = semantic_source["inventory_discovery_processes"]
        with captured_file_state(processes):
            validate_focused_live_successor_catalog_semantics_v1(
                catalog,
                **self._vector_semantic_kwargs(semantic_source),
            )

    def test_public_builder_parser_wire_and_semantic_validator(self) -> None:
        catalog = build_focused_live_successor_catalog_v1(**self.inputs)
        validate_focused_live_successor_catalog_wire_v1(catalog)
        raw = canonical_jcs(catalog)
        self.assertEqual(parse_focused_live_successor_catalog_v1(raw), catalog)
        validate_focused_live_successor_catalog_semantics_v1(
            catalog, **self._semantic_kwargs()
        )
        self.assertEqual(catalog["current_inventory_count"], 1)
        self.assertEqual(catalog["replacement_baseline_row_count"], 1)
        self.assertEqual(catalog["in_process_jest_discovery"]["discovered_count"], 1)

    def test_process_contract_order_xor_timestamp_and_stable_identity(self) -> None:
        validate_inventory_discovery_process_authority_v1(
            self.processes,
            invocation_authority=self.invocation_authority,
            attempt_bounds=self.attempt_bounds,
        )
        cases: list[tuple[str, Any]] = []
        swapped = copy.deepcopy(self.processes)
        swapped[0], swapped[1] = swapped[1], swapped[0]
        cases.append(("order", swapped))
        numeric_time = copy.deepcopy(self.processes)
        numeric_time[0]["child_process"]["started_at"] = 20
        cases.append(("numeric timestamp", numeric_time))
        leading_zero = copy.deepcopy(self.processes)
        leading_zero[0]["child_process"]["started_at"] = "020"
        cases.append(("leading-zero timestamp", leading_zero))
        masquerade = copy.deepcopy(self.processes)
        masquerade[0]["output"]["report_path"] = str(self.report_paths["inventory.root-unittest"])
        cases.append(("cross-shape output", masquerade))
        sentinel = copy.deepcopy(self.processes)
        sentinel[2]["output"]["report_identity"]["file_id_hex"] = "0" * 32
        cases.append(("zero file ID", sentinel))
        sentinel_f = copy.deepcopy(self.processes)
        sentinel_f[2]["output"]["report_identity"]["file_id_hex"] = "f" * 32
        cases.append(("all-f file ID", sentinel_f))
        for case, processes in cases:
            with self.subTest(case=case), self.assertRaises(FocusedLiveSuccessorCatalogError):
                validate_inventory_discovery_process_set_v1(
                    processes,
                )

        zero_volume = copy.deepcopy(self.processes)
        zero_volume[2]["output"]["report_identity"]["volume_serial_number_hex"] = "0" * 16
        validate_inventory_discovery_process_set_v1(zero_volume)

    def test_process_contract_rejects_noncanonical_windows_paths_and_report_argv(self) -> None:
        drive_root = f"{Path(self.cwd).drive}\\"
        root_cwd_processes = copy.deepcopy(self.processes)
        for process in root_cwd_processes:
            process["cwd"] = drive_root
        validate_inventory_discovery_process_set_v1(root_cwd_processes)

        reserved_devices = (
            "CON",
            "PRN",
            "AUX",
            "NUL",
            "CONIN$",
            "CONOUT$",
            "CLOCK$",
            *(f"COM{index}" for index in range(1, 10)),
            *(f"LPT{index}" for index in range(1, 10)),
        )
        bad_paths = (
            r"C:\relative\..\path",
            r"C:\repeated\\separator",
            "C:\\trailing-dot.",
            "C:\\trailing-space ",
            r"C:\stream:ads",
            r"C:\bad<name",
            r"C:\bad>name",
            'C:\\bad"name',
            r"C:\bad|name",
            r"C:\bad?name",
            r"C:\bad*name",
            *(f"C:\\{device}\\file" for device in reserved_devices),
            *(f"C:\\{device}.txt" for device in reserved_devices),
            r"\\server\share\file",
        )
        for bad_path in bad_paths:
            processes = copy.deepcopy(self.processes)
            processes[0]["cwd"] = bad_path
            with self.subTest(path=bad_path), self.assertRaises(FocusedLiveSuccessorCatalogError):
                validate_inventory_discovery_process_set_v1(processes)

        for bad_report_path in (*bad_paths, "C:\\"):
            processes = copy.deepcopy(self.processes)
            argv = [self.executable, "inventory.root-unittest", "--output", bad_report_path]
            processes[2]["argv"] = argv
            processes[2]["child_process"]["args_hash"] = hashlib.sha256(canonical_jcs(argv)).hexdigest()
            processes[2]["output"]["report_path"] = bad_report_path
            with self.subTest(report_path=bad_report_path), self.assertRaises(FocusedLiveSuccessorCatalogError):
                validate_inventory_discovery_process_set_v1(processes)

        for argv in (
            [self.executable, "inventory.root-unittest"],
            [self.executable, "--output", str(self.report_paths["inventory.root-unittest"]), "--output", str(self.report_paths["inventory.root-unittest"])],
            [self.executable, "--output", str(self.report_paths["inventory.sdk-python-pytest"])],
        ):
            processes = copy.deepcopy(self.processes)
            processes[2]["argv"] = argv
            processes[2]["child_process"]["args_hash"] = hashlib.sha256(canonical_jcs(argv)).hexdigest()
            with self.subTest(argv=argv), self.assertRaises(FocusedLiveSuccessorCatalogError):
                validate_inventory_discovery_process_set_v1(processes)

    def test_parser_rejects_noncanonical_duplicate_nonfinite_and_floats(self) -> None:
        catalog = build_focused_live_successor_catalog_v1(**self.inputs)
        raw = canonical_jcs(catalog)
        duplicate = b'{"format_id":"a","format_id":"b"}'
        for case in (
            b" " + raw,
            duplicate,
            b'{"value":NaN}',
            b'{"value":Infinity}',
            b'{"value":1.0}',
            b"\xef\xbb\xbf" + raw,
        ):
            with self.subTest(raw=case[:32]), self.assertRaises(FocusedLiveSuccessorCatalogError):
                parse_focused_live_successor_catalog_v1(case)

    def test_empty_repository_catalog_is_valid(self) -> None:
        inputs = copy.deepcopy(self.inputs)
        inputs.update(
            replacement_baseline_row_count=0,
            current_inventory=[],
            resolved_successor_entries=[],
            execution_input_contracts=[],
            cargo_target_context_specs=[],
            successor_owner_map=[],
            jest_discovered_ids=[],
        )
        inputs["replacement_successor_catalog"]["successors"] = []
        inputs["jest_observation"]["discovered_count"] = 0
        inputs["jest_observation"]["discovered_test_ids_sha256"] = proof_hash(
            "kd4.in-process-jest-discovered-id-set.v1", []
        )
        catalog = build_focused_live_successor_catalog_v1(**inputs)
        self.assertEqual(catalog["replacement_baseline_row_count"], 0)
        self.assertEqual(catalog["distinct_successor_count"], 0)
        self.assertEqual(catalog["current_inventory_count"], 0)
        self.assertEqual(catalog["in_process_jest_discovery"]["discovered_count"], 0)
        self.assertEqual(parse_focused_live_successor_catalog_v1(canonical_jcs(catalog)), catalog)
        semantic_kwargs = self._semantic_kwargs()
        for key in (
            "current_inventory",
            "resolved_successor_entries",
            "execution_input_contracts",
            "cargo_target_context_specs",
            "successor_owner_map",
            "jest_discovered_ids",
            "jest_observation",
            "replacement_successor_catalog",
        ):
            semantic_kwargs[key] = inputs[key]
        semantic_kwargs["expected_replacement_baseline_row_count"] = 0
        validate_focused_live_successor_catalog_semantics_v1(catalog, **semantic_kwargs)

    def test_semantic_validator_rejects_each_independent_authority_mismatch(self) -> None:
        catalog = build_focused_live_successor_catalog_v1(**self.inputs)
        scalar_cases = {
            "expected_replacement_baseline_row_count": 2,
            "expected_frozen_inventory_hash": "3" * 64,
            "expected_start_fingerprint": "4" * 64,
            "expected_start_mutation_epoch": 8,
        }
        for name, replacement in scalar_cases.items():
            kwargs = self._semantic_kwargs()
            kwargs[name] = replacement
            with self.subTest(name=name), self.assertRaises(FocusedLiveSuccessorCatalogError):
                validate_focused_live_successor_catalog_semantics_v1(catalog, **kwargs)

        stale_jest = self._semantic_kwargs()
        stale_jest["jest_observation"] = copy.deepcopy(stale_jest["jest_observation"])
        stale_jest["jest_observation"]["discovered_count"] = 999
        with self.assertRaises(FocusedLiveSuccessorCatalogError):
            validate_focused_live_successor_catalog_semantics_v1(catalog, **stale_jest)

        route_mismatch_catalog = copy.deepcopy(catalog)
        route_mismatch_catalog["current_inventory"][0]["framework"] = "python-pytest"
        route_mismatch_catalog["replacement_successor_catalog"]["successors"][0][
            "framework"
        ] = "python-pytest"
        route_mismatch_catalog["in_process_jest_discovery"]["discovered_count"] = 0
        route_mismatch_catalog["in_process_jest_discovery"][
            "discovered_test_ids_sha256"
        ] = proof_hash("kd4.in-process-jest-discovered-id-set.v1", [])
        route_mismatch_catalog["current_inventory_hash"] = hashlib.sha256(
            canonical_jcs(
                {
                    "schema_version": 1,
                    "tests": route_mismatch_catalog["current_inventory"],
                }
            )
        ).hexdigest()
        route_mismatch_catalog["semantic_sha256"] = proof_hash(
            "kd4.focused-live-successor-catalog.v1.semantic",
            {
                key: value
                for key, value in route_mismatch_catalog.items()
                if key != "semantic_sha256"
            },
        )
        route_mismatch_authority = self._semantic_kwargs()
        route_mismatch_authority["current_inventory"] = copy.deepcopy(
            route_mismatch_catalog["current_inventory"]
        )
        route_mismatch_authority["replacement_successor_catalog"] = copy.deepcopy(
            route_mismatch_catalog["replacement_successor_catalog"]
        )
        route_mismatch_authority["jest_observation"] = copy.deepcopy(
            route_mismatch_catalog["in_process_jest_discovery"]
        )
        route_mismatch_authority["jest_discovered_ids"] = []
        with self.assertRaises(FocusedLiveSuccessorCatalogError):
            validate_focused_live_successor_catalog_semantics_v1(
                route_mismatch_catalog, **route_mismatch_authority
            )

        invalid_identifier_inputs = copy.deepcopy(self.inputs)
        invalid_identifier_inputs["replacement_successor_catalog"]["successors"][0][
            "validation_id"
        ] = "sdk/typescript/jest"
        with self.assertRaises(FocusedLiveSuccessorCatalogError):
            build_focused_live_successor_catalog_v1(**invalid_identifier_inputs)

    def _assert_invalid_vector_router_fails_closed_and_routes_every_public_api(self) -> None:
        base = copy.deepcopy(self.vectors["valid_vectors"][0])
        semantic_payload = copy.deepcopy(base)
        semantic_payload["catalog"] = []
        routed = (
            {
                "case": "meta-parse-value",
                "kind": "value",
                "api": "parse_focused_live_successor_catalog_v1",
                "value": "not-bytes",
            },
            {
                "case": "meta-wire-value",
                "kind": "value",
                "api": "validate_focused_live_successor_catalog_wire_v1",
                "value": [],
            },
            {
                "case": "meta-process-value",
                "kind": "value",
                "api": "validate_inventory_discovery_process_set_v1",
                "value": [],
            },
            {
                "case": "meta-process-authority-value",
                "kind": "value",
                "api": "validate_inventory_discovery_process_authority_v1",
                "value": {
                    "processes": [],
                    "invocation_authority": base["invocation_authority"],
                    "attempt_bounds": base["attempt_bounds"],
                },
            },
            {
                "case": "meta-builder-value",
                "kind": "value",
                "api": "build_focused_live_successor_catalog_v1",
                "value": {},
            },
            {
                "case": "meta-semantic-value",
                "kind": "value",
                "api": "validate_focused_live_successor_catalog_semantics_v1",
                "value": semantic_payload,
            },
        )
        for vector in routed:
            with self.subTest(api=vector["api"]), self.assertRaises(
                FocusedLiveSuccessorCatalogError
            ):
                self._invoke_invalid_vector(vector)

        for malformed in (
            {"case": "unknown-kind", "kind": "unknown", "api": None},
            {"case": "unknown-api", "kind": "value", "api": "unknown", "value": None},
            {
                "case": "kind-api-mismatch",
                "kind": "catalog",
                "api": "parse_focused_live_successor_catalog_v1",
                "value": {},
            },
            {
                "case": "process-authority-envelope-extra",
                "kind": "process-authority",
                "api": "validate_inventory_discovery_process_authority_v1",
                "value": {
                    "processes": [],
                    "invocation_authority": base["invocation_authority"],
                    "attempt_bounds": base["attempt_bounds"],
                    "extra": True,
                },
            },
        ):
            with self.subTest(case=malformed["case"]), self.assertRaises(AssertionError):
                self._invoke_invalid_vector(malformed)

    def test_schema_and_cross_language_vectors_are_consumed_read_only(self) -> None:
        self.assertEqual(self.schema["additionalProperties"], False)
        self.assertEqual(len(self.schema["required"]), 22)
        self.assertEqual(
            self.vectors["format_id"],
            "kd4.focused-live-successor-catalog.v1.test-vectors",
        )
        for vector in self.vectors["valid_vectors"]:
            with self.subTest(case=vector["case"]):
                catalog = vector["catalog"]
                validate_focused_live_successor_catalog_wire_v1(catalog)
                raw = vector["canonical_catalog_json"].encode("utf-8")
                self.assertEqual(parse_focused_live_successor_catalog_v1(raw), catalog)
                self.assertEqual(
                    vector["canonical_inventory_discovery_processes_json"].encode("utf-8"),
                    canonical_jcs(vector["inventory_discovery_processes"]),
                )
                expected_hashes = vector["expected_hashes"]
                for field in (
                    "semantic_sha256",
                    "successor_ids_sha256",
                    "successor_owner_map_sha256",
                    "current_inventory_hash",
                    "resolved_successor_entries_sha256",
                    "inventory_discovery_processes_sha256",
                ):
                    self.assertEqual(catalog[field], expected_hashes[field])
                self.assertEqual(
                    catalog["in_process_jest_discovery"]["discovered_test_ids_sha256"],
                    expected_hashes["in_process_jest_discovered_ids_sha256"],
                )
                with captured_file_state(vector["inventory_discovery_processes"]):
                    self.assertEqual(
                        build_focused_live_successor_catalog_v1(
                            **self._vector_builder_inputs(vector)
                        ),
                        catalog,
                    )
                    validate_focused_live_successor_catalog_semantics_v1(
                        catalog,
                        **self._vector_semantic_kwargs(vector),
                    )
        for vector in self.vectors["invalid_vectors"]:
            self.assertEqual(vector["expected"], "reject")
            with self.subTest(case=vector["case"]), self.assertRaises(FocusedLiveSuccessorCatalogError):
                self._invoke_invalid_vector(vector)
        self._assert_invalid_vector_router_fails_closed_and_routes_every_public_api()


if __name__ == "__main__":
    unittest.main()
