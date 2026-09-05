from __future__ import annotations

import argparse
import ast
import base64
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from typing import Any
import uuid

_REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
if str(_REPOSITORY_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPOSITORY_ROOT))

from scripts.completion_proof_inventory_v2 import ActiveHostApplicabilityIssuerV1
from scripts.completion_proof_inventory_v2 import DOCTEST_RECAPTURE_PACKAGE_SPECS
from scripts.completion_proof_inventory_v2 import DOCTEST_RECAPTURE_PARENT_TARGETS
from scripts.completion_proof_inventory_v2 import FROZEN_V1_BASELINE_ASSOCIATIONS_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_BASELINE_IDS_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_INVENTORY_RAW_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_INVENTORY_SEMANTIC_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_LEDGER_RAW_SHA256
from scripts.completion_proof_inventory_v2 import FROZEN_V1_WORKSPACE_FINGERPRINT
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_IDS
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_PATHS
from scripts.completion_proof_inventory_v2 import INVENTORY_V2_SCHEMA_RAW_SHA256S
from scripts.completion_proof_inventory_v2 import InventoryV2ContractError
from scripts.completion_proof_inventory_v2 import canonical_jcs
from scripts.completion_proof_inventory_v2 import doctest_recovered_child_sources_v1
from scripts.completion_proof_inventory_v2 import derive_frozen_v1_historical_replacement_graph_v1
from scripts.completion_proof_inventory_v2 import frozen_baseline_obligation_id_v2
from scripts.completion_proof_inventory_v2 import inventory_declaration_id_v2
from scripts.completion_proof_inventory_v2 import inventory_declaration_obligation_id_v2
from scripts.completion_proof_inventory_v2 import proof_hash
from scripts.completion_proof_inventory_v2 import validate_frozen_test_inventory_v2
from scripts.completion_proof_inventory_v2 import validate_doctest_recapture_packet_v1
from scripts.completion_proof_inventory_v2 import validate_inventory_ledger_predecessor_closure
from scripts.completion_proof_inventory_v2 import validate_inventory_recovery_authority_v1
from scripts.completion_proof_inventory_v2 import validate_recovery_transition_receipt_v1
from scripts.completion_proof_inventory_v2 import validate_runner_selector_v1
from scripts.completion_proof_inventory_v2 import validate_test_replacement_ledger_v2
from scripts.completion_proof_inventory_v2 import validate_unittest_recapture_packet_v1
from scripts.completion_proof_inventory_v2 import validate_unittest_source_provenance_exception_v1
from scripts.completion_proof_inventory_v2 import unittest_executable_parent_records_v1
from scripts.completion_proof_inventory_v2 import unittest_recovered_child_sources_v1
from scripts.completion_proof_inventory_v2 import unittest_subtest_manifests_v1
from scripts.rust_test_runner import Manifest
from scripts.rust_test_runner import RunnerError
from scripts.rust_test_runner import Target


V1_INVENTORY_PATH = ".codex/validation/frozen-test-inventory-v1.json"
V1_LEDGER_PATH = ".codex/validation/test-replacements-v1.json"
V2_INVENTORY_PATH = ".codex/validation/frozen-test-inventory-v2.json"
V2_RECOVERY_PATH = ".codex/validation/frozen-test-inventory-v2-recoveries.json"
V2_DOCTEST_RECAPTURE_PATH = (
    ".codex/validation/frozen-test-inventory-v2-doctest-recapture.json"
)
V2_UNITTEST_RECAPTURE_PATH = (
    ".codex/validation/frozen-test-inventory-v2-unittest-recapture.json"
)
V2_UNITTEST_SOURCE_EXCEPTIONS_PATH = ".codex/validation/frozen-test-inventory-v2-unittest-source-exceptions.json"
V2_RECOVERY_TRANSITION_RECEIPTS_PATH = (
    ".codex/validation/frozen-test-inventory-v2-recovery-transition-receipts.json"
)
V2_LEDGER_PATH = ".codex/validation/test-replacements-v2.json"
RUST_TEST_MANIFEST_PATH = "codex-rs/.config/kd4-rust-tests.toml"
BASELINE_COMMIT = "60bb133fa0a4f25e83851ab16d8c462e5f42ff95"
EXPECTED_BASELINE_COUNT = 15_544
EXPECTED_DECLARATION_COUNT = 15_548
EXPECTED_SOURCE_TREE_SHA256 = (
    "654591dd1ddda7a77312172ec7c70e60e80990590c7c74a7c3b08a445279d90e"
)
EXPECTED_REPOSITORY_IDENTITY_SHA256 = (
    "f386e4786f3a61829ecdd61e764fa9d65eddbd08f2902745c6480bce448573cc"
)
RECAPTURE_TOOLCHAIN = "1.95.0-x86_64-pc-windows-msvc"
EXPECTED_UNITTEST_LEDGER_IDENTITY_COUNT = 909
EXPECTED_UNITTEST_RECAPTURE_PARENT_COUNT = 893
EXPECTED_UNITTEST_HIDDEN_REPLACEMENT_COUNT = 16
UNITTEST_RECAPTURE_UNAVAILABLE = (
    "authenticated freeze workspace is unavailable; refusing bare-commit unittest "
    "recapture"
)

ROUTE_VALIDATION_IDS = {
    "argument-comment-lint-native": "tools.argument-comment-lint.native",
    "javascript-jest": "sdk.typescript.jest",
    "python-pytest": "sdk.python.pytest",
    "python-unittest": "maintenance.root-unittest",
    "rust-doctest": "rust.doctest.workspace",
    "rust-nextest": "rust.nextest.workspace",
    "windows-sandbox-smoke-native": "windows.sandbox-smoke",
}
V1_TO_V2_RUNNER_KIND = {
    "argument-comment-lint-native": "argument-comment-lint-native",
    "javascript-jest": "javascript-jest",
    "python-pytest": "python-pytest",
    "python-unittest": "python-unittest",
    "rust-doctest": "rust-doctest",
    "rust-nextest": "rust-nextest",
    "windows-sandbox-smoke": "windows-sandbox-smoke-native",
}
ARGUMENT_COMMENT_LINT_UI_CONSUMED_PATHS = {
    "tools/argument-comment-lint/ui/comment_mismatch.rs": (
        "tools/argument-comment-lint/ui/comment_mismatch.stderr"
    ),
    "tools/argument-comment-lint/ui/multiple_method_arguments.rs": (
        "tools/argument-comment-lint/ui/multiple_method_arguments.stderr"
    ),
    "tools/argument-comment-lint/ui/uncommented_literal.rs": (
        "tools/argument-comment-lint/ui/uncommented_literal.stderr"
    ),
}

SOURCE_ONLY_SPECS = (
    {
        "canonical_id": (
            "rust-nextest::codex-http-client::codex_http_client$"
            "outbound_proxy::tests::unsupported_platform_system_proxy_falls_back_explicitly"
        ),
        "line": 328,
        "native_id": (
            "codex-http-client::codex_http_client$outbound_proxy::tests::"
            "unsupported_platform_system_proxy_falls_back_explicitly"
        ),
        "required_hosts": ["linux"],
        "source_path": "codex-rs/http-client/src/outbound_proxy_tests.rs",
    },
    {
        "canonical_id": (
            "rust-nextest::codex-core::codex_core$tools::command_output_artifact::"
            "hardening_tests::read_rejects_uuid_named_symlink_outside_thread_directory"
        ),
        "line": 5985,
        "native_id": (
            "codex-core::codex_core$tools::command_output_artifact::hardening_tests::"
            "read_rejects_uuid_named_symlink_outside_thread_directory"
        ),
        "required_hosts": ["darwin", "linux"],
        "source_path": "codex-rs/core/src/tools/command_output_artifact.rs",
    },
    {
        "canonical_id": (
            "rust-nextest::codex-core::turn_latency_bench$turn_latency::tests::"
            "ab_worker_tree_cleanup_process_group_survives_root_exit"
        ),
        "line": 6010,
        "native_id": (
            "codex-core::turn_latency_bench$turn_latency::tests::"
            "ab_worker_tree_cleanup_process_group_survives_root_exit"
        ),
        "required_hosts": ["darwin", "linux"],
        "source_path": "codex-rs/core/benches/turn_latency/tests.rs",
    },
)

POST_BASELINE_CURRENT_SPECS = (
    {
        "canonical_id": (
            "python-unittest::scripts.test_completion_proof_typed_canonical."
            "CanonicalTypedJournalIntegrationTest."
            "test_canonical_attempt_consumes_real_broker_journal"
        ),
        "line": 59,
        "native_id": (
            "scripts.test_completion_proof_typed_canonical."
            "CanonicalTypedJournalIntegrationTest."
            "test_canonical_attempt_consumes_real_broker_journal"
        ),
        "required_hosts": ["darwin", "linux", "windows"],
        "source_path": "scripts/test_completion_proof_typed_canonical.py",
    },
)

UNITTEST_RUNNER_SITES = [
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

OWNED_ARTIFACT_PATTERNS = (
    "frozen-test-inventory-v2*.json",
    "test-replacements-v2*.json",
)

_JEST_CONFIG_PATH = "sdk/typescript/jest.config.cjs"
_JEST_OBSERVATION_FIELDS = frozenset(
    {
        "ancestor_titles",
        "column",
        "config_path",
        "file_path",
        "full_title",
        "line",
        "registration_ordinal",
    }
)
_JEST_REPORT_FIELDS = frozenset(
    {
        "complete",
        "num_failed_tests",
        "num_passed_tests",
        "num_pending_tests",
        "num_total_tests",
        "observations",
        "schema_version",
    }
)
_JEST_OBSERVER_TIMEOUT_SECONDS = 180
_JEST_DIAGNOSTIC_LIMIT = 4_000


class MaterializationError(RuntimeError):
    pass


def _read_frozen_json(path: Path, expected_sha256: str) -> tuple[bytes, dict[str, Any]]:
    raw = path.read_bytes()
    actual = hashlib.sha256(raw).hexdigest()
    if actual != expected_sha256:
        raise MaterializationError(
            f"frozen input hash mismatch for {path}: expected {expected_sha256}, got {actual}"
        )
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise MaterializationError(f"frozen input must be an object: {path}")
    return raw, value


def _execution_input_contract(*, owned: list[dict[str, Any]], consumed: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    projection = {
        "consumed": sorted(consumed or [], key=canonical_jcs),
        "owned": sorted(owned, key=canonical_jcs),
        "schema_version": 1,
    }
    digest = proof_hash("kd4.execution-input-contract.v1", projection)
    return {
        **projection,
        "contract_id": f"execution-input-contract-v1.{digest}",
        "contract_sha256": digest,
    }


def _cargo_context(
    *,
    manifest_path: Path,
    package_name: str,
    target_kind: str,
    target_name: str,
    target_source_path: Path,
    repo_root: Path,
    features: tuple[str, ...] = (),
) -> dict[str, Any]:
    projection = {
        "cargo_profile": "test",
        "feature_selection": {
            "additional_features": sorted(features),
            "kind": "default",
        },
        "package_manifest_path": manifest_path.relative_to(repo_root).as_posix(),
        "package_name": package_name,
        "schema_version": 1,
        "target_kind": target_kind,
        "target_name": target_name,
        "target_source_path": target_source_path.relative_to(repo_root).as_posix(),
        "workspace_manifest_path": "codex-rs/Cargo.toml",
    }
    return {
        **projection,
        "context_sha256": proof_hash("kd4.cargo-target-context-spec.v1", projection),
    }


def _manifest_index(repo_root: Path) -> dict[str, tuple[Path, dict[str, Any]]]:
    result: dict[str, tuple[Path, dict[str, Any]]] = {}
    for manifest in sorted((repo_root / "codex-rs").rglob("Cargo.toml")):
        value = tomllib.loads(manifest.read_text(encoding="utf-8"))
        name = value.get("package", {}).get("name")
        if isinstance(name, str):
            if name in result:
                raise MaterializationError(f"duplicate Cargo package name: {name}")
            result[name] = (manifest, value)
    return result


def _cargo_targets(manifest: Path, data: dict[str, Any]) -> list[tuple[str, str, Path]]:
    base = manifest.parent
    package_name = data["package"]["name"]
    result: list[tuple[str, str, Path]] = []
    lib = data.get("lib")
    if isinstance(lib, dict) or (base / "src/lib.rs").is_file():
        lib = lib if isinstance(lib, dict) else {}
        result.append(
            (
                str(lib.get("name", package_name.replace("-", "_"))),
                "proc-macro" if lib.get("proc-macro") else "lib",
                base / str(lib.get("path", "src/lib.rs")),
            )
        )
    for target_kind, table_name, folder in (
        ("bin", "bin", "src/bin"),
        ("test", "test", "tests"),
        ("bench", "bench", "benches"),
        ("example", "example", "examples"),
    ):
        for target in data.get(table_name, []):
            result.append(
                (
                    str(target["name"]),
                    target_kind,
                    base / str(target.get("path", f"{folder}/{target['name']}.rs")),
                )
            )
        auto_root = base / folder
        if auto_root.is_dir():
            result.extend((path.stem, target_kind, path) for path in auto_root.glob("*.rs"))
            result.extend((path.parent.name, target_kind, path) for path in auto_root.glob("*/main.rs"))
    if (base / "src/main.rs").is_file():
        result.append((package_name, "bin", base / "src/main.rs"))
    unique = {(name, kind, path.resolve()): (name, kind, path) for name, kind, path in result}
    return list(unique.values())


def _resolve_cargo_target(
    package_name: str,
    binary_name: str,
    manifests: dict[str, tuple[Path, dict[str, Any]]],
) -> tuple[Path, str, str, Path]:
    try:
        manifest, data = manifests[package_name]
    except KeyError as exc:
        raise MaterializationError(f"no checked-in manifest for Cargo package {package_name}") from exc
    targets = _cargo_targets(manifest, data)
    matches = [target for target in targets if target[0] == binary_name]
    if not matches and binary_name.endswith("_bench"):
        matches = [
            target for target in targets
            if target[0] == binary_name.removesuffix("_bench") and target[1] == "bench"
        ]
    if len(matches) != 1:
        raise MaterializationError(
            f"Cargo binary {package_name}::{binary_name} mapped to {len(matches)} targets"
        )
    target_name, target_kind, source = matches[0]
    if not source.is_file():
        raise MaterializationError(f"Cargo target source does not exist: {source}")
    return manifest, target_kind, target_name, source


def _resolve_manifest_target(
    target: Target,
    manifests: dict[str, tuple[Path, dict[str, Any]]],
) -> tuple[Path, str, str, Path]:
    try:
        manifest, data = manifests[target.package]
    except KeyError as exc:
        raise MaterializationError(
            f"Rust test manifest target {target.name!r} declares missing Cargo package "
            f"{target.package!r}"
        ) from exc
    cargo_targets = _cargo_targets(manifest, data)
    if target.selector_kind == "lib":
        matches = [item for item in cargo_targets if item[1] in {"lib", "proc-macro"}]
    else:
        matches = [
            item
            for item in cargo_targets
            if item[0] == target.selector_value and item[1] == target.selector_kind
        ]
    if len(matches) != 1:
        raise MaterializationError(
            f"Rust test manifest target {target.name!r} mapped to {len(matches)} Cargo targets"
        )
    target_name, target_kind, source = matches[0]
    if not source.is_file():
        raise MaterializationError(
            f"Rust test manifest target {target.name!r} source does not exist: {source}"
        )
    return manifest, target_kind, target_name, source


def _manifest_feature_contexts(
    repo_root: Path,
    manifests: dict[str, tuple[Path, dict[str, Any]]],
) -> tuple[
    bytes,
    dict[tuple[str, str, str], tuple[str, ...]],
    dict[str, tuple[str, Path, str, str, Path]],
]:
    manifest_path = repo_root / RUST_TEST_MANIFEST_PATH
    try:
        manifest = Manifest.load(manifest_path)
    except RunnerError as exc:
        raise MaterializationError(str(exc)) from exc
    raw = manifest_path.read_bytes()
    feature_contexts: dict[tuple[str, str, str], tuple[str, ...]] = {}
    owners: dict[tuple[str, str, str], str] = {}
    resolved_targets: dict[str, tuple[str, Path, str, str, Path]] = {}
    for name, target in manifest.targets.items():
        resolved = _resolve_manifest_target(target, manifests)
        resolved_targets[name] = (target.package, *resolved)
        _, target_kind, target_name, _ = resolved
        identity = (target.package, target_kind, target_name)
        features = tuple(sorted(target.features))
        prior = feature_contexts.get(identity)
        if prior is not None and prior != features:
            raise MaterializationError(
                "Rust test manifest has ambiguous feature selections for Cargo target "
                f"{target.package}::{target_kind}/{target_name}: "
                f"{owners[identity]!r} declares {list(prior)!r}, "
                f"{name!r} declares {list(features)!r}"
            )
        feature_contexts[identity] = features
        owners[identity] = name
    return raw, feature_contexts, resolved_targets


def _bounded_jest_diagnostic(value: object) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        text = value.decode("utf-8", errors="replace")
    else:
        text = str(value)
    text = text.strip()
    if len(text) <= _JEST_DIAGNOSTIC_LIMIT:
        return text
    return f"<truncated> {text[-_JEST_DIAGNOSTIC_LIMIT:]}"


def _write_jest_observer_modules(
    stage: Path, repo_root: Path, config_path: Path
) -> tuple[Path, Path, Path]:
    journal_path = stage / "observations.jsonl"
    report_path = stage / "report.json"
    environment_path = stage / "environment.cjs"
    reporter_path = stage / "reporter.cjs"

    environment_source = r'''"use strict";

const fs = require("node:fs");
const path = require("node:path");
const { createRequire } = require("node:module");
const { fileURLToPath } = require("node:url");

const configPath = __CONFIG_PATH__;
const repoRoot = __REPO_ROOT__;
const journalPath = __JOURNAL_PATH__;
const repoRequire = createRequire(configPath);
const environmentModule = repoRequire("jest-environment-node");
const NodeEnvironment =
  environmentModule.TestEnvironment ?? environmentModule.default ?? environmentModule;
const StackUtils = repoRequire("stack-utils");

function canonicalPath(value) {
  let candidate = value;
  if (candidate.startsWith("file://")) {
    candidate = fileURLToPath(candidate);
  }
  const resolved = path.resolve(candidate);
  return process.platform === "win32" ? resolved.toLowerCase() : resolved;
}

class Kd4JestObservationEnvironment extends NodeEnvironment {
  constructor(config, context) {
    super(config, context);
    this.testPath = path.resolve(context.testPath);
    this.stackUtils = new StackUtils({ cwd: path.dirname(configPath) });
    this.ordinals = new Map();
  }

  handleTestEvent(event, state) {
    if (event.name === "test_fn_start" || event.name === "hook_start") {
      throw new Error(`Jest observer attempted to execute ${event.name}`);
    }
    if (event.name !== "add_test") {
      return;
    }

    const tests = state.currentDescribeBlock.tests;
    const testEntry = tests[tests.length - 1];
    if (!testEntry || testEntry.name !== event.testName) {
      throw new Error(`Circus add_test state mismatch for ${event.testName}`);
    }

    // The Circus state handler runs before the environment handler. Rewriting
    // the registered entry here observes real registration while ensuring that
    // no test body or before/after hook can run.
    testEntry.mode = "skip";

    const expectedPath = canonicalPath(this.testPath);
    let location = null;
    const stack = event.asyncError && event.asyncError.stack;
    for (const line of typeof stack === "string" ? stack.split(/\r?\n/) : []) {
      const frame = this.stackUtils.parseLine(line);
      if (!frame || typeof frame.file !== "string") {
        continue;
      }
      if (canonicalPath(frame.file) === expectedPath) {
        location = frame;
        break;
      }
    }
    if (
      !location ||
      !Number.isInteger(location.line) ||
      location.line < 1 ||
      !Number.isInteger(location.column) ||
      location.column < 1
    ) {
      throw new Error(`Jest registration location is unavailable for ${this.testPath}`);
    }

    const ancestorTitles = [];
    for (let block = testEntry.parent; block && block.parent; block = block.parent) {
      ancestorTitles.unshift(block.name);
    }
    const filePath = path.relative(repoRoot, this.testPath).split(path.sep).join("/");
    const fullTitle = [...ancestorTitles, testEntry.name].join(" ");
    const nativeId = `${filePath}\u0000${fullTitle}`;
    const registrationOrdinal = this.ordinals.get(nativeId) ?? 0;
    this.ordinals.set(nativeId, registrationOrdinal + 1);

    fs.appendFileSync(
      journalPath,
      `${JSON.stringify({
        ancestor_titles: ancestorTitles,
        column: location.column,
        config_path: "sdk/typescript/jest.config.cjs",
        file_path: filePath,
        full_title: fullTitle,
        line: location.line,
        registration_ordinal: registrationOrdinal,
      })}\n`,
      "utf8",
    );
  }
}

module.exports = Kd4JestObservationEnvironment;
'''
    environment_source = (
        environment_source.replace("__CONFIG_PATH__", json.dumps(str(config_path)))
        .replace("__REPO_ROOT__", json.dumps(str(repo_root)))
        .replace("__JOURNAL_PATH__", json.dumps(str(journal_path)))
    )

    reporter_source = r'''"use strict";

const fs = require("node:fs");

const journalPath = __JOURNAL_PATH__;
const reportPath = __REPORT_PATH__;

class Kd4JestObservationReporter {
  onRunStart() {
    fs.writeFileSync(journalPath, "", "utf8");
  }

  onRunComplete(_contexts, results) {
    const raw = fs.readFileSync(journalPath, "utf8");
    const observations = raw.trim()
      ? raw.trim().split(/\r?\n/).map((line) => JSON.parse(line))
      : [];
    const report = {
      complete: true,
      num_failed_tests: results.numFailedTests,
      num_passed_tests: results.numPassedTests,
      num_pending_tests: results.numPendingTests,
      num_total_tests: results.numTotalTests,
      observations,
      schema_version: 1,
    };
    const temporaryPath = `${reportPath}.${process.pid}.tmp`;
    fs.writeFileSync(
      temporaryPath,
      JSON.stringify(report),
      { encoding: "utf8", flag: "wx" },
    );
    fs.renameSync(temporaryPath, reportPath);
  }
}

module.exports = Kd4JestObservationReporter;
'''
    reporter_source = reporter_source.replace(
        "__JOURNAL_PATH__", json.dumps(str(journal_path))
    ).replace("__REPORT_PATH__", json.dumps(str(report_path)))

    environment_path.write_text(environment_source, encoding="utf-8", newline="\n")
    reporter_path.write_text(reporter_source, encoding="utf-8", newline="\n")
    return environment_path, reporter_path, report_path


def _run_jest_observer(repo_root: Path) -> dict[str, Any]:
    repo_root = repo_root.resolve()
    config_path = repo_root / _JEST_CONFIG_PATH
    jest_path = repo_root / "node_modules/jest/bin/jest.js"
    for label, path in (("Jest config", config_path), ("Jest CLI", jest_path)):
        if not path.is_file():
            raise MaterializationError(f"{label} is unavailable: {path}")

    with tempfile.TemporaryDirectory(prefix="kd4-jest-observer-") as temp:
        stage = Path(temp)
        environment_path, reporter_path, report_path = _write_jest_observer_modules(
            stage, repo_root, config_path
        )
        command = [
            "node",
            str(jest_path),
            "--config",
            str(config_path),
            "--runInBand",
            "--no-cache",
            "--env",
            str(environment_path),
            "--reporters",
            "default",
            "--reporters",
            str(reporter_path),
        ]
        environment = os.environ.copy()
        environment["CI"] = "1"
        environment["RUN_REAL_CODEX_TESTS"] = "0"
        try:
            completed = subprocess.run(
                command,
                cwd=config_path.parent,
                check=False,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=_JEST_OBSERVER_TIMEOUT_SECONDS,
                env=environment,
            )
        except subprocess.TimeoutExpired as error:
            diagnostic = _bounded_jest_diagnostic(error.stderr or error.stdout)
            suffix = f": {diagnostic}" if diagnostic else ""
            raise MaterializationError(
                "Jest observation timed out after "
                f"{_JEST_OBSERVER_TIMEOUT_SECONDS} seconds{suffix}"
            ) from error
        except OSError as error:
            raise MaterializationError(
                f"Jest observation could not start: {error}"
            ) from error

        if completed.returncode != 0:
            diagnostic = _bounded_jest_diagnostic(completed.stderr or completed.stdout)
            suffix = f": {diagnostic}" if diagnostic else ""
            raise MaterializationError(
                f"Jest observation failed with exit code {completed.returncode}{suffix}"
            )
        try:
            raw_report = report_path.read_text(encoding="utf-8")
        except OSError as error:
            raise MaterializationError(
                "Jest observation completed without a finalized report"
            ) from error
        try:
            report = json.loads(raw_report)
        except json.JSONDecodeError as error:
            raise MaterializationError(
                "Jest observation report is not valid JSON"
            ) from error
    if not isinstance(report, dict):
        raise MaterializationError("Jest observation report must be an object")
    return report


def _jest_selector_index(repo_root: Path) -> dict[str, dict[str, Any]]:
    repo_root = repo_root.resolve()
    report = _run_jest_observer(repo_root)
    if set(report) != _JEST_REPORT_FIELDS:
        raise MaterializationError(
            "Jest observation report fields mismatch: "
            f"expected {sorted(_JEST_REPORT_FIELDS)!r}, got {sorted(report)!r}"
        )
    schema_version = report["schema_version"]
    if (
        not isinstance(schema_version, int)
        or isinstance(schema_version, bool)
        or schema_version != 1
        or report["complete"] is not True
    ):
        raise MaterializationError("Jest observation report is not finalized")
    for field in (
        "num_failed_tests",
        "num_passed_tests",
        "num_pending_tests",
        "num_total_tests",
    ):
        value = report[field]
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise MaterializationError(f"Jest observation report {field} is invalid")
    observations = report["observations"]
    if not isinstance(observations, list):
        raise MaterializationError(
            "Jest observation report observations must be an array"
        )
    total = report["num_total_tests"]
    if total == 0:
        raise MaterializationError("Jest observation discovered no tests")
    if len(observations) != total:
        raise MaterializationError(
            "Jest observation count mismatch: "
            f"observed {len(observations)}, Jest reported {total}"
        )
    if (
        report["num_pending_tests"] != total
        or report["num_passed_tests"] != 0
        or report["num_failed_tests"] != 0
    ):
        raise MaterializationError(
            "Jest observation executed a test body or hook instead of skipping all "
            "tests"
        )

    result: dict[str, dict[str, Any]] = {}
    seen_selectors: set[tuple[str, str, int]] = set()
    for index, observation in enumerate(observations):
        if not isinstance(observation, dict):
            raise MaterializationError(
                f"Jest observation {index} must be an object"
            )
        if set(observation) != _JEST_OBSERVATION_FIELDS:
            raise MaterializationError(
                f"Jest observation {index} fields mismatch: "
                f"expected {sorted(_JEST_OBSERVATION_FIELDS)!r}, "
                f"got {sorted(observation)!r}"
            )
        selector = {"kind": "javascript-jest", **observation}
        try:
            validate_runner_selector_v1(selector)
        except InventoryV2ContractError as error:
            raise MaterializationError(
                f"Jest observation {index} violates RunnerSelectorV1: {error}"
            ) from error
        if selector["config_path"] != _JEST_CONFIG_PATH:
            raise MaterializationError(
                f"Jest observation {index} used unexpected config_path"
            )
        test_path = (repo_root / selector["file_path"]).resolve()
        try:
            test_path.relative_to(repo_root)
        except ValueError as error:
            raise MaterializationError(
                f"Jest observation {index} resolves outside the repository"
            ) from error
        if not test_path.is_file():
            raise MaterializationError(
                f"Jest observation {index} source does not exist: "
                f"{selector['file_path']}"
            )

        native_id = f"{selector['file_path']}::{selector['full_title']}"
        selector_identity = (
            selector["file_path"],
            selector["full_title"],
            selector["registration_ordinal"],
        )
        if selector_identity in seen_selectors:
            raise MaterializationError(f"duplicate Jest selector: {native_id}")
        seen_selectors.add(selector_identity)
        if native_id in result:
            raise MaterializationError(f"ambiguous Jest selector: {native_id}")
        result[native_id] = selector
    return result


def _build_entry(
    *,
    baseline: dict[str, Any],
    contract_sha256: str,
    context_by_binary: dict[tuple[str, str], dict[str, Any]],
    jest_selectors: dict[str, dict[str, Any]],
    doctest_ordinals: Counter[tuple[str, str]],
) -> dict[str, Any]:
    framework = baseline["framework"]
    runner_kind = V1_TO_V2_RUNNER_KIND[framework]
    route_id = f"test-route.{runner_kind}.v1"
    validation_id = ROUTE_VALIDATION_IDS[runner_kind]
    identity = {
        "kind": "test",
        "route_id": route_id,
        "test_id": baseline["baseline_id"],
        "validation_id": validation_id,
    }
    native_id = baseline["native_id"]
    context: dict[str, Any] | None = None
    if runner_kind == "rust-nextest":
        package_name, rest = native_id.split("::", 1)
        binary_name, harness_name = rest.split("$", 1)
        context = context_by_binary[(package_name, binary_name)]
        selector = {
            "cargo_target_context_spec_sha256": context["context_sha256"],
            "harness_test_name": harness_name,
            "kind": runner_kind,
            "nextest_binary_id": f"{package_name}::{binary_name}",
        }
    elif runner_kind == "rust-doctest":
        source_path = baseline["source"]
        item_path = native_id.split(" - ", 1)[1].rsplit(" (line ", 1)[0]
        context = context_by_binary[("doctest-source", source_path)]
        ordinal_key = (source_path, item_path)
        selector = {
            "cargo_target_context_spec_sha256": context["context_sha256"],
            "declaration_ordinal": doctest_ordinals[ordinal_key],
            "harness_test_name": native_id,
            "item_path": item_path,
            "kind": runner_kind,
            "source_path": source_path,
        }
        doctest_ordinals[ordinal_key] += 1
    elif runner_kind == "python-unittest":
        selector = {
            "kind": runner_kind,
            "parent_test_id": native_id,
            "selection_unit": "parent-with-all-declared-subtests",
            "subtest_manifest_sha256": proof_hash(
                "kd4.python-unittest-subtest-manifest.pending-recapture.v1",
                {"baseline_id": baseline["baseline_id"], "native_id": native_id},
            ),
        }
    elif runner_kind == "python-pytest":
        selector = {"kind": runner_kind, "node_id": native_id}
    elif runner_kind == "javascript-jest":
        try:
            selector = jest_selectors[native_id]
        except KeyError as exc:
            raise MaterializationError(f"frozen Jest test is absent from checked-in source: {native_id}") from exc
    elif runner_kind in {"argument-comment-lint-native", "windows-sandbox-smoke-native"}:
        selector = {"case_id": native_id, "kind": runner_kind}
    else:
        raise MaterializationError(f"unsupported frozen framework: {framework}")
    applicability = {
        "kind": "host-set",
        "required_hosts": sorted(baseline["platforms"]),
    }
    return {
        "cargo_target_context_spec_sha256": context["context_sha256"] if context else None,
        "executable_identity": identity,
        "executable_identity_sha256": proof_hash("kd4.executable-identity.v1", identity),
        "execution_input_contract_sha256": contract_sha256,
        "platform_applicability": applicability,
        "platform_applicability_sha256": proof_hash("kd4.platform-applicability.v1", applicability),
        "runner_selector": selector,
        "runner_selector_sha256": proof_hash("kd4.runner-selector.v1", selector),
        "test_route_id": route_id,
        "validation_id": validation_id,
    }


def _source_only_declaration(
    spec: dict[str, Any],
    contract_sha256: str,
    context: dict[str, Any],
) -> dict[str, Any]:
    package_name, rest = spec["native_id"].split("::", 1)
    binary_name, harness_name = rest.split("$", 1)
    identity = {
        "kind": "test",
        "route_id": "test-route.rust-nextest.v1",
        "test_id": spec["canonical_id"],
        "validation_id": ROUTE_VALIDATION_IDS["rust-nextest"],
    }
    selector = {
        "cargo_target_context_spec_sha256": context["context_sha256"],
        "harness_test_name": harness_name,
        "kind": "rust-nextest",
        "nextest_binary_id": f"{package_name}::{binary_name}",
    }
    applicability = {"kind": "host-set", "required_hosts": spec["required_hosts"]}
    entry = {
        "cargo_target_context_spec_sha256": context["context_sha256"],
        "executable_identity": identity,
        "executable_identity_sha256": proof_hash("kd4.executable-identity.v1", identity),
        "execution_input_contract_sha256": contract_sha256,
        "platform_applicability": applicability,
        "platform_applicability_sha256": proof_hash("kd4.platform-applicability.v1", applicability),
        "runner_selector": selector,
        "runner_selector_sha256": proof_hash("kd4.runner-selector.v1", selector),
        "test_route_id": "test-route.rust-nextest.v1",
        "validation_id": ROUTE_VALIDATION_IDS["rust-nextest"],
    }
    evidence = {
        "confirmation_state": "deterministically-reconstructed-pending-off-host-compiled-confirmation",
        "line": spec["line"],
        "native_id": spec["native_id"],
        "source_path": spec["source_path"],
    }
    provenance_projection = {
        "evidence_paths": [spec["source_path"]],
        "evidence_sha256": proof_hash("kd4.source-only-declaration-evidence.v1", evidence),
        "kind": "platform-pending",
        "schema_version": 1,
    }
    provenance = {
        **provenance_projection,
        "receipt_sha256": proof_hash("kd4.provenance-receipt.v1", provenance_projection),
    }
    declaration = {
        "entry": entry,
        "kind": "missing-baseline",
        "source_provenance": provenance,
    }
    declaration["declaration_id"] = inventory_declaration_id_v2(
        declaration["kind"], entry, provenance
    )
    declaration["obligation_id"] = inventory_declaration_obligation_id_v2(
        declaration["kind"], entry, provenance
    )
    return declaration


def _post_baseline_current_declaration(
    spec: dict[str, Any], contract_sha256: str
) -> dict[str, Any]:
    identity = {
        "kind": "test",
        "route_id": "test-route.python-unittest.v1",
        "test_id": spec["canonical_id"],
        "validation_id": ROUTE_VALIDATION_IDS["python-unittest"],
    }
    subtest_manifest_sha256 = proof_hash(
        "kd4.python-unittest-subtest-manifest.source-declared.v1",
        {
            "declared_subtests": [],
            "parent_test_id": spec["native_id"],
            "source_path": spec["source_path"],
        },
    )
    selector = {
        "kind": "python-unittest",
        "parent_test_id": spec["native_id"],
        "selection_unit": "parent-with-all-declared-subtests",
        "subtest_manifest_sha256": subtest_manifest_sha256,
    }
    applicability = {"kind": "host-set", "required_hosts": spec["required_hosts"]}
    entry = {
        "cargo_target_context_spec_sha256": None,
        "executable_identity": identity,
        "executable_identity_sha256": proof_hash("kd4.executable-identity.v1", identity),
        "execution_input_contract_sha256": contract_sha256,
        "platform_applicability": applicability,
        "platform_applicability_sha256": proof_hash(
            "kd4.platform-applicability.v1", applicability
        ),
        "runner_selector": selector,
        "runner_selector_sha256": proof_hash("kd4.runner-selector.v1", selector),
        "test_route_id": "test-route.python-unittest.v1",
        "validation_id": ROUTE_VALIDATION_IDS["python-unittest"],
    }
    evidence = {
        "line": spec["line"],
        "native_id": spec["native_id"],
        "source_path": spec["source_path"],
        "subtest_manifest_sha256": subtest_manifest_sha256,
    }
    provenance_projection = {
        "evidence_paths": [spec["source_path"]],
        "evidence_sha256": proof_hash(
            "kd4.post-baseline-current-declaration-evidence.v1", evidence
        ),
        "kind": "source-declaration",
        "schema_version": 1,
    }
    provenance = {
        **provenance_projection,
        "receipt_sha256": proof_hash(
            "kd4.provenance-receipt.v1", provenance_projection
        ),
    }
    declaration = {
        "entry": entry,
        "kind": "post-baseline-current",
        "source_provenance": provenance,
    }
    declaration["declaration_id"] = inventory_declaration_id_v2(
        declaration["kind"], entry, provenance
    )
    declaration["obligation_id"] = inventory_declaration_obligation_id_v2(
        declaration["kind"], entry, provenance
    )
    return declaration


def _git_source_tree_sha256(repo_root: Path) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo_root), "ls-tree", "-r", "--full-tree", BASELINE_COMMIT],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0 or not result.stdout:
        raise MaterializationError(
            f"cannot resolve frozen source tree: {result.stderr.decode(errors='replace').strip()}"
        )
    return hashlib.sha256(result.stdout).hexdigest()


def _resolved_tool_identity(tool_name: str) -> tuple[Path, dict[str, Any]]:
    result = subprocess.run(
        ["rustup", "which", "--toolchain", RECAPTURE_TOOLCHAIN, tool_name],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        raise MaterializationError(
            f"cannot resolve {tool_name} from {RECAPTURE_TOOLCHAIN}: "
            f"{result.stderr.decode(errors='replace').strip()}"
        )
    try:
        executable = Path(result.stdout.decode("utf-8").strip()).resolve(strict=True)
    except (UnicodeDecodeError, OSError) as exc:
        raise MaterializationError(
            f"cannot resolve the installed {tool_name} executable"
        ) from exc
    version = subprocess.run(
        [str(executable), "-Vv"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if version.returncode != 0 or not version.stdout:
        raise MaterializationError(
            f"cannot identify {tool_name} from {RECAPTURE_TOOLCHAIN}: "
            f"{version.stderr.decode(errors='replace').strip()}"
        )
    executable_raw = executable.read_bytes()
    return executable, {
        "executable_path": str(executable),
        "executable_sha256": hashlib.sha256(executable_raw).hexdigest(),
        "version_verbose_base64": base64.b64encode(version.stdout).decode("ascii"),
        "version_verbose_sha256": hashlib.sha256(version.stdout).hexdigest(),
    }


def recapture_doctests(repo_root: Path, cargo_target_dir: Path) -> dict[str, Any]:
    """Run one complete baseline doctest-listing attempt or return no evidence."""

    repo_root = repo_root.resolve()
    if _git_source_tree_sha256(repo_root) != EXPECTED_SOURCE_TREE_SHA256:
        raise MaterializationError("frozen baseline source-tree identity mismatch")
    repository_identity = proof_hash(
        "kd4.frozen-repository-identity.v1",
        {
            "baseline_commit": BASELINE_COMMIT,
            "inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        },
    )
    if repository_identity != EXPECTED_REPOSITORY_IDENTITY_SHA256:
        raise MaterializationError("frozen repository identity mismatch")

    cargo_path, cargo_identity = _resolved_tool_identity("cargo")
    rustc_path, rustc_identity = _resolved_tool_identity("rustc")
    rustdoc_path, rustdoc_identity = _resolved_tool_identity("rustdoc")
    toolchain = {
        "cargo": cargo_identity,
        "name": RECAPTURE_TOOLCHAIN,
        "rustc": rustc_identity,
        "rustdoc": rustdoc_identity,
    }

    with tempfile.TemporaryDirectory(prefix="kd4-doctest-recapture-") as temporary:
        temporary_root = Path(temporary)
        archive_path = temporary_root / "baseline.tar"
        source_root = temporary_root / "source"
        source_root.mkdir()
        archive_argv = ["git", "archive", "--format=tar", BASELINE_COMMIT]
        with archive_path.open("wb") as archive_file:
            archive_result = subprocess.run(
                archive_argv,
                cwd=repo_root,
                check=False,
                stdout=archive_file,
                stderr=subprocess.PIPE,
            )
        if archive_result.returncode != 0:
            raise MaterializationError(
                "cannot create isolated baseline archive: "
                + archive_result.stderr.decode(errors="replace").strip()
            )
        archive_raw = archive_path.read_bytes()
        if not archive_raw:
            raise MaterializationError("isolated baseline archive is empty")
        with tarfile.open(archive_path, mode="r:") as archive:
            archive.extractall(source_root, filter="data")
        isolated_cargo_root = source_root / "codex-rs"
        if not (isolated_cargo_root / "Cargo.lock").is_file():
            raise MaterializationError("isolated baseline archive has no Cargo.lock")

        environment = os.environ.copy()
        environment.update(
            {
                "CARGO_NET_OFFLINE": "true",
                "CARGO_TARGET_DIR": str(cargo_target_dir.resolve()),
                "RUSTC": str(rustc_path),
                "RUSTDOC": str(rustdoc_path),
                "RUSTUP_TOOLCHAIN": RECAPTURE_TOOLCHAIN,
            }
        )
        runs: list[dict[str, Any]] = []
        parent_ordinals: Counter[str] = Counter()
        global_ordinal = 0
        for package_name, target_id in DOCTEST_RECAPTURE_PACKAGE_SPECS:
            command_argv = [
                "cargo", "test", "--locked", "--offline", "-p", package_name,
                "--doc", "--", "--list", "--format", "terse",
            ]
            result = subprocess.run(
                [str(cargo_path), *command_argv[1:]],
                cwd=isolated_cargo_root,
                env=environment,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            if result.returncode != 0:
                raise MaterializationError(
                    f"doctest recapture package {package_name} returned nonzero; "
                    "the complete attempt is discarded: "
                    + result.stderr.decode(errors="replace").strip()
                )
            try:
                listing_lines = [
                    line for line in result.stdout.decode("utf-8").splitlines()
                    if line.endswith(": test")
                ]
            except UnicodeDecodeError as exc:
                raise MaterializationError(
                    f"doctest recapture package {package_name} emitted non-UTF-8 output"
                ) from exc
            if not listing_lines:
                raise MaterializationError(
                    f"doctest recapture package {package_name} selected zero doctests; "
                    "the complete attempt is discarded"
                )
            occurrences: list[dict[str, Any]] = []
            for listing_line in listing_lines:
                normalized = listing_line[:-len(": test")].replace("/", "\\")
                parent_id = "rust-doctest::" + normalized
                if DOCTEST_RECAPTURE_PARENT_TARGETS.get(parent_id) != target_id:
                    raise MaterializationError(
                        f"doctest recapture emitted an unexpected identity: {listing_line}"
                    )
                parent_ordinal = parent_ordinals[parent_id]
                occurrences.append(
                    {
                        "global_ordinal": global_ordinal,
                        "parent_baseline_id": parent_id,
                        "parent_ordinal": parent_ordinal,
                        "raw_listing_line": listing_line,
                    }
                )
                parent_ordinals[parent_id] += 1
                global_ordinal += 1
            runs.append(
                {
                    "command_argv": command_argv,
                    "exit_code": result.returncode,
                    "package_name": package_name,
                    "raw_occurrences": occurrences,
                    "selected_count": len(occurrences),
                    "stderr_base64": base64.b64encode(result.stderr).decode("ascii"),
                    "stderr_sha256": hashlib.sha256(result.stderr).hexdigest(),
                    "stdout_base64": base64.b64encode(result.stdout).decode("ascii"),
                    "stdout_sha256": hashlib.sha256(result.stdout).hexdigest(),
                    "target_id": target_id,
                    "working_directory": "codex-rs",
                }
            )

        packet = {
            "attempt_id": str(uuid.uuid4()),
            "baseline_commit": BASELINE_COMMIT,
            "format_id": "kd4.doctest-recapture.v1",
            "parent_counts": [
                {"parent_baseline_id": parent, "raw_count": parent_ordinals[parent]}
                for parent in sorted(parent_ordinals)
            ],
            "raw_occurrence_count": global_ordinal,
            "repository_identity_sha256": repository_identity,
            "runs": runs,
            "schema_version": 1,
            "source_isolation": {
                "archive_command": archive_argv,
                "archive_sha256": hashlib.sha256(archive_raw).hexdigest(),
                "kind": "git-archive",
            },
            "source_tree_sha256": EXPECTED_SOURCE_TREE_SHA256,
            "toolchain": toolchain,
        }
        packet["receipt_sha256"] = proof_hash(
            "kd4.doctest-recapture-receipt.v1", packet
        )
        validate_doctest_recapture_packet_v1(packet)
        return packet


def _python_subtest_call_sites(source_path: Path) -> set[tuple[int, int, str]]:
    tree = ast.parse(source_path.read_text(encoding="utf-8"), filename=str(source_path))
    sites: set[tuple[int, int, str]] = set()

    class SiteVisitor(ast.NodeVisitor):
        def __init__(self) -> None:
            self.classes: list[str] = []
            self.functions: list[str] = []

        def visit_ClassDef(self, node: ast.ClassDef) -> None:
            self.classes.append(node.name)
            self.generic_visit(node)
            self.classes.pop()

        def visit_FunctionDef(self, node: ast.FunctionDef) -> None:
            self.functions.append(node.name)
            self.generic_visit(node)
            self.functions.pop()

        visit_AsyncFunctionDef = visit_FunctionDef

        def visit_Call(self, node: ast.Call) -> None:
            if (
                isinstance(node.func, ast.Attribute)
                and node.func.attr == "subTest"
                and self.classes
                and self.functions
            ):
                sites.add(
                    (
                        node.lineno,
                        node.col_offset + 1,
                        f"{self.classes[-1]}.{self.functions[-1]}",
                    )
                )
            self.generic_visit(node)

    SiteVisitor().visit(tree)
    return sites


def validate_recovery_source_anchors(
    recovery: dict[str, Any], frozen_inventory: dict[str, Any], repo_root: Path
) -> None:
    """Require every generated current-source anchor to resolve in this checkout."""

    records = {record["kind"]: record for record in recovery["records"]}
    unittest_sites = records["unittest"]["current_audit"]["runner_site_observations"]
    observed_by_path: dict[str, set[tuple[int, int, str]]] = {}
    for site in unittest_sites:
        relative = site["path"]
        if relative not in observed_by_path:
            source_path = repo_root / relative
            if not source_path.is_file():
                raise MaterializationError(f"recovery source anchor path is missing: {relative}")
            observed_by_path[relative] = _python_subtest_call_sites(source_path)
        anchor = (site["line"], site["column"], site["parent_id"])
        if anchor not in observed_by_path[relative]:
            raise MaterializationError(
                "recovery unittest source anchor does not resolve to a current subTest call: "
                f"{relative}:{site['line']}:{site['column']} ({site['parent_id']})"
            )

    doctest_rows = [
        row for row in frozen_inventory["tests"] if row["framework"] == "rust-doctest"
    ]
    declared_count = records["doctest"]["current_audit"]["declared_count"]
    if declared_count != len(doctest_rows):
        raise MaterializationError(
            f"recovery doctest declared count {declared_count} does not match current source anchors "
            f"{len(doctest_rows)}"
        )
    for row in doctest_rows:
        relative = row["source"]
        source_path = repo_root / relative
        if not source_path.is_file():
            raise MaterializationError(f"recovery doctest source anchor path is missing: {relative}")
        source = source_path.read_text(encoding="utf-8")
        if re.search(r"(?m)^\s*//[/!]\s*```(?:[A-Za-z_][A-Za-z0-9_-]*)?\s*$", source) is None:
            raise MaterializationError(
                f"recovery doctest source anchor has no current executable documentation fence: {relative}"
            )
        item_path = row["native_id"].split(" - ", 1)[1].rsplit(" (line ", 1)[0]
        terminal_item = item_path.rsplit("::", 1)[-1]
        if terminal_item != source_path.stem and re.search(
            rf"\b(?:struct|enum|trait|type)\s+{re.escape(terminal_item)}\b", source
        ) is None:
            raise MaterializationError(
                "recovery doctest source anchor does not contain its current declared item: "
                f"{relative} ({item_path})"
            )


def _recovery_authority(
    *,
    frozen_inventory: dict[str, Any],
    predecessor_entry_sha256s: dict[str, str],
    doctest_contexts: list[dict[str, Any]],
    doctest_recapture: dict[str, Any] | None,
    repo_root: Path,
    unittest_recapture: dict[str, Any] | None = None,
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    unittest_ledger_identities = sorted(
        row["baseline_id"]
        for row in frozen_inventory["tests"]
        if row["framework"] == "python-unittest"
    )
    hidden_unittest_replacements = [
        baseline_id
        for baseline_id in unittest_ledger_identities
        if baseline_id.startswith("hidden-at-freeze-v1::python-unittest::")
    ]
    unittest_recapture_parents = [
        baseline_id
        for baseline_id in unittest_ledger_identities
        if not baseline_id.startswith("hidden-at-freeze-v1::python-unittest::")
    ]
    if len(unittest_ledger_identities) != EXPECTED_UNITTEST_LEDGER_IDENTITY_COUNT:
        raise MaterializationError(
            "expected 909 frozen unittest ledger identities, got "
            f"{len(unittest_ledger_identities)}"
        )
    if len(unittest_recapture_parents) != EXPECTED_UNITTEST_RECAPTURE_PARENT_COUNT:
        raise MaterializationError(
            "expected 893 freeze-discovered unittest recapture parents, got "
            f"{len(unittest_recapture_parents)}"
        )
    if len(hidden_unittest_replacements) != EXPECTED_UNITTEST_HIDDEN_REPLACEMENT_COUNT:
        raise MaterializationError(
            "expected 16 hidden unittest replacement identities, got "
            f"{len(hidden_unittest_replacements)}"
        )
    doctest_targets = sorted(
        {
            f"{context['package_name']}::{context['target_kind']}::{context['target_name']}"
            for context in doctest_contexts
        }
    )
    frozen_source_authority = {
        "baseline_commit": BASELINE_COMMIT,
        "repository_identity_sha256": proof_hash(
            "kd4.frozen-repository-identity.v1",
            {"baseline_commit": BASELINE_COMMIT, "inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256},
        ),
        "source_tree_sha256": _git_source_tree_sha256(repo_root),
    }
    records = [
        {
            "current_audit": {
                "declared_count": 5,
                "kind": "doctest",
                "raw_count": None,
                "unique_count": 5,
            },
            "gap_id": "gap.doctest-raw-versus-unique",
            "kind": "doctest",
            "legacy_evidence": {
                "frozen_unique_count": 5,
                "historical_raw_count": None,
                "kind": "doctest",
            },
            "pending_requirement": {
                "baseline_commit": BASELINE_COMMIT,
                "kind": "doctest",
                "reasons": ["historical-raw-count-unknown", "off-host-recapture-required"],
                "required_package_targets": doctest_targets,
            },
            "recovery_id": "inventory-recovery.doctest.v1",
            "resolution": None,
            "state": "pending",
            "transition_receipt_sha256": None,
        },
        {
            "current_audit": {
                "executable_ast_call_count": 62,
                "excluded_embedded_fixture_count": 1,
                "kind": "unittest",
                "runner_site_observations": UNITTEST_RUNNER_SITES,
                "text_call_count": 63,
            },
            "gap_id": "gap.unittest-subtest-expansion",
            "kind": "unittest",
            "legacy_evidence": {
                "frozen_parent_count": EXPECTED_UNITTEST_LEDGER_IDENTITY_COUNT,
                "historical_subtest_call_count": None,
                "kind": "unittest",
            },
            "pending_requirement": {
                "baseline_commit": BASELINE_COMMIT,
                "expected_parent_output_sha256s": [
                    {
                        "output_sha256": predecessor_entry_sha256s[parent],
                        "parent_id": parent,
                    }
                    for parent in unittest_recapture_parents
                ],
                "kind": "unittest",
                "reasons": ["historical-subtest-count-unknown", "parent-output-recapture-required"],
                "required_parent_count": EXPECTED_UNITTEST_RECAPTURE_PARENT_COUNT,
                "required_parent_ids": unittest_recapture_parents,
            },
            "recovery_id": "inventory-recovery.unittest.v1",
            "resolution": None,
            "state": "pending",
            "transition_receipt_sha256": None,
        },
    ]
    recovery = {
        "format_id": "kd4.inventory-recovery-authority.v1",
        "frozen_source_authority": frozen_source_authority,
        "records": records,
        "schema_version": 1,
    }
    validate_recovery_source_anchors(recovery, frozen_inventory, repo_root)
    recovery["semantic_sha256"] = proof_hash(
        "kd4.inventory-recovery-authority.semantic.v1", recovery
    )
    recovery["self_hash"] = proof_hash(
        "kd4.inventory-recovery-authority.self.v1", recovery
    )
    validate_inventory_recovery_authority_v1(recovery)
    transition_receipts: list[dict[str, Any]] = []

    def finalize_recovery() -> dict[str, Any]:
        value = {
            "format_id": "kd4.inventory-recovery-authority.v1",
            "frozen_source_authority": frozen_source_authority,
            "records": records,
            "schema_version": 1,
        }
        validate_recovery_source_anchors(value, frozen_inventory, repo_root)
        value["semantic_sha256"] = proof_hash(
            "kd4.inventory-recovery-authority.semantic.v1", value
        )
        value["self_hash"] = proof_hash(
            "kd4.inventory-recovery-authority.self.v1", value
        )
        validate_inventory_recovery_authority_v1(value)
        return value

    def transition_for(
        *,
        authority_before_semantic_sha256: str,
        children: list[dict[str, Any]],
        parent_container_ids: list[str],
        recapture_receipt_sha256: str,
    ) -> dict[str, Any]:
        transition = {
            "authority_before_semantic_sha256": authority_before_semantic_sha256,
            "child_obligation_ids": sorted(
                "inventory-v2-recovered."
                + proof_hash("kd4.recovered-child-identity.v1", child)
                for child in children
            ),
            "frozen_source_authority_sha256": proof_hash(
                "kd4.frozen-source-authority.v1", frozen_source_authority
            ),
            "parent_container_ids": parent_container_ids,
            "recapture_receipt_sha256": recapture_receipt_sha256,
            "schema_version": 1,
        }
        transition["receipt_sha256"] = proof_hash(
            "kd4.recovery-transition-receipt.v1", transition
        )
        return transition

    if doctest_recapture is not None:
        validate_doctest_recapture_packet_v1(doctest_recapture)
        children = doctest_recovered_child_sources_v1(doctest_recapture)
        parent_container_ids = sorted(DOCTEST_RECAPTURE_PARENT_TARGETS)
        transition = transition_for(
            authority_before_semantic_sha256=recovery["semantic_sha256"],
            children=children,
            parent_container_ids=parent_container_ids,
            recapture_receipt_sha256=doctest_recapture["receipt_sha256"],
        )
        records[0] = {
            **records[0],
            "current_audit": {
                **records[0]["current_audit"],
                "raw_count": doctest_recapture["raw_occurrence_count"],
            },
            "legacy_evidence": {
                **records[0]["legacy_evidence"],
                "historical_raw_count": doctest_recapture["raw_occurrence_count"],
            },
            "pending_requirement": None,
            "resolution": {
                "child_sources": children,
                "parent_container_ids": parent_container_ids,
                "parent_recapture_outputs": [],
                "recapture_receipt_sha256": doctest_recapture["receipt_sha256"],
            },
            "state": "resolved",
            "transition_receipt_sha256": transition["receipt_sha256"],
        }
        transition_receipts.append(transition)
        recovery = finalize_recovery()

    if unittest_recapture is not None:
        validate_unittest_recapture_packet_v1(unittest_recapture)
        children = unittest_recovered_child_sources_v1(unittest_recapture)
        parents = unittest_executable_parent_records_v1(unittest_recapture)
        parent_container_ids = [parent["baseline_id"] for parent in parents]
        transition = transition_for(
            authority_before_semantic_sha256=recovery["semantic_sha256"],
            children=children,
            parent_container_ids=parent_container_ids,
            recapture_receipt_sha256=unittest_recapture["receipt_sha256"],
        )
        records[1] = {
            **records[1],
            "legacy_evidence": {**records[1]["legacy_evidence"], "historical_subtest_call_count": unittest_recapture["total_counts"]["subtest_occurrence_count"]},
            "pending_requirement": None,
            "resolution": {
                "child_sources": children,
                "parent_container_ids": parent_container_ids,
                "parent_recapture_outputs": [{"output_sha256": parent["predecessor_entry_sha256"], "parent_id": parent["baseline_id"]} for parent in parents],
                "recapture_receipt_sha256": unittest_recapture["receipt_sha256"],
            },
            "state": "resolved",
            "transition_receipt_sha256": transition["receipt_sha256"],
        }
        transition_receipts.append(transition)
        recovery = finalize_recovery()

    return recovery, transition_receipts


def _typed_v1_exception_provenance(
    predecessor_row: dict[str, Any],
    predecessor_entry: dict[str, Any],
) -> dict[str, Any]:
    tag = predecessor_row["provenance"]["kind"]
    projection = {
        "evidence_paths": [predecessor_entry["source"]],
        "evidence_sha256": proof_hash(
            "kd4.frozen-v1-exception-evidence.v1",
            {"inventory_entry": predecessor_entry, "ledger_row": predecessor_row},
        ),
        "kind": tag,
        "schema_version": 1,
    }
    return {
        **projection,
        "receipt_sha256": proof_hash("kd4.provenance-receipt.v1", projection),
    }


def build_materialized_bundle(
    repo_root: Path,
    doctest_recapture_raw: bytes | None = None,
    unittest_recapture_raw: bytes | None = None,
) -> dict[str, Any]:
    repo_root = repo_root.resolve()
    source_exceptions = None
    source_exception_path = repo_root / V2_UNITTEST_SOURCE_EXCEPTIONS_PATH
    if source_exception_path.is_file():
        source_exception_raw = source_exception_path.read_bytes()
        source_exceptions = json.loads(source_exception_raw)
        validate_unittest_source_provenance_exception_v1(source_exceptions)
        if canonical_jcs(source_exceptions) != source_exception_raw:
            raise MaterializationError("unittest source exception must be exact canonical JSON")
    unittest_recapture = None
    if unittest_recapture_raw is not None:
        try:
            unittest_recapture = json.loads(unittest_recapture_raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise MaterializationError("unittest recapture artifact is invalid JSON") from exc
        if canonical_jcs(unittest_recapture) != unittest_recapture_raw:
            raise MaterializationError("unittest recapture artifact must be exact canonical JSON bytes")
        try:
            validate_unittest_recapture_packet_v1(unittest_recapture)
        except InventoryV2ContractError as exc:
            raise MaterializationError(f"unittest recapture artifact is invalid: {exc}") from exc
    doctest_recapture: dict[str, Any] | None = None
    if doctest_recapture_raw is not None:
        try:
            doctest_recapture = json.loads(doctest_recapture_raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise MaterializationError("doctest recapture artifact is invalid JSON") from exc
        if canonical_jcs(doctest_recapture) != doctest_recapture_raw:
            raise MaterializationError(
                "doctest recapture artifact must be exact canonical JSON bytes"
            )
        validate_doctest_recapture_packet_v1(doctest_recapture)
    _, frozen_inventory = _read_frozen_json(
        repo_root / V1_INVENTORY_PATH, FROZEN_V1_INVENTORY_RAW_SHA256
    )
    frozen_ledger_raw, frozen_ledger = _read_frozen_json(
        repo_root / V1_LEDGER_PATH, FROZEN_V1_LEDGER_RAW_SHA256
    )
    historical_replacement_graph = derive_frozen_v1_historical_replacement_graph_v1(
        frozen_ledger
    )
    historical_replacement_ids_by_baseline = {
        baseline_id: []
        for baseline_id in historical_replacement_graph["baseline_ids"]
    }
    for edge in historical_replacement_graph["edges"]:
        historical_replacement_ids_by_baseline[edge["baseline_id"]].append(
            edge["replacement_id"]
        )
    predecessor_entries = {row["baseline_id"]: row for row in frozen_inventory["tests"]}
    predecessor_rows = {row["baseline_id"]: row for row in frozen_ledger["rows"]}
    baseline_ids = sorted(predecessor_entries)
    if (
        len(baseline_ids) != EXPECTED_BASELINE_COUNT
        or baseline_ids != sorted(predecessor_rows)
        or len(predecessor_rows) != EXPECTED_BASELINE_COUNT
    ):
        raise MaterializationError("V1 inventory and ledger do not form the exact 15,544-row baseline")

    associations = [
        {
            "baseline_id": baseline_id,
            "predecessor_entry_sha256": proof_hash(
                "kd4.frozen-v1-inventory-entry.v1", predecessor_entries[baseline_id]
            ),
        }
        for baseline_id in baseline_ids
    ]
    predecessor_entry_sha256s = {
        association["baseline_id"]: association["predecessor_entry_sha256"]
        for association in associations
    }
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

    source_paths = sorted(
        {row["source"] for row in frozen_inventory["tests"]}
        | {spec["source_path"] for spec in SOURCE_ONLY_SPECS}
        | {spec["source_path"] for spec in POST_BASELINE_CURRENT_SPECS}
    )
    contracts_by_source = {
        source: _execution_input_contract(
            owned=[{"kind": "exact", "path": source}],
            consumed=(
                [
                    {
                        "kind": "exact",
                        "path": ARGUMENT_COMMENT_LINT_UI_CONSUMED_PATHS[source],
                    }
                ]
                if source in ARGUMENT_COMMENT_LINT_UI_CONSUMED_PATHS
                else None
            ),
        )
        for source in source_paths
    }
    documentation_contract = _execution_input_contract(
        owned=[{"kind": "glob", "pattern": "**/*.md", "root": "docs"}]
    )
    source_map_contract = _execution_input_contract(
        owned=[{"kind": "exact", "path": "SOURCEMAP.md"}]
    )
    contracts = sorted(
        [*contracts_by_source.values(), documentation_contract, source_map_contract],
        key=canonical_jcs,
    )

    manifests = _manifest_index(repo_root)
    rust_test_manifest_raw, manifest_features, manifest_targets = (
        _manifest_feature_contexts(repo_root, manifests)
    )
    context_by_binary: dict[tuple[str, str], dict[str, Any]] = {}

    def add_context(
        package_name: str,
        binary_name: str,
        resolved: tuple[Path, str, str, Path] | None = None,
    ) -> None:
        key = (package_name, binary_name)
        manifest, target_kind, target_name, source = resolved or _resolve_cargo_target(
            package_name, binary_name, manifests
        )
        features = manifest_features.get((package_name, target_kind, target_name), ())
        context = _cargo_context(
            manifest_path=manifest,
            package_name=package_name,
            target_kind=target_kind,
            target_name=target_name,
            target_source_path=source,
            repo_root=repo_root,
            features=features,
        )
        prior = context_by_binary.get(key)
        if prior is not None and prior != context:
            raise MaterializationError(
                f"Cargo binary {package_name}::{binary_name} has ambiguous execution contexts"
            )
        context_by_binary[key] = context

    for package_name, manifest, target_kind, target_name, source in manifest_targets.values():
        add_context(
            package_name,
            target_name,
            (manifest, target_kind, target_name, source),
        )
    for row in frozen_inventory["tests"]:
        if row["framework"] != "rust-nextest":
            continue
        package_name, rest = row["native_id"].split("::", 1)
        binary_name = rest.split("$", 1)[0]
        key = (package_name, binary_name)
        if key not in context_by_binary:
            add_context(package_name, binary_name)
    for spec in SOURCE_ONLY_SPECS:
        package_name, rest = spec["native_id"].split("::", 1)
        binary_name = rest.split("$", 1)[0]
        key = (package_name, binary_name)
        if key not in context_by_binary:
            add_context(package_name, binary_name)
    doctest_contexts: list[dict[str, Any]] = []
    for row in frozen_inventory["tests"]:
        if row["framework"] != "rust-doctest":
            continue
        source_path = repo_root / row["source"]
        manifest = next(
            (parent / "Cargo.toml" for parent in source_path.parents if (parent / "Cargo.toml").is_file()),
            None,
        )
        if manifest is None or manifest.parent == repo_root:
            raise MaterializationError(f"no Cargo manifest owns doctest source {row['source']}")
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        package_name = data["package"]["name"]
        lib_targets = [target for target in _cargo_targets(manifest, data) if target[1] in {"lib", "proc-macro"}]
        if len(lib_targets) != 1:
            raise MaterializationError(f"doctest source does not map to one library target: {row['source']}")
        target_name, target_kind, target_source = lib_targets[0]
        context = _cargo_context(
            manifest_path=manifest,
            package_name=package_name,
            target_kind=target_kind,
            target_name=target_name,
            target_source_path=target_source,
            repo_root=repo_root,
            features=manifest_features.get(
                (package_name, target_kind, target_name), ()
            ),
        )
        context_by_binary[("doctest-source", row["source"])] = context
        doctest_contexts.append(context)

    cargo_contexts_by_hash = {
        context["context_sha256"]: context for context in context_by_binary.values()
    }
    cargo_contexts = sorted(cargo_contexts_by_hash.values(), key=lambda item: item["context_sha256"])
    jest_selectors = _jest_selector_index(repo_root)
    doctest_ordinals: Counter[tuple[str, str]] = Counter()
    declarations: list[dict[str, Any]] = []
    declarations_by_baseline: dict[str, dict[str, Any]] = {}
    unittest_manifests = {} if unittest_recapture is None else {
        item["parent_baseline_id"]: item["manifest_sha256"]
        for item in unittest_subtest_manifests_v1(unittest_recapture)
    }
    for baseline_id in baseline_ids:
        baseline = predecessor_entries[baseline_id]
        entry = _build_entry(
            baseline=baseline,
            contract_sha256=contracts_by_source[baseline["source"]]["contract_sha256"],
            context_by_binary=context_by_binary,
            jest_selectors=jest_selectors,
            doctest_ordinals=doctest_ordinals,
        )
        if baseline_id in unittest_manifests:
            entry["runner_selector"]["subtest_manifest_sha256"] = unittest_manifests[baseline_id]
            entry["runner_selector_sha256"] = proof_hash(
                "kd4.runner-selector.v1", entry["runner_selector"]
            )
        declaration = {
            "baseline_id": baseline_id,
            "entry": entry,
            "kind": "frozen-baseline",
            "predecessor_entry_sha256": predecessor_entry_sha256s[baseline_id],
        }
        declarations.append(declaration)
        declarations_by_baseline[baseline_id] = declaration
    for spec in SOURCE_ONLY_SPECS:
        package_name, rest = spec["native_id"].split("::", 1)
        binary_name = rest.split("$", 1)[0]
        declarations.append(
            _source_only_declaration(
                spec,
                contracts_by_source[spec["source_path"]]["contract_sha256"],
                context_by_binary[(package_name, binary_name)],
            )
        )
    for spec in POST_BASELINE_CURRENT_SPECS:
        declarations.append(
            _post_baseline_current_declaration(
                spec,
                contracts_by_source[spec["source_path"]]["contract_sha256"],
            )
        )
    declarations.sort(key=canonical_jcs)
    if len(declarations) != EXPECTED_DECLARATION_COUNT:
        raise MaterializationError(
            f"expected {EXPECTED_DECLARATION_COUNT:,} V2 declarations, got {len(declarations)}"
        )

    recovery, transition_receipts = _recovery_authority(
        frozen_inventory=frozen_inventory,
        predecessor_entry_sha256s=predecessor_entry_sha256s,
        doctest_contexts=doctest_contexts,
        doctest_recapture=doctest_recapture,
        repo_root=repo_root,
        unittest_recapture=unittest_recapture,
    )
    for transition_receipt in transition_receipts:
        validate_recovery_transition_receipt_v1(transition_receipt)
    recovery_raw = canonical_jcs(recovery)
    schema_resources = [
        {"path": path, "raw_sha256": raw_sha256, "schema_id": schema_id}
        for path, raw_sha256, schema_id in zip(
            INVENTORY_V2_SCHEMA_PATHS,
            INVENTORY_V2_SCHEMA_RAW_SHA256S,
            INVENTORY_V2_SCHEMA_IDS,
        )
    ]
    routes = [
        {
            "route_id": f"test-route.{kind}.v1",
            "runner_kind": kind,
            "validation_id": ROUTE_VALIDATION_IDS[kind],
        }
        for kind in ROUTE_VALIDATION_IDS
    ]
    action_routes = sorted(
        [
            {
                "action_id": "documentation.markdown",
                "execution_input_contract_sha256": documentation_contract["contract_sha256"],
                "validation_id": "documentation.markdown",
            },
            {
                "action_id": "maintenance.source-map",
                "execution_input_contract_sha256": source_map_contract["contract_sha256"],
                "validation_id": "maintenance.source-map",
            },
        ],
        key=canonical_jcs,
    )
    materialization_source_projection = {
        "predecessor_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "predecessor_ledger_raw_sha256": FROZEN_V1_LEDGER_RAW_SHA256,
        "rust_test_manifest": {
            "path": RUST_TEST_MANIFEST_PATH,
            "raw_sha256": hashlib.sha256(rust_test_manifest_raw).hexdigest(),
        },
        "post_baseline_current_specs": list(POST_BASELINE_CURRENT_SPECS),
        "source_only_specs": list(SOURCE_ONLY_SPECS),
    }
    if doctest_recapture is not None:
        materialization_source_projection["doctest_recapture"] = {
            "path": V2_DOCTEST_RECAPTURE_PATH,
            "raw_sha256": hashlib.sha256(doctest_recapture_raw).hexdigest(),
            "receipt_sha256": doctest_recapture["receipt_sha256"],
        }
    if unittest_recapture is not None:
        materialization_source_projection["unittest_recapture"] = {
            "path": V2_UNITTEST_RECAPTURE_PATH,
            "raw_sha256": hashlib.sha256(unittest_recapture_raw).hexdigest(),
            "receipt_sha256": unittest_recapture["receipt_sha256"],
        }
    inventory = {
        "action_routes": action_routes,
        "authority": {
            "format_id": "kd4-frozen-test-inventory-v2",
            "raw_sha256": hashlib.sha256(canonical_jcs(materialization_source_projection)).hexdigest(),
            "schema_sha256": INVENTORY_V2_SCHEMA_RAW_SHA256S[1],
            "semantic_sha256": "0" * 64,
            "self_hash": "0" * 64,
        },
        "cargo_target_context_specs": cargo_contexts,
        "declaration_universe": declarations,
        "execution_input_contracts": contracts,
        "format_id": "kd4-frozen-test-inventory-v2",
        "predecessor": {
            "inventory_hash": FROZEN_V1_INVENTORY_SEMANTIC_SHA256,
            "raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
            "recorded_baseline_workspace_fingerprint": FROZEN_V1_WORKSPACE_FINGERPRINT,
            "test_count": EXPECTED_BASELINE_COUNT,
        },
        "predecessor_reconciliation": reconciliation,
        "recovery_authority": {
            "path": V2_RECOVERY_PATH,
            "raw_sha256": hashlib.sha256(recovery_raw).hexdigest(),
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
    inventory["authority"]["self_hash"] = proof_hash(
        "kd4.frozen-test-inventory-v2.authority.self.v1",
        {
            key: inventory["authority"][key]
            for key in ("format_id", "raw_sha256", "schema_sha256", "semantic_sha256")
        },
    )

    ledger_rows: list[dict[str, Any]] = []
    for baseline_id in baseline_ids:
        predecessor_row = predecessor_rows[baseline_id]
        declaration = declarations_by_baseline[baseline_id]
        resolution = predecessor_row["resolution"]
        if doctest_recapture is not None and baseline_id in DOCTEST_RECAPTURE_PARENT_TARGETS:
            transition = transition_receipts[0]
            child_obligation_ids = sorted(
                child_id
                for child_id, child in zip(
                    transition["child_obligation_ids"],
                    recovery["records"][0]["resolution"]["child_sources"],
                )
                if child["parent_baseline_id"] == baseline_id
            )
            disposition = {
                "child_obligation_ids": child_obligation_ids,
                "kind": "recovered-container",
                "transition_receipt_sha256": transition["receipt_sha256"],
            }
        elif resolution == "unresolved":
            disposition: dict[str, Any] = {"kind": "unresolved"}
        elif resolution == "replacement":
            replacement_ids = historical_replacement_ids_by_baseline.get(baseline_id)
            if replacement_ids is None:
                raise MaterializationError(
                    "V1 replacement row is absent from the frozen historical graph"
                )
            predecessor_row_sha256 = proof_hash(
                "kd4.frozen-v1-replacement-ledger-row.v1", predecessor_row
            )
            disposition = {
                "contract": {
                    "accepted": None,
                    "candidate": None,
                    "legacy_replacement_hint": {
                        "predecessor_row_sha256": predecessor_row_sha256,
                        "replacement_ids": replacement_ids,
                    },
                    "state": "pending-review",
                },
                "edge_ids": sorted([
                    "replacement-edge-v2."
                    + proof_hash(
                        "kd4.legacy-replacement-edge.v1",
                        {
                            "baseline_id": baseline_id,
                            "predecessor_row_sha256": predecessor_row_sha256,
                            "replacement_id": replacement_id,
                        },
                    )
                    for replacement_id in replacement_ids
                ]),
                "kind": "replacement",
                "stage2_incorrect_behavior_ids": None,
            }
        elif resolution == "exception":
            disposition = {
                "exception": {
                    "kind": "pending-legacy",
                    "provenance_receipt": _typed_v1_exception_provenance(
                        predecessor_row, predecessor_entries[baseline_id]
                    ),
                    "tag": predecessor_row["provenance"]["kind"],
                },
                "kind": "exception",
            }
        else:
            raise MaterializationError(f"unknown V1 ledger resolution: {resolution}")
        ledger_rows.append(
            {
                "baseline_id": baseline_id,
                "disposition": disposition,
                "obligation_id": frozen_baseline_obligation_id_v2(declaration),
            }
        )
    for declaration in declarations:
        if declaration["kind"] == "frozen-baseline":
            continue
        if declaration["kind"] == "post-baseline-current":
            disposition = {
                "inventory_entry_semantic_sha256": proof_hash(
                    "kd4.executable-inventory-entry.v2", declaration["entry"]
                ),
                "kind": "current",
            }
        elif declaration["kind"] == "missing-baseline":
            disposition = {
                "exception": {
                    "kind": "pending-legacy",
                    "provenance_receipt": declaration["source_provenance"],
                    "tag": "platform-pending",
                },
                "kind": "exception",
            }
        else:
            raise MaterializationError(
                f"unknown nonbaseline declaration kind: {declaration['kind']}"
            )
        ledger_rows.append(
            {
                "baseline_id": None,
                "disposition": disposition,
                "obligation_id": declaration["obligation_id"],
            }
        )
    if unittest_recapture is not None:
        for child in recovery["records"][1]["resolution"]["child_sources"]:
            ledger_rows.append({
                "baseline_id": None,
                "disposition": {"kind": "unresolved"},
                "obligation_id": "inventory-v2-recovered." + proof_hash("kd4.recovered-child-identity.v1", child),
            })
    ledger_rows.sort(key=canonical_jcs)
    ledger = {
        "format_id": "kd4.test-replacement-ledger.v2",
        "inventory_authority": {
            "path": V2_INVENTORY_PATH,
            "raw_sha256": inventory["authority"]["raw_sha256"],
            "semantic_sha256": inventory["authority"]["semantic_sha256"],
            "self_hash": inventory["authority"]["self_hash"],
        },
        "rows": ledger_rows,
        "schema_version": 2,
        "trusted_defect_receipts": None,
    }
    ledger["semantic_sha256"] = proof_hash("kd4.test-replacement-ledger.v2.semantic", ledger)
    ledger["self_hash"] = proof_hash("kd4.test-replacement-ledger.v2.self", ledger)

    validate_frozen_test_inventory_v2(inventory)
    validate_test_replacement_ledger_v2(ledger)
    issuer = ActiveHostApplicabilityIssuerV1(
        "39bc53cc-ec47-4ea4-a940-b9a874779c30",
        hashlib.sha256(b"kd4.inventory-v2-a2.dormant-validation-only.v1").digest(),
    )
    validate_inventory_ledger_predecessor_closure(
        inventory,
        ledger,
        recovery_raw,
        transition_receipts,
        issuer,
        doctest_recapture_raw,
        unittest_recapture_raw,
        frozen_ledger_raw,
    )

    documents = {
        V2_INVENTORY_PATH: inventory,
        V2_RECOVERY_PATH: recovery,
        V2_RECOVERY_TRANSITION_RECEIPTS_PATH: transition_receipts,
        V2_LEDGER_PATH: ledger,
    }
    raw_documents = {path: canonical_jcs(document) for path, document in documents.items()}
    if source_exceptions is not None:
        documents[V2_UNITTEST_SOURCE_EXCEPTIONS_PATH] = source_exceptions
        raw_documents[V2_UNITTEST_SOURCE_EXCEPTIONS_PATH] = source_exception_raw
    if doctest_recapture is not None:
        documents[V2_DOCTEST_RECAPTURE_PATH] = doctest_recapture
        raw_documents[V2_DOCTEST_RECAPTURE_PATH] = doctest_recapture_raw
    if unittest_recapture is not None:
        documents[V2_UNITTEST_RECAPTURE_PATH] = unittest_recapture
        raw_documents[V2_UNITTEST_RECAPTURE_PATH] = unittest_recapture_raw
        documents[V2_UNITTEST_SOURCE_EXCEPTIONS_PATH] = unittest_recapture["source_provenance_exception"]
        raw_documents[V2_UNITTEST_SOURCE_EXCEPTIONS_PATH] = canonical_jcs(unittest_recapture["source_provenance_exception"])
    framework_counts = Counter(
        declaration["entry"]["runner_selector"]["kind"] for declaration in declarations
    )
    disposition_counts = Counter(row["disposition"]["kind"] for row in ledger_rows)
    return {
        "documents": documents,
        "raw_documents": raw_documents,
        "summary": {
            "artifact_sha256": {
                path: hashlib.sha256(raw).hexdigest() for path, raw in raw_documents.items()
            },
            "authority_hashes": {
                "inventory_semantic_sha256": inventory["authority"]["semantic_sha256"],
                "inventory_self_hash": inventory["authority"]["self_hash"],
                "ledger_semantic_sha256": ledger["semantic_sha256"],
                "ledger_self_hash": ledger["self_hash"],
                "recovery_semantic_sha256": recovery["semantic_sha256"],
                "recovery_self_hash": recovery["self_hash"],
            },
            "counts": {
                "cargo_target_contexts": len(cargo_contexts),
                "frozen_baseline_declarations": EXPECTED_BASELINE_COUNT,
                "inventory_declarations": len(declarations),
                "ledger_rows": len(ledger_rows),
                "post_baseline_current_declarations": len(
                    POST_BASELINE_CURRENT_SPECS
                ),
                "recovery_records": len(recovery["records"]),
                "recovery_transition_receipts": len(transition_receipts),
                "source_only_declarations": len(SOURCE_ONLY_SPECS),
            },
            "disposition_counts": dict(sorted(disposition_counts.items())),
            "framework_counts": dict(sorted(framework_counts.items())),
            "executed_test_count": 0,
            "materialized_declaration_count": len(declarations),
            "mode": (
                "unittest-and-doctest-recovery-materialization"
                if unittest_recapture is not None and doctest_recapture is not None
                else "unittest-recovery-materialization"
                if unittest_recapture is not None
                else "doctest-recovery-materialization"
                if doctest_recapture is not None
                else "dormant-materialization"
            ),
        },
    }


def recapture_unittests(
    repo_root: Path, *, source_exceptions: Path | None = None,
    codex_executable: Path | None = None, output: Path | None = None,
) -> dict[str, Any]:
    if source_exceptions is None or codex_executable is None or output is None:
        raise MaterializationError(UNITTEST_RECAPTURE_UNAVAILABLE)
    if output.exists():
        raise MaterializationError("unittest recapture output already exists")
    _, inventory = _read_frozen_json(repo_root / V1_INVENTORY_PATH, FROZEN_V1_INVENTORY_RAW_SHA256)
    records = sorted([
        {"baseline_id": row["baseline_id"], "native_id": row["native_id"], "predecessor_entry_sha256": proof_hash("kd4.frozen-v1-inventory-entry.v1", row)}
        for row in inventory["tests"]
        if row["framework"] == "python-unittest" and not row["baseline_id"].startswith("hidden-at-freeze-v1::")
    ], key=lambda row: row["baseline_id"])
    output.parent.mkdir(parents=True, exist_ok=True)
    manifest = output.parent / ("unittest-parent-manifest-" + str(uuid.uuid4()) + ".json")
    manifest.write_bytes(canonical_jcs({
        "baseline_commit": BASELINE_COMMIT, "format_id": "kd4.unittest-parent-manifest.v1",
        "frozen_inventory_raw_sha256": FROZEN_V1_INVENTORY_RAW_SHA256,
        "parent_records": records, "schema_version": 1,
        "source_tree_sha256": EXPECTED_SOURCE_TREE_SHA256,
    }))
    completed = subprocess.run([
        sys.executable, str(Path(__file__).with_name("completion_proof_unittest_recapture.py")),
        "controller", "--repo-root", str(repo_root), "--manifest", str(manifest),
        "--output", str(output), "--source-exceptions", str(source_exceptions),
        "--codex-executable", str(codex_executable),
    ], stdin=subprocess.DEVNULL, check=False)
    if completed.returncode:
        raise MaterializationError(f"unittest recapture controller failed with exit {completed.returncode}")
    packet = json.loads(output.read_bytes())
    validate_unittest_recapture_packet_v1(packet)
    return packet


def _require_output_kind(path: Path, *, directory: bool) -> None:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return
    correct_kind = stat.S_ISDIR(metadata.st_mode) if directory else stat.S_ISREG(metadata.st_mode)
    reparse_point = getattr(metadata, "st_file_attributes", 0) & getattr(
        stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0
    )
    if not correct_kind or reparse_point:
        expected = "directory" if directory else "regular file"
        raise MaterializationError(f"V2 output destination must be a {expected}: {path}")


def _write_or_check(bundle: dict[str, Any], output_root: Path, *, write: bool) -> None:
    expected_paths = set(bundle["raw_documents"])
    validation_root = output_root / ".codex/validation"
    if write:
        for parent in (output_root, output_root / ".codex", validation_root):
            _require_output_kind(parent, directory=True)
        for relative in bundle["raw_documents"]:
            _require_output_kind(output_root / relative, directory=False)
    unexpected: set[str] = set()
    if validation_root.is_dir():
        for pattern in OWNED_ARTIFACT_PATTERNS:
            for candidate in validation_root.glob(pattern):
                relative = candidate.relative_to(output_root).as_posix()
                if relative not in expected_paths and not candidate.name.endswith(".schema.json"):
                    unexpected.add(relative)
    if unexpected:
        raise MaterializationError(
            "unexpected owned V2 artifacts: " + ", ".join(sorted(unexpected))
        )

    if write:
        validation_root.mkdir(parents=True, exist_ok=True)
        # Finish preparing every artifact before replacing any existing bytes.
        # Individual replacements are atomic; this does not activate V2 or
        # provide a transaction across all output files.
        with tempfile.TemporaryDirectory(
            prefix=".inventory-v2-stage-", dir=validation_root
        ) as stage_name:
            staged: list[tuple[Path, Path]] = []
            for index, (relative, raw) in enumerate(bundle["raw_documents"].items()):
                staged_path = Path(stage_name) / str(index)
                with staged_path.open("xb") as handle:
                    handle.write(raw)
                    handle.flush()
                    os.fsync(handle.fileno())
                staged.append((staged_path, output_root / relative))
            for staged_path, destination in staged:
                staged_path.replace(destination)
        return

    mismatches: list[str] = []
    for relative, raw in bundle["raw_documents"].items():
        destination = output_root / relative
        if not destination.is_file() or destination.read_bytes() != raw:
            mismatches.append(relative)
    if mismatches:
        raise MaterializationError(
            "materialized V2 artifacts differ from deterministic output: " + ", ".join(mismatches)
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Recapture or materialize KD4 Inventory V2 recovery artifacts"
    )
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output-root", type=Path)
    doctest_packet_source = parser.add_mutually_exclusive_group()
    doctest_packet_source.add_argument(
        "--doctest-recapture-packet",
        type=Path,
        help="use this exact canonical packet while materializing",
    )
    doctest_packet_source.add_argument(
        "--without-doctest-recapture-packet",
        action="store_true",
        help="materialize the pre-recapture dormant state even when the repository has a saved packet",
    )
    unittest_packet_source = parser.add_mutually_exclusive_group()
    unittest_packet_source.add_argument(
        "--unittest-recapture-packet",
        type=Path,
        help="use this exact canonical unittest packet while materializing",
    )
    unittest_packet_source.add_argument(
        "--without-unittest-recapture-packet",
        action="store_true",
        help="omit unittest recapture even when the repository has a saved packet",
    )
    parser.add_argument(
        "--cargo-target-dir",
        type=Path,
        help="serialized Cargo target directory for direct doctest recapture",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    parser.add_argument("--unittest-source-exceptions", type=Path)
    parser.add_argument("--codex-executable", type=Path)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--recapture-doctests", action="store_true")
    mode.add_argument("--recapture-unittests", action="store_true")
    args = parser.parse_args(argv)
    output_root = (args.output_root or args.repo_root).resolve()
    try:
        if args.recapture_doctests:
            if args.doctest_recapture_packet is not None or args.without_doctest_recapture_packet:
                raise MaterializationError(
                    "doctest packet selection is invalid during a direct recapture"
                )
            cargo_target_dir = args.cargo_target_dir or (
                args.repo_root / "codex-rs" / "target-doctest-recapture-v1"
            )
            packet = recapture_doctests(args.repo_root, cargo_target_dir)
            raw = canonical_jcs(packet)
            destination = output_root / V2_DOCTEST_RECAPTURE_PATH
            destination.parent.mkdir(parents=True, exist_ok=True)
            temporary = destination.with_suffix(destination.suffix + ".tmp")
            temporary.write_bytes(raw)
            temporary.replace(destination)
            print(
                json.dumps(
                    {
                        "artifact_sha256": hashlib.sha256(raw).hexdigest(),
                        "attempt_id": packet["attempt_id"],
                        "operation": "recapture-doctests",
                        "package_run_count": len(packet["runs"]),
                        "raw_occurrence_count": packet["raw_occurrence_count"],
                        "selected_count": packet["raw_occurrence_count"],
                    },
                    sort_keys=True,
                    separators=(",", ":"),
                )
            )
            return 0
        if args.recapture_unittests:
            packet = recapture_unittests(
                args.repo_root, source_exceptions=args.unittest_source_exceptions,
                codex_executable=args.codex_executable,
                output=output_root / V2_UNITTEST_RECAPTURE_PATH,
            )
            exception = packet["source_provenance_exception"]
            print(json.dumps({
                "operation": "recapture-unittests",
                "attempt_id": packet["attempt_id"],
                "selected_count": packet["total_counts"]["selected_parent_count"],
                "excepted_source_count": len(exception["baseline_ids"]),
                "excepted_historical_execution_count": len(
                    exception["historical_execution_extension"]["baseline_ids"]
                ),
            }, sort_keys=True))
            return 0
        if args.without_doctest_recapture_packet:
            recapture_raw = None
        elif args.doctest_recapture_packet is not None:
            packet_path = args.doctest_recapture_packet
            if not packet_path.is_file():
                raise MaterializationError(
                    "explicit --doctest-recapture-packet does not exist or is not a file: "
                    f"{packet_path}"
                )
            recapture_raw = packet_path.read_bytes()
        else:
            packet_path = args.repo_root / V2_DOCTEST_RECAPTURE_PATH
            recapture_raw = packet_path.read_bytes() if packet_path.is_file() else None
        if args.without_unittest_recapture_packet:
            unittest_recapture_raw = None
        elif args.unittest_recapture_packet is not None:
            unittest_packet_path = args.unittest_recapture_packet
            if not unittest_packet_path.is_file():
                raise MaterializationError(
                    "explicit --unittest-recapture-packet does not exist or is not a file: "
                    f"{unittest_packet_path}"
                )
            unittest_recapture_raw = unittest_packet_path.read_bytes()
        else:
            unittest_packet_path = args.repo_root / V2_UNITTEST_RECAPTURE_PATH
            unittest_recapture_raw = unittest_packet_path.read_bytes() if unittest_packet_path.is_file() else None
        bundle = build_materialized_bundle(
            args.repo_root, recapture_raw, unittest_recapture_raw
        )
        _write_or_check(bundle, output_root, write=args.write)
    except (MaterializationError, OSError, ValueError, KeyError, json.JSONDecodeError) as exc:
        print(f"inventory-v2 materialization failed: {exc}", file=sys.stderr)
        return 1
    summary = dict(bundle["summary"])
    summary["operation"] = "write" if args.write else "check"
    print(json.dumps(summary, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
