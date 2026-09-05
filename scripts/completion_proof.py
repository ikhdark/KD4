#!/usr/bin/env python3
"""Create a fresh, structured whole-repository validation attempt report.

This command is the test runner. The compiled KD4 CompletionProofGate is the
verifier: it supplies a private nonce and output path, then independently
decides whether a successful report can be registered as proof.
"""

from __future__ import annotations

import argparse
import configparser
import hashlib
import importlib.util
import json
import os
import platform
import re
import secrets
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import unicodedata
import uuid
from dataclasses import dataclass
from multiprocessing.connection import Listener
from pathlib import Path, PurePosixPath, PureWindowsPath
from typing import Any, Iterable, Mapping, Sequence

try:
    from scripts import focused_live_successor_catalog as _focused_catalog
    from scripts.completion_proof_canonical import canonical_jcs as _canonical_jcs
    from scripts.current_evidence_successor_projection import (
        build_current_successor_projection_v1 as _build_current_successor_projection,
    )
except ImportError:  # pragma: no cover - direct script execution
    # Just executes focused recipes from a temporary Python launcher. runpy
    # preserves that launcher's import path, so anchor sibling imports here.
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import focused_live_successor_catalog as _focused_catalog
    from completion_proof_canonical import canonical_jcs as _canonical_jcs
    from current_evidence_successor_projection import (
        build_current_successor_projection_v1 as _build_current_successor_projection,
    )

_BOUNDED_PROCESS_MODULE_NAME = "_kd4_completion_proof_bounded_process"
_BOUNDED_PROCESS_PATH = Path(__file__).resolve().with_name("bounded_process.py")
_TRUSTED_BOUNDED_PROCESS_SHA256 = (
    "01b48baabc3131089e23810341ccb694c3097809e03d08df6454ff8b9f741182"
)
_BOUNDED_PROCESS_SOURCE_BYTES = _BOUNDED_PROCESS_PATH.read_bytes()
if (
    hashlib.sha256(_BOUNDED_PROCESS_SOURCE_BYTES).hexdigest()
    != _TRUSTED_BOUNDED_PROCESS_SHA256
):
    raise RuntimeError(
        "bounded process helper differs from the identity embedded in the trusted "
        "completion-proof runner"
    )
_bounded_process_spec = importlib.util.spec_from_file_location(
    _BOUNDED_PROCESS_MODULE_NAME,
    _BOUNDED_PROCESS_PATH,
)
if _bounded_process_spec is None or _bounded_process_spec.loader is None:
    raise RuntimeError(
        f"cannot load trusted process supervisor {_BOUNDED_PROCESS_PATH}"
    )
_bounded_process_module = importlib.util.module_from_spec(_bounded_process_spec)
sys.modules[_BOUNDED_PROCESS_MODULE_NAME] = _bounded_process_module
exec(  # noqa: S102 - execute the exact parent-captured helper bytes
    compile(
        _BOUNDED_PROCESS_SOURCE_BYTES,
        str(_BOUNDED_PROCESS_PATH),
        "exec",
    ),
    _bounded_process_module.__dict__,
)
DEFAULT_STDERR_LIMIT_BYTES = _bounded_process_module.DEFAULT_STDERR_LIMIT_BYTES
DEFAULT_STDOUT_LIMIT_BYTES = _bounded_process_module.DEFAULT_STDOUT_LIMIT_BYTES
run_bounded_process = _bounded_process_module.run_bounded_process

_CHILD_VALIDATION_MODULE_NAME = "_kd4_completion_proof_child_validation_report"
_CHILD_VALIDATION_PATH = Path(__file__).resolve().with_name(
    "child_validation_report.py"
)
_TRUSTED_CHILD_VALIDATION_SHA256 = (
    "5d4d64d228846046efb53133a8c6a8e928625a3e34673e5ce66458924c4ae244"
)
_CHILD_VALIDATION_SOURCE_BYTES = _CHILD_VALIDATION_PATH.read_bytes()
if hashlib.sha256(_CHILD_VALIDATION_SOURCE_BYTES).hexdigest() != (
    _TRUSTED_CHILD_VALIDATION_SHA256
):
    raise RuntimeError(
        "child-validation helper differs from the identity embedded in the trusted "
        "completion-proof runner"
    )
_child_validation_spec = importlib.util.spec_from_file_location(
    _CHILD_VALIDATION_MODULE_NAME,
    _CHILD_VALIDATION_PATH,
)
if _child_validation_spec is None or _child_validation_spec.loader is None:
    raise RuntimeError(
        f"cannot load trusted child-validation broker {_CHILD_VALIDATION_PATH}"
    )
_child_validation_module = importlib.util.module_from_spec(_child_validation_spec)
sys.modules[_CHILD_VALIDATION_MODULE_NAME] = _child_validation_module
_CHILD_BOUNDED_PROCESS_MODULE_NAME = "_kd4_child_validation_bounded_process"
_child_bounded_process_spec = importlib.util.spec_from_file_location(
    _CHILD_BOUNDED_PROCESS_MODULE_NAME,
    _BOUNDED_PROCESS_PATH,
)
if _child_bounded_process_spec is None or _child_bounded_process_spec.loader is None:
    raise RuntimeError(
        f"cannot load child process supervisor {_BOUNDED_PROCESS_PATH}"
    )
_child_bounded_process_module = importlib.util.module_from_spec(
    _child_bounded_process_spec
)
sys.modules[_CHILD_BOUNDED_PROCESS_MODULE_NAME] = _child_bounded_process_module
exec(  # noqa: S102 - child parser and broker share the captured supervisor bytes
    compile(
        _BOUNDED_PROCESS_SOURCE_BYTES,
        str(_BOUNDED_PROCESS_PATH),
        "exec",
    ),
    _child_bounded_process_module.__dict__,
)
exec(  # noqa: S102 - execute the exact bytes later supplied to the broker
    compile(
        _CHILD_VALIDATION_SOURCE_BYTES,
        str(_CHILD_VALIDATION_PATH),
        "exec",
    ),
    _child_validation_module.__dict__,
)
ActionProcessObservation = _child_validation_module.ActionProcessObservation
JournalInvocationBinding = _child_validation_module.JournalInvocationBinding
JournalContractError = _child_validation_module.JournalContractError
ProcessIdentity = _child_validation_module.ProcessIdentity
parse_child_validation_journal = (
    _child_validation_module.parse_child_validation_journal
)


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG = REPO_ROOT / ".codex" / "validation" / "completion-proof.toml"
EXACT_COMMAND = "just completion-proof"
REPORT_TYPE = "CompletionProofAttemptReportV2"
FOCUSED_COMMAND_TEMPLATE = "just completion-focused {validation_id}"
FOCUSED_REPORT_TYPE = "FocusedValidationAttemptReportV2"
CURRENT_EVIDENCE_VALIDATION_ID = "inventory.current-evidence"
CURRENT_EVIDENCE_VALIDATION_IDS = (
    "maintenance.root-unittest",
    "sdk.python.pytest",
)
VALIDATION_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
KD4_POLICY_ID = "kd4"
KD4_FROZEN_INVENTORY_HASH = (
    "a2fb8c0b806853b6375d92cfa6daf985ea35a5c4d4ecd49cf1d6da6f23359152"
)
REQUIRED_RUNTIME_ENV = (
    "CODEX_COMPLETION_PROOF_NONCE",
    "CODEX_COMPLETION_PROOF_REPORT",
    "CODEX_COMPLETION_PROOF_ATTEMPT_ID",
    "CODEX_COMPLETION_PROOF_PARENT_PID",
    "CODEX_COMPLETION_PROOF_REPOSITORY",
    "CODEX_COMPLETION_PROOF_START_FINGERPRINT",
    "CODEX_COMPLETION_PROOF_MUTATION_EPOCH",
    "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256",
    "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT",
)
WORKSPACE_MAX_PATHS = 256
WORKSPACE_MAX_BYTES = 64 * 1024 * 1024
TEST_SURFACE_MAX_LISTING_BYTES = 128 * 1024 * 1024
TEST_SURFACE_MAX_PATHS = 1_000_000
TEST_SURFACE_MAX_CANDIDATE_BYTES = 16 * 1024 * 1024
TEST_SURFACE_MAX_GITIGNORE_BLOBS = 100_000
TEST_SURFACE_MAX_GITIGNORE_BYTES = 64 * 1024 * 1024
PROCESS_STDOUT_LIMIT_BYTES = DEFAULT_STDOUT_LIMIT_BYTES
PROCESS_STDERR_LIMIT_BYTES = DEFAULT_STDERR_LIMIT_BYTES
GIT_PROCESS_TIMEOUT_SECONDS = 5.0
# Ignored-marker discovery traverses generated trees even with narrow pathspecs.
GIT_IGNORED_TEST_SURFACE_TIMEOUT_SECONDS = 30.0
RUNNER_ATTESTATION_TIMEOUT_SECONDS = 5.0
RUNNER_ATTESTATION_RESPONSE_LIMIT_BYTES = 16
FOCUSED_EVIDENCE_EXCHANGE_TIMEOUT_SECONDS = 30.0
FOCUSED_EVIDENCE_ACK_LEN = 48
FOCUSED_EVIDENCE_MEMBER_NAMES = (
    "catalog",
    "process",
    "unittest_collect",
    "unittest_exec",
    "pytest_collect",
    "pytest_exec",
)
TYPED_VALIDATION_BROKER_RESPONSE_LIMIT_BYTES = 64 * 1024 * 1024
TYPED_VALIDATION_BROKER_SHUTDOWN_GRACE_SECONDS = 15
_TYPED_VALIDATION_BROKER_BOOTSTRAP = """\
import hashlib
import pathlib
import sys
import types

payload = sys.stdin.buffer.read()
if hashlib.sha256(payload).hexdigest() != sys.argv[2]:
    raise SystemExit("parent-staged broker payload digest mismatch")
if len(payload) < 8:
    raise SystemExit("parent-staged broker payload is truncated")
bounded_size = int.from_bytes(payload[:8], "big")
bounded_end = 8 + bounded_size
if bounded_size <= 0 or bounded_end >= len(payload):
    raise SystemExit("parent-staged broker payload framing is invalid")
bounded_source = payload[8:bounded_end]
broker_source = payload[bounded_end:]
bounded_name = "_kd4_child_validation_bounded_process"
bounded_module = types.ModuleType(bounded_name)
bounded_module.__file__ = "<parent-staged-bounded-process>"
sys.modules[bounded_name] = bounded_module
exec(
    compile(bounded_source, bounded_module.__file__, "exec"),
    bounded_module.__dict__,
)
launch_arguments = [
    str(pathlib.Path(sys.executable).resolve()),
    "-I",
    "-c",
    sys.argv[1],
    *sys.argv[1:],
]
sys.argv = ["<parent-staged-child-validation>", *sys.argv[3:]]
broker_globals = {
    "__name__": "__main__",
    "__file__": "<parent-staged-child-validation>",
    "_BROKER_LAUNCH_ARGUMENTS": launch_arguments,
}
exec(
    compile(broker_source, broker_globals["__file__"], "exec"),
    broker_globals,
)
"""
CACHED_INDEX_ARGS = (
    "ls-files",
    "--cached",
    "--stage",
    "-v",
    "-f",
    "-z",
    "--",
    ".",
)
CACHED_INDEX_REGULAR_MODES = frozenset({"100644", "100755"})
CACHED_INDEX_SUPPORTED_MODES = frozenset({*CACHED_INDEX_REGULAR_MODES, "120000"})
DOCUMENT_SUFFIXES = frozenset({".md", ".mdx", ".rst", ".txt", ".adoc", ".markdown"})
RESULT_CLASSES = frozenset(
    {"confirmed_pass", "confirmed_validation_failure", "pre_result_error"}
)
BUILTIN_VALIDATIONS = {
    "inventory-reconciliation": "inventory.frozen-reconciliation",
    "rust-nextest": "rust.nextest.workspace",
    "rust-doctest": "rust.doctest.workspace",
    "python-unittest": "maintenance.root-unittest",
    "python-pytest": "sdk.python.pytest",
    "javascript-jest": "sdk.typescript.jest",
    "argument-comment-lint-native": "tools.argument-comment-lint.native",
    "windows-sandbox-smoke": "windows.sandbox-smoke",
}
VALIDATION_RUNNERS = frozenset({*BUILTIN_VALIDATIONS, "typed-validation", "rust-gate"})
NATIVE_ADAPTERS: dict[str, dict[str, str]] = {
    "argument-comment-lint-native": {
        "validation_id": "tools.argument-comment-lint.native",
        "framework": "argument-comment-lint-native",
        "relative_path": "tools/argument-comment-lint/native_test_runner.py",
        "inventory_report_type": "ArgumentCommentLintNativeTestInventoryV1",
        "execution_report_type": "ArgumentCommentLintNativeTestReportV1",
    },
    "windows-sandbox-smoke": {
        "validation_id": "windows.sandbox-smoke",
        "framework": "windows-sandbox-smoke",
        "relative_path": "codex-rs/windows-sandbox-rs/sandbox_smoketests.py",
        "inventory_report_type": "WindowsSandboxSmokeCaseListV1",
        "execution_report_type": "WindowsSandboxSmokeCaseReportV1",
    },
}
KD4_TYPED_VALIDATIONS = {
    "sdk.typescript.typecheck": "typescript-typecheck",
    "sdk.python.ruff": "python-ruff",
    "maintenance.script-audit": "script-audit",
    "maintenance.source-map": "source-map-consistency",
    "maintenance.rust-test-manifest": "rust-test-manifest",
    "generated.config-schema": "generated-config-schema",
    "generated.app-server-schema": "generated-app-server-schema",
    "generated.config-proto": "generated-config-proto",
    "generated.exec-server-relay-proto": "generated-exec-server-relay-proto",
    "wrapper.codex-cli": "codex-cli-wrapper",
}
KD4_RUST_GATES = {
    "rust.named-gate.adaptive-reasoning-contract": "adaptive-reasoning-contract",
    "rust.named-gate.app-server-schema-protocol": "app-server-schema-protocol",
    "rust.named-gate.apply-patch-scenarios": "apply-patch-scenarios",
    "rust.named-gate.config-schema-protocol": "config-schema-protocol",
    "rust.named-gate.windows-sandbox-core-exec": "windows-sandbox-core-exec",
    "rust.named-gate.core-helper-resolution": "core-helper-resolution",
    "rust.named-gate.core-stdio-helper-regressions": "core-stdio-helper-regressions",
    "rust.named-gate.rmcp-streamable-http": "rmcp-streamable-http",
    "rust.named-gate.tools-json-schema-policy-fixtures": (
        "tools-json-schema-policy-fixtures"
    ),
    "windows.process-coverage": "windows-process-coverage",
}
KD4_VALIDATION_IDS = frozenset(
    {*BUILTIN_VALIDATIONS.values(), *KD4_TYPED_VALIDATIONS, *KD4_RUST_GATES}
)
KD4_VALIDATION_COUNT = 28
if len(KD4_VALIDATION_IDS) != KD4_VALIDATION_COUNT:
    raise RuntimeError("KD4 completion-proof policy must contain exactly 28 validations")
EVIDENCE_KINDS = frozenset(
    {"structured_test", "typed_non_test", "inventory_reconciliation", "infrastructure"}
)
TEST_SURFACE_PYTHON_ROOTS = (
    ".codex/environments",
    ".codex/hooks",
    "scripts",
    "codex-cli/scripts",
    "codex-rs/app-server-test-client/scripts",
    "codex-rs/config/scripts",
    "codex-rs/scripts",
    "codex-rs/skills/src/assets/samples",
    "sdk/python/scripts",
    "tools/argument-comment-lint",
)
TEST_SURFACE_JAVASCRIPT_RUNNERS = frozenset(
    {
        "@jest/globals",
        "@playwright/test",
        "@vitest/runner",
        "ava",
        "cypress",
        "jasmine",
        "jest",
        "karma",
        "mocha",
        "tap",
        "tape",
        "ts-jest",
        "vitest",
    }
)
TEST_SURFACE_JAVASCRIPT_CONFIG_RE = re.compile(
    r"^(jest|playwright|vitest)\.config\.(?:cjs|cts|js|mjs|mts|ts)$"
)
TEST_SURFACE_JAVASCRIPT_TEST_RE = re.compile(
    r"\.(test|spec)\.(?:cjs|cts|js|jsx|mjs|mts|ts|tsx)$"
)
TEST_SURFACE_IGNORED_PATHSPECS = (
    ":(top)justfile",
    ":(glob,top)**/Cargo.toml",
    ":(glob,top)**/test_*.py",
    ":(glob,top)**/*_test.py",
    ":(glob,top)**/package.json",
    ":(glob,top)**/pyproject.toml",
    ":(glob,top)**/pytest.ini",
    ":(glob,top)**/setup.cfg",
    ":(glob,top)**/tox.ini",
    *(
        f":(glob,top)**/{runner}.config.{suffix}"
        for runner in ("jest", "playwright", "vitest")
        for suffix in ("cjs", "cts", "js", "mjs", "mts", "ts")
    ),
    *(
        f":(glob,top)**/*.{kind}.{suffix}"
        for kind in ("test", "spec")
        for suffix in ("cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx")
    ),
    ":(glob,top)**/*_test.go",
    ":(glob,top)**/*.bats",
)
TEST_SURFACE_REVIEWED_JUSTFILE_SHA256: dict[str, str] = {
    "justfile": "71442d688363ccf73e8400a1f8c3ae44d160246e4569df4dff4afc1f38cefc02",
}
TEST_SURFACE_REVIEWED_PACKAGE_SCRIPTS: dict[
    str, dict[str, tuple[str, tuple[str, ...]]]
] = {
    "package.json": {
        "audit:scripts": (
            "node scripts/run-python.js scripts/root_maintenance.py audit-scripts",
            ("maintenance.script-audit",),
        ),
        "format": (
            "node scripts/run-python.js scripts/format.py --check --only prettier",
            ("reviewed-non-test-command",),
        ),
        "format:fix": (
            "node scripts/run-python.js scripts/format.py --write --only prettier",
            ("reviewed-non-test-command",),
        ),
        "format:python": (
            "node scripts/run-python.js scripts/format.py --check "
            "--only python-scripts",
            ("reviewed-non-test-command",),
        ),
        "format:python:fix": (
            "node scripts/run-python.js scripts/format.py --write "
            "--only python-scripts",
            ("reviewed-non-test-command",),
        ),
        "lint:markdown": (
            "markdownlint-cli2",
            ("reviewed-non-test-command",),
        ),
        "lint:python": (
            "node scripts/run-python.js scripts/root_maintenance.py lint-python",
            ("reviewed-non-test-command",),
        ),
        "lint:python:fix": (
            "node scripts/run-python.js scripts/root_maintenance.py lint-python --fix",
            ("reviewed-non-test-command",),
        ),
        "test:scripts": (
            "node scripts/run-python.js scripts/root_maintenance.py test-python",
            ("python-unittest",),
        ),
        "test:scripts:changed": (
            "node scripts/run-python.js scripts/root_maintenance.py "
            "test-python --changed",
            ("python-unittest",),
        ),
    },
    "sdk/typescript/package.json": {
        "build": ("tsup", ("reviewed-non-test-command",)),
        "build:watch": ("tsup --watch", ("reviewed-non-test-command",)),
        "coverage": ("jest --coverage", ("javascript-jest",)),
        "format": ("prettier --check .", ("reviewed-non-test-command",)),
        "format:fix": ("prettier --write .", ("reviewed-non-test-command",)),
        "lint": (
            'pnpm eslint "src/**/*.ts" "tests/**/*.ts"',
            ("reviewed-non-test-command",),
        ),
        "lint:fix": (
            'pnpm eslint --fix "src/**/*.ts" "tests/**/*.ts"',
            ("reviewed-non-test-command",),
        ),
        "prepare": ("pnpm run build", ("reviewed-non-test-command",)),
        "test": ("jest", ("javascript-jest",)),
        "test:watch": ("jest --watch", ("javascript-jest",)),
        "typecheck": (
            "tsc --noEmit --rootDir . --allowImportingTsExtensions",
            ("reviewed-non-test-command",),
        ),
    },
}
TEST_SURFACE_GIT_REDIRECTION_ENV = frozenset(
    {
        "git_alternate_object_directories",
        "git_ceiling_directories",
        "git_common_dir",
        "git_config",
        "git_config_count",
        "git_config_global",
        "git_config_nosystem",
        "git_config_parameters",
        "git_config_system",
        "git_dir",
        "git_discovery_across_filesystem",
        "git_graft_file",
        "git_implicit_work_tree",
        "git_index_file",
        "git_internal_super_prefix",
        "git_namespace",
        "git_object_directory",
        "git_prefix",
        "git_quarantine_path",
        "git_replace_ref_base",
        "git_shallow_file",
        "git_super_prefix",
        "git_work_tree",
    }
)


class ProofError(RuntimeError):
    """A configuration, invocation, selection, or infrastructure error."""


@dataclass(frozen=True)
class _TestSurfacePath:
    relative_path: str
    tracked: bool
    index_mode: str | None


@dataclass(frozen=True)
class _TestSurfaceMarker:
    kind: str
    detail: str = ""
    value: str = ""


@dataclass(frozen=True)
class _PinnedGitIgnoreBlob:
    relative_path: str
    content: bytes
    sha256: str
    line_count: int


@dataclass(frozen=True)
class _PinnedGitIgnoreMatch:
    source: str
    line: int
    pattern: str
    blob_sha256: str


def canonical_json(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _validation_input_contract(config: Mapping[str, Any]) -> dict[str, object]:
    content_paths = sorted(
        {
            str(value).replace("\\", "/")
            for field_name in ("owned_paths", "consumed_paths")
            for value in config.get(field_name, [])
        }
    )
    return {
        "schema_version": 1,
        "content_paths": content_paths,
        "path_set_paths": sorted(
            {
                str(value).replace("\\", "/")
                for value in config.get("path_set_paths", [])
            }
        ),
        "evidence_path_manifests": sorted(
            {
                str(value).replace("\\", "/")
                for value in config.get("evidence_path_manifests", [])
            }
        ),
    }


def _validation_input_contract_digest(config: Mapping[str, Any]) -> str:
    return sha256_bytes(canonical_json(_validation_input_contract(config)))


def _portable_exit_code(value: int | None) -> int | None:
    if value is None:
        return None
    if 0x80000000 <= value <= 0xFFFFFFFF:
        return value - 0x100000000
    if -(2**31) <= value <= 2**31 - 1:
        return value
    return None


def _strip_completion_proof_env(
    source: Mapping[str, str],
) -> dict[str, str]:
    """Remove private proof inputs and test switches using Windows semantics."""
    prefix = "codex_completion_proof_"
    return {
        name: value
        for name, value in source.items()
        if not name.casefold().startswith(prefix)
    }


def _run_bounded_git_process(
    repo_root: Path,
    args: Sequence[str],
    *,
    env: Mapping[str, str],
    input_bytes: bytes | None = None,
    timeout_seconds: float | None = None,
) -> subprocess.CompletedProcess[bytes]:
    command = [
        "git",
        "-c",
        "core.hooksPath=NUL" if os.name == "nt" else "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        *args,
    ]
    result = run_bounded_process(
        command,
        cwd=repo_root,
        env=env,
        timeout_seconds=(
            GIT_PROCESS_TIMEOUT_SECONDS if timeout_seconds is None else timeout_seconds
        ),
        stdin_bytes=input_bytes,
        stdout_limit_bytes=TEST_SURFACE_MAX_LISTING_BYTES,
        stderr_limit_bytes=PROCESS_STDERR_LIMIT_BYTES,
    )
    if result.supervision_error is not None or result.returncode is None:
        diagnostic = result.supervision_error or "Git process returned no result"
        raise ProofError(
            f"git {' '.join(args)} ended in an infrastructure error: {diagnostic}"
        )
    return subprocess.CompletedProcess(
        args=command,
        returncode=result.returncode,
        stdout=result.stdout,
        stderr=result.stderr,
    )


def _git(
    repo_root: Path,
    args: Sequence[str],
    *,
    timeout_seconds: float | None = None,
) -> bytes:
    env = _strip_completion_proof_env(os.environ)
    env["GIT_OPTIONAL_LOCKS"] = "0"
    result = _run_bounded_git_process(
        repo_root,
        args,
        env=env,
        timeout_seconds=timeout_seconds,
    )
    if result.returncode != 0:
        raise ProofError(
            f"git {' '.join(args)} failed with exit {result.returncode}: "
            + result.stderr.decode("utf-8", errors="replace").strip()
        )
    return result.stdout


def _workspace_paths(status_bytes: bytes) -> list[str]:
    if not status_bytes:
        return []
    if not status_bytes.endswith(b"\0"):
        raise ProofError("malformed porcelain-v2 workspace status")

    paths: set[str] = set()
    records = status_bytes[:-1].split(b"\0")
    index = 0
    while index < len(records):
        record = records[index]
        if not record:
            raise ProofError("malformed porcelain-v2 workspace status")
        record_type = record[:1]
        if record_type == b"?":
            if record[1:2] != b" ":
                raise ProofError("malformed porcelain-v2 workspace status")
            raw_path = record[2:]
        else:
            field_index = {b"1": 8, b"2": 9, b"u": 10}.get(record_type)
            if field_index is None:
                raise ProofError("unknown porcelain-v2 workspace status record")
            if record[1:2] != b" ":
                raise ProofError("malformed porcelain-v2 workspace status")
            fields = record.split(b" ", field_index)
            if len(fields) <= field_index:
                raise ProofError("malformed porcelain-v2 workspace status")
            raw_path = fields[field_index]
        paths.add(_strict_workspace_path(raw_path))
        if record_type == b"2":
            index += 1
            if index >= len(records):
                raise ProofError("porcelain-v2 rename omitted its original path")
            paths.add(_strict_workspace_path(records[index]))
        index += 1
    return sorted(paths)


def _strict_workspace_path(raw_path: bytes) -> str:
    try:
        path = raw_path.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ProofError("workspace status contains a non-UTF-8 path") from error
    normalized = path.replace("\\", "/")
    if (
        not path
        or Path(path).is_absolute()
        or PureWindowsPath(path).is_absolute()
        or bool(PureWindowsPath(path).drive)
        or any(part in {"", ".", ".."} for part in normalized.split("/"))
        or normalized.casefold() == ".git"
        or normalized.casefold().startswith(".git/")
    ):
        raise ProofError("workspace status contains an unsafe repository path")
    return path


def _strict_test_surface_path(raw_path: bytes, *, label: str) -> str:
    try:
        path = raw_path.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ProofError(f"{label} contains a non-UTF-8 path") from error
    pure = PurePosixPath(path)
    if (
        not path
        or "\\" in path
        or path.startswith("/")
        or PureWindowsPath(path).is_absolute()
        or bool(PureWindowsPath(path).drive)
        or any(part in {"", ".", ".."} for part in path.split("/"))
        or pure.as_posix() != path
        or any(ord(character) < 0x20 or ord(character) == 0x7F for character in path)
        or path.casefold() == ".git"
        or path.casefold().startswith(".git/")
    ):
        raise ProofError(f"{label} contains an unsafe repository path")
    return path


def _test_surface_nul_records(output: bytes, *, label: str) -> list[bytes]:
    if len(output) > TEST_SURFACE_MAX_LISTING_BYTES:
        raise ProofError(
            f"{label} exceeds the {TEST_SURFACE_MAX_LISTING_BYTES}-byte limit"
        )
    if not output:
        return []
    if not output.endswith(b"\0"):
        raise ProofError(f"{label} is not NUL terminated")
    records = output[:-1].split(b"\0")
    if any(not record for record in records):
        raise ProofError(f"{label} contains an empty path record")
    return records


def _cached_index_entries(
    repo_root: Path,
) -> tuple[bytes, list[_TestSurfacePath]]:
    """Read one visibility-tagged cached-index snapshot and fail closed."""
    label = "cached index listing"
    output = _git(repo_root, CACHED_INDEX_ARGS)
    entries: list[_TestSurfacePath] = []
    seen_paths: set[str] = set()
    casefolded_paths: dict[str, str] = {}
    for record in _test_surface_nul_records(output, label=label):
        header, separator, raw_path = record.partition(b"\t")
        fields = header.split(b" ")
        if separator != b"\t" or len(fields) != 4 or not raw_path:
            raise ProofError(f"{label} is malformed")
        raw_tag, raw_mode, raw_object_id, raw_stage = fields
        if (
            len(raw_tag) != 1
            or re.fullmatch(rb"[0-7]{6}", raw_mode) is None
            or len(raw_object_id) not in {40, 64}
            or re.fullmatch(rb"[0-9a-f]+", raw_object_id) is None
            or re.fullmatch(rb"[0-3]", raw_stage) is None
        ):
            raise ProofError(f"{label} is malformed")
        relative = _strict_test_surface_path(raw_path, label=label)
        if raw_tag != b"H":
            displayed_tag = raw_tag.decode("ascii", errors="backslashreplace")
            raise ProofError(
                f"{label} contains an invisible or unsupported entry tag "
                f"{displayed_tag!r}: {relative}"
            )
        if raw_stage != b"0":
            raise ProofError(f"{label} contains an unmerged index entry: {relative}")
        mode = raw_mode.decode("ascii")
        if mode == "160000":
            raise ProofError(f"{label} contains a gitlink: {relative}")
        if mode not in CACHED_INDEX_SUPPORTED_MODES:
            raise ProofError(
                f"{label} contains unsupported index mode {mode}: {relative}"
            )
        if mode == "120000" and _is_test_surface_candidate(relative):
            raise ProofError(f"tracked test-system marker is a symlink: {relative}")
        if relative in seen_paths:
            raise ProofError(f"{label} repeats path {relative!r}")
        seen_paths.add(relative)
        case_key = relative.lower()
        previous = casefolded_paths.setdefault(case_key, relative)
        if previous != relative:
            raise ProofError(
                f"{label} contains a Windows case-fold collision: "
                f"{previous!r} and {relative!r}"
            )
        entries.append(
            _TestSurfacePath(
                relative_path=relative,
                tracked=True,
                index_mode=mode,
            )
        )
    if len(entries) > TEST_SURFACE_MAX_PATHS:
        raise ProofError(
            f"{label} has {len(entries)} paths; limit is {TEST_SURFACE_MAX_PATHS}"
        )
    return output, entries


def _test_surface_private_git_env(global_config: Path) -> dict[str, str]:
    env: dict[str, str] = {}
    for name, value in _strip_completion_proof_env(os.environ).items():
        normalized = name.casefold()
        if normalized in TEST_SURFACE_GIT_REDIRECTION_ENV:
            continue
        if normalized.startswith(("git_config_key_", "git_config_value_")):
            continue
        if normalized.startswith("git_trace"):
            continue
        env[name] = value
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_CONFIG_GLOBAL"] = str(global_config.resolve())
    env["GIT_OPTIONAL_LOCKS"] = "0"
    env["GIT_TERMINAL_PROMPT"] = "0"
    return env


def _test_surface_git_process(
    repo_root: Path,
    args: Sequence[str],
    *,
    env: Mapping[str, str],
    input_bytes: bytes | None = None,
) -> subprocess.CompletedProcess[bytes]:
    return _run_bounded_git_process(
        repo_root,
        args,
        env=env,
        input_bytes=input_bytes,
    )


def _checked_test_surface_git(
    repo_root: Path,
    args: Sequence[str],
    *,
    env: Mapping[str, str],
    input_bytes: bytes | None = None,
    allowed_exit_codes: frozenset[int] = frozenset({0}),
) -> subprocess.CompletedProcess[bytes]:
    result = _test_surface_git_process(
        repo_root,
        args,
        env=env,
        input_bytes=input_bytes,
    )
    if result.returncode not in allowed_exit_codes:
        raise ProofError(
            f"git {' '.join(args)} failed with exit {result.returncode}: "
            + result.stderr.decode("utf-8", errors="replace").strip()
        )
    return result


def _write_private_test_surface_file(path: Path, content: bytes) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("xb") as output:
            output.write(content)
    except (FileExistsError, OSError) as error:
        raise ProofError(
            f"cannot materialize private test-system ignore oracle path {path.name!r}: "
            f"{error}"
        ) from error


def _test_surface_object_id(raw: bytes, *, label: str) -> str:
    if len(raw) not in {40, 64} or re.fullmatch(rb"[0-9a-f]+", raw) is None:
        raise ProofError(f"{label} contains an invalid Git object ID")
    return raw.decode("ascii")


def _pinned_head_gitignore_blobs(
    repo_root: Path,
) -> dict[str, _PinnedGitIgnoreBlob]:
    with tempfile.TemporaryDirectory(prefix="kd4-head-ignore-config-") as temp_name:
        private_root = Path(temp_name)
        global_config = private_root / "global.gitconfig"
        _write_private_test_surface_file(global_config, b"")
        env = _test_surface_private_git_env(global_config)
        tree_result = _checked_test_surface_git(
            repo_root,
            ["--no-replace-objects", "rev-parse", "--verify", "HEAD^{tree}"],
            env=env,
        )
        if re.fullmatch(rb"[0-9a-f]{40}(?:\r?\n)", tree_result.stdout):
            tree_id = tree_result.stdout.rstrip(b"\r\n").decode("ascii")
        elif re.fullmatch(rb"[0-9a-f]{64}(?:\r?\n)", tree_result.stdout):
            tree_id = tree_result.stdout.rstrip(b"\r\n").decode("ascii")
        else:
            raise ProofError("pinned HEAD tree identity is malformed")

        listing = _checked_test_surface_git(
            repo_root,
            [
                "--no-replace-objects",
                "ls-tree",
                "-r",
                "-z",
                "--full-tree",
                tree_id,
            ],
            env=env,
        ).stdout
        ignore_objects: list[tuple[str, str]] = []
        seen_paths: set[str] = set()
        for record in _test_surface_nul_records(
            listing,
            label="pinned HEAD tree listing",
        ):
            header, separator, raw_path = record.partition(b"\t")
            fields = header.split(b" ")
            if separator != b"\t" or len(fields) != 3 or not raw_path:
                raise ProofError("pinned HEAD tree listing is malformed")
            raw_mode, raw_type, raw_object_id = fields
            if re.fullmatch(rb"[0-7]{6}", raw_mode) is None or raw_type not in {
                b"blob",
                b"commit",
                b"tree",
            }:
                raise ProofError("pinned HEAD tree listing is malformed")
            object_id = _test_surface_object_id(
                raw_object_id,
                label="pinned HEAD tree listing",
            )
            if len(object_id) != len(tree_id):
                raise ProofError("pinned HEAD tree mixes Git object ID formats")
            if raw_path != b".gitignore" and not raw_path.endswith(b"/.gitignore"):
                continue
            relative = _strict_test_surface_path(
                raw_path,
                label="pinned HEAD .gitignore listing",
            )
            if relative in seen_paths:
                raise ProofError(
                    f"pinned HEAD tree repeats .gitignore path {relative!r}"
                )
            seen_paths.add(relative)
            if raw_mode == b"120000" and raw_type == b"blob":
                continue
            if raw_mode not in {b"100644", b"100755"} or raw_type != b"blob":
                raise ProofError(
                    f"pinned HEAD .gitignore is not a regular blob: {relative}"
                )
            ignore_objects.append((relative, object_id))

        if len(ignore_objects) > TEST_SURFACE_MAX_GITIGNORE_BLOBS:
            raise ProofError(
                f"pinned HEAD has {len(ignore_objects)} regular .gitignore blobs; "
                f"limit is {TEST_SURFACE_MAX_GITIGNORE_BLOBS}"
            )

        object_cache: dict[str, tuple[bytes, str]] = {}
        total_bytes = 0
        blobs: dict[str, _PinnedGitIgnoreBlob] = {}
        for relative, object_id in ignore_objects:
            cached = object_cache.get(object_id)
            if cached is None:
                size_output = _checked_test_surface_git(
                    repo_root,
                    ["--no-replace-objects", "cat-file", "-s", object_id],
                    env=env,
                ).stdout
                size_match = re.fullmatch(rb"(0|[1-9][0-9]*)(?:\r?\n)", size_output)
                if size_match is None:
                    raise ProofError(
                        f"pinned HEAD .gitignore blob size is malformed: {relative}"
                    )
                blob_size = int(size_match.group(1))
                if blob_size > TEST_SURFACE_MAX_CANDIDATE_BYTES:
                    raise ProofError(
                        "pinned HEAD .gitignore blob exceeds the "
                        f"{TEST_SURFACE_MAX_CANDIDATE_BYTES}-byte file limit: "
                        f"{relative}"
                    )
                content = _checked_test_surface_git(
                    repo_root,
                    ["--no-replace-objects", "cat-file", "blob", object_id],
                    env=env,
                ).stdout
                if len(content) != blob_size:
                    raise ProofError(
                        f"pinned HEAD .gitignore blob length changed: {relative}"
                    )
                cached = (content, sha256_bytes(content))
                object_cache[object_id] = cached
            content, blob_sha256 = cached
            total_bytes += len(content)
            if total_bytes > TEST_SURFACE_MAX_GITIGNORE_BYTES:
                raise ProofError(
                    "pinned HEAD .gitignore blobs exceed the "
                    f"{TEST_SURFACE_MAX_GITIGNORE_BYTES}-byte total limit"
                )
            blobs[relative] = _PinnedGitIgnoreBlob(
                relative_path=relative,
                content=content,
                sha256=blob_sha256,
                line_count=len(content.splitlines()),
            )
        return blobs


def _source_core_ignore_case(
    repo_root: Path,
    *,
    env: Mapping[str, str],
) -> str | None:
    result = _test_surface_git_process(
        repo_root,
        [
            "config",
            "--local",
            "--no-includes",
            "--null",
            "--get-all",
            "core.ignoreCase",
        ],
        env=env,
    )
    if result.returncode == 1 and not result.stdout and not result.stderr:
        return None
    if result.returncode != 0:
        raise ProofError(
            "cannot read repository core.ignoreCase: "
            + result.stderr.decode("utf-8", errors="replace").strip()
        )
    if not result.stdout.endswith(b"\0"):
        raise ProofError("repository core.ignoreCase value is malformed")
    values = result.stdout[:-1].split(b"\0")
    if len(values) != 1:
        raise ProofError("repository has multiple core.ignoreCase values")
    try:
        value = values[0].decode("utf-8")
    except UnicodeDecodeError as error:
        raise ProofError("repository core.ignoreCase value is not UTF-8") from error
    if any(ord(character) < 0x20 or ord(character) == 0x7F for character in value):
        raise ProofError("repository core.ignoreCase value contains control characters")
    return value


def _materialize_test_surface_oracle_file(
    oracle_root: Path,
    relative: str,
    content: bytes,
) -> None:
    path = oracle_root.joinpath(*PurePosixPath(relative).parts)
    _write_private_test_surface_file(path, content)


def _parse_test_surface_ignore_matches(
    output: bytes,
    *,
    candidates: Sequence[str],
    blobs: Mapping[str, _PinnedGitIgnoreBlob],
) -> set[str]:
    if len(output) > TEST_SURFACE_MAX_LISTING_BYTES:
        raise ProofError(
            "test-system ignore oracle output exceeds the "
            f"{TEST_SURFACE_MAX_LISTING_BYTES}-byte limit"
        )
    if candidates and not output.endswith(b"\0"):
        raise ProofError("test-system ignore oracle output is not NUL terminated")
    fields = output[:-1].split(b"\0") if output else []
    if len(fields) != len(candidates) * 4:
        raise ProofError(
            "test-system ignore oracle did not return exactly one result per candidate"
        )

    observed_matches: dict[str, _PinnedGitIgnoreMatch] = {}
    for index, expected_path in enumerate(candidates):
        raw_source, raw_line, raw_pattern, raw_path = fields[index * 4 : index * 4 + 4]
        reported_path = _strict_test_surface_path(
            raw_path,
            label="test-system ignore oracle output",
        )
        if reported_path != expected_path:
            raise ProofError(
                "test-system ignore oracle returned candidates out of order: "
                f"expected {expected_path!r}, got {reported_path!r}"
            )
        if not raw_source and not raw_line and not raw_pattern:
            continue
        if not raw_source or not raw_line or not raw_pattern:
            raise ProofError("test-system ignore oracle returned a partial match")
        source = _strict_test_surface_path(
            raw_source,
            label="test-system ignore oracle source",
        )
        blob = blobs.get(source)
        if blob is None:
            raise ProofError("test-system ignore oracle used a non-HEAD source")
        if re.fullmatch(rb"[1-9][0-9]*", raw_line) is None:
            raise ProofError(
                "test-system ignore oracle returned an invalid line number"
            )
        line = int(raw_line)
        if line > blob.line_count:
            raise ProofError("test-system ignore oracle line exceeds its HEAD source")
        try:
            pattern = raw_pattern.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ProofError(
                "test-system ignore oracle pattern is not UTF-8"
            ) from error
        observed_matches[expected_path] = _PinnedGitIgnoreMatch(
            source=source,
            line=line,
            pattern=pattern,
            blob_sha256=blob.sha256,
        )

    return {
        relative
        for relative, match in observed_matches.items()
        if not match.pattern.startswith("!")
    }


def _head_ignored_test_surface_paths(
    repo_root: Path,
    candidates: Sequence[str],
) -> set[str]:
    if not candidates:
        return set()
    blobs = _pinned_head_gitignore_blobs(repo_root)
    with tempfile.TemporaryDirectory(prefix="kd4-test-ignore-oracle-") as temp_name:
        private_root = Path(temp_name)
        global_config = private_root / "global.gitconfig"
        _write_private_test_surface_file(global_config, b"")
        oracle_root = private_root / "repository"
        oracle_root.mkdir()
        env = _test_surface_private_git_env(global_config)
        source_ignore_case = _source_core_ignore_case(repo_root, env=env)
        _checked_test_surface_git(oracle_root, ["init", "--quiet"], env=env)
        info_exclude = oracle_root / ".git" / "info" / "exclude"
        try:
            if not stat.S_ISREG(info_exclude.lstat().st_mode):
                raise ProofError(
                    "private test-system ignore oracle exclude is not regular"
                )
            info_exclude.write_bytes(b"")
        except ProofError:
            raise
        except OSError as error:
            raise ProofError(
                f"cannot isolate private test-system ignore oracle excludes: {error}"
            ) from error

        unset = _test_surface_git_process(
            oracle_root,
            ["config", "--local", "--unset-all", "core.ignoreCase"],
            env=env,
        )
        if unset.returncode not in {0, 5}:
            raise ProofError(
                "cannot reset private test-system ignore oracle core.ignoreCase: "
                + unset.stderr.decode("utf-8", errors="replace").strip()
            )
        if source_ignore_case is not None:
            _checked_test_surface_git(
                oracle_root,
                [
                    "config",
                    "--local",
                    "--replace-all",
                    "core.ignoreCase",
                    source_ignore_case,
                ],
                env=env,
            )

        for relative, blob in sorted(blobs.items()):
            _materialize_test_surface_oracle_file(
                oracle_root,
                relative,
                blob.content,
            )
        for relative in candidates:
            _materialize_test_surface_oracle_file(oracle_root, relative, b"")

        input_bytes = b"".join(
            relative.encode("utf-8") + b"\0" for relative in candidates
        )
        check = _checked_test_surface_git(
            oracle_root,
            ["check-ignore", "--no-index", "-v", "-n", "-z", "--stdin"],
            env=env,
            input_bytes=input_bytes,
            allowed_exit_codes=frozenset({0, 1}),
        )
        if check.stderr:
            raise ProofError("test-system ignore oracle wrote unexpected diagnostics")
        return _parse_test_surface_ignore_matches(
            check.stdout,
            candidates=candidates,
            blobs=blobs,
        )


def _require_regular_ignored_test_surface_path(repo_root: Path, relative: str) -> None:
    path = repo_root.joinpath(*PurePosixPath(relative).parts)
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        raise ProofError(
            f"ignored test-system marker disappeared: {relative}"
        ) from None
    except OSError as error:
        raise ProofError(
            f"cannot inspect ignored test-system marker {relative}: {error}"
        ) from error
    if not stat.S_ISREG(metadata.st_mode):
        raise ProofError(
            f"ignored test-system marker is not a regular file: {relative}"
        )


def _test_surface_paths(repo_root: Path) -> list[_TestSurfacePath]:
    _, cached_entries = _cached_index_entries(repo_root)
    untracked = _git(
        repo_root,
        [
            "ls-files",
            "--others",
            "--exclude-per-directory=.gitignore",
            "-z",
            "--",
            ".",
        ],
    )
    ignored = _git(
        repo_root,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-per-directory=.gitignore",
            "-z",
            "--",
            *TEST_SURFACE_IGNORED_PATHSPECS,
        ],
        timeout_seconds=GIT_IGNORED_TEST_SURFACE_TIMEOUT_SECONDS,
    )
    by_path: dict[str, _TestSurfacePath] = {}
    for entry in cached_entries:
        relative = entry.relative_path
        if relative in by_path:
            raise ProofError(f"cached index listing repeats path {relative!r}")
        by_path[relative] = entry

    for raw_path in _test_surface_nul_records(
        untracked,
        label="untracked test-system surface listing",
    ):
        relative = _strict_test_surface_path(
            raw_path,
            label="untracked test-system surface listing",
        )
        by_path.setdefault(
            relative,
            _TestSurfacePath(
                relative_path=relative,
                tracked=False,
                index_mode=None,
            ),
        )

    ignored_by_path: dict[str, _TestSurfacePath] = {}
    for raw_path in _test_surface_nul_records(
        ignored,
        label="ignored test-system surface listing",
    ):
        relative = _strict_test_surface_path(
            raw_path,
            label="ignored test-system surface listing",
        )
        if not _is_test_surface_candidate(relative):
            continue
        if relative in ignored_by_path:
            raise ProofError(
                f"ignored test-system surface listing repeats path {relative!r}"
            )
        _require_regular_ignored_test_surface_path(repo_root, relative)
        ignored_by_path[relative] = _TestSurfacePath(
            relative_path=relative,
            tracked=False,
            index_mode=None,
        )

    all_paths = set(by_path) | set(ignored_by_path)
    if len(all_paths) > TEST_SURFACE_MAX_PATHS:
        raise ProofError(
            f"test-system surface has {len(all_paths)} paths; "
            f"limit is {TEST_SURFACE_MAX_PATHS}"
        )
    casefolded: dict[str, str] = {}
    for relative in sorted(all_paths):
        previous = casefolded.setdefault(relative.casefold(), relative)
        if previous != relative:
            raise ProofError(
                "test-system surface contains a Windows case-fold collision: "
                f"{previous!r} and {relative!r}"
            )

    head_exemptions = _head_ignored_test_surface_paths(
        repo_root,
        sorted(ignored_by_path),
    )
    for relative, entry in ignored_by_path.items():
        if relative not in head_exemptions:
            by_path[relative] = entry
    return [by_path[relative] for relative in sorted(by_path)]


def _is_python_test_marker(name: str) -> bool:
    return name.endswith(".py") and (
        name.startswith("test_") or name.endswith("_test.py")
    )


def _is_test_surface_candidate(relative: str) -> bool:
    name = PurePosixPath(relative).name
    return (
        relative == "justfile"
        or name == "Cargo.toml"
        or _is_python_test_marker(name)
        or name
        in {
            "package.json",
            "pyproject.toml",
            "pytest.ini",
            "setup.cfg",
            "tox.ini",
        }
        or TEST_SURFACE_JAVASCRIPT_CONFIG_RE.fullmatch(name) is not None
        or TEST_SURFACE_JAVASCRIPT_TEST_RE.search(name) is not None
        or name.endswith("_test.go")
        or name.endswith(".bats")
    )


def _read_test_surface_candidate(
    repo_root: Path,
    entry: _TestSurfacePath,
) -> bytes | None:
    if entry.tracked and entry.index_mode not in CACHED_INDEX_REGULAR_MODES:
        kind = {
            "120000": "symlink",
            "160000": "gitlink",
        }.get(entry.index_mode or "", "nonregular index entry")
        raise ProofError(
            f"tracked test-system marker is a {kind}: {entry.relative_path}"
        )
    path = repo_root.joinpath(*PurePosixPath(entry.relative_path).parts)
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        if entry.tracked:
            return None
        raise ProofError(
            f"untracked test-system marker disappeared: {entry.relative_path}"
        ) from None
    except OSError as error:
        raise ProofError(
            f"cannot inspect test-system marker {entry.relative_path}: {error}"
        ) from error
    if not stat.S_ISREG(metadata.st_mode):
        raise ProofError(
            f"test-system marker is not a regular file: {entry.relative_path}"
        )
    try:
        with path.open("rb") as source:
            opened_metadata = os.fstat(source.fileno())
            if not stat.S_ISREG(opened_metadata.st_mode):
                raise ProofError(
                    f"test-system marker is not a regular file: {entry.relative_path}"
                )
            content = source.read(TEST_SURFACE_MAX_CANDIDATE_BYTES + 1)
    except ProofError:
        raise
    except OSError as error:
        raise ProofError(
            f"cannot read test-system marker {entry.relative_path}: {error}"
        ) from error
    if len(content) > TEST_SURFACE_MAX_CANDIDATE_BYTES:
        raise ProofError(
            f"test-system marker exceeds the {TEST_SURFACE_MAX_CANDIDATE_BYTES}-byte "
            f"limit: {entry.relative_path}"
        )
    return content


def _test_surface_text(content: bytes, *, relative: str) -> str:
    try:
        return content.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ProofError(f"test-system manifest is not UTF-8: {relative}") from error


def _test_surface_toml(content: bytes, *, relative: str) -> dict[str, Any]:
    try:
        value = tomllib.loads(_test_surface_text(content, relative=relative))
    except tomllib.TOMLDecodeError as error:
        raise ProofError(
            f"cannot parse test-system manifest {relative}: {error}"
        ) from error
    if not isinstance(value, dict):
        raise ProofError(f"test-system manifest must be a table: {relative}")
    return value


def _is_pytest_requirement(value: object) -> bool:
    if not isinstance(value, str):
        return False
    return (
        re.match(
            r"^\s*pytest(?:\[[^\]]+\])?(?=$|\s|[!<=>~;@])",
            value,
            flags=re.IGNORECASE,
        )
        is not None
    )


def _contains_pytest_dependency(value: object) -> bool:
    if _is_pytest_requirement(value):
        return True
    if isinstance(value, list):
        return any(_contains_pytest_dependency(item) for item in value)
    if isinstance(value, dict):
        return any(
            (isinstance(key, str) and key.casefold().replace("_", "-") == "pytest")
            or _contains_pytest_dependency(item)
            for key, item in value.items()
        )
    return False


def _pyproject_pytest_markers(
    content: bytes,
    *,
    relative: str,
) -> list[_TestSurfaceMarker]:
    document = _test_surface_toml(content, relative=relative)
    markers: list[_TestSurfaceMarker] = []
    tool = document.get("tool")
    if isinstance(tool, dict) and "pytest" in tool:
        markers.append(_TestSurfaceMarker("pytest-config", "tool.pytest"))

    dependency_surfaces: list[object] = []
    project = document.get("project")
    if isinstance(project, dict):
        dependency_surfaces.extend(
            project.get(key)
            for key in ("dependencies", "optional-dependencies")
            if key in project
        )
    for key in ("dependency-groups",):
        if key in document:
            dependency_surfaces.append(document[key])
    if isinstance(tool, dict):
        poetry = tool.get("poetry")
        if isinstance(poetry, dict):
            dependency_surfaces.extend(
                poetry.get(key)
                for key in ("dependencies", "dev-dependencies", "group")
                if key in poetry
            )
        pdm = tool.get("pdm")
        if isinstance(pdm, dict) and "dev-dependencies" in pdm:
            dependency_surfaces.append(pdm["dev-dependencies"])
        uv = tool.get("uv")
        if isinstance(uv, dict) and "dev-dependencies" in uv:
            dependency_surfaces.append(uv["dev-dependencies"])
    if any(_contains_pytest_dependency(value) for value in dependency_surfaces):
        markers.append(_TestSurfaceMarker("pytest-dependency", "pytest"))
    return markers


def _ini_pytest_markers(
    content: bytes,
    *,
    relative: str,
) -> list[_TestSurfaceMarker]:
    parser = configparser.ConfigParser(interpolation=None)
    try:
        parser.read_string(_test_surface_text(content, relative=relative))
    except configparser.Error as error:
        raise ProofError(
            f"cannot parse test-system manifest {relative}: {error}"
        ) from error
    markers: list[_TestSurfaceMarker] = []
    if any(section.casefold() in {"pytest", "tool:pytest"} for section in parser):
        markers.append(_TestSurfaceMarker("pytest-config", "ini-section"))
    dependency_keys = {"deps", "install-requires", "tests-require"}
    for section in parser.sections():
        for key, value in parser.items(section):
            normalized_key = key.casefold().replace("_", "-")
            if normalized_key in dependency_keys and any(
                _is_pytest_requirement(line.strip()) for line in value.splitlines()
            ):
                markers.append(_TestSurfaceMarker("pytest-dependency", "pytest"))
                return markers
    return markers


def _package_json_markers(
    content: bytes,
    *,
    relative: str,
) -> list[_TestSurfaceMarker]:
    try:
        document = json.loads(_test_surface_text(content, relative=relative))
    except json.JSONDecodeError as error:
        raise ProofError(
            f"cannot parse test-system manifest {relative}: {error}"
        ) from error
    if not isinstance(document, dict):
        raise ProofError(f"test-system package manifest must be an object: {relative}")
    markers: list[_TestSurfaceMarker] = []
    scripts = document.get("scripts", {})
    if not isinstance(scripts, dict):
        raise ProofError(f"test-system package scripts must be an object: {relative}")
    for name, command in sorted(scripts.items()):
        if not isinstance(name, str):
            raise ProofError(f"test-system package script name is invalid: {relative}")
        if not isinstance(command, str):
            raise ProofError(
                f"test-system package script {name!r} must be a string: {relative}"
            )
        if command:
            markers.append(_TestSurfaceMarker("package-script-command", name, command))
        if name == "test" or name.startswith("test:"):
            markers.append(_TestSurfaceMarker("javascript-test-script", name, command))
    for group in (
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ):
        dependencies = document.get(group, {})
        if not isinstance(dependencies, dict):
            raise ProofError(
                f"test-system package dependency group {group!r} must be an object: "
                f"{relative}"
            )
        for dependency in sorted(dependencies):
            if dependency in TEST_SURFACE_JAVASCRIPT_RUNNERS:
                markers.append(
                    _TestSurfaceMarker("javascript-runner-dependency", dependency)
                )
    return markers


def _test_surface_markers(
    relative: str,
    content: bytes,
) -> list[_TestSurfaceMarker]:
    name = PurePosixPath(relative).name
    markers: list[_TestSurfaceMarker] = []
    if relative == "justfile":
        normalized = (
            _test_surface_text(content, relative=relative)
            .replace("\r\n", "\n")
            .replace("\r", "\n")
        )
        markers.append(
            _TestSurfaceMarker(
                "just-command-manifest",
                "lf-normalized-utf8-v1",
                sha256_bytes(normalized.encode("utf-8")),
            )
        )
    if name == "Cargo.toml":
        markers.append(_TestSurfaceMarker("cargo-manifest"))
    if _is_python_test_marker(name):
        markers.append(_TestSurfaceMarker("python-test-file"))
    if name == "pytest.ini":
        markers.append(_TestSurfaceMarker("pytest-config", "pytest.ini"))
    elif name == "pyproject.toml":
        markers.extend(_pyproject_pytest_markers(content, relative=relative))
    elif name in {"setup.cfg", "tox.ini"}:
        markers.extend(_ini_pytest_markers(content, relative=relative))
    if name == "package.json":
        markers.extend(_package_json_markers(content, relative=relative))
    config_match = TEST_SURFACE_JAVASCRIPT_CONFIG_RE.fullmatch(name)
    if config_match is not None:
        markers.append(
            _TestSurfaceMarker("javascript-runner-config", config_match.group(1))
        )
    test_match = TEST_SURFACE_JAVASCRIPT_TEST_RE.search(name)
    if test_match is not None:
        markers.append(_TestSurfaceMarker("javascript-test-file", test_match.group(1)))
    if name.endswith("_test.go"):
        markers.append(_TestSurfaceMarker("go-test-file"))
    if name.endswith(".bats"):
        markers.append(_TestSurfaceMarker("bats-test-file"))
    return sorted(
        set(markers),
        key=lambda marker: (marker.kind, marker.detail, marker.value),
    )


def _cargo_test_surface_paths(
    candidate_contents: Mapping[str, bytes],
) -> set[str]:
    root_manifest = "codex-rs/Cargo.toml"
    content = candidate_contents.get(root_manifest)
    if content is None:
        return set()
    document = _test_surface_toml(content, relative=root_manifest)
    workspace = document.get("workspace")
    if not isinstance(workspace, dict):
        raise ProofError(f"test-system Cargo workspace is missing: {root_manifest}")
    members = workspace.get("members")
    if not isinstance(members, list) or not members:
        raise ProofError(
            f"test-system Cargo workspace has no explicit members: {root_manifest}"
        )
    claimed = {root_manifest}
    for member in members:
        if not isinstance(member, str):
            raise ProofError("test-system Cargo workspace member must be a string")
        if any(character in member for character in "*?[]"):
            raise ProofError(
                "test-system Cargo workspace member must be exact, not a glob: "
                f"{member!r}"
            )
        normalized = _strict_test_surface_path(
            member.encode("utf-8"),
            label="test-system Cargo workspace members",
        )
        claimed.add(f"codex-rs/{normalized}/Cargo.toml")
    return claimed


def _is_beneath(relative: str, root: str) -> bool:
    return relative == root or relative.startswith(root + "/")


def _test_surface_marker_owners(
    relative: str,
    marker: _TestSurfaceMarker,
    *,
    cargo_paths: set[str],
) -> tuple[str, ...]:
    name = PurePosixPath(relative).name
    if marker.kind == "just-command-manifest":
        if (
            relative == "justfile"
            and marker.detail == "lf-normalized-utf8-v1"
            and marker.value == TEST_SURFACE_REVIEWED_JUSTFILE_SHA256.get(relative)
        ):
            return ("reviewed-command-surface",)
        return ()
    if marker.kind == "cargo-manifest":
        if relative in cargo_paths:
            return ("rust-nextest", "rust-doctest")
        if relative == "tools/argument-comment-lint/Cargo.toml":
            return ("argument-comment-lint-native",)
        return ()
    if marker.kind == "python-test-file":
        if _is_beneath(relative, "sdk/python/tests"):
            return ("python-pytest",)
        if name.startswith("test_") and any(
            _is_beneath(relative, root) for root in TEST_SURFACE_PYTHON_ROOTS
        ):
            return ("python-unittest",)
        return ()
    if marker.kind in {"pytest-config", "pytest-dependency"}:
        if relative in {
            "sdk/python/pyproject.toml",
            "sdk/python/pytest.ini",
            "sdk/python/setup.cfg",
            "sdk/python/tox.ini",
        }:
            return ("python-pytest",)
        return ()
    if marker.kind == "javascript-test-file":
        if (
            _is_beneath(relative, "sdk/typescript/tests")
            and marker.detail == "test"
            and name.endswith(".test.ts")
        ):
            return ("javascript-jest",)
        return ()
    if marker.kind == "javascript-runner-config":
        if relative == "sdk/typescript/jest.config.cjs" and marker.detail == "jest":
            return ("javascript-jest",)
        return ()
    if marker.kind == "javascript-test-script":
        known_scripts = {
            "package.json": {
                "test:scripts": (
                    "node scripts/run-python.js scripts/root_maintenance.py test-python"
                ),
                "test:scripts:changed": (
                    "node scripts/run-python.js scripts/root_maintenance.py "
                    "test-python --changed"
                ),
            },
            "sdk/typescript/package.json": {
                "test": "jest",
                "test:watch": "jest --watch",
            },
        }
        if marker.value == known_scripts.get(relative, {}).get(marker.detail):
            return (
                ("python-unittest",)
                if relative == "package.json"
                else ("javascript-jest",)
            )
        return ()
    if marker.kind == "package-script-command":
        reviewed = TEST_SURFACE_REVIEWED_PACKAGE_SCRIPTS.get(relative, {}).get(
            marker.detail
        )
        if reviewed is not None and marker.value == reviewed[0]:
            return reviewed[1]
        return ()
    if marker.kind == "javascript-runner-dependency":
        if relative == "sdk/typescript/package.json" and marker.detail in {
            "jest",
            "ts-jest",
        }:
            return ("javascript-jest",)
        return ()
    return ()


def _audit_test_system_surface(repo_root: Path) -> None:
    candidate_contents: dict[str, bytes] = {}
    for entry in _test_surface_paths(repo_root):
        if not _is_test_surface_candidate(entry.relative_path):
            continue
        content = _read_test_surface_candidate(repo_root, entry)
        if content is not None:
            candidate_contents[entry.relative_path] = content

    cargo_paths = _cargo_test_surface_paths(candidate_contents)
    unclaimed: list[str] = []
    for relative, content in sorted(candidate_contents.items()):
        for marker in _test_surface_markers(relative, content):
            owners = _test_surface_marker_owners(
                relative,
                marker,
                cargo_paths=cargo_paths,
            )
            if not owners:
                detail = f":{marker.detail}" if marker.detail else ""
                unclaimed.append(f"{relative} [{marker.kind}{detail}]")
    if unclaimed:
        displayed = ", ".join(unclaimed[:20])
        omitted = len(unclaimed) - min(len(unclaimed), 20)
        suffix = f", and {omitted} more" if omitted else ""
        raise ProofError(
            f"test-system surface audit found {len(unclaimed)} unclaimed marker(s): "
            f"{displayed}{suffix}"
        )


def workspace_fingerprint(repo_root: Path) -> str:
    """Mirror core's completion-proof HEAD plus worktree identity algorithm."""
    env = _strip_completion_proof_env(os.environ)
    env["GIT_OPTIONAL_LOCKS"] = "0"
    head_result = _run_bounded_git_process(
        repo_root,
        ["rev-parse", "--verify", "HEAD"],
        env=env,
    )
    head_identity: str | None = None
    if head_result.returncode == 0:
        try:
            parsed_head = head_result.stdout.decode("utf-8").strip()
        except UnicodeDecodeError as error:
            raise ProofError("git HEAD identity is not UTF-8") from error
        if parsed_head:
            head_identity = parsed_head
    cached_index_bytes, _ = _cached_index_entries(repo_root)
    status_bytes = _git(
        repo_root,
        [
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
    )
    paths = _workspace_paths(status_bytes)
    if len(paths) > WORKSPACE_MAX_PATHS:
        raise ProofError(
            f"workspace fingerprint has {len(paths)} changed paths; limit is {WORKSPACE_MAX_PATHS}"
        )
    manifest = bytearray(f"total_paths={len(paths)}\n".encode())
    observed_bytes = 0
    for relative in paths:
        path = repo_root / relative
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            metadata = None
        except OSError as error:
            raise ProofError(
                f"cannot inspect changed path {relative}: {error}"
            ) from error
        mode = metadata.st_mode if metadata is not None else 0
        declared_bytes = (
            metadata.st_size if metadata is not None and stat.S_ISREG(mode) else 0
        )
        observed_bytes += declared_bytes
        if observed_bytes > WORKSPACE_MAX_BYTES:
            raise ProofError(
                f"workspace fingerprint changed bytes exceed {WORKSPACE_MAX_BYTES}"
            )
        if metadata is None:
            kind = "missing"
            digest = ""
        elif stat.S_ISLNK(mode):
            kind = "symlink"
            digest = sha256_bytes(os.readlink(path).encode("utf-8"))
        elif stat.S_ISREG(mode):
            kind = "file"
            try:
                content = path.read_bytes()
            except OSError as error:
                raise ProofError(
                    f"cannot read changed path {relative}: {error}"
                ) from error
            digest = sha256_bytes(content)
        elif stat.S_ISDIR(mode):
            kind = "directory"
            digest = ""
        else:
            kind = "other"
            digest = ""
        manifest.extend(relative.encode("utf-8"))
        manifest.extend(b"\0")
        manifest.extend(kind.encode())
        manifest.extend(b"\0")
        manifest.extend(str(declared_bytes).encode())
        manifest.extend(b"\0")
        manifest.extend(digest.encode())
        manifest.extend(b"\n")
    worktree_identity = sha256_bytes(
        b"KD4_WORKSPACE_WORKTREE_GENERATION_V2\n"
        + cached_index_bytes
        + b"\0status\0"
        + status_bytes
        + b"\0manifest\0"
        + bytes(manifest)
    )
    completion_identity = bytearray(b"KD4_COMPLETION_PROOF_WORKSPACE_V3\0")
    if head_identity is None:
        completion_identity.extend(b"unborn-head\0")
    else:
        completion_identity.extend(b"head\0")
        completion_identity.extend(head_identity.encode("utf-8"))
    completion_identity.extend(b"\0worktree\0")
    completion_identity.extend(worktree_identity.encode("ascii"))
    return sha256_bytes(bytes(completion_identity))


def _host_identity() -> dict[str, str]:
    return {
        "hostname": socket.gethostname(),
        "system": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
    }


def _resolve_path(repo_root: Path, value: str) -> Path:
    path = Path(value)
    return path if path.is_absolute() else repo_root / path


def load_config(
    path: Path, *, allow_test_config: bool = False
) -> tuple[Path, dict[str, Any]]:
    resolved = path.resolve()
    if resolved != DEFAULT_CONFIG.resolve() and not allow_test_config:
        raise ProofError(
            "alternate completion-proof config is allowed only in runner integration tests"
        )
    try:
        config = tomllib.loads(resolved.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ProofError(
            f"cannot load completion-proof config {resolved}: {error}"
        ) from error
    allowed_fields = {
        "schema_version",
        "policy_id",
        "frozen_inventory_hash",
        "canonical_command",
        "focused_command",
        "documentation_command",
        "repository_root",
        "frozen_inventory",
        "replacement_ledger",
        "host_platform",
        "focused_inventory_evidence",
        "baseline_exception",
        "validation",
    }
    if allow_test_config:
        allowed_fields.add("testing_current_inventory")
    unknown_fields = sorted(set(config) - allowed_fields)
    if unknown_fields:
        raise ProofError(
            f"completion-proof config contains unknown fields: {unknown_fields}"
        )
    if config.get("schema_version") != 2:
        raise ProofError("completion-proof config schema_version must be 2")
    if config.get("canonical_command") != EXACT_COMMAND:
        raise ProofError(f"canonical_command must be exactly {EXACT_COMMAND!r}")
    if config.get("focused_command") != FOCUSED_COMMAND_TEMPLATE:
        raise ProofError(
            f"focused_command must be exactly {FOCUSED_COMMAND_TEMPLATE!r}"
        )
    policy_id = config.get("policy_id")
    if not isinstance(policy_id, str) or not policy_id.strip():
        raise ProofError("completion-proof policy_id must be a nonempty string")
    configured_inventory_hash = config.get("frozen_inventory_hash")
    if (
        not isinstance(configured_inventory_hash, str)
        or SHA256_RE.fullmatch(configured_inventory_hash) is None
    ):
        raise ProofError(
            "completion-proof frozen_inventory_hash must be a lowercase SHA-256"
        )
    if not allow_test_config and policy_id != KD4_POLICY_ID:
        raise ProofError(f"KD4 completion-proof policy_id must be {KD4_POLICY_ID!r}")
    if not allow_test_config and configured_inventory_hash != KD4_FROZEN_INVENTORY_HASH:
        raise ProofError(
            "KD4 completion-proof frozen_inventory_hash does not match the immutable "
            "baseline policy"
        )
    documentation_command = config.get("documentation_command")
    if (
        not isinstance(documentation_command, str)
        or not documentation_command
        or documentation_command != documentation_command.strip()
    ):
        raise ProofError("documentation_command must be an exact nonempty string")
    if not allow_test_config and documentation_command != "just source-map-check":
        raise ProofError(
            "KD4 documentation_command must be exactly 'just source-map-check'"
        )
    focused_inventory_evidence = config.get("focused_inventory_evidence")
    if focused_inventory_evidence is not None:
        if (
            not isinstance(focused_inventory_evidence, dict)
            or set(focused_inventory_evidence) != {"validation_ids"}
            or focused_inventory_evidence.get("validation_ids")
            != list(CURRENT_EVIDENCE_VALIDATION_IDS)
        ):
            raise ProofError(
                "focused_inventory_evidence must contain only the exact ordered "
                f"validation_ids {list(CURRENT_EVIDENCE_VALIDATION_IDS)!r}"
            )
    configured_root = config.get("repository_root")
    repo_root = (
        _resolve_path(resolved.parent, str(configured_root)).resolve()
        if configured_root
        else REPO_ROOT.resolve()
    )
    return repo_root, config


def validation_configs(
    config: Mapping[str, Any],
    *,
    allow_test_config: bool,
) -> dict[str, dict[str, Any]]:
    raw_validations = config.get("validation")
    if not isinstance(raw_validations, list) or not raw_validations:
        raise ProofError(
            "completion-proof config must contain nonzero validation entries"
        )
    by_runner: dict[str, dict[str, Any]] = {}
    ids: set[str] = set()
    for raw in raw_validations:
        if not isinstance(raw, dict):
            raise ProofError("completion-proof config contains a non-object validation")
        item = dict(raw)
        validation_id = str(item.get("id", ""))
        runner = str(item.get("runner", ""))
        if not validation_id or validation_id in ids:
            raise ProofError(f"duplicate or empty validation ID {validation_id!r}")
        if validation_id == CURRENT_EVIDENCE_VALIDATION_ID:
            raise ProofError(
                "inventory.current-evidence is a focused preparatory mode and cannot "
                "be declared as a required validation"
            )
        if runner not in VALIDATION_RUNNERS:
            raise ProofError(
                f"validation {validation_id} has unknown runner {runner!r}"
            )
        allowed_fields = {
            "id",
            "runner",
            "owned_paths",
            "consumed_paths",
            "path_set_paths",
            "evidence_path_manifests",
            "timeout_seconds",
        }
        if runner == "typed-validation":
            # `intended_ids` is a recognized-but-forbidden field. Keep it in the
            # schema long enough to produce the specific anti-forgery error below.
            allowed_fields.update({"validation_type", "intended_ids"})
            if allow_test_config:
                allowed_fields.update(
                    {"command", "cwd", "validation_failure_exit_codes"}
                )
        elif runner == "rust-gate":
            allowed_fields.add("gate")
        unknown_fields = sorted(set(item) - allowed_fields)
        if unknown_fields:
            raise ProofError(
                f"validation {validation_id} contains unknown fields: {unknown_fields}"
            )
        if runner in by_runner and runner not in {"typed-validation", "rust-gate"}:
            raise ProofError(
                f"completion-proof config repeats built-in runner {runner}"
            )
        for field_name in ("owned_paths", "consumed_paths"):
            values = item.get(field_name)
            if (
                not isinstance(values, list)
                or not values
                or not all(isinstance(value, str) and value.strip() for value in values)
            ):
                raise ProofError(
                    f"validation {validation_id} must have nonempty {field_name}"
                )
        for field_name in ("path_set_paths", "evidence_path_manifests"):
            values = item.get(field_name, [])
            if not isinstance(values, list) or not all(
                isinstance(value, str) and value.strip() and value == value.strip()
                for value in values
            ):
                raise ProofError(
                    f"validation {validation_id} must have valid {field_name}"
                )
        for value in item.get("evidence_path_manifests", []):
            normalized = value.replace("\\", "/")
            path = PurePosixPath(normalized)
            if (
                path.is_absolute()
                or normalized in {"", "."}
                or any(part in {"", ".", ".."} for part in path.parts)
                or any(character in normalized for character in "*?[")
            ):
                raise ProofError(
                    f"validation {validation_id} evidence_path_manifests must "
                    f"contain exact repository-relative paths: {value!r}"
                )
        timeout_seconds = item.get("timeout_seconds")
        if not isinstance(timeout_seconds, int) or timeout_seconds <= 0:
            raise ProofError(
                f"validation {validation_id} must have a positive integer timeout_seconds"
            )
        if runner == "typed-validation":
            validation_type = item.get("validation_type")
            if (
                not isinstance(validation_type, str)
                or not validation_type
                or validation_type != validation_type.strip()
                or VALIDATION_ID_RE.fullmatch(validation_type) is None
            ):
                raise ProofError(
                    f"typed validation {validation_id} has an invalid validation_type"
                )
            if "intended_ids" in item:
                raise ProofError(
                    f"typed validation {validation_id} cannot declare synthetic intended_ids"
                )
            command = item.get("command")
            validation_failure_exit_codes = item.get("validation_failure_exit_codes")
            if allow_test_config:
                if (
                    not isinstance(command, list)
                    or not command
                    or not all(
                        isinstance(argument, str) and argument for argument in command
                    )
                ):
                    raise ProofError(
                        f"test-only typed validation {validation_id} has no exact command"
                    )
                if not isinstance(validation_failure_exit_codes, list):
                    raise ProofError(
                        f"test-only typed validation {validation_id} must explicitly "
                        "declare validation_failure_exit_codes"
                    )
                if any(
                    isinstance(code, bool)
                    or not isinstance(code, int)
                    or code <= 0
                    or code > 2**31 - 1
                    for code in validation_failure_exit_codes
                ) or len(validation_failure_exit_codes) != len(
                    set(validation_failure_exit_codes)
                ):
                    raise ProofError(
                        f"test-only typed validation {validation_id} has invalid "
                        "validation_failure_exit_codes"
                    )
            else:
                forbidden = sorted(
                    field
                    for field in ("command", "validation_failure_exit_codes", "cwd")
                    if field in item
                )
                if forbidden:
                    raise ProofError(
                        f"KD4 typed validation {validation_id} cannot override "
                        f"code-owned execution fields {forbidden}"
                    )
            by_runner[f"typed-validation:{validation_id}"] = item
        elif runner == "rust-gate":
            gate = item.get("gate")
            if (
                not isinstance(gate, str)
                or not gate
                or gate != gate.strip()
                or VALIDATION_ID_RE.fullmatch(gate) is None
            ):
                raise ProofError(
                    f"Rust gate validation {validation_id} has an invalid gate name"
                )
            forbidden = sorted(
                field
                for field in (
                    "command",
                    "intended_ids",
                    "validation_type",
                    "validation_failure_exit_codes",
                    "cwd",
                )
                if field in item
            )
            if forbidden:
                raise ProofError(
                    f"Rust gate validation {validation_id} cannot declare {forbidden}"
                )
            if any(
                configured.get("gate") == gate
                for key, configured in by_runner.items()
                if key.startswith("rust-gate:")
            ):
                raise ProofError(f"completion-proof config repeats Rust gate {gate!r}")
            by_runner[f"rust-gate:{validation_id}"] = item
        else:
            if "validation_type" in item:
                raise ProofError(
                    f"structured runner {runner} cannot declare validation_type"
                )
            expected_id = BUILTIN_VALIDATIONS[runner]
            if validation_id != expected_id:
                raise ProofError(
                    f"built-in runner {runner} must use validation ID {expected_id}"
                )
            by_runner[runner] = item
        ids.add(validation_id)
    if not allow_test_config:
        expected_runners = {
            validation_id: runner
            for runner, validation_id in BUILTIN_VALIDATIONS.items()
        }
        expected_runners.update(
            {validation_id: "rust-gate" for validation_id in KD4_RUST_GATES}
        )
        expected_runners.update(
            {
                validation_id: "typed-validation"
                for validation_id in KD4_TYPED_VALIDATIONS
            }
        )
        actual_by_id = {
            str(item["id"]): str(item["runner"]) for item in by_runner.values()
        }
        if actual_by_id != expected_runners:
            missing = sorted(set(expected_runners) - set(actual_by_id))
            unexpected = sorted(set(actual_by_id) - set(expected_runners))
            mismatched = sorted(
                validation_id
                for validation_id in set(actual_by_id) & set(expected_runners)
                if actual_by_id[validation_id] != expected_runners[validation_id]
            )
            raise ProofError(
                "KD4 completion-proof validation policy mismatch: "
                f"missing={missing}, unexpected={unexpected}, mismatched={mismatched}"
            )
        if set(actual_by_id) != KD4_VALIDATION_IDS:
            raise ProofError(
                "KD4 completion-proof accepted validation set must contain exactly "
                f"{KD4_VALIDATION_COUNT} policy IDs"
            )
        typed_types = {
            validation_id: str(
                next(
                    item["validation_type"]
                    for item in by_runner.values()
                    if item["id"] == validation_id
                )
            )
            for validation_id in KD4_TYPED_VALIDATIONS
        }
        if typed_types != KD4_TYPED_VALIDATIONS:
            raise ProofError(
                "KD4 typed-validation semantic mapping does not match policy"
            )
        rust_gates = {
            validation_id: str(
                next(
                    item["gate"]
                    for item in by_runner.values()
                    if item["id"] == validation_id
                )
            )
            for validation_id in KD4_RUST_GATES
        }
        if rust_gates != KD4_RUST_GATES:
            raise ProofError("KD4 named Rust gate mapping does not match policy")
    return by_runner


def _network_disabled_env() -> dict[str, str]:
    # Validation code is part of the repository being certified. It must not
    # inherit the runtime's private attempt nonce/report path or the runner's
    # integration-test inputs, because either would let a validation child
    # impersonate the top-level certification process. Workers that genuinely
    # need a test-only config enter through the private Python unittest call
    # boundary; no environment variable grants that authority.
    env = _strip_completion_proof_env(os.environ)
    env.update(
        {
            "CARGO_NET_OFFLINE": "true",
            "UV_OFFLINE": "1",
            "NEXTEST_EXPERIMENTAL_LIBTEST_JSON": "1",
            "HTTP_PROXY": "http://127.0.0.1:9",
            "HTTPS_PROXY": "http://127.0.0.1:9",
            "ALL_PROXY": "http://127.0.0.1:9",
            "NO_PROXY": "127.0.0.1,localhost,::1",
            "no_proxy": "127.0.0.1,localhost,::1",
            "RUN_REAL_CODEX_TESTS": "0",
            "CI": "1",
        }
    )
    return env


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as input_file:
            for chunk in iter(lambda: input_file.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise ProofError(f"cannot hash executable identity {path}: {error}") from error
    return digest.hexdigest()


@dataclass(frozen=True)
class FileIdentityStart:
    requested: str
    resolved_path: str
    sha256_before: str

    def finish(self) -> tuple[dict[str, str], str | None]:
        try:
            sha256_after = _sha256_file(Path(self.resolved_path))
        except ProofError as error:
            sha256_after = ""
            identity_error = str(error)
        else:
            identity_error = None
            if sha256_after != self.sha256_before:
                identity_error = f"executable identity changed during execution: {self.resolved_path}"
        return (
            {
                "requested": self.requested,
                "resolved_path": self.resolved_path,
                "sha256_before": self.sha256_before,
                "sha256_after": sha256_after,
            },
            identity_error,
        )


@dataclass(frozen=True)
class RunnerProcessIdentityStart:
    pid: int
    parent_pid: int
    started_at: int
    args_hash: str
    executable: FileIdentityStart
    entrypoint: FileIdentityStart

    def finish(self) -> tuple[dict[str, object], str | None]:
        executable_identity, executable_error = self.executable.finish()
        entrypoint_identity, entrypoint_error = self.entrypoint.finish()
        errors = [
            error for error in (executable_error, entrypoint_error) if error is not None
        ]
        return (
            {
                "pid": self.pid,
                "parent_pid": self.parent_pid,
                "started_at": self.started_at,
                "ended_at": time.time_ns(),
                "args_hash": self.args_hash,
                "executable_identity": executable_identity,
                "entrypoint_identity": entrypoint_identity,
            },
            "; ".join(errors) if errors else None,
        )


def _runner_executable_path() -> Path:
    if os.name != "nt":
        return Path(sys.executable)

    import ctypes
    from ctypes import wintypes

    # A Windows venv redirector keeps its launcher in sys.executable, while the
    # authenticated pipe peer runs the base interpreter. Bind the actual image.
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetCurrentProcess.argtypes = []
    kernel32.GetCurrentProcess.restype = wintypes.HANDLE
    kernel32.QueryFullProcessImageNameW.argtypes = [
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPWSTR,
        ctypes.POINTER(wintypes.DWORD),
    ]
    kernel32.QueryFullProcessImageNameW.restype = wintypes.BOOL
    buffer = ctypes.create_unicode_buffer(32768)
    size = wintypes.DWORD(len(buffer))
    if not kernel32.QueryFullProcessImageNameW(
        kernel32.GetCurrentProcess(), 0, buffer, ctypes.byref(size)
    ):
        raise ProofError(
            "cannot query runner process executable identity: "
            f"{ctypes.WinError(ctypes.get_last_error())}"
        )
    return Path(buffer.value)


def _capture_runner_process_identity() -> RunnerProcessIdentityStart:
    started_at = time.time_ns()
    return RunnerProcessIdentityStart(
        pid=os.getpid(),
        parent_pid=os.getppid(),
        started_at=started_at,
        args_hash=sha256_bytes(canonical_json(sys.argv)),
        executable=_capture_file_identity(sys.executable, _runner_executable_path()),
        entrypoint=_capture_file_identity(str(Path(__file__)), Path(__file__)),
    )


class _RunnerAttestationChannel:
    def __init__(self, channel: Any) -> None:
        self._channel = channel

    def close(self) -> None:
        try:
            self._channel.close()
        except OSError:
            pass

    def _write_all(self, value: bytes) -> None:
        if hasattr(self._channel, "sendall"):
            self._channel.sendall(value)
        else:
            view = memoryview(value)
            while view:
                written = self._channel.write(view)
                if written is None or written <= 0:
                    raise OSError("focused evidence channel write made no progress")
                view = view[written:]

    def _read_exact(self, count: int) -> bytes:
        value = bytearray()
        while len(value) < count:
            if hasattr(self._channel, "recv"):
                chunk = self._channel.recv(count - len(value))
            else:
                chunk = self._channel.read(count - len(value))
            if not chunk:
                break
            value.extend(chunk)
        return bytes(value)

    def exchange_focused_evidence(
        self, members: Sequence[tuple[str, bytes]]
    ) -> str:
        if tuple(name for name, _ in members) != FOCUSED_EVIDENCE_MEMBER_NAMES:
            raise ProofError("focused evidence members are not the exact ordered set")
        if any(not value for _, value in members):
            raise ProofError("focused evidence members must be nonempty")
        payload = b"".join(value for _, value in members)
        manifest = _canonical_jcs(
            {
                "schema_version": 1,
                "members": [
                    {
                        "name": name,
                        "length": len(value),
                        "sha256": sha256_bytes(value),
                    }
                    for name, value in members
                ],
            }
        )
        if not 0 < len(manifest) <= 16 * 1024:
            raise ProofError("focused evidence manifest length is out of bounds")
        if len(payload) > 128 * 1024 * 1024:
            raise ProofError("focused evidence payload exceeds its limit")
        header = b"".join(
            (
                b"KD4EVID1",
                bytes((1, 0)),
                len(members).to_bytes(2, "big"),
                (0).to_bytes(4, "big"),
                len(manifest).to_bytes(4, "big"),
                len(payload).to_bytes(8, "big"),
            )
        )
        frame = header + manifest + payload
        digest = hashlib.sha256(frame).digest()
        result: list[bytes] = []
        errors: list[BaseException] = []
        completed = threading.Event()

        def exchange() -> None:
            try:
                if hasattr(self._channel, "settimeout"):
                    self._channel.settimeout(FOCUSED_EVIDENCE_EXCHANGE_TIMEOUT_SECONDS)
                self._write_all(frame)
                result.append(self._read_exact(FOCUSED_EVIDENCE_ACK_LEN))
            except BaseException as error:
                errors.append(error)
            finally:
                completed.set()

        worker = threading.Thread(
            target=exchange,
            name="completion-proof-focused-evidence",
            daemon=True,
        )
        worker.start()
        completed.wait(FOCUSED_EVIDENCE_EXCHANGE_TIMEOUT_SECONDS)
        if not completed.is_set():
            self.close()
            raise ProofError("focused evidence acknowledgement timed out")
        if errors:
            raise ProofError(
                f"focused evidence exchange failed: {errors[0]}"
            ) from errors[0]
        acknowledgement = result[0] if result else b""
        if len(acknowledgement) != FOCUSED_EVIDENCE_ACK_LEN:
            raise ProofError("focused evidence acknowledgement was truncated")
        if (
            acknowledgement[:8] != b"KD4EVACK"
            or acknowledgement[8:10] != bytes((1, 0))
            or acknowledgement[10:12] != (0).to_bytes(2, "big")
            or acknowledgement[12:16] != (0).to_bytes(4, "big")
            or acknowledgement[16:] != digest
        ):
            raise ProofError("focused evidence acknowledgement was rejected or invalid")
        return digest.hex()


def _attest_runner_process(
    runtime: Mapping[str, str],
    runner_identity: RunnerProcessIdentityStart,
    *,
    retain_channel: bool = False,
) -> _RunnerAttestationChannel | None:
    endpoint = runtime["CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT"]
    payload = (
        canonical_json(
            {
                "schema_version": 1,
                "attempt_id": runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
                "nonce": runtime["CODEX_COMPLETION_PROOF_NONCE"],
                "process_id": runner_identity.pid,
                "entrypoint_path": runner_identity.entrypoint.resolved_path,
            }
        )
        + b"\n"
    )
    deadline = time.monotonic() + RUNNER_ATTESTATION_TIMEOUT_SECONDS
    completed = threading.Event()
    lock = threading.Lock()
    response_holder: list[bytes] = []
    retained_holder: list[_RunnerAttestationChannel] = []
    error_holder: list[BaseException] = []
    channel_holder: list[Any] = []

    def exchange() -> None:
        channel: Any | None = None
        try:
            if os.name == "nt":
                channel = open(endpoint, "r+b", buffering=0)
                with lock:
                    channel_holder.append(channel)
                channel.write(payload)
                response = channel.readline(
                    RUNNER_ATTESTATION_RESPONSE_LIMIT_BYTES + 1
                )
            else:
                channel = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                with lock:
                    channel_holder.append(channel)
                channel.settimeout(max(0.001, deadline - time.monotonic()))
                channel.connect(endpoint)
                channel.settimeout(max(0.001, deadline - time.monotonic()))
                channel.sendall(payload)
                response = b""
                while not response.endswith(b"\n"):
                    if len(response) > RUNNER_ATTESTATION_RESPONSE_LIMIT_BYTES:
                        break
                    channel.settimeout(max(0.001, deadline - time.monotonic()))
                    chunk = channel.recv(
                        RUNNER_ATTESTATION_RESPONSE_LIMIT_BYTES + 1 - len(response)
                    )
                    if not chunk:
                        break
                    response += chunk
            with lock:
                response_holder.append(response)
                if retain_channel and response == b"ok\n":
                    if hasattr(channel, "settimeout"):
                        channel.settimeout(None)
                    retained_holder.append(_RunnerAttestationChannel(channel))
                    channel = None
        except BaseException as error:
            with lock:
                error_holder.append(error)
        finally:
            if channel is not None:
                try:
                    channel.close()
                except OSError:
                    pass
            completed.set()

    worker = threading.Thread(
        target=exchange,
        name="completion-proof-runner-attestation",
        daemon=True,
    )
    worker.start()
    completed.wait(max(0.0, deadline - time.monotonic()))
    if not completed.is_set():
        with lock:
            channels = list(channel_holder)
        for channel in channels:
            try:
                channel.close()
            except OSError:
                pass
        raise ProofError(
            "could not complete private runner process attestation: "
            f"timed out after {RUNNER_ATTESTATION_TIMEOUT_SECONDS:g} seconds"
        )
    with lock:
        errors = list(error_holder)
        responses = list(response_holder)
    if errors:
        error = errors[0]
        raise ProofError(
            f"could not complete private runner process attestation: {error}"
        ) from error
    response = responses[0] if responses else b""
    if len(response) > RUNNER_ATTESTATION_RESPONSE_LIMIT_BYTES:
        raise ProofError(
            "private runner process attestation response exceeded its limit"
        )
    if response != b"ok\n":
        raise ProofError("private runner process attestation was rejected")
    if retain_channel:
        with lock:
            retained = list(retained_holder)
        if len(retained) != 1:
            raise ProofError("private runner attestation channel was not retained")
        return retained[0]
    return None


def _capture_file_identity(requested: str, path: Path) -> FileIdentityStart:
    try:
        resolved = path.resolve(strict=True)
        metadata = resolved.stat()
    except OSError as error:
        raise ProofError(
            f"cannot resolve executable identity {requested!r}: {error}"
        ) from error
    if not stat.S_ISREG(metadata.st_mode):
        raise ProofError(
            f"executable identity {requested!r} does not resolve to a regular file"
        )
    return FileIdentityStart(
        requested=requested,
        resolved_path=str(resolved),
        sha256_before=_sha256_file(resolved),
    )


def _resolve_launch_target(
    requested: str,
    *,
    cwd: Path,
    env: Mapping[str, str],
) -> FileIdentityStart:
    if not requested:
        raise ProofError("cannot launch an empty executable")
    requested_path = Path(requested)
    if requested_path.is_absolute():
        candidate = requested_path
    elif requested_path.parent != Path("."):
        candidate = cwd / requested_path
    else:
        found = shutil.which(requested, path=env.get("PATH"))
        if found is None:
            raise ProofError(f"cannot resolve launch target {requested!r} on PATH")
        candidate = Path(found)
    return _capture_file_identity(requested, candidate)


def _unresolved_identity(requested: str) -> dict[str, str]:
    return {
        "requested": requested,
        "resolved_path": "",
        "sha256_before": "",
        "sha256_after": "",
    }


@dataclass
class ChildProcess:
    validation_id: str
    execution_id: str
    pid: int
    executable: str
    args_hash: str
    started_at: int
    ended_at: int
    exit_code: int | None
    launch_target_identity: Mapping[str, str]
    launched_argv: tuple[str, ...] = ()
    launched_cwd: str = ""
    stdout_bytes: bytes = b""

    def as_json(self) -> dict[str, object]:
        return {
            "validation_id": self.validation_id,
            "execution_id": self.execution_id,
            "pid": self.pid,
            "executable": self.executable,
            "args_hash": self.args_hash,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "exit_code": self.exit_code,
            "launch_target_identity": dict(self.launch_target_identity),
        }


@dataclass
class ProcessResult:
    returncode: int | None
    stdout: str
    stderr: str
    child: ChildProcess
    invocation_error: str | None = None


def _unlaunched_child(
    *,
    validation_id: str,
    execution_id: str,
    command: Sequence[str],
) -> ChildProcess:
    now = time.time_ns()
    requested = str(command[0]) if command else ""
    return ChildProcess(
        validation_id=validation_id,
        execution_id=execution_id,
        pid=0,
        executable="",
        args_hash=sha256_bytes(canonical_json(list(command))),
        started_at=now,
        ended_at=now,
        exit_code=None,
        launch_target_identity=_unresolved_identity(requested),
    )


def run_process(
    *,
    validation_id: str,
    execution_id: str,
    command: Sequence[str],
    cwd: Path,
    env: Mapping[str, str],
    timeout_seconds: int,
    stdin_bytes: bytes | None = None,
) -> ProcessResult:
    requested = str(command[0]) if command else ""
    child_env = _strip_completion_proof_env(env)
    try:
        identity_start = _resolve_launch_target(
            requested,
            cwd=cwd,
            env=child_env,
        )
    except ProofError as error:
        now = time.time_ns()
        requested_command = list(command)
        child = ChildProcess(
            validation_id=validation_id,
            execution_id=execution_id,
            pid=0,
            executable="",
            args_hash=sha256_bytes(canonical_json(requested_command)),
            started_at=now,
            ended_at=now,
            exit_code=None,
            launch_target_identity=_unresolved_identity(requested),
        )
        return ProcessResult(
            returncode=None,
            stdout="",
            stderr="",
            child=child,
            invocation_error=str(error),
        )
    launched_command = [identity_start.resolved_path, *map(str, command[1:])]
    started_at = time.time_ns()
    args_hash = sha256_bytes(canonical_json(launched_command))
    bounded = run_bounded_process(
        launched_command,
        cwd=cwd,
        env=child_env,
        timeout_seconds=timeout_seconds,
        stdin_bytes=stdin_bytes,
        stdout_limit_bytes=PROCESS_STDOUT_LIMIT_BYTES,
        stderr_limit_bytes=PROCESS_STDERR_LIMIT_BYTES,
    )
    pid = bounded.pid
    stdout = bounded.stdout.decode("utf-8", errors="replace")
    stderr = bounded.stderr.decode("utf-8", errors="replace")
    returncode = _portable_exit_code(bounded.returncode)
    invocation_error = bounded.supervision_error
    if bounded.returncode is not None and returncode is None:
        invocation_error = (
            f"{invocation_error}; runner returned an exit code outside the signed "
            "32-bit report contract"
            if invocation_error
            else "runner returned an exit code outside the signed 32-bit report contract"
        )
    launch_target_identity, identity_error = identity_start.finish()
    if identity_error:
        invocation_error = (
            f"{invocation_error}; {identity_error}"
            if invocation_error
            else identity_error
        )
    child = ChildProcess(
        validation_id=validation_id,
        execution_id=execution_id,
        pid=pid,
        executable=identity_start.resolved_path,
        args_hash=args_hash,
        started_at=started_at,
        ended_at=time.time_ns(),
        exit_code=returncode,
        launch_target_identity=launch_target_identity,
        launched_argv=tuple(launched_command),
        launched_cwd=str(cwd.resolve()),
        stdout_bytes=bounded.stdout,
    )
    return ProcessResult(
        returncode=returncode,
        stdout=stdout,
        stderr=stderr,
        child=child,
        invocation_error=invocation_error,
    )


def _rust_inventory(
    repo_root: Path, env: Mapping[str, str], temp_dir: Path
) -> tuple[list[dict[str, object]], ChildProcess]:
    execution_id = str(uuid.uuid4())
    result = run_process(
        validation_id="inventory.rust-nextest",
        execution_id=execution_id,
        command=["cargo", "nextest", "list", "--workspace", "-T", "json"],
        cwd=repo_root / "codex-rs",
        env=env,
        timeout_seconds=1800,
    )
    if result.invocation_error or result.returncode != 0:
        raise ProofError(
            result.invocation_error
            or f"Rust nextest discovery failed with exit {result.returncode}: {result.stderr[-4000:]}"
        )
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ProofError("Rust nextest discovery did not return valid JSON") from error
    suites = value.get("rust-suites")
    if not isinstance(suites, dict):
        raise ProofError("Rust nextest discovery omitted rust-suites")
    rows: list[dict[str, object]] = []
    for binary_id, suite in suites.items():
        if not isinstance(suite, dict):
            raise ProofError(f"invalid nextest suite {binary_id}")
        package = str(suite.get("package-name", ""))
        binary_name = str(suite.get("binary-name", ""))
        testcases = suite.get("testcases")
        if not package or not binary_name or not isinstance(testcases, dict):
            raise ProofError(f"nextest suite {binary_id} is missing package/testcases")
        cwd = Path(str(suite.get("cwd", repo_root / "codex-rs")))
        try:
            source = cwd.resolve().relative_to(repo_root).as_posix()
        except ValueError:
            source = f"codex-rs/{package}"
        for test_name, testcase in testcases.items():
            if not isinstance(testcase, dict):
                raise ProofError(f"invalid nextest case {binary_id}::{test_name}")
            native_id = f"{package}::{binary_name}${test_name}"
            rows.append(
                {
                    "baseline_id": f"rust-nextest::{native_id}",
                    "framework": "rust-nextest",
                    "native_id": native_id,
                    "source": source,
                    "ignored": bool(testcase.get("ignored", False)),
                    "platforms": ["windows"],
                }
            )
    if len(rows) != value.get("test-count") or not rows:
        raise ProofError(
            "Rust nextest discovery count mismatch or zero selection: "
            f"declared={value.get('test-count')} parsed={len(rows)}"
        )
    return rows, result.child


def _load_report(path: Path, *, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ProofError(f"{label} did not produce a valid report: {error}") from error
    if not isinstance(value, dict):
        raise ProofError(f"{label} report must be an object")
    return value


def _load_unittest_collection_report(path: Path) -> dict[str, Any]:
    label = "root unittest discovery"

    def object_from_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ProofError(f"{label} report contains duplicate JSON keys")
            value[key] = item
        return value

    def reject_constant(value: str) -> None:
        raise ProofError(f"{label} report contains invalid JSON constant {value}")

    try:
        value = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=object_from_pairs,
            parse_constant=reject_constant,
        )
    except ProofError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ProofError(f"{label} did not produce a valid report: {error}") from error
    if not isinstance(value, dict):
        raise ProofError(f"{label} report must be an object")
    return value


def _valid_unittest_collection_string(value: object, *, nonempty: bool = True) -> bool:
    return (
        isinstance(value, str)
        and (bool(value) or not nonempty)
        and unicodedata.normalize("NFC", value) == value
        and not any(
            ord(character) < 0x20 or ord(character) == 0x7F for character in value
        )
    )


def _unittest_collection_source(repo_root: Path, value: object) -> str:
    if not _valid_unittest_collection_string(value):
        raise ProofError("root unittest discovery test has invalid source path")
    assert isinstance(value, str)
    parsed = PurePosixPath(value)
    if (
        "\\" in value
        or ":" in value
        or parsed.is_absolute()
        or parsed.suffix != ".py"
        or not parsed.parts
        or any(part in {"", ".", ".."} for part in parsed.parts)
        or parsed.as_posix() != value
    ):
        raise ProofError("root unittest discovery test has invalid source path")
    repository = repo_root.resolve()
    resolved = (repository / Path(*parsed.parts)).resolve()
    try:
        resolved.relative_to(repository)
    except ValueError as error:
        raise ProofError(
            "root unittest discovery test source escapes repository"
        ) from error
    if not resolved.is_file():
        raise ProofError("root unittest discovery test source is not a file")
    return value


def _parse_unittest_collection_v2(
    report: dict[str, Any], repo_root: Path
) -> list[tuple[str, str, bool]]:
    expected_report_keys = {
        "schema_version",
        "report_type",
        "framework",
        "classification",
        "tests",
        "selected_count",
        "discovery_errors",
        "duplicate_ids",
        "metadata_errors",
    }
    if set(report) != expected_report_keys:
        raise ProofError("root unittest discovery report has invalid keys")
    if type(report["schema_version"]) is not int or report["schema_version"] != 2:
        raise ProofError("root unittest discovery report has invalid schema version")
    if report["report_type"] != "CompletionProofUnittestCollectionV2":
        raise ProofError("root unittest discovery report has invalid report type")
    if report["framework"] != "python-unittest":
        raise ProofError("root unittest discovery report has invalid framework")
    if report["classification"] != "discovered":
        raise ProofError("root unittest discovery report is not discovered")

    tests = report["tests"]
    selected_count = report["selected_count"]
    discovery_errors = report["discovery_errors"]
    duplicate_ids = report["duplicate_ids"]
    metadata_errors = report["metadata_errors"]
    if not isinstance(tests, list):
        raise ProofError("root unittest discovery report tests must be a list")
    if type(selected_count) is not int or selected_count != len(tests):
        raise ProofError("root unittest discovery selected count is inconsistent")
    for values, label in (
        (discovery_errors, "discovery errors"),
        (duplicate_ids, "duplicate IDs"),
        (metadata_errors, "metadata errors"),
    ):
        if not isinstance(values, list) or not all(
            _valid_unittest_collection_string(item) for item in values
        ):
            raise ProofError(f"root unittest discovery {label} must be valid strings")
        if values != sorted(set(values)):
            raise ProofError(f"root unittest discovery {label} are not canonical")

    seen_native_ids: set[str] = set()
    observed_duplicate_ids: set[str] = set()
    parsed_tests: list[tuple[str, str, bool]] = []
    expected_test_keys = {
        "id",
        "source_path",
        "declared_subtest_sites",
        "skipped_at_discovery",
        "skip_reason",
    }
    for item in tests:
        if not isinstance(item, dict) or set(item) != expected_test_keys:
            raise ProofError("root unittest discovery test has invalid keys")
        native_id = item["id"]
        if not _valid_unittest_collection_string(native_id):
            raise ProofError("root unittest discovery test has invalid ID")
        assert isinstance(native_id, str)
        source_path = _unittest_collection_source(repo_root, item["source_path"])
        source_module = ".".join(
            PurePosixPath(source_path).with_suffix("").parts
        )
        if not native_id.startswith(f"{source_module}."):
            raise ProofError(
                "root unittest discovery test source path does not match ID module"
            )
        declared_sites = item["declared_subtest_sites"]
        if not isinstance(declared_sites, list):
            raise ProofError(
                "root unittest discovery declared subtest sites must be a list"
            )
        parsed_sites: list[tuple[str, int, int]] = []
        for site in declared_sites:
            if not isinstance(site, dict) or set(site) != {"path", "line", "column"}:
                raise ProofError(
                    "root unittest discovery declared subtest site has invalid keys"
                )
            if site["path"] != source_path:
                raise ProofError(
                    "root unittest discovery declared subtest site path is inconsistent"
                )
            line = site["line"]
            column = site["column"]
            if (
                type(line) is not int
                or line <= 0
                or type(column) is not int
                or column <= 0
            ):
                raise ProofError(
                    "root unittest discovery declared subtest site location is invalid"
                )
            parsed_sites.append((source_path, line, column))
        if parsed_sites != sorted(set(parsed_sites)):
            raise ProofError(
                "root unittest discovery declared subtest sites are not canonical"
            )

        skipped = item["skipped_at_discovery"]
        skip_reason = item["skip_reason"]
        if type(skipped) is not bool:
            raise ProofError("root unittest discovery skip state is invalid")
        if not _valid_unittest_collection_string(skip_reason, nonempty=False):
            raise ProofError("root unittest discovery skip reason is invalid")
        if skipped != bool(skip_reason):
            raise ProofError(
                "root unittest discovery skip state and reason are inconsistent"
            )
        if native_id in seen_native_ids:
            observed_duplicate_ids.add(native_id)
        seen_native_ids.add(native_id)
        parsed_tests.append((native_id, source_path, skipped))

    observed_duplicates = sorted(observed_duplicate_ids)
    if duplicate_ids != observed_duplicates:
        raise ProofError("root unittest discovery duplicate IDs are inconsistent")
    if observed_duplicates:
        raise ProofError("root unittest discovery contains duplicate IDs")
    if discovery_errors:
        raise ProofError("root unittest discovery contains discovery errors")
    if metadata_errors:
        raise ProofError("root unittest discovery contains metadata errors")
    if not parsed_tests:
        raise ProofError("root unittest discovery selected zero tests")
    return parsed_tests


def _unittest_inventory(
    repo_root: Path, env: Mapping[str, str], temp_dir: Path
) -> tuple[list[dict[str, object]], ChildProcess]:
    output = temp_dir / "unittest-collection.json"
    execution_id = str(uuid.uuid4())
    result = run_process(
        validation_id="inventory.root-unittest",
        execution_id=execution_id,
        command=[
            "uv",
            "run",
            "--offline",
            "--frozen",
            "--project",
            "scripts",
            "python",
            str(repo_root / "scripts" / "completion_proof_unittest.py"),
            "collect",
            "--output",
            str(output),
        ],
        cwd=repo_root,
        env=env,
        timeout_seconds=600,
    )
    report = _load_unittest_collection_report(output)
    if result.invocation_error or result.returncode != 0:
        raise ProofError(
            result.invocation_error
            or f"root unittest discovery failed: {json.dumps(report)[:4000]}"
        )
    parsed_tests = _parse_unittest_collection_v2(report, repo_root)
    rows = [
        {
            "baseline_id": f"python-unittest::{native_id}",
            "framework": "python-unittest",
            "native_id": native_id,
            "source": source_path,
            "ignored": ignored,
            "platforms": ["windows"],
        }
        for native_id, source_path, ignored in parsed_tests
    ]
    return rows, result.child


def _doctest_inventory(
    repo_root: Path, env: Mapping[str, str]
) -> tuple[list[dict[str, object]], ChildProcess]:
    execution_id = str(uuid.uuid4())
    result = run_process(
        validation_id="inventory.rust-doctest",
        execution_id=execution_id,
        command=[
            "cargo",
            "test",
            "--workspace",
            "--doc",
            "--",
            "--list",
            "--format",
            "terse",
        ],
        cwd=repo_root / "codex-rs",
        env=env,
        timeout_seconds=1800,
    )
    if result.invocation_error or result.returncode != 0:
        raise ProofError(
            result.invocation_error
            or f"Rust doctest discovery failed with exit {result.returncode}: {result.stderr[-4000:]}"
        )
    native_ids = sorted(
        {
            line.removesuffix(": test").strip()
            for line in result.stdout.splitlines()
            if line.strip().endswith(": test")
        }
    )
    rows = [
        {
            "baseline_id": f"rust-doctest::{native_id}",
            "framework": "rust-doctest",
            "native_id": native_id,
            "source": f"codex-rs/{native_id.split(' - ', 1)[0].replace(chr(92), '/')}",
            "ignored": False,
            "platforms": ["windows"],
        }
        for native_id in native_ids
    ]
    if not rows:
        raise ProofError("Rust doctest discovery selected zero tests")
    return rows, result.child


def _load_pytest_collection_report(
    path: Path, repo_root: Path
) -> tuple[dict[str, Any], list[dict[str, object]]]:
    label = "Python SDK pytest discovery"

    def object_from_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ProofError(f"{label} report contains duplicate JSON keys")
            value[key] = item
        return value

    def reject_constant(value: str) -> None:
        raise ProofError(f"{label} report contains invalid JSON constant {value}")

    try:
        report = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=object_from_pairs,
            parse_constant=reject_constant,
        )
    except ProofError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ProofError(f"{label} did not produce a valid report: {error}") from error
    if not isinstance(report, dict):
        raise ProofError(f"{label} report must be an object")

    expected_report_keys = {
        "schema_version",
        "framework",
        "classification",
        "tests",
        "selected_count",
        "duplicate_ids",
        "collection_errors",
        "pytest_exit_code",
    }
    if set(report) != expected_report_keys:
        raise ProofError("Python SDK pytest discovery report has invalid keys")
    if type(report["schema_version"]) is not int or report["schema_version"] != 1:
        raise ProofError(
            "Python SDK pytest discovery report has invalid schema version"
        )
    if report["framework"] != "python-pytest":
        raise ProofError("Python SDK pytest discovery report has invalid framework")
    if report["classification"] != "discovered":
        raise ProofError("Python SDK pytest discovery report is not discovered")
    if (
        type(report["pytest_exit_code"]) is not int
        or report["pytest_exit_code"] != 0
    ):
        raise ProofError(
            "Python SDK pytest discovery report has invalid pytest exit code"
        )

    tests = report["tests"]
    selected_count = report["selected_count"]
    duplicate_ids = report["duplicate_ids"]
    collection_errors = report["collection_errors"]
    if not isinstance(tests, list):
        raise ProofError("Python SDK pytest discovery report tests must be a list")
    if type(selected_count) is not int or selected_count != len(tests):
        raise ProofError("Python SDK pytest discovery selected count is inconsistent")
    if not isinstance(duplicate_ids, list) or not all(
        isinstance(item, str)
        and bool(item)
        and unicodedata.normalize("NFC", item) == item
        and not any(
            ord(character) < 0x20 or ord(character) == 0x7F for character in item
        )
        for item in duplicate_ids
    ):
        raise ProofError("Python SDK pytest discovery duplicate IDs are invalid")
    if duplicate_ids != sorted(set(duplicate_ids)):
        raise ProofError("Python SDK pytest discovery duplicate IDs are not canonical")
    if not isinstance(collection_errors, list) or not all(
        isinstance(item, str)
        and bool(item)
        and unicodedata.normalize("NFC", item) == item
        and not any(
            ord(character) < 0x20 or ord(character) == 0x7F for character in item
        )
        for item in collection_errors
    ):
        raise ProofError("Python SDK pytest discovery collection errors are invalid")
    if collection_errors != sorted(set(collection_errors)):
        raise ProofError(
            "Python SDK pytest discovery collection errors are not canonical"
        )

    seen_native_ids: set[str] = set()
    observed_duplicate_ids: set[str] = set()
    parsed_tests: list[tuple[str, str, bool]] = []
    sdk_root = (repo_root / "sdk" / "python").resolve()
    tests_root = (sdk_root / "tests").resolve()
    try:
        tests_root.relative_to(sdk_root)
    except ValueError as error:
        raise ProofError("Python SDK pytest tests root escapes sdk/python") from error
    for item in tests:
        if not isinstance(item, dict) or set(item) != {"id", "skip_markers"}:
            raise ProofError("Python SDK pytest discovery test has invalid keys")
        native_id = item["id"]
        skip_markers = item["skip_markers"]
        if (
            not isinstance(native_id, str)
            or not native_id
            or unicodedata.normalize("NFC", native_id) != native_id
            or any(
                ord(character) < 0x20 or ord(character) == 0x7F
                for character in native_id
            )
        ):
            raise ProofError("Python SDK pytest discovery test has invalid ID")
        if not isinstance(skip_markers, list):
            raise ProofError("Python SDK pytest discovery skip markers must be a list")
        for marker in skip_markers:
            if not isinstance(marker, dict) or set(marker) != {"name", "reason"}:
                raise ProofError("Python SDK pytest discovery skip marker has invalid keys")
            if (
                not isinstance(marker["name"], str)
                or marker["name"] not in {"skip", "skipif"}
                or not isinstance(marker["reason"], str)
                or unicodedata.normalize("NFC", marker["reason"])
                != marker["reason"]
                or any(
                    ord(character) < 0x20 or ord(character) == 0x7F
                    for character in marker["reason"]
                )
            ):
                raise ProofError("Python SDK pytest discovery skip marker is invalid")

        node_path, separator, selector = native_id.partition("::")
        parsed_path = PurePosixPath(node_path)
        if (
            not separator
            or not selector
            or "\\" in node_path
            or ":" in node_path
            or parsed_path.is_absolute()
            or parsed_path.suffix != ".py"
            or not parsed_path.parts
            or parsed_path.parts[0] != "tests"
            or any(part in {"", ".", ".."} for part in parsed_path.parts)
            or parsed_path.as_posix() != node_path
        ):
            raise ProofError("Python SDK pytest discovery test has invalid source path")
        resolved_source = (sdk_root / Path(*parsed_path.parts)).resolve()
        try:
            resolved_source.relative_to(tests_root)
        except ValueError as error:
            raise ProofError(
                "Python SDK pytest discovery test source escapes sdk/python/tests"
            ) from error
        if not resolved_source.is_file():
            raise ProofError("Python SDK pytest discovery test source is not a file")
        if native_id in seen_native_ids:
            observed_duplicate_ids.add(native_id)
        seen_native_ids.add(native_id)
        parsed_tests.append((native_id, node_path, bool(skip_markers)))

    observed_duplicates = sorted(observed_duplicate_ids)
    if duplicate_ids != observed_duplicates:
        raise ProofError("Python SDK pytest discovery duplicate IDs are inconsistent")
    if observed_duplicates:
        raise ProofError("Python SDK pytest discovery contains duplicate IDs")
    if collection_errors:
        raise ProofError("Python SDK pytest discovery contains collection errors")
    if not parsed_tests:
        raise ProofError("Python SDK pytest discovery selected zero tests")

    rows = [
        {
            "baseline_id": f"python-pytest::{native_id}",
            "framework": "python-pytest",
            "native_id": native_id,
            "source": f"sdk/python/{node_path}",
            "ignored": ignored,
            "platforms": ["windows"],
        }
        for native_id, node_path, ignored in parsed_tests
    ]
    rows.sort(key=lambda row: str(row["baseline_id"]))
    return report, rows


def _pytest_inventory(
    repo_root: Path, env: Mapping[str, str], temp_dir: Path
) -> tuple[list[dict[str, object]], ChildProcess]:
    output = temp_dir / "pytest-collection.json"
    execution_id = str(uuid.uuid4())
    result = run_process(
        validation_id="inventory.sdk-python-pytest",
        execution_id=execution_id,
        command=[
            "uv",
            "run",
            "--offline",
            "--frozen",
            "--directory",
            str(repo_root / "sdk" / "python"),
            "--group",
            "dev",
            "python",
            str(repo_root / "scripts" / "completion_proof_pytest.py"),
            "collect",
            "--output",
            str(output),
        ],
        cwd=repo_root,
        env=env,
        timeout_seconds=900,
    )
    report, rows = _load_pytest_collection_report(output, repo_root)
    if result.invocation_error or result.returncode != 0:
        raise ProofError(
            result.invocation_error
            or f"Python SDK pytest discovery failed: {json.dumps(report)[:4000]}"
        )
    return rows, result.child


_DESCRIBE_RE = re.compile(r'^\s*describe\s*\(\s*(["\'])(.*?)\1\s*,.*?\{\s*$')
_TEST_RE = re.compile(
    r'^\s*(it|test)(\.[A-Za-z_][A-Za-z0-9_]*)?\s*\(\s*(["\'])(.*?)\3\s*,'
)
_ANY_TEST_CALL_RE = re.compile(r"\b(?:it|test)(?:\.[A-Za-z_][A-Za-z0-9_]*)*\s*\(")


def _brace_delta(line: str) -> int:
    delta = 0
    quote: str | None = None
    escaped = False
    index = 0
    while index < len(line):
        char = line[index]
        if quote is not None:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
        elif char in {"'", '"', "`"}:
            quote = char
        elif char == "/" and index + 1 < len(line) and line[index + 1] == "/":
            break
        elif char == "{":
            delta += 1
        elif char == "}":
            delta -= 1
        index += 1
    return delta


def _jest_inventory(repo_root: Path) -> list[dict[str, object]]:
    tests_root = repo_root / "sdk" / "typescript" / "tests"
    files = sorted(tests_root.glob("**/*.test.ts"))
    if not files:
        raise ProofError("TypeScript SDK Jest discovery selected zero test files")
    rows: list[dict[str, object]] = []
    unknown: list[str] = []
    for path in files:
        depth = 0
        describes: list[tuple[int, str]] = []
        text = path.read_text(encoding="utf-8")
        for line_number, line in enumerate(text.splitlines(), start=1):
            while describes and describes[-1][0] > depth:
                describes.pop()
            describe = _DESCRIBE_RE.match(line)
            test = _TEST_RE.match(line)
            if describe is not None:
                title = describe.group(2)
                describes.append((depth + max(1, _brace_delta(line)), title))
            elif test is not None:
                modifier = test.group(2) or ""
                if modifier not in {"", ".skip", ".todo"}:
                    unknown.append(
                        f"{path}:{line_number}: unsupported Jest modifier {modifier}"
                    )
                title = test.group(4)
                full_name = " ".join([*(item[1] for item in describes), title])
                source = path.relative_to(repo_root).as_posix()
                native_id = f"{source}::{full_name}"
                rows.append(
                    {
                        "baseline_id": f"javascript-jest::{native_id}",
                        "framework": "javascript-jest",
                        "native_id": native_id,
                        "source": source,
                        "ignored": modifier in {".skip", ".todo"},
                        "platforms": ["windows"],
                    }
                )
            elif _ANY_TEST_CALL_RE.search(line):
                unknown.append(
                    f"{path}:{line_number}: unrecognized dynamic Jest test declaration"
                )
            depth += _brace_delta(line)
    if unknown:
        raise ProofError("unknown Jest test declarations:\n" + "\n".join(unknown))
    if not rows:
        raise ProofError("TypeScript SDK Jest discovery selected zero tests")
    return rows


def _native_adapter_path(repo_root: Path, runner: str) -> Path:
    adapter = NATIVE_ADAPTERS.get(runner)
    if adapter is None:
        raise ProofError(f"unknown native adapter runner {runner!r}")
    path = (repo_root / adapter["relative_path"]).resolve()
    try:
        path.relative_to(repo_root.resolve())
    except ValueError as error:
        raise ProofError(f"native adapter escaped the repository: {path}") from error
    return path


def _argument_lint_source(repo_root: Path, item: Mapping[str, object]) -> str:
    kind = item.get("kind")
    test_id = item.get("id")
    if not isinstance(test_id, str) or not test_id:
        raise ProofError("argument-comment-lint inventory contains an empty test ID")
    if kind == "rust-lib":
        relative = (
            "tools/argument-comment-lint/src/comment_parser.rs"
            if "::comment_parser." in test_id
            else "tools/argument-comment-lint/src/lib.rs"
        )
    elif kind == "rust-bin":
        relative = "tools/argument-comment-lint/src/bin/argument-comment-lint.rs"
    elif kind == "rust-doctest":
        relative = "tools/argument-comment-lint/src/lib.rs"
    elif kind == "dylint-ui":
        ui_case = item.get("ui_case")
        if not isinstance(ui_case, str) or not ui_case:
            raise ProofError(
                f"argument-comment-lint UI test {test_id!r} omitted its source case"
            )
        relative = f"tools/argument-comment-lint/ui/{ui_case}.rs"
    else:
        raise ProofError(
            f"argument-comment-lint test {test_id!r} has unknown kind {kind!r}"
        )
    if not (repo_root / relative).is_file():
        raise ProofError(
            f"argument-comment-lint test {test_id!r} source does not exist: {relative}"
        )
    return relative


def _validate_argument_lint_inventory_item(item: Mapping[str, object]) -> str:
    expected_fields = {
        "id",
        "kind",
        "cargo_target",
        "native_id",
        "ui_case",
        "doctest_item",
        "doctest_ordinal",
    }
    if set(item) != expected_fields:
        raise ProofError(
            "argument-comment-lint inventory item did not match its trusted schema"
        )
    test_id = item.get("id")
    kind = item.get("kind")
    cargo_target = item.get("cargo_target")
    native_id = item.get("native_id")
    ui_case = item.get("ui_case")
    doctest_item = item.get("doctest_item")
    doctest_ordinal = item.get("doctest_ordinal")
    expected_targets = {
        "rust-lib": ["--lib"],
        "rust-bin": ["--bin", "argument-comment-lint"],
        "rust-doctest": ["--doc"],
        "dylint-ui": ["--lib"],
    }
    if (
        not isinstance(test_id, str)
        or not isinstance(kind, str)
        or kind not in expected_targets
        or not test_id.startswith(f"argument-comment-lint::{kind}::")
        or cargo_target != expected_targets[kind]
        or not isinstance(native_id, str)
        or not native_id
    ):
        raise ProofError(
            "argument-comment-lint inventory item contained an invalid identity"
        )
    if kind == "dylint-ui":
        if (
            native_id != "ui"
            or not isinstance(ui_case, str)
            or not ui_case
            or doctest_item is not None
            or doctest_ordinal is not None
        ):
            raise ProofError(
                "argument-comment-lint UI inventory item had an invalid mapping"
            )
    elif kind == "rust-doctest":
        if (
            ui_case is not None
            or not isinstance(doctest_item, str)
            or not doctest_item
            or isinstance(doctest_ordinal, bool)
            or not isinstance(doctest_ordinal, int)
            or doctest_ordinal < 0
        ):
            raise ProofError(
                "argument-comment-lint doctest inventory item had an invalid mapping"
            )
    elif ui_case is not None or doctest_item is not None or doctest_ordinal is not None:
        raise ProofError(
            "argument-comment-lint native inventory item had unexpected mapping fields"
        )
    return test_id


def _native_adapter_inventory(
    repo_root: Path,
    env: Mapping[str, str],
    runner: str,
) -> tuple[list[dict[str, object]], ChildProcess]:
    adapter = NATIVE_ADAPTERS.get(runner)
    if adapter is None:
        raise ProofError(f"unknown native adapter runner {runner!r}")
    adapter_path = _native_adapter_path(repo_root, runner)
    entrypoint_identity = _capture_file_identity(str(adapter_path), adapter_path)
    execution_id = str(uuid.uuid4())
    command = [sys.executable, str(adapter_path)]
    if runner == "argument-comment-lint-native":
        command.append("list")
    else:
        command.append("--list-json")
    result = run_process(
        validation_id=f"inventory.{adapter['validation_id']}",
        execution_id=execution_id,
        command=command,
        cwd=repo_root,
        env=env,
        timeout_seconds=300,
    )
    _, entrypoint_error = entrypoint_identity.finish()
    if result.invocation_error or entrypoint_error or result.returncode != 0:
        raise ProofError(
            result.invocation_error
            or entrypoint_error
            or (
                f"{runner} inventory failed with exit {result.returncode}: "
                f"{result.stderr[-4000:]}"
            )
        )
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ProofError(f"{runner} inventory did not return valid JSON") from error
    if not isinstance(report, dict):
        raise ProofError(f"{runner} inventory report must be an object")
    if (
        report.get("schema_version") != 1
        or report.get("report_type") != adapter["inventory_report_type"]
    ):
        raise ProofError(f"{runner} inventory returned an untrusted report schema")

    rows: list[dict[str, object]] = []
    if runner == "argument-comment-lint-native":
        raw_items = report.get("tests")
        count = report.get("count")
        if (
            not isinstance(raw_items, list)
            or not raw_items
            or isinstance(count, bool)
            or not isinstance(count, int)
            or count <= 0
            or count != len(raw_items)
        ):
            raise ProofError(
                "argument-comment-lint inventory count did not match its tests"
            )
        semantic_ids: list[str] = []
        for raw_item in raw_items:
            if not isinstance(raw_item, dict):
                raise ProofError(
                    "argument-comment-lint inventory contains a non-object test"
                )
            native_id = _validate_argument_lint_inventory_item(raw_item)
            semantic_ids.append(native_id)
            rows.append(
                {
                    "baseline_id": native_id,
                    "framework": adapter["framework"],
                    "native_id": native_id,
                    "source": _argument_lint_source(repo_root, raw_item),
                    "ignored": False,
                    "platforms": ["darwin", "linux", "windows"],
                }
            )
        if len(semantic_ids) != len(set(semantic_ids)):
            raise ProofError("argument-comment-lint inventory contains duplicate IDs")
    else:
        raw_items = report.get("cases")
        if (
            report.get("validation_id") != "windows-sandbox-smoke"
            or report.get("host_platform") != "windows"
            or not isinstance(raw_items, list)
        ):
            raise ProofError(
                "Windows sandbox inventory omitted its validation identity"
            )
        if len(raw_items) != 46:
            raise ProofError(
                f"Windows sandbox inventory selected {len(raw_items)} cases, expected 46"
            )
        source = "codex-rs/windows-sandbox-rs/sandbox_smoketests.py"
        if not (repo_root / source).is_file():
            raise ProofError(f"Windows sandbox smoke source does not exist: {source}")
        for raw_item in raw_items:
            if not isinstance(raw_item, dict):
                raise ProofError("Windows sandbox inventory contains a non-object case")
            native_id = raw_item.get("id")
            name = raw_item.get("name")
            if (
                not isinstance(native_id, str)
                or not native_id.startswith(
                    "python-script-case::windows-sandbox-smoke::"
                )
                or not isinstance(name, str)
                or not name
            ):
                raise ProofError("Windows sandbox inventory contains an invalid case")
            rows.append(
                {
                    "baseline_id": native_id,
                    "framework": adapter["framework"],
                    "native_id": native_id,
                    "source": source,
                    "ignored": False,
                    "platforms": ["windows"],
                }
            )

    ids = [str(row["baseline_id"]) for row in rows]
    if len(ids) != len(set(ids)):
        raise ProofError(f"{runner} inventory contains duplicate IDs")
    return rows, result.child


def discover_inventory(
    repo_root: Path,
    *,
    temp_dir: Path,
    jest_observation: dict[str, object] | None = None,
) -> tuple[list[dict[str, object]], list[ChildProcess]]:
    _audit_test_system_surface(repo_root)
    env = _network_disabled_env()
    rows: list[dict[str, object]] = []
    children: list[ChildProcess] = []
    rust_rows, rust_child = _rust_inventory(repo_root, env, temp_dir)
    rows.extend(rust_rows)
    children.append(rust_child)
    doctest_rows, doctest_child = _doctest_inventory(repo_root, env)
    rows.extend(doctest_rows)
    children.append(doctest_child)
    unittest_rows, unittest_child = _unittest_inventory(repo_root, env, temp_dir)
    rows.extend(unittest_rows)
    children.append(unittest_child)
    pytest_rows, pytest_child = _pytest_inventory(repo_root, env, temp_dir)
    rows.extend(pytest_rows)
    children.append(pytest_child)
    jest_started_at = time.time_ns()
    jest_rows = _jest_inventory(repo_root)
    jest_ended_at = time.time_ns()
    rows.extend(jest_rows)
    if jest_observation is not None:
        jest_ids = sorted(str(row["baseline_id"]) for row in jest_rows)
        jest_observation.update(
            {
                "observation_id": "inventory.sdk.typescript.jest",
                "execution_id": str(uuid.uuid4()),
                "runner_pid": os.getpid(),
                "started_at": str(jest_started_at),
                "ended_at": str(jest_ended_at),
                "discovered_count": len(jest_ids),
                "discovered_test_ids_sha256": hashlib.sha256(
                    _canonical_jcs(jest_ids)
                ).hexdigest(),
            }
        )
    for runner in ("argument-comment-lint-native", "windows-sandbox-smoke"):
        native_rows, native_child = _native_adapter_inventory(repo_root, env, runner)
        rows.extend(native_rows)
        children.append(native_child)
    rows.sort(key=lambda row: str(row["baseline_id"]))
    ids = [str(row["baseline_id"]) for row in rows]
    duplicates = sorted({test_id for test_id in ids if ids.count(test_id) > 1})
    if duplicates:
        raise ProofError(f"duplicate inventory identities: {duplicates[:20]}")
    return rows, children


def inventory_hash(rows: Iterable[Mapping[str, object]]) -> str:
    normalized = []
    for row in rows:
        normalized.append(
            {
                "baseline_id": str(row["baseline_id"]),
                "framework": str(row["framework"]),
                "native_id": str(row["native_id"]),
                "source": str(row["source"]),
                "ignored": bool(row.get("ignored", False)),
                "platforms": sorted(str(item) for item in row.get("platforms", [])),
            }
        )
    normalized.sort(key=lambda row: row["baseline_id"])
    return sha256_bytes(canonical_json({"schema_version": 1, "tests": normalized}))


def _load_json_object(path: Path, *, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ProofError(f"cannot load {label} {path}: {error}") from error
    if not isinstance(value, dict):
        raise ProofError(f"{label} must be a JSON object")
    return value


def _manifest_paths(repo_root: Path, config: Mapping[str, Any]) -> tuple[Path, Path]:
    frozen = config.get("frozen_inventory")
    ledger = config.get("replacement_ledger")
    if not isinstance(frozen, str) or not isinstance(ledger, str):
        raise ProofError("config must name frozen_inventory and replacement_ledger")
    return _resolve_path(repo_root, frozen), _resolve_path(repo_root, ledger)


def _current_reconciliation_status(
    repo_root: Path, config: Mapping[str, Any]
) -> tuple[int, int]:
    _, ledger_path = _manifest_paths(repo_root, config)
    ledger = _load_json_object(ledger_path, label="configured replacement ledger")
    if ledger.get("schema_version") != 1 or not isinstance(ledger.get("rows"), list):
        raise ProofError("configured replacement ledger has an invalid envelope")
    unresolved = 0
    pending_replacement_review = 0
    for row in ledger["rows"]:
        resolution = row.get("resolution")
        if resolution == "unresolved":
            unresolved += 1
        elif resolution == "replacement":
            pending_replacement_review += 1
    return unresolved, pending_replacement_review


def load_frozen_inventory(
    repo_root: Path,
    config: Mapping[str, Any],
) -> tuple[dict[str, Any], list[dict[str, Any]], str]:
    frozen_path, _ = _manifest_paths(repo_root, config)
    frozen = _load_json_object(frozen_path, label="frozen inventory")
    if frozen.get("schema_version") != 1:
        raise ProofError("frozen inventory schema_version must be 1")
    tests = frozen.get("tests")
    if not isinstance(tests, list) or not tests:
        raise ProofError("frozen inventory must contain a nonzero tests list")
    rows = [dict(row) for row in tests if isinstance(row, dict)]
    if len(rows) != len(tests):
        raise ProofError("frozen inventory contains a non-object row")
    actual_hash = inventory_hash(rows)
    if frozen.get("inventory_hash") != actual_hash:
        raise ProofError("frozen inventory hash does not match its rows")
    if config.get("frozen_inventory_hash") != actual_hash:
        raise ProofError(
            "configured frozen_inventory_hash does not match the frozen inventory rows"
        )
    ids = [str(row.get("baseline_id", "")) for row in rows]
    if not all(ids) or len(ids) != len(set(ids)):
        raise ProofError("frozen inventory has empty or duplicate baseline IDs")
    return frozen, rows, actual_hash


@dataclass
class Reconciliation:
    required_by_framework: dict[str, list[str]]
    exceptions: list[dict[str, Any]]
    additions: list[dict[str, Any]]
    overrides: list[dict[str, Any]]
    current_ids: set[str]


def _active_inventory_platform() -> str:
    return platform.system().casefold()


def _validate_exception_row_semantics(
    *,
    baseline_id: str,
    kind: str,
    row: Mapping[str, object],
    row_role: str,
) -> None:
    context = f"{baseline_id}: {kind} exception {row_role} inventory row"
    if kind == "live-service":
        if row.get("ignored") is not True:
            raise ProofError(f"{context} must have ignored=true")
        return
    if kind not in {"off-host", "platform-pending"}:
        return

    platforms = row.get("platforms")
    if (
        not isinstance(platforms, list)
        or not platforms
        or not all(
            isinstance(item, str) and bool(item) and item.strip() == item
            for item in platforms
        )
    ):
        raise ProofError(f"{context} must have exact nonempty platform names")
    normalized_platforms = {item.casefold() for item in platforms}
    active_platform = _active_inventory_platform()
    if active_platform in normalized_platforms:
        raise ProofError(f"{context} includes active platform {active_platform!r}")


def reconcile_inventory(
    repo_root: Path,
    config: Mapping[str, Any],
    current_rows: Sequence[Mapping[str, object]],
    *,
    known_validation_ids: set[str] | None = None,
) -> Reconciliation:
    frozen, baseline_rows, frozen_hash = load_frozen_inventory(repo_root, config)
    del frozen
    _, ledger_path = _manifest_paths(repo_root, config)
    ledger = _load_json_object(ledger_path, label="replacement ledger")
    unknown_ledger_fields = sorted(
        set(ledger)
        - {
            "schema_version",
            "frozen_inventory_hash",
            "rows",
            "additions",
            "overrides",
        }
    )
    if unknown_ledger_fields:
        raise ProofError(
            f"replacement ledger contains unknown fields: {unknown_ledger_fields}"
        )
    if ledger.get("schema_version") != 1:
        raise ProofError("replacement ledger schema_version must be 1")
    if ledger.get("frozen_inventory_hash") != frozen_hash:
        raise ProofError("replacement ledger is bound to a different frozen inventory")
    raw_ledger_rows = ledger.get("rows")
    if not isinstance(raw_ledger_rows, list):
        raise ProofError("replacement ledger rows must be a list")
    ledger_rows: dict[str, dict[str, Any]] = {}
    for value in raw_ledger_rows:
        if not isinstance(value, dict):
            raise ProofError("replacement ledger contains a non-object row")
        baseline_id = str(value.get("baseline_id", ""))
        if not baseline_id or baseline_id in ledger_rows:
            raise ProofError(
                f"replacement ledger duplicate/empty baseline ID {baseline_id!r}"
            )
        ledger_rows[baseline_id] = value
    baseline_by_id = {str(row["baseline_id"]): row for row in baseline_rows}
    baseline_ids = set(baseline_by_id)
    if set(ledger_rows) != baseline_ids:
        missing = sorted(baseline_ids - set(ledger_rows))
        extra = sorted(set(ledger_rows) - baseline_ids)
        raise ProofError(
            f"replacement ledger does not exactly cover baseline; missing={missing[:20]} extra={extra[:20]}"
        )
    current_by_id = {str(row["baseline_id"]): row for row in current_rows}
    if len(current_by_id) != len(current_rows):
        raise ProofError("current inventory has duplicate IDs")
    referenced_current: set[str] = set()
    exceptions: list[dict[str, Any]] = []
    executable_ids: set[str] = set()
    errors: list[str] = []
    for baseline_id in sorted(baseline_ids):
        row = ledger_rows[baseline_id]
        resolution = str(row.get("resolution", ""))
        if resolution == "replacement":
            expected_fields = {
                "baseline_id",
                "resolution",
                "replacement_ids",
                "preserved_behavior",
                "product_path",
                "validation_id",
            }
            unknown_fields = sorted(set(row) - expected_fields)
            missing_fields = sorted(expected_fields - set(row))
            if unknown_fields or missing_fields:
                errors.append(
                    f"{baseline_id}: replacement fields mismatch; "
                    f"missing={missing_fields} unknown={unknown_fields}"
                )
            replacements = row.get("replacement_ids")
            if not isinstance(replacements, list) or not replacements:
                errors.append(f"{baseline_id}: replacement has no replacement_ids")
                continue
            replacement_ids = [str(item) for item in replacements]
            frozen_replacement_ids = sorted(set(replacement_ids) & baseline_ids)
            if frozen_replacement_ids:
                errors.append(
                    f"{baseline_id}: replacement IDs must be disjoint from frozen "
                    f"baseline IDs: {frozen_replacement_ids}"
                )
            if baseline_id in current_by_id:
                errors.append(f"{baseline_id}: old baseline test is still discovered")
            missing = [item for item in replacement_ids if item not in current_by_id]
            if missing:
                errors.append(
                    f"{baseline_id}: replacement IDs are not discovered: {missing}"
                )
            for field_name in ("preserved_behavior", "product_path", "validation_id"):
                if not str(row.get(field_name, "")).strip():
                    errors.append(f"{baseline_id}: replacement is missing {field_name}")
            validation_id = str(row.get("validation_id", ""))
            if (
                known_validation_ids is not None
                and validation_id not in known_validation_ids
            ):
                errors.append(
                    f"{baseline_id}: replacement names unknown validation {validation_id!r}"
                )
            referenced_current.update(replacement_ids)
            executable_ids.update(replacement_ids)
        elif resolution == "exception":
            expected_fields = {"baseline_id", "resolution", "provenance"}
            unknown_fields = sorted(set(row) - expected_fields)
            missing_fields = sorted(expected_fields - set(row))
            if unknown_fields or missing_fields:
                errors.append(
                    f"{baseline_id}: exception fields mismatch; "
                    f"missing={missing_fields} unknown={unknown_fields}"
                )
            provenance = row.get("provenance")
            if not isinstance(provenance, dict):
                errors.append(f"{baseline_id}: exception is missing provenance")
                continue
            provenance_fields = {"kind", "source", "text"}
            if set(provenance) != provenance_fields:
                errors.append(
                    f"{baseline_id}: exception provenance fields mismatch; "
                    f"missing={sorted(provenance_fields - set(provenance))} "
                    f"unknown={sorted(set(provenance) - provenance_fields)}"
                )
            kind = str(provenance.get("kind", ""))
            if kind not in {
                "protected",
                "generated",
                "live-service",
                "off-host",
                "platform-pending",
            }:
                errors.append(f"{baseline_id}: invalid exception kind {kind!r}")
            else:
                try:
                    _validate_exception_row_semantics(
                        baseline_id=baseline_id,
                        kind=kind,
                        row=baseline_by_id[baseline_id],
                        row_role="frozen",
                    )
                except ProofError as error:
                    errors.append(str(error))
            if (
                not str(provenance.get("source", "")).strip()
                or not str(provenance.get("text", "")).strip()
            ):
                errors.append(
                    f"{baseline_id}: exception provenance must include source and text"
                )
            exceptions.append(
                {
                    "baseline_id": baseline_id,
                    "kind": kind,
                    "source": provenance.get("source"),
                    "text": provenance.get("text"),
                }
            )
            if baseline_id not in current_by_id:
                errors.append(f"{baseline_id}: exception test is no longer discovered")
            else:
                if kind in {
                    "protected",
                    "generated",
                    "live-service",
                    "off-host",
                    "platform-pending",
                }:
                    try:
                        _validate_exception_row_semantics(
                            baseline_id=baseline_id,
                            kind=kind,
                            row=current_by_id[baseline_id],
                            row_role="current",
                        )
                    except ProofError as error:
                        errors.append(str(error))
                referenced_current.add(baseline_id)
                if kind in {"protected", "generated"}:
                    executable_ids.add(baseline_id)
        elif resolution == "unresolved":
            if set(row) != {"baseline_id", "resolution"}:
                errors.append(
                    f"{baseline_id}: unresolved row contains fields other than "
                    "baseline_id and resolution"
                )
            errors.append(f"{baseline_id}: baseline resolution remains unresolved")
        else:
            errors.append(f"{baseline_id}: unknown resolution {resolution!r}")

    raw_additions = ledger.get("additions", [])
    if not isinstance(raw_additions, list):
        raise ProofError("replacement ledger additions must be a list")
    additions: list[dict[str, Any]] = []
    addition_ids: set[str] = set()
    addition_fields = {
        "test_id",
        "preserved_behavior",
        "product_path",
        "validation_id",
        "provenance",
    }
    provenance_fields = {"kind", "source", "text"}
    for index, raw_addition in enumerate(raw_additions):
        if not isinstance(raw_addition, dict):
            errors.append(f"addition {index}: entry is not an object")
            continue
        unknown_fields = sorted(set(raw_addition) - addition_fields)
        missing_fields = sorted(addition_fields - set(raw_addition))
        test_id = str(raw_addition.get("test_id", ""))
        if unknown_fields or missing_fields:
            errors.append(
                f"addition {index}: fields mismatch; missing={missing_fields} "
                f"unknown={unknown_fields}"
            )
        if not test_id or test_id in addition_ids:
            errors.append(f"addition {index}: duplicate or empty test_id {test_id!r}")
            continue
        addition_ids.add(test_id)
        if test_id in baseline_ids:
            errors.append(
                f"addition {test_id}: a frozen baseline ID cannot be a policy addition"
            )
        if test_id not in current_by_id:
            errors.append(f"addition {test_id}: test is not currently discovered")
        for field_name in ("preserved_behavior", "product_path", "validation_id"):
            if not str(raw_addition.get(field_name, "")).strip():
                errors.append(f"addition {test_id}: missing {field_name}")
        validation_id = str(raw_addition.get("validation_id", ""))
        if (
            known_validation_ids is not None
            and validation_id not in known_validation_ids
        ):
            errors.append(
                f"addition {test_id}: names unknown validation {validation_id!r}"
            )
        provenance = raw_addition.get("provenance")
        if not isinstance(provenance, dict):
            errors.append(f"addition {test_id}: missing provenance")
        else:
            if set(provenance) != provenance_fields:
                errors.append(
                    f"addition {test_id}: provenance fields mismatch; "
                    f"missing={sorted(provenance_fields - set(provenance))} "
                    f"unknown={sorted(set(provenance) - provenance_fields)}"
                )
            if provenance.get("kind") != "policy-addition":
                errors.append(
                    f"addition {test_id}: provenance kind must be 'policy-addition'"
                )
            if (
                not str(provenance.get("source", "")).strip()
                or not str(provenance.get("text", "")).strip()
            ):
                errors.append(
                    f"addition {test_id}: provenance requires nonempty source and text"
                )
        additions.append(dict(raw_addition))
        referenced_current.add(test_id)
        executable_ids.add(test_id)

    unmapped_current = sorted(set(current_by_id) - referenced_current)
    if unmapped_current:
        errors.append(
            f"current inventory contains unmapped IDs: {unmapped_current[:20]}"
        )
    if errors:
        raise ProofError("inventory reconciliation failed:\n" + "\n".join(errors[:200]))
    required_by_framework: dict[str, list[str]] = {}
    for test_id in sorted(executable_ids):
        row = current_by_id[test_id]
        required_by_framework.setdefault(str(row["framework"]), []).append(
            str(row["native_id"])
        )
    for framework, ids in required_by_framework.items():
        if not ids:
            raise ProofError(f"zero required selection for framework {framework}")
    raw_overrides = ledger.get("overrides", [])
    if not isinstance(raw_overrides, list):
        raise ProofError("replacement ledger overrides must be a list")
    overrides = [dict(item) for item in raw_overrides if isinstance(item, dict)]
    if len(overrides) != len(raw_overrides):
        raise ProofError("replacement ledger contains a non-object override")
    for index, override in enumerate(overrides):
        unknown_keys = sorted(set(override) - {"text", "source", "provenance"})
        if unknown_keys:
            raise ProofError(
                f"replacement ledger override {index} has unknown keys: {unknown_keys}"
            )
        for field in ("text", "source", "provenance"):
            value = override.get(field)
            if not isinstance(value, str) or not value.strip():
                raise ProofError(
                    f"replacement ledger override {index} requires nonempty {field}"
                )
    return Reconciliation(
        required_by_framework=required_by_framework,
        exceptions=exceptions,
        additions=additions,
        overrides=overrides,
        current_ids=set(current_by_id),
    )


def _result_hash(report: Mapping[str, object]) -> str:
    return sha256_bytes(canonical_json(report))


def _validation_report(
    *,
    validation_id: str,
    execution_id: str,
    runner: str,
    runner_selector: str | None = None,
    classification: str,
    intended: Sequence[str],
    selected: Sequence[str],
    executed: Sequence[str],
    outcomes: Sequence[Mapping[str, object]],
    exit_code: int | None,
    evidence_kind: str = "structured_test",
    validation_type: str | None = None,
    input_contract_digest: str | None = None,
    diagnostic: str = "",
) -> dict[str, object]:
    if classification not in RESULT_CLASSES:
        raise AssertionError(classification)
    if evidence_kind not in EVIDENCE_KINDS:
        raise AssertionError(evidence_kind)
    if evidence_kind == "typed_non_test" and not validation_type:
        raise AssertionError("typed non-test evidence requires validation_type")
    if evidence_kind != "typed_non_test" and validation_type is not None:
        raise AssertionError("only typed non-test evidence has validation_type")
    if runner not in {*VALIDATION_RUNNERS, "infrastructure"}:
        raise AssertionError(f"unsupported validation runner {runner!r}")
    if runner == "rust-gate":
        if (
            not runner_selector
            or runner_selector != runner_selector.strip()
            or VALIDATION_ID_RE.fullmatch(runner_selector) is None
        ):
            raise AssertionError("Rust gate evidence requires an exact runner selector")
    elif runner_selector is not None:
        raise AssertionError("only Rust gate evidence has a runner selector")
    body: dict[str, object] = {
        "id": validation_id,
        "execution_id": execution_id,
        "runner": runner,
        "runner_selector": runner_selector,
        "evidence_kind": evidence_kind,
        "validation_type": validation_type,
        "input_contract_digest": input_contract_digest,
        "classification": classification,
        "intended_ids": list(intended),
        "selected_ids": list(selected),
        "executed_ids": list(executed),
        "intended_count": len(intended),
        "selected_count": len(selected),
        "executed_count": len(executed),
        "outcomes": list(outcomes),
        "exit_code": exit_code,
        "diagnostic": diagnostic[-8000:],
        "confirmed_failure_ids": [
            str(item.get("id")) for item in outcomes if item.get("outcome") == "failed"
        ],
    }
    body["report_hash"] = _result_hash(body)
    return body


def _confirmed_failure_evidence(
    *,
    intended: Sequence[str],
    selected: Sequence[str],
    executed: Sequence[str],
    outcomes: Sequence[Mapping[str, object]],
) -> tuple[list[str], list[dict[str, object]]] | None:
    intended_ids = list(intended)
    selected_ids = list(selected)
    executed_ids = list(executed)
    if (
        not intended_ids
        or selected_ids != intended_ids
        or len(intended_ids) != len(set(intended_ids))
        or len(executed_ids) != len(set(executed_ids))
        or not set(executed_ids).issubset(set(selected_ids))
        or len(outcomes) != len(executed_ids)
    ):
        return None
    outcome_ids: list[str] = []
    normalized: list[dict[str, object]] = []
    for raw in outcomes:
        test_id = raw.get("id")
        if not isinstance(test_id, str) or not test_id:
            return None
        outcome_ids.append(test_id)
        normalized.append(dict(raw))
    if outcome_ids != executed_ids or len(outcome_ids) != len(set(outcome_ids)):
        return None
    terminal = [
        item for item in normalized if item.get("outcome") in {"passed", "failed"}
    ]
    if not any(item.get("outcome") == "failed" for item in terminal):
        return None
    return [str(item["id"]) for item in terminal], terminal


def _run_structured_wrapper(
    *,
    validation_id: str,
    framework: str,
    intended: Sequence[str],
    command: Sequence[str],
    report_path: Path,
    cwd: Path,
    env: Mapping[str, str],
    timeout_seconds: int,
    temp_dir: Path,
    proof_attempt_id: str,
    proof_scope: str,
    raw_report_bytes: list[bytes] | None = None,
) -> tuple[dict[str, object], ChildProcess]:
    execution_id = str(uuid.uuid4())
    if not intended:
        if raw_report_bytes is not None:
            raw_report_bytes.append(b"null")
        child = _unlaunched_child(
            validation_id=validation_id,
            execution_id=execution_id,
            command=command,
        )
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner=framework,
                classification="pre_result_error",
                intended=[],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic=f"zero intended {framework} selection",
            ),
            child,
        )
    expected_path = temp_dir / f"{validation_id}-expected.json"
    expected_path.write_text(json.dumps(list(intended)), encoding="utf-8")
    receipt_nonce = secrets.token_hex(32)
    rendered_command = [
        str(expected_path)
        if item == "{expected_file}"
        else str(report_path)
        if item == "{report_file}"
        else item
        for item in command
    ]
    rendered_command.extend(
        [
            "--proof-attempt-id",
            proof_attempt_id,
            "--proof-execution-id",
            execution_id,
            "--proof-receipt-nonce",
            receipt_nonce,
            "--proof-scope",
            proof_scope,
        ]
    )
    result = run_process(
        validation_id=validation_id,
        execution_id=execution_id,
        command=rendered_command,
        cwd=cwd,
        env=env,
        timeout_seconds=timeout_seconds,
    )
    if not report_path.exists():
        if raw_report_bytes is not None:
            raw_report_bytes.append(b"null")
        report = _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner=framework,
            classification="pre_result_error",
            intended=intended,
            selected=[],
            executed=[],
            outcomes=[],
            exit_code=result.returncode,
            diagnostic=result.invocation_error
            or result.stderr
            or "runner omitted structured report",
        )
        return report, result.child
    retained_report: bytes | None = None
    try:
        retained_report = report_path.read_bytes()
        raw = _load_report(report_path, label=validation_id)
    except (OSError, ProofError) as error:
        if raw_report_bytes is not None:
            raw_report_bytes.append(retained_report or b"null")
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner=framework,
                classification="pre_result_error",
                intended=intended,
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=result.returncode,
                diagnostic=f"structured report could not be retained: {error}",
            ),
            result.child,
        )
    if raw_report_bytes is not None:
        raw_report_bytes.append(retained_report)
    classification = str(raw.get("classification", "pre_result_error"))
    if classification not in RESULT_CLASSES:
        classification = "pre_result_error"
    raw_selected = raw.get("selected_ids")
    raw_executed = raw.get("executed_ids")
    raw_outcomes = raw.get("outcomes")
    raw_declared_intended = raw.get("intended_ids")
    raw_started = raw.get("started_ids")
    raw_terminal = raw.get("terminal_ids")
    lists_are_typed = (
        isinstance(raw_selected, list)
        and all(isinstance(item, str) and item for item in raw_selected)
        and isinstance(raw_executed, list)
        and all(isinstance(item, str) and item for item in raw_executed)
        and isinstance(raw_outcomes, list)
        and all(isinstance(item, dict) for item in raw_outcomes)
        and isinstance(raw_declared_intended, list)
        and all(isinstance(item, str) and item for item in raw_declared_intended)
        and isinstance(raw_started, list)
        and all(isinstance(item, str) and item for item in raw_started)
        and isinstance(raw_terminal, list)
        and all(isinstance(item, str) and item for item in raw_terminal)
    )
    selected = list(raw_selected) if lists_are_typed else []
    executed = list(raw_executed) if lists_are_typed else []
    outcomes = [dict(item) for item in raw_outcomes] if lists_are_typed else []
    raw_intended = list(raw_declared_intended) if lists_are_typed else []
    started = list(raw_started) if lists_are_typed else []
    terminal = list(raw_terminal) if lists_are_typed else []
    selection_confirmed = raw.get("selection_confirmed") is True
    binding_valid = (
        raw.get("schema_version") == 2
        and raw.get("report_type") == "CompletionProofStructuredTestReportV2"
        and raw.get("framework") == framework
        and raw.get("proof_attempt_id") == proof_attempt_id
        and raw.get("proof_execution_id") == execution_id
        and raw.get("proof_receipt_nonce") == receipt_nonce
        and raw.get("proof_scope") == proof_scope
    )
    identity_lists_valid = (
        lists_are_typed
        and raw_intended == list(intended)
        and selected == list(intended)
        and selection_confirmed
        and len(started) == len(set(started))
        and len(terminal) == len(set(terminal))
        and len(executed) == len(set(executed))
        and terminal == executed
        and started[: len(terminal)] == terminal
        and list(intended)[: len(started)] == started
    )
    if not binding_valid:
        classification = "pre_result_error"
        selected = []
        executed = []
        outcomes = []
        started = []
        terminal = []
        selection_confirmed = False
    elif not lists_are_typed or not identity_lists_valid:
        classification = "pre_result_error"
    failure_evidence = (
        _confirmed_failure_evidence(
            intended=intended,
            selected=selected,
            executed=executed,
            outcomes=outcomes,
        )
        if binding_valid and identity_lists_valid
        else None
    )
    child_launched = result.child.pid > 0
    failure_survived_process_result = (
        failure_evidence is not None
        and child_launched
        and (result.returncode not in {None, 0} or bool(result.invocation_error))
    )
    if failure_survived_process_result:
        classification = "confirmed_validation_failure"
        assert failure_evidence is not None
        executed, outcomes = failure_evidence
    else:
        observed_classification, executed, outcomes = _classify_observed_results(
            intended=intended,
            selected=selected,
            executed=executed,
            outcomes=outcomes,
            returncode=result.returncode,
            invocation_error=result.invocation_error,
            validation_failure_exit_codes=frozenset(),
        )
        complete_lifecycle = (
            started == list(intended)
            and terminal == list(intended)
            and executed == list(intended)
        )
        if classification == "confirmed_pass" and complete_lifecycle:
            classification = observed_classification
        else:
            classification = "pre_result_error"
            if failure_evidence is None:
                executed = []
                outcomes = []
    diagnostic = result.invocation_error or result.stderr
    if not binding_valid:
        diagnostic = "structured report invocation binding mismatch"
    elif not identity_lists_valid:
        diagnostic = "structured report lifecycle identity mismatch"
    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner=framework,
            classification=classification,
            intended=intended,
            selected=selected,
            executed=executed,
            outcomes=outcomes,
            exit_code=result.returncode,
            diagnostic=diagnostic,
        ),
        result.child,
    )


def _classify_observed_results(
    *,
    intended: Sequence[str],
    selected: Sequence[str],
    executed: Sequence[str],
    outcomes: Sequence[Mapping[str, object]],
    returncode: int | None,
    invocation_error: str | None,
    validation_failure_exit_codes: frozenset[int],
) -> tuple[str, list[str], list[dict[str, object]]]:
    executed_ids = list(executed)
    normalized_outcomes = [dict(item) for item in outcomes]
    failure_evidence = _confirmed_failure_evidence(
        intended=intended,
        selected=selected,
        executed=executed_ids,
        outcomes=normalized_outcomes,
    )
    if failure_evidence is not None and (
        returncode in validation_failure_exit_codes or invocation_error is not None
    ):
        failed_executed, failed_outcomes = failure_evidence
        return "confirmed_validation_failure", failed_executed, failed_outcomes
    non_results = [
        item
        for item in normalized_outcomes
        if item.get("outcome") not in {"passed", "failed"}
    ]
    exact = (
        bool(intended)
        and list(selected) == list(intended)
        and executed_ids == list(intended)
        and len(selected) == len(set(selected))
        and len(executed_ids) == len(set(executed_ids))
        and len(normalized_outcomes) == len(executed_ids)
        and [item.get("id") for item in normalized_outcomes] == executed_ids
    )
    if invocation_error or not exact or non_results:
        return "pre_result_error", executed_ids, normalized_outcomes
    if returncode == 0 and all(
        item.get("outcome") == "passed" for item in normalized_outcomes
    ):
        return "confirmed_pass", executed_ids, normalized_outcomes
    return "pre_result_error", executed_ids, normalized_outcomes


def _normalize_complete_observed_results(
    *,
    intended: Sequence[str],
    executed: Sequence[str],
    outcomes: Sequence[Mapping[str, object]],
    label: str,
) -> tuple[list[str], list[dict[str, object]], list[str]]:
    """Normalize a complete exact result set without hiding malformed evidence."""
    intended_ids = list(intended)
    executed_ids = list(executed)
    normalized_outcomes = [dict(item) for item in outcomes]
    errors: list[str] = []
    if not intended_ids:
        errors.append(f"{label} intended selection was empty")
    if len(intended_ids) != len(set(intended_ids)):
        errors.append(f"{label} intended selection contained duplicate IDs")
    if len(executed_ids) != len(set(executed_ids)):
        errors.append(f"{label} emitted duplicate executed IDs")

    outcome_ids: list[str] = []
    for item in normalized_outcomes:
        test_id = item.get("id")
        if not isinstance(test_id, str) or not test_id:
            errors.append(f"{label} emitted an outcome without a nonempty ID")
            continue
        outcome_ids.append(test_id)
        if item.get("outcome") not in {"passed", "failed"}:
            errors.append(
                f"{label} emitted non-result outcome {item.get('outcome')!r} "
                f"for {test_id}"
            )
    if len(outcome_ids) != len(set(outcome_ids)):
        errors.append(f"{label} emitted duplicate outcome IDs")
    if len(normalized_outcomes) != len(executed_ids) or outcome_ids != executed_ids:
        errors.append(f"{label} outcomes did not exactly match executed IDs")

    intended_set = set(intended_ids)
    executed_set = set(executed_ids)
    missing = [test_id for test_id in intended_ids if test_id not in executed_set]
    unexpected = [test_id for test_id in executed_ids if test_id not in intended_set]
    if missing:
        errors.append(f"{label} omitted intended IDs: {missing[:20]}")
    if unexpected:
        errors.append(f"{label} emitted unexpected IDs: {unexpected[:20]}")
    if errors:
        return executed_ids, normalized_outcomes, errors

    outcomes_by_id = {str(item["id"]): item for item in normalized_outcomes}
    return (
        intended_ids,
        [outcomes_by_id[test_id] for test_id in intended_ids],
        [],
    )


def _native_diagnostic(
    *,
    runner: str,
    entrypoint_identity: Mapping[str, str] | None,
    native_report_sha256: str,
    message: str,
    pre_result_ids: Sequence[str] = (),
) -> str:
    return json.dumps(
        {
            "runner": runner,
            "adapter_entrypoint_identity": dict(entrypoint_identity or {}),
            "native_report_sha256": native_report_sha256,
            "pre_result_ids": list(pre_result_ids),
            "message": message,
        },
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )


def _native_adapter_outcome(
    raw: Mapping[str, object],
    *,
    outcome: str,
    entrypoint_identity: Mapping[str, str],
    native_report_sha256: str,
) -> dict[str, object]:
    return {
        "id": str(raw["id"]),
        "outcome": outcome,
        "native_outcome": dict(raw),
        "adapter_entrypoint_identity": dict(entrypoint_identity),
        "native_report_sha256": native_report_sha256,
    }


def _current_fork_codex_binary(
    repo_root: Path, env: Mapping[str, str]
) -> Path:
    workspace_root = repo_root / "codex-rs"
    cargo_target = env.get("CARGO_TARGET_DIR")
    target_root = Path(cargo_target) if cargo_target else workspace_root / "target"
    if not target_root.is_absolute():
        target_root = workspace_root / target_root
    executable = "codex.exe" if os.name == "nt" else "codex"
    return (target_root / "debug" / executable).resolve()


def _run_native_adapter(
    repo_root: Path,
    runner: str,
    intended: Sequence[str],
    env: Mapping[str, str],
    temp_dir: Path,
    *,
    timeout_seconds: int,
) -> tuple[dict[str, object], ChildProcess]:
    adapter = NATIVE_ADAPTERS.get(runner)
    if adapter is None:
        raise ProofError(f"unknown native adapter runner {runner!r}")
    validation_id = adapter["validation_id"]
    execution_id = str(uuid.uuid4())
    adapter_path = _native_adapter_path(repo_root, runner)
    base_command = [sys.executable, str(adapter_path)]
    if not intended:
        child = _unlaunched_child(
            validation_id=validation_id,
            execution_id=execution_id,
            command=base_command,
        )
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner=runner,
                classification="pre_result_error",
                intended=[],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic=f"zero intended {runner} selection",
            ),
            child,
        )

    try:
        entrypoint_start = _capture_file_identity(str(adapter_path), adapter_path)
    except ProofError as error:
        child = _unlaunched_child(
            validation_id=validation_id,
            execution_id=execution_id,
            command=base_command,
        )
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner=runner,
                classification="pre_result_error",
                intended=intended,
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic=str(error),
            ),
            child,
        )

    report_path: Path | None = None
    if runner == "argument-comment-lint-native":
        command = [
            *base_command,
            "run",
            *(part for test_id in intended for part in ("--test", test_id)),
        ]
        cwd = repo_root / "tools" / "argument-comment-lint"
    else:
        report_path = temp_dir / f"{validation_id}-{execution_id}.json"
        attempt_root = temp_dir / f"{validation_id}-{execution_id}-attempt"
        codex_binary = _current_fork_codex_binary(repo_root, env)
        command = [
            *base_command,
            *(part for test_id in intended for part in ("--run-case", test_id)),
            "--report-json",
            str(report_path),
            "--attempt-root",
            str(attempt_root),
            "--codex-bin",
            str(codex_binary),
            "--build-current-codex",
        ]
        cwd = repo_root

    result = run_process(
        validation_id=validation_id,
        execution_id=execution_id,
        command=command,
        cwd=cwd,
        env=env,
        timeout_seconds=timeout_seconds,
    )
    entrypoint_identity, entrypoint_error = entrypoint_start.finish()
    raw_bytes = b""
    if runner == "argument-comment-lint-native":
        raw_bytes = result.stdout.encode("utf-8")
    elif report_path is not None:
        try:
            raw_bytes = report_path.read_bytes()
        except OSError:
            raw_bytes = b""
    native_report_sha256 = sha256_bytes(raw_bytes) if raw_bytes else ""

    def pre_result(
        message: str,
        *,
        selected: Sequence[str] = (),
        pre_result_ids: Sequence[str] = (),
    ) -> tuple[dict[str, object], ChildProcess]:
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner=runner,
                classification="pre_result_error",
                intended=intended,
                selected=selected,
                executed=[],
                outcomes=[],
                exit_code=result.returncode,
                diagnostic=_native_diagnostic(
                    runner=runner,
                    entrypoint_identity=entrypoint_identity,
                    native_report_sha256=native_report_sha256,
                    message=message,
                    pre_result_ids=pre_result_ids,
                ),
            ),
            result.child,
        )

    if entrypoint_error:
        return pre_result(entrypoint_error)
    if not raw_bytes:
        return pre_result(
            result.stderr or "native adapter omitted its structured report"
        )
    try:
        raw = json.loads(raw_bytes)
    except json.JSONDecodeError:
        return pre_result("native adapter report was not valid JSON")
    if not isinstance(raw, dict):
        return pre_result("native adapter report was not an object")
    if (
        raw.get("schema_version") != 1
        or raw.get("report_type") != adapter["execution_report_type"]
    ):
        return pre_result("native adapter report schema was not trusted")

    try:
        if runner == "argument-comment-lint-native":
            raw_intended = raw["intended_validation_ids"]
            raw_selected = raw["selected_validation_ids"]
            raw_executed = raw["actually_executed_validation_ids"]
            raw_outcomes = raw["outcomes"]
            aggregate = raw["result"]
        else:
            if (
                raw.get("validation_id") != "windows-sandbox-smoke"
                or raw.get("host_platform") != "windows"
                or raw.get("selection_error") is not None
            ):
                raise ValueError(
                    "Windows sandbox report identity or selection was invalid"
                )
            codex_identity = raw.get("codex_executable_identity")
            codex_build = raw.get("codex_build")
            expected_codex = _current_fork_codex_binary(repo_root, env)
            if not isinstance(codex_identity, dict):
                raise ValueError("Windows sandbox report omitted Codex identity")
            if set(codex_identity) != {
                "requested",
                "resolved_path",
                "sha256_before",
                "sha256_after",
            }:
                raise ValueError("Windows sandbox Codex identity was malformed")
            reported_path = Path(str(codex_identity["resolved_path"])).resolve()
            if os.path.normcase(str(reported_path)) != os.path.normcase(
                str(expected_codex)
            ):
                raise ValueError(
                    "Windows sandbox did not use the exact current-fork Codex binary"
                )
            before_hash = codex_identity["sha256_before"]
            after_hash = codex_identity["sha256_after"]
            if (
                not isinstance(before_hash, str)
                or not re.fullmatch(r"[0-9a-f]{64}", before_hash)
                or after_hash != before_hash
                or _sha256_file(expected_codex) != after_hash
            ):
                raise ValueError(
                    "Windows sandbox Codex executable hash was invalid or unstable"
                )
            expected_build_command = [
                shutil.which("cargo", path=env.get("PATH")) or "cargo",
                "build",
                "--locked",
                "-p",
                "codex-cli",
                "--bin",
                "codex",
            ]
            if (
                not isinstance(codex_build, dict)
                or codex_build.get("command") != expected_build_command
                or codex_build.get("cwd") != str((repo_root / "codex-rs").resolve())
                or codex_build.get("exit_code") != 0
            ):
                raise ValueError(
                    "Windows sandbox did not prove a successful current-fork Codex build"
                )
            raw_intended = raw["intended_case_ids"]
            raw_selected = raw["selected_case_ids"]
            raw_executed = raw["executed_case_ids"]
            raw_outcomes = raw["results"]
            aggregate = None
        if (
            not isinstance(raw_intended, list)
            or not all(isinstance(item, str) for item in raw_intended)
            or not isinstance(raw_selected, list)
            or not all(isinstance(item, str) for item in raw_selected)
            or not isinstance(raw_executed, list)
            or not all(isinstance(item, str) for item in raw_executed)
            or not isinstance(raw_outcomes, list)
            or not all(isinstance(item, dict) for item in raw_outcomes)
        ):
            raise ValueError("native adapter report contained invalid selection fields")
        if raw_intended != list(intended) or raw_selected != list(intended):
            raise ValueError("native adapter did not select the exact intended IDs")
        if len(raw_executed) != len(set(raw_executed)) or not set(
            raw_executed
        ).issubset(set(intended)):
            raise ValueError("native adapter reported invalid executed IDs")
        outcome_ids = [item.get("id") for item in raw_outcomes]
        if outcome_ids != list(intended) or len(outcome_ids) != len(set(outcome_ids)):
            raise ValueError(
                "native adapter outcomes did not exactly cover intended IDs"
            )
        if runner == "windows-sandbox-smoke" and any(
            item.get("codex_executable_identity") != codex_identity
            or item.get("codex_build") != codex_build
            for item in raw_outcomes
        ):
            raise ValueError(
                "Windows sandbox leaf outcomes did not bind the Codex executable"
            )

        terminal: list[dict[str, object]] = []
        pre_result_ids: list[str] = []
        raw_started_ids: list[str] = []
        for item in raw_outcomes:
            case_id = str(item["id"])
            if runner == "argument-comment-lint-native":
                native_classification = item.get("classification")
                executed = item.get("executed")
                if not isinstance(executed, bool):
                    raise ValueError(
                        f"native outcome {case_id!r} omitted executed state"
                    )
                if executed:
                    raw_started_ids.append(case_id)
                if native_classification == "confirmed_pass" and executed:
                    outcome = "passed"
                elif (
                    native_classification == "confirmed_validation_failure" and executed
                ):
                    outcome = "failed"
                elif native_classification == "pre_result_error":
                    pre_result_ids.append(case_id)
                    continue
                else:
                    raise ValueError(f"native outcome {case_id!r} was inconsistent")
            else:
                status = item.get("status")
                launches = item.get("sandbox_launches")
                if (
                    isinstance(launches, bool)
                    or not isinstance(launches, int)
                    or launches < 0
                ):
                    raise ValueError(
                        f"Windows sandbox outcome {case_id!r} had invalid launches"
                    )
                if launches > 0:
                    raw_started_ids.append(case_id)
                if status == "passed" and launches > 0:
                    outcome = "passed"
                elif status == "failed" and launches > 0:
                    outcome = "failed"
                elif status == "pre_result_error":
                    pre_result_ids.append(case_id)
                    continue
                else:
                    raise ValueError(
                        f"Windows sandbox outcome {case_id!r} was inconsistent"
                    )
            terminal.append(
                _native_adapter_outcome(
                    item,
                    outcome=outcome,
                    entrypoint_identity=entrypoint_identity,
                    native_report_sha256=native_report_sha256,
                )
            )
        if raw_executed != raw_started_ids:
            raise ValueError(
                "native adapter executed IDs did not match its per-leaf outcomes"
            )
        if runner == "windows-sandbox-smoke":
            counts = raw.get("counts")
            expected_counts = {
                "intended": len(intended),
                "selected": len(intended),
                "executed": len(raw_started_ids),
                "passed": sum(item["outcome"] == "passed" for item in terminal),
                "failed": sum(item["outcome"] == "failed" for item in terminal),
                "pre_result_error": len(pre_result_ids),
            }
            if not isinstance(counts, dict) or any(
                counts.get(key) != value for key, value in expected_counts.items()
            ):
                raise ValueError("Windows sandbox aggregate counts were inconsistent")
    except (KeyError, TypeError, ValueError) as error:
        return pre_result(str(error))

    failed = [item for item in terminal if item["outcome"] == "failed"]
    if failed:
        if (
            result.returncode != 1
            and result.invocation_error is None
        ) or (
            runner == "argument-comment-lint-native"
            and aggregate != "confirmed_validation_failure"
        ):
            return pre_result(
                "native adapter did not return its required validation-failure exit",
                selected=raw_selected,
                pre_result_ids=pre_result_ids,
            )
        classification = "confirmed_validation_failure"
    elif result.invocation_error:
        return pre_result(
            result.invocation_error,
            selected=raw_selected,
            pre_result_ids=pre_result_ids,
        )
    elif pre_result_ids:
        if result.returncode != 2 or (
            runner == "argument-comment-lint-native" and aggregate != "pre_result_error"
        ):
            return pre_result(
                "native adapter did not return its required pre-result exit",
                selected=raw_selected,
                pre_result_ids=pre_result_ids,
            )
        return pre_result(
            "one or more intended native leaves produced no confirmed result",
            selected=raw_selected,
            pre_result_ids=pre_result_ids,
        )
    else:
        if (
            len(terminal) != len(intended)
            or result.returncode != 0
            or (
                runner == "argument-comment-lint-native"
                and aggregate != "confirmed_pass"
            )
        ):
            return pre_result(
                "native adapter did not return an exact confirmed pass",
                selected=raw_selected,
            )
        classification = "confirmed_pass"

    executed = [str(item["id"]) for item in terminal]
    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner=runner,
            classification=classification,
            intended=intended,
            selected=raw_selected,
            executed=executed,
            outcomes=terminal,
            exit_code=result.returncode,
            diagnostic=_native_diagnostic(
                runner=runner,
                entrypoint_identity=entrypoint_identity,
                native_report_sha256=native_report_sha256,
                message=(
                    "native adapter returned structured per-leaf outcomes"
                    + (
                        f"; later runner error: {result.invocation_error}"
                        if result.invocation_error
                        else ""
                    )
                ),
                pre_result_ids=pre_result_ids,
            ),
        ),
        result.child,
    )


def _parse_workspace_nextest_events(
    output: str,
) -> tuple[list[str], list[dict[str, str]], list[str], list[str]]:
    """Parse full-ID nextest events without inferring or rewriting observations."""
    started: list[str] = []
    started_ids: set[str] = set()
    terminal: dict[str, str] = {}
    blocking_errors: list[str] = []
    trailing_parse_errors: list[str] = []
    confirmed_failure_seen = False
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            error = f"invalid nextest event JSON: {line[:500]}"
            (trailing_parse_errors if confirmed_failure_seen else blocking_errors).append(
                error
            )
            continue
        if not isinstance(event, dict):
            error = f"nextest event record is not an object: {line[:500]}"
            (trailing_parse_errors if confirmed_failure_seen else blocking_errors).append(
                error
            )
            continue
        if event.get("type") != "test":
            continue
        test_id = event.get("name")
        event_name = event.get("event")
        if not isinstance(test_id, str) or not test_id:
            error = "nextest test event omitted its name"
            (trailing_parse_errors if confirmed_failure_seen else blocking_errors).append(
                error
            )
            continue
        if event_name == "started":
            if test_id in started_ids:
                blocking_errors.append(
                    f"nextest emitted duplicate start for {test_id}"
                )
                continue
            started.append(test_id)
            started_ids.add(test_id)
            continue
        if event_name == "ok":
            outcome = "passed"
        elif event_name in {"failed", "error", "timed_out"}:
            outcome = "failed"
        elif event_name in {"ignored", "skipped"}:
            outcome = "skipped"
        else:
            continue
        if test_id not in started_ids:
            blocking_errors.append(
                f"nextest emitted a terminal result before its start for {test_id}"
            )
        if test_id in terminal:
            blocking_errors.append(
                f"nextest emitted duplicate terminal result for {test_id}"
            )
            continue
        terminal[test_id] = outcome
        if test_id in started_ids and outcome == "failed":
            confirmed_failure_seen = True
    outcomes = [
        {"id": test_id, "outcome": terminal.get(test_id, "unknown")}
        for test_id in started
    ]
    return started, outcomes, blocking_errors, trailing_parse_errors


def _run_rust_nextest(
    repo_root: Path,
    intended: Sequence[str],
    env: Mapping[str, str],
    *,
    timeout_seconds: int,
) -> tuple[dict[str, object], ChildProcess]:
    validation_id = "rust.nextest.workspace"
    execution_id = str(uuid.uuid4())
    command = [
        "cargo",
        "nextest",
        "run",
        "--workspace",
        "--profile",
        "completion-proof",
        "--no-fail-fast",
        "--run-ignored",
        "all",
        "--ignore-default-filter",
        "--retries",
        "0",
        "--no-tests=fail",
        "--message-format",
        "libtest-json-plus",
        "--message-format-version",
        "0.1",
    ]
    if not intended:
        child = _unlaunched_child(
            validation_id=validation_id,
            execution_id=execution_id,
            command=command,
        )
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner="rust-nextest",
                classification="pre_result_error",
                intended=[],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic="zero intended Rust nextest selection",
            ),
            child,
        )
    result = run_process(
        validation_id=validation_id,
        execution_id=execution_id,
        command=command,
        cwd=repo_root / "codex-rs",
        env=env,
        timeout_seconds=timeout_seconds,
    )
    started, outcomes, blocking_errors, trailing_parse_errors = (
        _parse_workspace_nextest_events(result.stdout)
    )
    started, outcomes, observation_errors = _normalize_complete_observed_results(
        intended=intended,
        executed=started,
        outcomes=outcomes,
        label="nextest",
    )
    evidence_errors = [
        *blocking_errors,
        *trailing_parse_errors,
        *observation_errors,
    ]
    classification, started, outcomes = _classify_observed_results(
        intended=intended,
        selected=intended,
        executed=started,
        outcomes=outcomes,
        returncode=result.returncode,
        invocation_error=result.invocation_error
        or (
            "nextest event evidence was invalid"
            if blocking_errors or observation_errors
            else None
        ),
        validation_failure_exit_codes=frozenset({100}),
    )
    if blocking_errors or observation_errors:
        classification = "pre_result_error"
    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner="rust-nextest",
            classification=classification,
            intended=intended,
            selected=intended,
            executed=started,
            outcomes=outcomes,
            exit_code=result.returncode,
            diagnostic=(
                result.invocation_error
                or "\n".join([*evidence_errors, result.stderr[-6000:]])
            ),
        ),
        result.child,
    )


def _parse_doctest_libtest_events(
    output: str,
) -> tuple[list[str], list[dict[str, str]], list[str]]:
    started: list[str] = []
    started_ids: set[str] = set()
    terminal: dict[str, str] = {}
    errors: list[str] = []
    suite_count = 0
    suite_declared_total = 0
    active_suite_count: int | None = None
    active_suite_started: list[str] = []
    active_suite_terminal_outcomes: list[str] = []

    def object_from_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ProofError("Rust doctest libtest JSON contains duplicate keys")
            value[key] = item
        return value

    def reject_constant(value: str) -> None:
        raise ProofError(f"Rust doctest libtest JSON contains invalid constant {value}")

    for raw_line in output.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        try:
            event = json.loads(
                line,
                object_pairs_hook=object_from_pairs,
                parse_constant=reject_constant,
            )
        except (ProofError, json.JSONDecodeError) as error:
            errors.append(f"invalid Rust doctest libtest JSON event: {error}")
            continue
        if not isinstance(event, dict):
            errors.append("Rust doctest libtest JSON event was not an object")
            continue
        event_type = event.get("type")
        event_name = event.get("event")
        if event_type == "suite":
            if event_name == "started":
                test_count = event.get("test_count")
                if (
                    active_suite_count is not None
                    or isinstance(test_count, bool)
                    or not isinstance(test_count, int)
                    or test_count < 0
                ):
                    errors.append("Rust doctest suite start was duplicate or invalid")
                    continue
                suite_count += 1
                suite_declared_total += test_count
                active_suite_count = test_count
                active_suite_started = []
                active_suite_terminal_outcomes = []
                continue
            if event_name not in {"ok", "failed"} or active_suite_count is None:
                errors.append("Rust doctest suite terminal was missing its suite start")
                continue
            count_fields = ("passed", "failed", "ignored", "measured", "filtered_out")
            counts = [event.get(field) for field in count_fields]
            if any(
                isinstance(value, bool) or not isinstance(value, int) or value < 0
                for value in counts
            ):
                errors.append("Rust doctest suite terminal counts were invalid")
            else:
                passed, failed, ignored, measured, filtered_out = counts
                observed_passed = active_suite_terminal_outcomes.count("passed")
                observed_failed = active_suite_terminal_outcomes.count("failed")
                observed_ignored = active_suite_terminal_outcomes.count("skipped")
                if (
                    len(active_suite_started) != active_suite_count
                    or len(active_suite_terminal_outcomes) != active_suite_count
                    or passed + failed + ignored + measured != active_suite_count
                    or passed != observed_passed
                    or failed != observed_failed
                    or ignored != observed_ignored
                    or measured != 0
                    or filtered_out != 0
                    or (event_name == "ok" and failed != 0)
                    or (event_name == "failed" and failed == 0)
                ):
                    errors.append(
                        "Rust doctest suite lifecycle did not match its declared count"
                    )
            active_suite_count = None
            active_suite_started = []
            active_suite_terminal_outcomes = []
            continue
        if event_type != "test" or event_name not in {
            "started",
            "ok",
            "failed",
            "ignored",
        }:
            errors.append("Rust doctest stdout contained a non-libtest lifecycle event")
            continue
        test_id = event.get("name")
        if not isinstance(test_id, str) or not test_id:
            errors.append("Rust doctest test event omitted its nonempty name")
            continue
        if active_suite_count is None:
            errors.append(f"Rust doctest test event preceded suite start for {test_id}")
            continue
        if event_name == "started":
            if test_id in started_ids:
                errors.append(f"Rust doctest emitted duplicate start for {test_id}")
                continue
            started.append(test_id)
            started_ids.add(test_id)
            active_suite_started.append(test_id)
            continue
        if test_id not in started_ids or test_id not in active_suite_started:
            errors.append(
                f"Rust doctest emitted terminal {event_name} before start for {test_id}"
            )
            continue
        if test_id in terminal:
            errors.append(f"Rust doctest emitted duplicate terminal for {test_id}")
            continue
        outcome = {"ok": "passed", "failed": "failed", "ignored": "skipped"}[
            event_name
        ]
        terminal[test_id] = outcome
        active_suite_terminal_outcomes.append(outcome)

    if active_suite_count is not None:
        errors.append("Rust doctest libtest JSON omitted a suite terminal event")
    if suite_count == 0:
        errors.append("Rust doctest libtest JSON omitted a suite start event")
    if suite_declared_total == 0:
        errors.append("Rust doctest libtest JSON selected zero tests")
    if len(started) != suite_declared_total or len(terminal) != suite_declared_total:
        errors.append("Rust doctest lifecycle count did not match suite selection")
    outcomes = [
        {"id": test_id, "outcome": terminal.get(test_id, "unknown")}
        for test_id in started
    ]
    return started, outcomes, errors


def _run_rust_doctests(
    repo_root: Path,
    intended: Sequence[str],
    env: Mapping[str, str],
    *,
    timeout_seconds: int,
) -> tuple[dict[str, object], ChildProcess]:
    validation_id = "rust.doctest.workspace"
    execution_id = str(uuid.uuid4())
    command = [
        "cargo",
        "test",
        "--workspace",
        "--doc",
        "--",
        "--include-ignored",
        "-Z",
        "unstable-options",
        "--format",
        "json",
    ]
    if not intended:
        child = _unlaunched_child(
            validation_id=validation_id,
            execution_id=execution_id,
            command=command,
        )
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner="rust-doctest",
                classification="pre_result_error",
                intended=[],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic="zero intended Rust doctest selection",
            ),
            child,
        )
    doctest_env = dict(env)
    doctest_env["RUST_TEST_NOCAPTURE"] = "0"
    doctest_env["RUSTC_BOOTSTRAP"] = "-1"
    result = run_process(
        validation_id=validation_id,
        execution_id=execution_id,
        command=command,
        cwd=repo_root / "codex-rs",
        env=doctest_env,
        timeout_seconds=timeout_seconds,
    )
    executed, outcomes, lifecycle_errors = _parse_doctest_libtest_events(result.stdout)
    executed, outcomes, observation_errors = _normalize_complete_observed_results(
        intended=intended,
        executed=executed,
        outcomes=outcomes,
        label="Rust doctest",
    )
    evidence_errors = [*lifecycle_errors, *observation_errors]
    classification, executed, outcomes = _classify_observed_results(
        intended=intended,
        selected=intended,
        executed=executed,
        outcomes=outcomes,
        returncode=result.returncode,
        invocation_error=result.invocation_error
        or ("Rust doctest libtest evidence was invalid" if evidence_errors else None),
        validation_failure_exit_codes=frozenset({101}),
    )
    if evidence_errors:
        classification = "pre_result_error"
        executed = []
        outcomes = []
    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner="rust-doctest",
            classification=classification,
            intended=intended,
            selected=intended,
            executed=executed,
            outcomes=outcomes,
            exit_code=result.returncode,
            diagnostic=result.invocation_error
            or "\n".join([*evidence_errors, result.stderr[-6000:]]),
        ),
        result.child,
    )


def _run_jest(
    repo_root: Path,
    intended: Sequence[str],
    env: Mapping[str, str],
    temp_dir: Path,
    *,
    timeout_seconds: int,
) -> tuple[dict[str, object], ChildProcess]:
    validation_id = "sdk.typescript.jest"
    execution_id = str(uuid.uuid4())
    output = temp_dir / "jest-results.json"
    journal = temp_dir / "jest-results.jsonl"
    reporter = temp_dir / "completion-proof-jest-reporter.cjs"
    selection_nonce = uuid.uuid4().hex
    reporter.write_text(
        """\
const fs = require("fs");
class CompletionProofReporter {
  onRunStart() {
    fs.writeFileSync(
      process.env.KD4_COMPLETION_PROOF_JEST_JOURNAL,
      JSON.stringify({
        event: "run_started",
        selectionNonce: process.env.KD4_COMPLETION_PROOF_JEST_SELECTION_NONCE,
      }) + "\\n",
      {encoding: "utf8"},
    );
  }
  onTestResult(test, result) {
    for (const assertion of result.testResults || []) {
      fs.appendFileSync(
        process.env.KD4_COMPLETION_PROOF_JEST_JOURNAL,
        JSON.stringify({event: "assertion", file: test.path, fullName: assertion.fullName, status: assertion.status}) + "\\n",
        {encoding: "utf8"},
      );
    }
  }
}
module.exports = CompletionProofReporter;
""",
        encoding="utf-8",
    )
    command = [
        "node",
        str(repo_root / "node_modules" / "jest" / "bin" / "jest.js"),
        "--runInBand",
        "--no-cache",
        "--reporters",
        "default",
        "--reporters",
        str(reporter),
        "--json",
        "--outputFile",
        str(output),
    ]
    if not intended:
        child = _unlaunched_child(
            validation_id=validation_id,
            execution_id=execution_id,
            command=command,
        )
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner="javascript-jest",
                classification="pre_result_error",
                intended=[],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic="zero intended Jest selection",
            ),
            child,
        )
    jest_env = dict(env)
    jest_env["KD4_COMPLETION_PROOF_JEST_JOURNAL"] = str(journal)
    jest_env["KD4_COMPLETION_PROOF_JEST_SELECTION_NONCE"] = selection_nonce
    result = run_process(
        validation_id=validation_id,
        execution_id=execution_id,
        command=command,
        cwd=repo_root,
        env=jest_env,
        timeout_seconds=timeout_seconds,
    )
    journal_observed: dict[str, str] = {}
    result_observed: dict[str, str] = {}
    journal_errors: list[str] = []
    result_errors: list[str] = []

    def record(
        observed: dict[str, str],
        test_file_value: object,
        full_name: object,
        status_value: object,
        errors: list[str],
    ) -> None:
        test_file = Path(str(test_file_value))
        try:
            source = test_file.resolve().relative_to(repo_root).as_posix()
        except ValueError:
            source = test_file.as_posix()
        test_id = f"{source}::{full_name}"
        status = str(status_value)
        if test_id in observed:
            errors.append(f"Jest emitted duplicate assertion result for {test_id}")
            return
        observed[test_id] = {
            "passed": "passed",
            "failed": "failed",
            "pending": "skipped",
            "todo": "skipped",
            "disabled": "skipped",
        }.get(status, "unknown")

    saw_run_start = False
    if journal.exists():
        for line in journal.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                journal_errors.append("Jest journal contained invalid JSON")
                continue
            if not isinstance(event, dict):
                journal_errors.append("Jest journal contained a non-object event")
                continue
            if event.get("event") == "run_started":
                if event.get("selectionNonce") != selection_nonce:
                    journal_errors.append("Jest run-start nonce did not match")
                elif saw_run_start:
                    journal_errors.append("Jest emitted duplicate run-start events")
                else:
                    saw_run_start = True
            elif event.get("event") == "assertion":
                if not saw_run_start:
                    journal_errors.append(
                        "Jest emitted an assertion before its run-start event"
                    )
                record(
                    journal_observed,
                    event.get("file"),
                    event.get("fullName"),
                    event.get("status"),
                    journal_errors,
                )
            else:
                journal_errors.append("Jest journal contained an unknown event")

    final_report = False
    if output.exists():
        try:
            raw = _load_report(output, label="Jest")
        except ProofError:
            raw = {}
        else:
            final_report = True
            raw_suites = raw.get("testResults")
            if not isinstance(raw_suites, list):
                result_errors.append("Jest final report omitted its testResults list")
                raw_suites = []
            for suite in raw_suites:
                if not isinstance(suite, dict):
                    result_errors.append(
                        "Jest final report contained a non-object suite"
                    )
                    continue
                raw_assertions = suite.get("assertionResults")
                if not isinstance(raw_assertions, list):
                    result_errors.append(
                        "Jest final report suite omitted its assertionResults list"
                    )
                    continue
                for assertion in raw_assertions:
                    if not isinstance(assertion, dict):
                        result_errors.append(
                            "Jest final report contained a non-object assertion"
                        )
                        continue
                    record(
                        result_observed,
                        suite.get("name", ""),
                        assertion.get("fullName", ""),
                        assertion.get("status", "unknown"),
                        result_errors,
                    )
    journal_executed = list(journal_observed)
    journal_outcomes = [
        {"id": test_id, "outcome": journal_observed[test_id]}
        for test_id in journal_executed
    ]
    journal_executed, journal_outcomes, journal_normalization_errors = (
        _normalize_complete_observed_results(
            intended=intended,
            executed=journal_executed,
            outcomes=journal_outcomes,
            label="Jest journal",
        )
    )
    result_executed = list(result_observed)
    result_outcomes = [
        {"id": test_id, "outcome": result_observed[test_id]}
        for test_id in result_executed
    ]
    result_executed, result_outcomes, result_normalization_errors = (
        _normalize_complete_observed_results(
            intended=intended,
            executed=result_executed,
            outcomes=result_outcomes,
            label="Jest final report",
        )
    )
    agreement_errors: list[str] = []
    if (
        not journal_normalization_errors
        and not result_normalization_errors
        and journal_outcomes != result_outcomes
    ):
        agreement_errors.append("Jest journal and final report outcomes disagreed")
    observation_errors = [
        *journal_errors,
        *journal_normalization_errors,
        *result_errors,
        *result_normalization_errors,
        *agreement_errors,
    ]
    selected = list(intended) if saw_run_start else []
    journal_failure_evidence = (
        _confirmed_failure_evidence(
            intended=intended,
            selected=selected,
            executed=list(journal_observed),
            outcomes=[
                {"id": test_id, "outcome": journal_observed[test_id]}
                for test_id in journal_observed
            ],
        )
        if saw_run_start and not journal_errors
        else None
    )
    final_evidence_usable = (
        final_report
        and not result_errors
        and not result_normalization_errors
    )
    if final_report:
        executed = result_executed
        outcomes = result_outcomes
    else:
        executed = journal_executed
        outcomes = journal_outcomes
    evidence_error = (
        None
        if final_report and saw_run_start and not observation_errors
        else (
            "Jest reporter did not confirm the intended selection"
            if not saw_run_start
            else (
                "Jest omitted or corrupted its final JSON report"
                if not final_report
                else "Jest result evidence was invalid"
            )
        )
    )
    preserve_journal_failure = (
        journal_failure_evidence is not None
        and not agreement_errors
        and (not final_evidence_usable or result.invocation_error is not None)
    )
    if preserve_journal_failure:
        classification = "confirmed_validation_failure"
        executed, outcomes = journal_failure_evidence
    elif evidence_error:
        classification = "pre_result_error"
    else:
        classification, executed, outcomes = _classify_observed_results(
            intended=intended,
            executed=executed,
            outcomes=outcomes,
            selected=selected,
            returncode=result.returncode,
            invocation_error=result.invocation_error,
            validation_failure_exit_codes=frozenset({1}),
        )
    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner="javascript-jest",
            classification=classification,
            intended=intended,
            selected=selected,
            executed=executed,
            outcomes=outcomes,
            exit_code=result.returncode,
            diagnostic=(
                "\n".join(
                    part
                    for part in [
                        result.invocation_error or "",
                        *([evidence_error] if evidence_error else []),
                        *observation_errors,
                        result.stderr[-6000:],
                    ]
                    if part
                )
            ),
        ),
        result.child,
    )


def _expand_command(repo_root: Path, command: Sequence[object]) -> list[str]:
    replacements = {
        "{repo}": str(repo_root),
        "{python}": sys.executable,
    }
    rendered = []
    for item in command:
        value = str(item)
        for marker, replacement in replacements.items():
            value = value.replace(marker, replacement)
        rendered.append(value)
    return rendered


def _production_typed_validation_spec(
    repo_root: Path,
    validation_type: str,
) -> tuple[list[str], Path, frozenset[int]]:
    specs: dict[str, tuple[list[str], str, frozenset[int]]] = {
        "typescript-typecheck": (
            [
                "node",
                "{repo}/node_modules/typescript/bin/tsc",
                "--noEmit",
                "--rootDir",
                ".",
                "--allowImportingTsExtensions",
            ],
            "sdk/typescript",
            frozenset({2}),
        ),
        "python-ruff": (
            [
                "uv",
                "run",
                "--offline",
                "--frozen",
                "--directory",
                "{repo}/sdk/python",
                "--group",
                "dev",
                "ruff",
                "check",
                ".",
            ],
            ".",
            frozenset({1}),
        ),
        "script-audit": (
            [
                "{python}",
                "{repo}/scripts/root_maintenance.py",
                "audit-scripts",
                "--quick",
                "--strict",
            ],
            ".",
            frozenset({1}),
        ),
        "source-map-consistency": (
            ["just", "source-map-check-only"],
            ".",
            frozenset({1}),
        ),
        "rust-test-manifest": (
            ["{python}", "{repo}/scripts/rust_test_runner.py", "check-manifest"],
            ".",
            frozenset({2}),
        ),
        "generated-config-schema": (
            [
                "{python}",
                "{repo}/scripts/config_schema_check.py",
                "--mode",
                "check",
            ],
            ".",
            frozenset({1, 100}),
        ),
        "generated-app-server-schema": (
            [
                "{python}",
                "{repo}/scripts/app_server_schema_runtime_check.py",
                "--mode",
                "check",
            ],
            ".",
            frozenset({1, 100}),
        ),
        "generated-config-proto": (
            [
                "powershell",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                "{repo}/codex-rs/config/scripts/generate-proto.ps1",
                "-Check",
            ],
            ".",
            frozenset({1, 101}),
        ),
        "generated-exec-server-relay-proto": (
            [
                "cargo",
                "run",
                "--locked",
                "--manifest-path",
                "{repo}/codex-rs/Cargo.toml",
                "-p",
                "codex-exec-server",
                "--example",
                "generate-relay-proto",
                "--",
                "--check",
            ],
            ".",
            frozenset({1, 101}),
        ),
        "codex-cli-wrapper": (
            ["just", "codex-cli-wrapper-check"],
            ".",
            frozenset({1}),
        ),
    }
    try:
        command, cwd_value, failure_exit_codes = specs[validation_type]
    except KeyError as error:
        raise ProofError(
            f"unknown code-owned typed validation {validation_type!r}"
        ) from error
    return (
        _expand_command(repo_root, command),
        _resolve_path(repo_root, cwd_value),
        failure_exit_codes,
    )


@dataclass(frozen=True)
class _TypedValidationLaunchPrecommit:
    request_payload: bytes
    request_sha256: str
    command: tuple[str, ...]
    executable_sha256: str
    journal_path: Path
    binding: JournalInvocationBinding
    action_execution_id: str


def _typed_validation_precommit(
    *,
    validation_id: str,
    validation_type: str,
    execution_id: str,
    input_contract_digest: str,
    proof_attempt_id: str,
    proof_receipt_nonce: str,
    proof_scope: str,
    command: Sequence[str],
    cwd: Path,
    env: Mapping[str, str],
    timeout_seconds: int,
    allowed_failures: frozenset[int],
    journal_path: Path,
) -> _TypedValidationLaunchPrecommit:
    child_env = {
        key: value
        for key, value in _strip_completion_proof_env(env).items()
        if key.casefold()
        != _child_validation_module.BROKER_AUTHKEY_ENV.casefold()
    }
    identity = _resolve_launch_target(
        str(command[0]) if command else "",
        cwd=cwd,
        env=child_env,
    )
    launched_command = (identity.resolved_path, *map(str, command[1:]))
    action_execution_id = str(uuid.uuid4())
    binding = JournalInvocationBinding(
        proof_attempt_id=proof_attempt_id,
        proof_execution_id=execution_id,
        proof_receipt_nonce=proof_receipt_nonce,
        proof_scope=proof_scope,
        validation_id=validation_id,
        validation_type=validation_type,
        input_contract_digest=input_contract_digest,
    )
    request = {
        "protocol": _child_validation_module.BROKER_PROTOCOL,
        "binding": binding.as_record_fields(),
        "action_id": validation_id,
        "action_execution_id": action_execution_id,
        "subjects": [validation_id],
        "command": list(launched_command),
        "cwd": str(cwd.resolve()),
        "env": child_env,
        "timeout_seconds": timeout_seconds,
        "stdout_limit_bytes": PROCESS_STDOUT_LIMIT_BYTES,
        "stderr_limit_bytes": PROCESS_STDERR_LIMIT_BYTES,
        "validation_failure_exit_codes": sorted(allowed_failures),
        "journal_path": str(journal_path.resolve()),
        "launch_target_identity": {
            "requested": identity.requested,
            "resolved_path": identity.resolved_path,
            "sha256_before": identity.sha256_before,
        },
    }
    payload = canonical_json(request)
    return _TypedValidationLaunchPrecommit(
        request_payload=payload,
        request_sha256=sha256_bytes(payload),
        command=launched_command,
        executable_sha256=identity.sha256_before,
        journal_path=journal_path.resolve(),
        binding=binding,
        action_execution_id=action_execution_id,
    )


def _typed_validation_broker_exchange(
    *,
    validation_id: str,
    execution_id: str,
    precommit: _TypedValidationLaunchPrecommit,
    env: Mapping[str, str],
    timeout_seconds: int,
) -> tuple[
    ProcessResult,
    bytes | None,
    bytes | None,
    str | None,
    tuple[str, ...],
]:
    authkey = secrets.token_bytes(32)
    listener = Listener(("127.0.0.1", 0), family="AF_INET", authkey=authkey)
    address = listener.address
    if not isinstance(address, tuple) or len(address) != 2:
        listener.close()
        raise ProofError("typed-validation broker returned an invalid IPC address")
    host, port = address
    response: list[bytes] = []
    completion: list[bytes] = []
    exchange_errors: list[str] = []

    def exchange() -> None:
        try:
            with listener.accept() as connection:
                connection.send_bytes(precommit.request_payload)
                response.append(
                    connection.recv_bytes(TYPED_VALIDATION_BROKER_RESPONSE_LIMIT_BYTES)
                )
                completion.append(
                    connection.recv_bytes(TYPED_VALIDATION_BROKER_RESPONSE_LIMIT_BYTES)
                )
        except Exception as error:  # noqa: BLE001 - authenticated IPC fails closed
            exchange_errors.append(f"{type(error).__name__}: {error}")

    exchange_thread = threading.Thread(
        target=exchange,
        name="typed-validation-broker-exchange",
        daemon=True,
    )
    exchange_thread.start()
    broker_env = {
        key: value
        for key, value in _strip_completion_proof_env(env).items()
        if key.casefold()
        != _child_validation_module.BROKER_AUTHKEY_ENV.casefold()
    }
    broker_env[_child_validation_module.BROKER_AUTHKEY_ENV] = authkey.hex()
    broker_payload = (
        len(_BOUNDED_PROCESS_SOURCE_BYTES).to_bytes(8, "big")
        + _BOUNDED_PROCESS_SOURCE_BYTES
        + _CHILD_VALIDATION_SOURCE_BYTES
    )
    broker_payload_sha256 = sha256_bytes(broker_payload)
    broker_command = [
        # A Windows venv launcher spawns another PID. The isolated, stdlib-only
        # broker must be the process observed by its parent, so launch the
        # actual interpreter image used by this runner.
        str(_runner_executable_path()),
        "-I",
        "-c",
        _TYPED_VALIDATION_BROKER_BOOTSTRAP,
        _TYPED_VALIDATION_BROKER_BOOTSTRAP,
        broker_payload_sha256,
        "typed-validation-broker",
        "--host",
        str(host),
        "--port",
        str(port),
    ]
    try:
        result = run_process(
            validation_id=validation_id,
            execution_id=execution_id,
            command=broker_command,
            cwd=REPO_ROOT,
            env=broker_env,
            timeout_seconds=(
                timeout_seconds + TYPED_VALIDATION_BROKER_SHUTDOWN_GRACE_SECONDS
            ),
            stdin_bytes=broker_payload,
        )
    finally:
        listener.close()
    exchange_thread.join(TYPED_VALIDATION_BROKER_SHUTDOWN_GRACE_SECONDS)
    if exchange_thread.is_alive():
        exchange_errors.append("authenticated broker exchange did not stop")
    return (
        result,
        response[0] if len(response) == 1 else None,
        completion[0] if len(completion) == 1 else None,
        "; ".join(exchange_errors) if exchange_errors else None,
        tuple(broker_command),
    )


def _typed_validation_broker_observation(
    *,
    response_payload: bytes,
    precommit: _TypedValidationLaunchPrecommit,
    outer: ProcessResult,
    broker_command: Sequence[str],
) -> tuple[ProcessIdentity, ActionProcessObservation]:
    try:
        raw = json.loads(response_payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProofError("typed-validation broker response is not valid JSON") from error
    if not isinstance(raw, dict) or canonical_json(raw) != response_payload:
        raise ProofError("typed-validation broker response is not canonical JSON")
    observed_keys = {
        "protocol",
        "request_sha256",
        "status",
        "producer",
        "observation",
    }
    if set(raw) != observed_keys or raw.get("status") != "observed":
        diagnostic = raw.get("diagnostic") if isinstance(raw, dict) else None
        raise ProofError(
            "typed-validation broker did not return an action observation"
            + (f": {diagnostic}" if isinstance(diagnostic, str) else "")
        )
    if raw["protocol"] != _child_validation_module.BROKER_PROTOCOL:
        raise ProofError("typed-validation broker response protocol mismatch")
    if raw["request_sha256"] != precommit.request_sha256:
        raise ProofError("typed-validation broker response request binding mismatch")
    producer_raw = raw["producer"]
    observation_raw = raw["observation"]
    if not isinstance(producer_raw, dict) or not isinstance(observation_raw, dict):
        raise ProofError("typed-validation broker response identities are invalid")
    try:
        producer = ProcessIdentity.from_mapping(producer_raw)
        observation = ActionProcessObservation.from_mapping(observation_raw)
    except JournalContractError as error:
        raise ProofError(f"typed-validation broker observation is invalid: {error}") from error
    outer_identity = outer.child.launch_target_identity
    expected_outer_hash = outer_identity.get("sha256_before")
    expected_outer_argv = [outer.child.executable, *map(str, broker_command[1:])]
    if (
        producer.pid != outer.child.pid
        or os.path.normcase(producer.executable_path)
        != os.path.normcase(outer.child.executable)
        or producer.executable_sha256 != expected_outer_hash
        or producer.argv_sha256
        != _child_validation_module.hash_arguments(expected_outer_argv)
        or producer.started_at_unix_ns < outer.child.started_at
        or producer.started_at_unix_ns > outer.child.ended_at
    ):
        raise ProofError("typed-validation broker producer identity mismatch")
    if (
        observation.action_id != precommit.binding.validation_id
        or observation.action_execution_id != precommit.action_execution_id
        or observation.subjects != (precommit.binding.validation_id,)
        or os.path.normcase(observation.process.executable_path)
        != os.path.normcase(precommit.command[0])
        or observation.process.executable_sha256 != precommit.executable_sha256
        or observation.process.argv_sha256
        != _child_validation_module.hash_arguments(precommit.command)
    ):
        raise ProofError("typed-validation broker action observation mismatch")
    return producer, observation


def _typed_validation_broker_completion(
    *,
    completion_payload: bytes,
    precommit: _TypedValidationLaunchPrecommit,
    outer: ProcessResult,
    producer: ProcessIdentity,
) -> None:
    try:
        raw = json.loads(completion_payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProofError(
            "typed-validation broker completion is not valid JSON"
        ) from error
    if not isinstance(raw, dict) or canonical_json(raw) != completion_payload:
        raise ProofError("typed-validation broker completion is not canonical JSON")
    if set(raw) != {
        "protocol",
        "request_sha256",
        "status",
        "producer_ended_at_unix_ns",
        "producer_executable_sha256_after",
    } or raw.get("status") != "completed":
        raise ProofError("typed-validation broker did not return completion evidence")
    if raw["protocol"] != _child_validation_module.BROKER_PROTOCOL:
        raise ProofError("typed-validation broker completion protocol mismatch")
    if raw["request_sha256"] != precommit.request_sha256:
        raise ProofError("typed-validation broker completion request binding mismatch")
    producer_ended_at = raw["producer_ended_at_unix_ns"]
    producer_after = raw["producer_executable_sha256_after"]
    if (
        isinstance(producer_ended_at, bool)
        or not isinstance(producer_ended_at, int)
        or producer_ended_at < producer.started_at_unix_ns
        or producer_ended_at > outer.child.ended_at
        or not isinstance(producer_after, str)
        or SHA256_RE.fullmatch(producer_after) is None
    ):
        raise ProofError("typed-validation broker completion lifetime is invalid")
    if producer_after != outer.child.launch_target_identity.get("sha256_after"):
        raise ProofError("typed-validation broker completion identity mismatch")


def _run_typed_validation(
    repo_root: Path,
    config: Mapping[str, Any],
    env: Mapping[str, str],
    *,
    allow_test_config: bool,
    proof_attempt_id: str,
    proof_receipt_nonce: str,
    proof_scope: str,
) -> tuple[dict[str, object], ChildProcess]:
    validation_id = str(config["id"])
    validation_type = str(config["validation_type"])
    execution_id = str(uuid.uuid4())
    if allow_test_config:
        command = _expand_command(repo_root, config["command"])
        cwd = _resolve_path(repo_root, str(config.get("cwd", ".")))
        allowed_failures = frozenset(
            int(item) for item in config["validation_failure_exit_codes"]
        )
    else:
        command, cwd, allowed_failures = _production_typed_validation_spec(
            repo_root,
            validation_type,
        )
    timeout_seconds = int(config.get("timeout_seconds", 3600))
    input_contract_digest = _validation_input_contract_digest(config)
    with tempfile.TemporaryDirectory(prefix="kd4-typed-validation-broker-") as temp:
        journal_path = Path(temp) / "child-validation.ndjson"
        try:
            precommit = _typed_validation_precommit(
                validation_id=validation_id,
                validation_type=validation_type,
                execution_id=execution_id,
                input_contract_digest=input_contract_digest,
                proof_attempt_id=proof_attempt_id,
                proof_receipt_nonce=proof_receipt_nonce,
                proof_scope=proof_scope,
                command=command,
                cwd=cwd,
                env=env,
                timeout_seconds=timeout_seconds,
                allowed_failures=allowed_failures,
                journal_path=journal_path,
            )
        except (ProofError, JournalContractError, OSError, ValueError) as error:
            child = _unlaunched_child(
                validation_id=validation_id,
                execution_id=execution_id,
                command=command,
            )
            classification = "pre_result_error"
            diagnostic = str(error)
            return (
                _validation_report(
                    validation_id=validation_id,
                    execution_id=execution_id,
                    runner="typed-validation",
                    evidence_kind="typed_non_test",
                    validation_type=validation_type,
                    input_contract_digest=input_contract_digest,
                    classification=classification,
                    intended=[validation_id],
                    selected=[],
                    executed=[],
                    outcomes=[],
                    exit_code=None,
                    diagnostic=diagnostic,
                ),
                child,
            )
        (
            result,
            response_payload,
            completion_payload,
            exchange_error,
            broker_command,
        ) = (
            _typed_validation_broker_exchange(
                validation_id=validation_id,
                execution_id=execution_id,
                precommit=precommit,
                env=env,
                timeout_seconds=timeout_seconds,
            )
        )
        classification = "pre_result_error"
        diagnostic_parts = [
            part for part in (result.invocation_error, exchange_error) if part
        ]
        observation = None
        producer = None
        completion_valid = False
        if response_payload is None:
            diagnostic_parts.append("typed-validation broker returned no response")
        else:
            try:
                producer, observation = _typed_validation_broker_observation(
                    response_payload=response_payload,
                    precommit=precommit,
                    outer=result,
                    broker_command=broker_command,
                )
            except (ProofError, JournalContractError, OSError, ValueError) as error:
                diagnostic_parts.append(str(error))
        if producer is not None and observation is not None:
            if completion_payload is None:
                diagnostic_parts.append(
                    "typed-validation broker returned no completion response"
                )
            else:
                try:
                    _typed_validation_broker_completion(
                        completion_payload=completion_payload,
                        precommit=precommit,
                        outer=result,
                        producer=producer,
                    )
                    completion_valid = True
                except (ProofError, JournalContractError, OSError, ValueError) as error:
                    diagnostic_parts.append(str(error))
            try:
                verdict = parse_child_validation_journal(
                    journal_path,
                    expected_journal_path=precommit.journal_path,
                    expected_binding=precommit.binding,
                    expected_intended_ids=[validation_id],
                    expected_selected_ids=[validation_id],
                    expected_producer=producer,
                    expected_action_processes={validation_id: observation.process},
                    expected_action_observations={validation_id: observation},
                    outer_ended_at_unix_ns=result.child.ended_at,
                    outer_executable_sha256_after=str(
                        result.child.launch_target_identity.get("sha256_after", "")
                    ),
                    outer_exit_code=result.returncode,
                )
                classification = verdict.classification
                diagnostic_parts.extend(verdict.diagnostics)
                if classification == "confirmed_pass" and (
                    not completion_valid
                    or result.invocation_error is not None
                    or exchange_error is not None
                ):
                    classification = "pre_result_error"
            except (ProofError, JournalContractError, OSError, ValueError) as error:
                diagnostic_parts.append(str(error))
        if observation is not None:
            diagnostic_parts.insert(0, observation.diagnostic)
        diagnostic = "; ".join(part for part in diagnostic_parts if part)
    executed_action = classification in {
        "confirmed_pass",
        "confirmed_validation_failure",
    }
    action_ids = [validation_id] if executed_action else []
    outcomes = (
        [
            {
                "id": validation_id,
                "outcome": (
                    "passed" if classification == "confirmed_pass" else "failed"
                ),
            }
        ]
        if executed_action
        else []
    )
    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner="typed-validation",
            evidence_kind="typed_non_test",
            validation_type=validation_type,
            input_contract_digest=input_contract_digest,
            classification=classification,
            intended=[validation_id],
            selected=action_ids,
            executed=action_ids,
            outcomes=outcomes,
            exit_code=(observation.exit_code if observation is not None else None),
            diagnostic=diagnostic,
        ),
        result.child,
    )


def _rust_gate_intended_ids(repo_root: Path, gate: str) -> list[str]:
    manifest_path = repo_root / "codex-rs" / ".config" / "kd4-rust-tests.toml"
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ProofError(f"cannot load named Rust gate manifest: {error}") from error
    if manifest.get("version") != 1:
        raise ProofError("named Rust gate manifest version must be 1")
    gates = manifest.get("gates")
    if not isinstance(gates, dict) or gate not in gates:
        raise ProofError(f"unknown named Rust gate {gate!r}")
    gate_config = gates[gate]
    if not isinstance(gate_config, dict):
        raise ProofError(f"named Rust gate {gate!r} is not a table")
    steps = gate_config.get("steps")
    if not isinstance(steps, list) or not steps:
        raise ProofError(f"named Rust gate {gate!r} has zero steps")
    intended: list[str] = []
    for index, step in enumerate(steps):
        if not isinstance(step, dict):
            raise ProofError(f"named Rust gate {gate!r} step {index} is not a table")
        tests = step.get("tests")
        if (
            not isinstance(tests, list)
            or not tests
            or not all(isinstance(test_id, str) and test_id for test_id in tests)
        ):
            raise ProofError(
                f"named Rust gate {gate!r} step {index} has invalid test IDs"
            )
        intended.extend(tests)
    if len(intended) != len(set(intended)):
        raise ProofError(f"named Rust gate {gate!r} contains duplicate test IDs")
    return intended


def _run_rust_named_gate(
    *,
    repo_root: Path,
    config: Mapping[str, Any],
    env: Mapping[str, str],
    temp_dir: Path,
) -> tuple[dict[str, object], ChildProcess]:
    validation_id = str(config.get("id", ""))
    gate = str(config.get("gate", ""))
    execution_id = str(uuid.uuid4())
    intended = _rust_gate_intended_ids(repo_root, gate)
    report_path = temp_dir / f"rust-gate-{execution_id}.json"
    command = [
        sys.executable,
        str(repo_root / "scripts" / "rust_test_runner.py"),
        "run-gate-proof",
        "--profile",
        "completion-proof",
        gate,
        "--proof-report",
        str(report_path),
        "--proof-execution-id",
        execution_id,
    ]
    gate_env = dict(env)
    if gate == "windows-process-coverage":
        gate_env["CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS"] = "1"
    result = run_process(
        validation_id=validation_id,
        execution_id=execution_id,
        command=command,
        cwd=repo_root,
        env=gate_env,
        timeout_seconds=int(config.get("timeout_seconds", 3600)),
    )

    classification = "pre_result_error"
    selected: list[str] = []
    executed: list[str] = []
    outcomes: list[dict[str, str]] = []
    diagnostic_parts = [
        part for part in (result.invocation_error, result.stderr) if part
    ]
    raw: dict[str, Any] | None = None
    if report_path.exists():
        try:
            raw = _load_report(report_path, label=validation_id)
        except ProofError as error:
            diagnostic_parts.append(str(error))
    elif not result.invocation_error:
        diagnostic_parts.append("named Rust gate runner omitted its structured report")

    if raw is not None:
        raw_intended = raw.get("intended_ids")
        raw_selected = raw.get("selected_ids")
        raw_executed = raw.get("executed_ids")
        raw_outcomes = raw.get("outcomes")
        lists_are_typed = (
            isinstance(raw_intended, list)
            and all(isinstance(item, str) and item for item in raw_intended)
            and isinstance(raw_selected, list)
            and all(isinstance(item, str) and item for item in raw_selected)
            and isinstance(raw_executed, list)
            and all(isinstance(item, str) and item for item in raw_executed)
            and isinstance(raw_outcomes, list)
            and all(isinstance(item, dict) for item in raw_outcomes)
        )
        parsed_outcomes: list[dict[str, str]] = []
        outcomes_are_typed = lists_are_typed
        if lists_are_typed:
            for item in raw_outcomes:
                outcome_id = item.get("id")
                outcome = item.get("outcome")
                if (
                    not isinstance(outcome_id, str)
                    or not outcome_id
                    or outcome not in {"passed", "failed", "unknown", "skipped"}
                ):
                    outcomes_are_typed = False
                    break
                parsed_outcomes.append({"id": outcome_id, "outcome": str(outcome)})
        exact_envelope = (
            raw.get("schema_version") == 1
            and raw.get("report_type") == "RustNamedGateExecutionReportV1"
            and raw.get("gate") == gate
            and raw.get("execution_id") == execution_id
            and lists_are_typed
            and outcomes_are_typed
            and raw_intended == intended
            and raw_selected == intended
            and len(raw_selected) == len(set(raw_selected))
            and len(raw_executed) == len(set(raw_executed))
            and set(raw_executed).issubset(set(intended))
            and [item["id"] for item in parsed_outcomes] == raw_executed
            and len(parsed_outcomes) == len({item["id"] for item in parsed_outcomes})
        )
        raw_diagnostic = raw.get("diagnostic")
        if isinstance(raw_diagnostic, str) and raw_diagnostic:
            diagnostic_parts.append(raw_diagnostic)
        confirmed_outcomes = [
            item for item in parsed_outcomes if item["outcome"] in {"passed", "failed"}
        ]
        confirmed_executed = [item["id"] for item in confirmed_outcomes]
        has_confirmed_failure = any(
            item["outcome"] == "failed" for item in confirmed_outcomes
        )
        if (
            exact_envelope
            and raw.get("classification") == "confirmed_pass"
            and result.invocation_error is None
            and result.returncode == 0
            and len(raw_executed) == len(intended)
            and set(raw_executed) == set(intended)
            and all(item["outcome"] == "passed" for item in parsed_outcomes)
        ):
            classification = "confirmed_pass"
            selected = list(intended)
            executed = list(raw_executed)
            outcomes = parsed_outcomes
        elif (
            exact_envelope
            and raw.get("classification") == "confirmed_validation_failure"
            and result.child.exit_code == 100
            and (
                (result.returncode == 100 and result.invocation_error is None)
                or (result.returncode is None and result.invocation_error is not None)
            )
            and has_confirmed_failure
            and confirmed_executed
            and confirmed_executed == raw_executed
        ):
            # A later runner fault cannot erase a test failure that already
            # produced a trusted terminal event. Only completed test outcomes
            # are carried as failure evidence; partial starts remain diagnostic.
            classification = "confirmed_validation_failure"
            selected = list(intended)
            executed = confirmed_executed
            outcomes = confirmed_outcomes
        else:
            diagnostic_parts.append(
                "named Rust gate report did not prove an exact fresh execution"
            )

    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner="rust-gate",
            runner_selector=gate,
            classification=classification,
            intended=intended,
            selected=selected,
            executed=executed,
            outcomes=outcomes,
            exit_code=result.returncode,
            diagnostic="\n".join(diagnostic_parts),
        ),
        result.child,
    )


def _runtime_inputs() -> dict[str, str]:
    missing = [name for name in REQUIRED_RUNTIME_ENV if not os.environ.get(name)]
    if missing:
        raise ProofError(
            "missing runtime completion-proof input(s): " + ", ".join(missing)
        )
    values = {name: os.environ[name] for name in REQUIRED_RUNTIME_ENV}
    try:
        uuid.UUID(values["CODEX_COMPLETION_PROOF_ATTEMPT_ID"])
    except ValueError as error:
        raise ProofError("CODEX_COMPLETION_PROOF_ATTEMPT_ID must be a UUID") from error
    if len(values["CODEX_COMPLETION_PROOF_NONCE"]) < 32:
        raise ProofError("CODEX_COMPLETION_PROOF_NONCE is too short")
    if (
        SHA256_RE.fullmatch(
            values["CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256"]
        )
        is None
    ):
        raise ProofError(
            "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256 must be a "
            "lowercase SHA-256"
        )
    try:
        if int(values["CODEX_COMPLETION_PROOF_PARENT_PID"]) <= 0:
            raise ValueError
        if int(values["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]) < 0:
            raise ValueError
    except ValueError as error:
        raise ProofError(
            "completion-proof parent PID and mutation epoch must be nonnegative integers"
        ) from error
    return values


def _reserve_report(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as error:
        raise ProofError(
            f"completion-proof report already exists; copied/cached output is rejected: {path}"
        ) from error
    os.close(descriptor)


def _lock_path(report_path: Path, repository_root: Path) -> Path:
    repo_key = sha256_bytes(str(repository_root).casefold().encode("utf-8"))[:20]
    return report_path.parent / f".completion-proof-{repo_key}.lock"


def _acquire_lock(path: Path, attempt_id: str) -> int:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        if os.fstat(descriptor).st_size == 0:
            os.write(descriptor, b"\0")
            os.fsync(descriptor)
        os.lseek(descriptor, 0, os.SEEK_SET)
        try:
            if os.name == "nt":
                import msvcrt

                msvcrt.locking(descriptor, msvcrt.LK_NBLCK, 1)
            else:
                import fcntl

                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            raise ProofError(f"another completion-proof attempt owns {path}") from error

        owner = canonical_json(
            {
                "attempt_id": attempt_id,
                "pid": os.getpid(),
                "started_ns": time.time_ns(),
            }
        )
        os.lseek(descriptor, 0, os.SEEK_SET)
        os.write(descriptor, owner)
        os.ftruncate(descriptor, len(owner))
        os.fsync(descriptor)
        return descriptor
    except Exception:
        os.close(descriptor)
        raise


def _release_lock(descriptor: int) -> None:
    try:
        os.lseek(descriptor, 0, os.SEEK_SET)
        if os.name == "nt":
            import msvcrt

            msvcrt.locking(descriptor, msvcrt.LK_UNLCK, 1)
        else:
            import fcntl

            fcntl.flock(descriptor, fcntl.LOCK_UN)
    finally:
        os.close(descriptor)


def _testing_inventory(
    repo_root: Path,
    config: Mapping[str, Any],
    *,
    allow_test_config: bool,
) -> list[dict[str, object]] | None:
    value = config.get("testing_current_inventory")
    if value is None:
        return None
    if not allow_test_config:
        raise ProofError(
            "testing_current_inventory is forbidden outside runner integration tests"
        )
    path = _resolve_path(repo_root, str(value))
    report = _load_json_object(path, label="testing current inventory")
    rows = report.get("tests")
    if (
        not isinstance(rows, list)
        or not rows
        or not all(isinstance(row, dict) for row in rows)
    ):
        raise ProofError("testing current inventory must contain nonzero object rows")
    return [dict(row) for row in rows]


def _reconciliation_projection(
    reconciliation: Reconciliation,
) -> dict[str, object]:
    return {
        "required_by_framework": {
            framework: list(ids)
            for framework, ids in sorted(reconciliation.required_by_framework.items())
        },
        "exceptions": list(reconciliation.exceptions),
        "additions": list(reconciliation.additions),
        "overrides": list(reconciliation.overrides),
        "current_ids": sorted(reconciliation.current_ids),
    }


def _reconciliation_worker(
    config_path: Path,
    input_path: Path,
    *,
    allow_test_config: bool,
) -> int:
    repo_root, config = load_config(config_path, allow_test_config=allow_test_config)
    configured = validation_configs(
        config,
        allow_test_config=allow_test_config,
    )
    payload = _load_json_object(input_path, label="inventory reconciliation input")
    if payload.get("schema_version") != 1:
        raise ProofError("inventory reconciliation input schema_version must be 1")
    execution_id = str(payload.get("execution_id", ""))
    try:
        uuid.UUID(execution_id)
    except ValueError as error:
        raise ProofError(
            "inventory reconciliation input execution_id must be a UUID"
        ) from error
    raw_rows = payload.get("current_rows")
    if (
        not isinstance(raw_rows, list)
        or not raw_rows
        or not all(isinstance(row, dict) for row in raw_rows)
    ):
        raise ProofError(
            "inventory reconciliation input must contain nonzero current_rows"
        )
    current_rows = [dict(row) for row in raw_rows]
    raw_known_ids = payload.get("known_validation_ids")
    if (
        not isinstance(raw_known_ids, list)
        or not raw_known_ids
        or not all(isinstance(item, str) and item for item in raw_known_ids)
        or len(raw_known_ids) != len(set(raw_known_ids))
    ):
        raise ProofError(
            "inventory reconciliation input has invalid known_validation_ids"
        )
    configured_ids = sorted(str(item["id"]) for item in configured.values())
    if sorted(raw_known_ids) != configured_ids:
        raise ProofError(
            "inventory reconciliation input validation IDs do not match config"
        )
    reconciliation = reconcile_inventory(
        repo_root,
        config,
        current_rows,
        known_validation_ids=set(raw_known_ids),
    )
    selected_ids = sorted(reconciliation.current_ids)
    if not selected_ids:
        raise ProofError("inventory reconciliation selected zero test IDs")
    _, _, frozen_digest = load_frozen_inventory(repo_root, config)
    projection = _reconciliation_projection(reconciliation)
    print(
        json.dumps(
            {
                "schema_version": 1,
                "execution_id": execution_id,
                "frozen_inventory_hash": frozen_digest,
                "current_inventory_hash": inventory_hash(current_rows),
                "selected_ids": selected_ids,
                "executed_ids": selected_ids,
                "reconciliation_hash": sha256_bytes(canonical_json(projection)),
            },
            sort_keys=True,
        ),
        flush=True,
    )
    return 0


def _run_inventory_reconciliation(
    *,
    repo_root: Path,
    config_path: Path,
    current_rows: Sequence[Mapping[str, object]],
    known_validation_ids: set[str],
    reconciliation: Reconciliation,
    frozen_inventory_hash: str,
    temp_dir: Path,
    env: Mapping[str, str],
    timeout_seconds: int,
    allow_test_config: bool,
    validation_config: Mapping[str, Any],
) -> tuple[dict[str, object], ChildProcess]:
    validation_id = "inventory.frozen-reconciliation"
    execution_id = str(uuid.uuid4())
    intended = sorted(reconciliation.current_ids)
    worker_arguments = [
        "--config",
        str(config_path.resolve()),
        "reconciliation-worker",
        "--input",
        str(temp_dir / f"inventory-reconciliation-{execution_id}.json"),
    ]
    if allow_test_config:
        unittest_code = (
            "import runpy,sys; "
            "module=runpy.run_path(sys.argv[1]); "
            "raise SystemExit(module['_unittest_main'](sys.argv[2:]))"
        )
        command = [
            sys.executable,
            "-c",
            unittest_code,
            str(Path(__file__).resolve()),
            *worker_arguments,
        ]
    else:
        command = [sys.executable, str(Path(__file__).resolve()), *worker_arguments]
    if not intended:
        child = _unlaunched_child(
            validation_id=validation_id,
            execution_id=execution_id,
            command=command,
        )
        return (
            _validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner="inventory-reconciliation",
                classification="pre_result_error",
                intended=[],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                evidence_kind="inventory_reconciliation",
                input_contract_digest=_validation_input_contract_digest(
                    validation_config
                ),
                diagnostic="inventory reconciliation selected zero test IDs",
            ),
            child,
        )
    input_path = Path(worker_arguments[-1])
    _write_new_json(
        input_path,
        {
            "schema_version": 1,
            "execution_id": execution_id,
            "current_rows": [dict(row) for row in current_rows],
            "known_validation_ids": sorted(known_validation_ids),
        },
    )
    result = run_process(
        validation_id=validation_id,
        execution_id=execution_id,
        command=command,
        cwd=repo_root,
        env=env,
        timeout_seconds=timeout_seconds,
    )
    diagnostic = result.invocation_error or result.stderr
    selected: list[str] = []
    executed: list[str] = []
    classification = "pre_result_error"
    if result.returncode == 0 and not result.invocation_error:
        try:
            raw = json.loads(result.stdout)
        except json.JSONDecodeError:
            diagnostic = "inventory reconciliation worker returned invalid JSON"
        else:
            if not isinstance(raw, dict):
                diagnostic = "inventory reconciliation worker report is not an object"
            else:
                selected = [str(item) for item in raw.get("selected_ids", [])]
                executed = [str(item) for item in raw.get("executed_ids", [])]
                expected_reconciliation_hash = sha256_bytes(
                    canonical_json(_reconciliation_projection(reconciliation))
                )
                exact = (
                    raw.get("schema_version") == 1
                    and raw.get("execution_id") == execution_id
                    and raw.get("frozen_inventory_hash") == frozen_inventory_hash
                    and raw.get("current_inventory_hash")
                    == inventory_hash(current_rows)
                    and raw.get("reconciliation_hash") == expected_reconciliation_hash
                    and selected == intended
                    and executed == intended
                    and len(selected) == len(set(selected))
                    and len(executed) == len(set(executed))
                )
                if exact:
                    classification = "confirmed_pass"
                else:
                    diagnostic = (
                        "inventory reconciliation worker result did not match the "
                        "current attempt"
                    )
    outcomes = [
        {"id": item, "outcome": "passed"}
        for item in executed
        if classification == "confirmed_pass"
    ]
    return (
        _validation_report(
            validation_id=validation_id,
            execution_id=execution_id,
            runner="inventory-reconciliation",
            evidence_kind="inventory_reconciliation",
            input_contract_digest=_validation_input_contract_digest(validation_config),
            classification=classification,
            intended=intended,
            selected=selected,
            executed=executed,
            outcomes=outcomes,
            exit_code=result.returncode,
            diagnostic=diagnostic,
        ),
        result.child,
    )


def _attempt_classification(validations: Sequence[Mapping[str, object]]) -> str:
    classifications = [str(item.get("classification", "")) for item in validations]
    if "pre_result_error" in classifications:
        return "pre_result_error"
    if "confirmed_validation_failure" in classifications:
        return "confirmed_validation_failure"
    if validations and all(item == "confirmed_pass" for item in classifications):
        return "confirmed_pass"
    return "pre_result_error"


def _canonical_validation_id_closure(
    accepted_ids: Iterable[str], validations: Sequence[Mapping[str, object]]
) -> list[str]:
    expected = frozenset(accepted_ids)
    observed = [str(item.get("id", "")) for item in validations]
    observed_set = set(observed)
    missing = sorted(expected - observed_set)
    unexpected = sorted(observed_set - expected)
    duplicated = sorted(
        validation_id
        for validation_id in observed_set
        if observed.count(validation_id) > 1
    )
    if not missing and not unexpected and not duplicated:
        return []
    return [
        "canonical final validation ID closure mismatch: "
        f"missing={missing}, unexpected={unexpected}, duplicated={duplicated}"
    ]


def _launched_child_error(
    validation: Mapping[str, object], child: ChildProcess
) -> str | None:
    validation_id = validation.get("id")
    execution_id = validation.get("execution_id")
    if child.validation_id != validation_id or child.execution_id != execution_id:
        return "validation and child process identities did not match"
    if (
        isinstance(child.pid, bool)
        or not isinstance(child.pid, int)
        or child.pid <= 0
        or not child.executable
    ):
        return "validation child process was not launched"
    if SHA256_RE.fullmatch(child.args_hash) is None:
        return "validation child process arguments were not authenticated"
    if (
        isinstance(child.started_at, bool)
        or not isinstance(child.started_at, int)
        or child.started_at <= 0
        or isinstance(child.ended_at, bool)
        or not isinstance(child.ended_at, int)
        or child.ended_at < child.started_at
    ):
        return "validation child process timing was invalid"
    identity = child.launch_target_identity
    requested = identity.get("requested")
    resolved_path = identity.get("resolved_path")
    before = identity.get("sha256_before")
    after = identity.get("sha256_after")
    if (
        not isinstance(requested, str)
        or not requested
        or not isinstance(resolved_path, str)
        or not resolved_path
        or os.path.normcase(resolved_path) != os.path.normcase(child.executable)
        or not isinstance(before, str)
        or SHA256_RE.fullmatch(before) is None
        or not isinstance(after, str)
        or after != before
    ):
        return "validation child process launch target was unresolved or changed"
    return None


def _enforce_child_evidence_contract(
    validations: Sequence[dict[str, object]], children: Sequence[ChildProcess]
) -> list[str]:
    confirmed = [
        item
        for item in validations
        if item.get("classification")
        in {"confirmed_pass", "confirmed_validation_failure"}
    ]
    validation_ids = [str(item.get("id", "")) for item in confirmed]
    execution_ids = [str(item.get("execution_id", "")) for item in confirmed]
    child_keys = [(child.validation_id, child.execution_id) for child in children]
    child_validation_ids = [child.validation_id for child in children]
    child_execution_ids = [child.execution_id for child in children]
    errors: list[str] = []

    for validation in confirmed:
        validation_id = str(validation.get("id", ""))
        execution_id = str(validation.get("execution_id", ""))
        item_errors: list[str] = []
        try:
            uuid.UUID(execution_id)
        except (ValueError, AttributeError):
            item_errors.append("validation execution identity was invalid")
        if validation_ids.count(validation_id) != 1:
            item_errors.append("validation identity was duplicated")
        if execution_ids.count(execution_id) != 1:
            item_errors.append("validation execution identity was duplicated")
        matches = [
            child
            for child in children
            if child.validation_id == validation_id
            and child.execution_id == execution_id
        ]
        if len(matches) != 1:
            item_errors.append(
                "validation did not have exactly one matching child process"
            )
        else:
            child_error = _launched_child_error(validation, matches[0])
            if child_error:
                item_errors.append(child_error)
        if child_validation_ids.count(validation_id) != 1:
            item_errors.append("child validation identity was missing or duplicated")
        if child_execution_ids.count(execution_id) != 1:
            item_errors.append("child execution identity was missing or duplicated")
        if item_errors:
            message = "; ".join(dict.fromkeys(item_errors))
            validation["classification"] = "pre_result_error"
            validation["confirmed_failure_ids"] = []
            prior = str(validation.get("diagnostic", ""))
            validation["diagnostic"] = "; ".join(
                part for part in (prior, message) if part
            )[-8000:]
            validation.pop("report_hash", None)
            validation["report_hash"] = _result_hash(validation)
            errors.append(f"{validation_id}: {message}")

    validation_keys = {
        (str(item.get("id", "")), str(item.get("execution_id", "")))
        for item in validations
    }
    orphaned = [key for key in child_keys if key not in validation_keys]
    if orphaned:
        errors.append("attempt contained a child process without a matching validation")
    return errors


def _attempt_report(
    *,
    runtime: Mapping[str, str],
    policy_id: str,
    runner_identity: RunnerProcessIdentityStart,
    repo_root: Path,
    inventory_digest: str,
    observed_start: str,
    observed_end: str,
    validations: Sequence[Mapping[str, object]],
    children: Sequence[ChildProcess],
    exceptions: Sequence[Mapping[str, object]],
    overrides: Sequence[Mapping[str, object]],
    fatal_error: str,
    classification: str,
    report_type: str = REPORT_TYPE,
    exact_command: str = EXACT_COMMAND,
    focused_validation_id: str | None = None,
    runner_identity_result: tuple[dict[str, object], str | None] | None = None,
) -> dict[str, object]:
    runner_process_identity, runner_identity_error = (
        runner_identity.finish()
        if runner_identity_result is None
        else runner_identity_result
    )
    report: dict[str, object] = {
        "schema_version": 2,
        "report_type": report_type,
        "exact_command": exact_command,
        "policy_id": policy_id,
        "policy_runner_bundle_sha256": runtime[
            "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256"
        ],
        "nonce": runtime["CODEX_COMPLETION_PROOF_NONCE"],
        "attempt_id": runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
        "parent_pid": int(runtime["CODEX_COMPLETION_PROOF_PARENT_PID"]),
        "observed_runner_parent_pid": os.getppid(),
        "runner_process_identity": runner_process_identity,
        "repository_root": str(repo_root),
        "host_identity": _host_identity(),
        "inventory_hash": inventory_digest,
        "start_fingerprint": runtime["CODEX_COMPLETION_PROOF_START_FINGERPRINT"],
        "end_fingerprint": observed_end,
        "start_mutation_epoch": int(runtime["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]),
        "end_mutation_epoch": int(runtime["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]),
        "attempt_classification": classification,
        "validations": list(validations),
        "child_processes": [child.as_json() for child in children],
        "exceptions": list(exceptions),
        "overrides": list(overrides),
        "workspace": {
            "observed_start_fingerprint": observed_start,
            "observed_end_fingerprint": observed_end,
        },
        "fatal_error": (
            f"{fatal_error}; {runner_identity_error}"
            if fatal_error and runner_identity_error
            else runner_identity_error or fatal_error
        ),
    }
    if runner_identity_error:
        report["attempt_classification"] = "pre_result_error"
    if focused_validation_id is not None:
        report["focused_validation_id"] = focused_validation_id
    report["attempt_report_hash"] = sha256_bytes(canonical_json(report))
    return report


def _write_report(path: Path, report: Mapping[str, object]) -> None:
    payload = (
        json.dumps(report, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    ).encode("utf-8")
    temporary_path = path.parent / f".completion-proof-report-{uuid.uuid4()}.tmp"
    descriptor: int | None = None
    try:
        descriptor = os.open(
            temporary_path,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
        view = memoryview(payload)
        while view:
            written = os.write(descriptor, view)
            if written <= 0:
                raise OSError("completion-proof report write made no progress")
            view = view[written:]
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = None
        os.replace(temporary_path, path)
    except BaseException:
        if descriptor is not None:
            os.close(descriptor)
        try:
            temporary_path.unlink()
        except FileNotFoundError:
            pass
        raise


def _focused_pre_result(validation_id: str, diagnostic: str) -> dict[str, object]:
    return _validation_report(
        validation_id=validation_id or "completion-focused.invocation",
        execution_id=str(uuid.uuid4()),
        runner="infrastructure",
        evidence_kind="infrastructure",
        classification="pre_result_error",
        intended=[],
        selected=[],
        executed=[],
        outcomes=[],
        exit_code=None,
        diagnostic=diagnostic,
    )


def _focused_inventory_rows(
    *,
    repo_root: Path,
    config: Mapping[str, Any],
    runner: str,
    temp_dir: Path,
    allow_test_config: bool,
) -> list[dict[str, object]]:
    if runner == "inventory-reconciliation":
        _audit_test_system_surface(repo_root)
    testing_rows = _testing_inventory(
        repo_root,
        config,
        allow_test_config=allow_test_config,
    )
    if testing_rows is not None:
        return testing_rows
    env = _network_disabled_env()
    if runner == "rust-nextest":
        rows, _ = _rust_inventory(repo_root, env, temp_dir)
        return rows
    if runner == "rust-doctest":
        rows, _ = _doctest_inventory(repo_root, env)
        return rows
    if runner == "python-unittest":
        rows, _ = _unittest_inventory(repo_root, env, temp_dir)
        return rows
    if runner == "python-pytest":
        rows, _ = _pytest_inventory(repo_root, env, temp_dir)
        return rows
    if runner == "javascript-jest":
        return _jest_inventory(repo_root)
    if runner in NATIVE_ADAPTERS:
        rows, _ = _native_adapter_inventory(repo_root, env, runner)
        return rows
    if runner == "inventory-reconciliation":
        rows, _ = discover_inventory(repo_root, temp_dir=temp_dir)
        return rows
    raise ProofError(f"focused validation has unsupported runner {runner!r}")


def _focused_framework_ids(
    config: Mapping[str, Any],
    rows: Sequence[Mapping[str, object]],
    framework: str,
) -> list[str]:
    active_platform = platform.system().casefold()
    intended: list[str] = []
    for row in rows:
        if str(row.get("framework", "")) != framework:
            continue
        platforms = row.get("platforms")
        if (
            not isinstance(platforms, list)
            or not platforms
            or not all(isinstance(item, str) and item for item in platforms)
        ):
            raise ProofError(f"focused {framework} inventory row has invalid platforms")
        if active_platform not in {item.casefold() for item in platforms}:
            continue
        exception = _exception_for_row(config, row)
        if exception is not None and exception["kind"] not in {
            "protected",
            "generated",
        }:
            continue
        native_id = str(row.get("native_id", ""))
        if not native_id:
            raise ProofError(
                f"focused {framework} inventory row has an empty native ID"
            )
        intended.append(native_id)
    intended.sort()
    if len(intended) != len(set(intended)):
        raise ProofError(f"focused {framework} inventory contains duplicate native IDs")
    return intended


def _run_focused_validation(
    *,
    repo_root: Path,
    config_path: Path,
    config: Mapping[str, Any],
    configured_validations: Mapping[str, Mapping[str, Any]],
    item: Mapping[str, Any],
    inventory_digest: str,
    temp_dir: Path,
    allow_test_config: bool,
    proof_attempt_id: str,
    proof_receipt_nonce: str,
) -> tuple[dict[str, object], ChildProcess]:
    runner = str(item["runner"])
    env = _network_disabled_env()
    if runner == "typed-validation":
        return _run_typed_validation(
            repo_root,
            item,
            env,
            allow_test_config=allow_test_config,
            proof_attempt_id=proof_attempt_id,
            proof_receipt_nonce=proof_receipt_nonce,
            proof_scope="focused",
        )
    if runner == "rust-gate":
        return _run_rust_named_gate(
            repo_root=repo_root,
            config=item,
            env=env,
            temp_dir=temp_dir,
        )

    rows = _focused_inventory_rows(
        repo_root=repo_root,
        config=config,
        runner=runner,
        temp_dir=temp_dir,
        allow_test_config=allow_test_config,
    )
    if runner == "inventory-reconciliation":
        known_validation_ids = {
            str(value["id"]) for value in configured_validations.values()
        }
        reconciliation = reconcile_inventory(
            repo_root,
            config,
            rows,
            known_validation_ids=known_validation_ids,
        )
        return _run_inventory_reconciliation(
            repo_root=repo_root,
            config_path=config_path,
            current_rows=rows,
            known_validation_ids=known_validation_ids,
            reconciliation=reconciliation,
            frozen_inventory_hash=inventory_digest,
            temp_dir=temp_dir,
            env=env,
            timeout_seconds=int(item["timeout_seconds"]),
            allow_test_config=allow_test_config,
            validation_config=item,
        )

    framework = {
        "rust-nextest": "rust-nextest",
        "rust-doctest": "rust-doctest",
        "python-unittest": "python-unittest",
        "python-pytest": "python-pytest",
        "javascript-jest": "javascript-jest",
        "argument-comment-lint-native": "argument-comment-lint-native",
        "windows-sandbox-smoke": "windows-sandbox-smoke",
    }[runner]
    intended = _focused_framework_ids(config, rows, framework)
    timeout_seconds = int(item["timeout_seconds"])
    if runner == "rust-nextest":
        return _run_rust_nextest(
            repo_root,
            intended,
            env,
            timeout_seconds=timeout_seconds,
        )
    if runner == "rust-doctest":
        return _run_rust_doctests(
            repo_root,
            intended,
            env,
            timeout_seconds=timeout_seconds,
        )
    if runner == "python-unittest":
        return _run_structured_wrapper(
            validation_id="maintenance.root-unittest",
            framework=framework,
            intended=intended,
            command=[
                "uv",
                "run",
                "--offline",
                "--frozen",
                "--project",
                "scripts",
                "python",
                str(repo_root / "scripts" / "completion_proof_unittest.py"),
                "run",
                "--expected-file",
                "{expected_file}",
                "--output",
                "{report_file}",
            ],
            report_path=temp_dir / "focused-root-unittest-results.json",
            cwd=repo_root,
            env=env,
            timeout_seconds=timeout_seconds,
            temp_dir=temp_dir,
            proof_attempt_id=proof_attempt_id,
            proof_scope="focused",
        )
    if runner == "python-pytest":
        return _run_structured_wrapper(
            validation_id="sdk.python.pytest",
            framework=framework,
            intended=intended,
            command=[
                "uv",
                "run",
                "--offline",
                "--frozen",
                "--directory",
                str(repo_root / "sdk" / "python"),
                "--group",
                "dev",
                "python",
                str(repo_root / "scripts" / "completion_proof_pytest.py"),
                "run",
                "--expected-file",
                "{expected_file}",
                "--output",
                "{report_file}",
            ],
            report_path=temp_dir / "focused-sdk-python-pytest-results.json",
            cwd=repo_root,
            env=env,
            timeout_seconds=timeout_seconds,
            temp_dir=temp_dir,
            proof_attempt_id=proof_attempt_id,
            proof_scope="focused",
        )
    if runner == "javascript-jest":
        return _run_jest(
            repo_root,
            intended,
            env,
            temp_dir,
            timeout_seconds=timeout_seconds,
        )
    if runner in NATIVE_ADAPTERS:
        return _run_native_adapter(
            repo_root,
            runner,
            intended,
            env,
            temp_dir,
            timeout_seconds=timeout_seconds,
        )
    raise AssertionError(runner)


def _current_evidence_process_contract(
    children: Sequence[ChildProcess], *, temp_dir: Path
) -> tuple[list[dict[str, object]], dict[str, object]]:
    expected_roles = (
        "inventory.rust-nextest",
        "inventory.rust-doctest",
        "inventory.root-unittest",
        "inventory.sdk-python-pytest",
        "inventory.tools.argument-comment-lint.native",
        "inventory.windows.sandbox-smoke",
    )
    if tuple(child.validation_id for child in children) != expected_roles:
        raise ProofError("current-evidence discovery did not retain the exact process set")
    report_paths = {
        "inventory.root-unittest": temp_dir / "unittest-collection.json",
        "inventory.sdk-python-pytest": temp_dir / "pytest-collection.json",
    }
    processes: list[dict[str, object]] = []
    authority: list[dict[str, object]] = []
    for child in children:
        if not child.launched_argv or not child.launched_cwd:
            raise ProofError(
                f"current-evidence discovery {child.validation_id} omitted launch authority"
            )
        child_json = child.as_json()
        child_json["started_at"] = str(child.started_at)
        child_json["ended_at"] = str(child.ended_at)
        argv = list(child.launched_argv)
        report_path = report_paths.get(child.validation_id)
        if report_path is None:
            output: dict[str, object] = {
                "kind": "stdout",
                "stdout_sha256": sha256_bytes(child.stdout_bytes),
            }
            output_kind = "stdout"
            authority_report_path: str | None = None
        else:
            try:
                raw_report = report_path.read_bytes()
            except OSError as error:
                raise ProofError(
                    f"cannot retain {child.validation_id} collection report: {error}"
                ) from error
            report_text = str(report_path.resolve())
            output = {
                "kind": "report-file",
                "report_path": report_text,
                "report_identity": _focused_catalog._current_windows_file_identity(
                    report_text,
                    f"{child.validation_id} collection report",
                ),
                "report_sha256": sha256_bytes(raw_report),
            }
            output_kind = "report-file"
            authority_report_path = report_text
        process = {
            "role": child.validation_id,
            "child_process": child_json,
            "argv": argv,
            "cwd": child.launched_cwd,
            "output": output,
        }
        processes.append(process)
        authority.append(
            {
                "role": child.validation_id,
                "executable": child.executable,
                "argv": argv,
                "cwd": child.launched_cwd,
                "output_kind": output_kind,
                "report_path": authority_report_path,
            }
        )
    return processes, {"expected_processes": authority}


def _run_current_evidence_validation(
    *,
    repo_root: Path,
    config: Mapping[str, Any],
    item: Mapping[str, Any],
    current_rows: Sequence[Mapping[str, object]],
    temp_dir: Path,
    proof_attempt_id: str,
    raw_report_bytes: list[bytes],
) -> tuple[dict[str, object], ChildProcess]:
    runner = str(item["runner"])
    if runner not in {"python-unittest", "python-pytest"}:
        raise ProofError(f"current-evidence validation has invalid runner {runner!r}")
    framework = runner
    intended = _focused_framework_ids(config, current_rows, framework)
    if runner == "python-unittest":
        command = [
            "uv", "run", "--offline", "--frozen", "--project", "scripts",
            "python", str(repo_root / "scripts" / "completion_proof_unittest.py"),
            "run", "--expected-file", "{expected_file}", "--output", "{report_file}",
        ]
        report_path = temp_dir / "current-evidence-root-unittest-results.json"
    else:
        command = [
            "uv", "run", "--offline", "--frozen", "--directory",
            str(repo_root / "sdk" / "python"), "--group", "dev", "python",
            str(repo_root / "scripts" / "completion_proof_pytest.py"), "run",
            "--expected-file", "{expected_file}", "--output", "{report_file}",
        ]
        report_path = temp_dir / "current-evidence-sdk-python-pytest-results.json"
    return _run_structured_wrapper(
        validation_id=str(item["id"]),
        framework=framework,
        intended=intended,
        command=command,
        report_path=report_path,
        cwd=repo_root,
        env=_network_disabled_env(),
        timeout_seconds=int(item["timeout_seconds"]),
        temp_dir=temp_dir,
        proof_attempt_id=proof_attempt_id,
        proof_scope="focused",
        raw_report_bytes=raw_report_bytes,
    )


def _run_current_evidence_attempt(
    config_path: Path, *, allow_test_config: bool
) -> int:
    runtime = _runtime_inputs()
    runner_identity = _capture_runner_process_identity()
    report_path = Path(runtime["CODEX_COMPLETION_PROOF_REPORT"]).resolve()
    _reserve_report(report_path)
    channel: _RunnerAttestationChannel | None = None
    lock_descriptor: int | None = None
    validations: list[dict[str, object]] = []
    children: list[ChildProcess] = []
    inventory_digest = ""
    observed_start = ""
    observed_end = ""
    repo_root = REPO_ROOT.resolve()
    policy_id = KD4_POLICY_ID
    fatal_error = ""
    unresolved_baseline_count: int | None = None
    pending_replacement_review_count: int | None = None
    unresolved_projection: Mapping[str, object] | None = None
    frame_members: list[tuple[str, bytes]] | None = None
    final_runner_identity: tuple[dict[str, object], str | None] | None = None

    def report(classification: str, diagnostic: str) -> dict[str, object]:
        return _attempt_report(
            runtime=runtime,
            policy_id=policy_id,
            runner_identity=runner_identity,
            repo_root=repo_root,
            inventory_digest=inventory_digest,
            observed_start=observed_start,
            observed_end=observed_end,
            validations=validations,
            children=children,
            exceptions=[],
            overrides=[],
            fatal_error=diagnostic,
            classification=classification,
            report_type=FOCUSED_REPORT_TYPE,
            exact_command=f"just completion-focused {CURRENT_EVIDENCE_VALIDATION_ID}",
            focused_validation_id=CURRENT_EVIDENCE_VALIDATION_ID,
            runner_identity_result=final_runner_identity,
        )

    try:
        channel = _attest_runner_process(
            runtime, runner_identity, retain_channel=True
        )
        assert channel is not None
        repo_root, config = load_config(
            config_path, allow_test_config=allow_test_config
        )
        policy_id = str(config["policy_id"])
        configured = validation_configs(config, allow_test_config=allow_test_config)
        focused = config.get("focused_inventory_evidence")
        if (
            not isinstance(focused, dict)
            or focused.get("validation_ids") != list(CURRENT_EVIDENCE_VALIDATION_IDS)
        ):
            raise ProofError("inventory.current-evidence is not declared by policy")
        selected_by_id = {
            str(item["id"]): item for item in configured.values()
            if str(item["id"]) in CURRENT_EVIDENCE_VALIDATION_IDS
        }
        if tuple(selected_by_id) != CURRENT_EVIDENCE_VALIDATION_IDS:
            selected_by_id = {
                validation_id: next(
                    item for item in configured.values()
                    if str(item["id"]) == validation_id
                )
                for validation_id in CURRENT_EVIDENCE_VALIDATION_IDS
            }
        if tuple(str(selected_by_id[item]["runner"]) for item in CURRENT_EVIDENCE_VALIDATION_IDS) != (
            "python-unittest", "python-pytest"
        ):
            raise ProofError("inventory.current-evidence component runner mapping changed")
        active_platform = platform.system().casefold()
        if str(config.get("host_platform", "")).casefold() != active_platform:
            raise ProofError("current-evidence host does not match configured host")
        runtime_repo = Path(runtime["CODEX_COMPLETION_PROOF_REPOSITORY"]).resolve()
        if os.path.normcase(str(runtime_repo)) != os.path.normcase(str(repo_root)):
            raise ProofError("runtime repository does not match configured repository")
        try:
            report_path.relative_to(repo_root)
        except ValueError:
            pass
        else:
            raise ProofError("private current-evidence reports must be outside repository")
        lock_descriptor = _acquire_lock(
            _lock_path(report_path, repo_root),
            runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
        )
        observed_start = workspace_fingerprint(repo_root)
        if observed_start != runtime["CODEX_COMPLETION_PROOF_START_FINGERPRINT"]:
            raise ProofError("workspace changed before current-evidence startup")
        _, _, inventory_digest = load_frozen_inventory(repo_root, config)
        (
            unresolved_baseline_count,
            pending_replacement_review_count,
        ) = _current_reconciliation_status(repo_root, config)
        with tempfile.TemporaryDirectory(prefix="kd4-current-evidence-") as temp_name:
            temp_dir = Path(temp_name)
            jest_observation: dict[str, object] = {}
            current_rows, discovery_children = discover_inventory(
                repo_root,
                temp_dir=temp_dir,
                jest_observation=jest_observation,
            )
            discovery_cutoff = time.time_ns()
            raw_exec_reports: list[bytes] = []
            for validation_id in CURRENT_EVIDENCE_VALIDATION_IDS:
                validation, child = _run_current_evidence_validation(
                    repo_root=repo_root,
                    config=config,
                    item=selected_by_id[validation_id],
                    current_rows=current_rows,
                    temp_dir=temp_dir,
                    proof_attempt_id=runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
                    raw_report_bytes=raw_exec_reports,
                )
                validations.append(validation)
                children.append(child)
                _write_report(
                    report_path,
                    report("pre_result_error", "current-evidence attempt is incomplete"),
                )
            observed_end = workspace_fingerprint(repo_root)
            if observed_end != observed_start:
                raise ProofError("workspace changed during current-evidence attempt")
            processes, invocation_authority = _current_evidence_process_contract(
                discovery_children, temp_dir=temp_dir
            )
            attempt_bounds = {
                "attempt_id": runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
                "runner_pid": os.getpid(),
                "started_at": str(runner_identity.started_at),
                "reconciliation_started_at": str(discovery_cutoff),
                "ended_at": str(time.time_ns()),
            }
            jest_ids = sorted(
                str(row["baseline_id"])
                for row in current_rows
                if row["framework"] == "javascript-jest"
            )
            frozen_inventory_path, replacement_ledger_path = _manifest_paths(
                repo_root, config
            )
            successor_projection = _build_current_successor_projection(
                repo_root=repo_root,
                current_inventory=current_rows,
                frozen_inventory_path=frozen_inventory_path,
                replacement_ledger_path=replacement_ledger_path,
                unittest_collection_report=temp_dir / "unittest-collection.json",
                pytest_collection_report=temp_dir / "pytest-collection.json",
            )
            unresolved_projection = successor_projection["unresolved_projection"]
            catalog = _focused_catalog.build_focused_live_successor_catalog_v1(
                attempt_id=runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
                focused_validation_id=CURRENT_EVIDENCE_VALIDATION_ID,
                frozen_inventory_hash=inventory_digest,
                start_fingerprint=observed_start,
                start_mutation_epoch=int(runtime["CODEX_COMPLETION_PROOF_MUTATION_EPOCH"]),
                replacement_baseline_row_count=successor_projection[
                    "replacement_baseline_row_count"
                ],
                current_inventory=current_rows,
                resolved_successor_entries=successor_projection[
                    "resolved_successor_entries"
                ],
                execution_input_contracts=successor_projection[
                    "execution_input_contracts"
                ],
                cargo_target_context_specs=successor_projection[
                    "cargo_target_context_specs"
                ],
                replacement_successor_catalog=successor_projection[
                    "replacement_successor_catalog"
                ],
                successor_owner_map=successor_projection["successor_owner_map"],
                inventory_discovery_processes=processes,
                invocation_authority=invocation_authority,
                attempt_bounds=attempt_bounds,
                jest_observation=jest_observation,
                jest_discovered_ids=jest_ids,
            )
            frame_members = [
                ("catalog", _canonical_jcs(catalog)),
                ("process", _canonical_jcs(processes)),
                ("unittest_collect", (temp_dir / "unittest-collection.json").read_bytes()),
                ("unittest_exec", raw_exec_reports[0]),
                ("pytest_collect", (temp_dir / "pytest-collection.json").read_bytes()),
                ("pytest_exec", raw_exec_reports[1]),
            ]
            child_errors = _enforce_child_evidence_contract(validations, children)
            if child_errors:
                raise ProofError("; ".join(child_errors))
            attempt_classification = _attempt_classification(validations)
            final_runner_identity = runner_identity.finish()
            if final_runner_identity[1]:
                raise ProofError(final_runner_identity[1])
            _write_report(report_path, report(attempt_classification, ""))
            channel.exchange_focused_evidence(frame_members)
    except ProofError as error:
        fatal_error = str(error)
    except Exception as error:  # noqa: BLE001 - trusted runner fails closed
        fatal_error = f"unexpected current-evidence runner error: {type(error).__name__}: {error}"

    if fatal_error:
        known = {str(item.get("id", "")) for item in validations}
        for validation_id in CURRENT_EVIDENCE_VALIDATION_IDS:
            if validation_id not in known:
                validations.append(_focused_pre_result(validation_id, fatal_error))
        attempt_classification = "pre_result_error"
        if not observed_end and repo_root.exists():
            try:
                observed_end = workspace_fingerprint(repo_root)
            except ProofError:
                observed_end = ""
        if final_runner_identity is None:
            final_runner_identity = runner_identity.finish()
        _write_report(report_path, report(attempt_classification, fatal_error))
    else:
        attempt_classification = _attempt_classification(validations)
    if channel is not None:
        channel.close()
    if lock_descriptor is not None:
        _release_lock(lock_descriptor)
    authority_status = (
        f"authority ledger unresolved={unresolved_baseline_count}, "
        f"pending-replacement-review={pending_replacement_review_count}"
        if unresolved_baseline_count is not None
        and pending_replacement_review_count is not None
        else "authority ledger status unavailable"
    )
    if attempt_classification == "confirmed_pass":
        assert unresolved_projection is not None
        unresolved_replacement_baselines = unresolved_projection[
            "unresolved_replacement_baseline_row_count"
        ]
        unresolved_replacement_edges = unresolved_projection[
            "unresolved_replacement_edge_count"
        ]
        unresolved_successors = unresolved_projection["unresolved_successor_count"]
        status = (
            f"{authority_status}; "
            "fresh replacement projection unresolved-baselines="
            f"{unresolved_replacement_baselines}, "
            f"unresolved-edges={unresolved_replacement_edges}, "
            f"unresolved-successors={unresolved_successors}"
        )
        print(
            "CURRENT INVENTORY EVIDENCE CAPTURED: reconciliation was not performed; "
            f"{status}; this is not completion proof; report {report_path}",
            flush=True,
        )
        return 0
    print(
        f"CURRENT INVENTORY EVIDENCE {attempt_classification.upper()} "
        "(reconciliation was not performed; "
        f"{authority_status}; not completion proof): "
        f"{fatal_error or 'see structured report'}",
        file=sys.stderr,
        flush=True,
    )
    return 1 if attempt_classification == "confirmed_validation_failure" else 2


def _run_focused_attempt(
    config_path: Path,
    requested_id: str,
    *,
    allow_test_config: bool = False,
) -> int:
    if requested_id == CURRENT_EVIDENCE_VALIDATION_ID:
        return _run_current_evidence_attempt(
            config_path, allow_test_config=allow_test_config
        )
    runtime = _runtime_inputs()
    runner_identity = _capture_runner_process_identity()
    report_path = Path(runtime["CODEX_COMPLETION_PROOF_REPORT"]).resolve()
    _reserve_report(report_path)
    lock_descriptor: int | None = None
    validations: list[dict[str, object]] = []
    children: list[ChildProcess] = []
    inventory_digest = ""
    observed_start = ""
    observed_end = ""
    repo_root = REPO_ROOT.resolve()
    policy_id = KD4_POLICY_ID
    fatal_error = ""
    exact_command = f"just completion-focused {requested_id}"

    def focused_report(
        classification: str,
        diagnostic: str,
        *,
        runner_identity_result: tuple[dict[str, object], str | None] | None = None,
    ) -> dict[str, object]:
        return _attempt_report(
            runtime=runtime,
            policy_id=policy_id,
            runner_identity=runner_identity,
            repo_root=repo_root,
            inventory_digest=inventory_digest,
            observed_start=observed_start,
            observed_end=observed_end,
            validations=validations,
            children=children,
            exceptions=[],
            overrides=[],
            fatal_error=diagnostic,
            classification=classification,
            report_type=FOCUSED_REPORT_TYPE,
            exact_command=exact_command,
            focused_validation_id=requested_id,
            runner_identity_result=runner_identity_result,
        )

    try:
        _attest_runner_process(runtime, runner_identity)
        repo_root, config = load_config(
            config_path, allow_test_config=allow_test_config
        )
        policy_id = str(config["policy_id"])
        configured_validations = validation_configs(
            config,
            allow_test_config=allow_test_config,
        )
        configured_platform = str(config.get("host_platform", "")).casefold()
        active_platform = platform.system().casefold()
        if configured_platform != active_platform:
            raise ProofError(
                f"focused validation host {active_platform!r} does not match "
                f"configured host {configured_platform!r}"
            )
        runtime_repo = Path(runtime["CODEX_COMPLETION_PROOF_REPOSITORY"]).resolve()
        if os.path.normcase(str(runtime_repo)) != os.path.normcase(str(repo_root)):
            raise ProofError(
                f"runtime repository {runtime_repo} does not match configured repository "
                f"{repo_root}"
            )
        try:
            report_path.relative_to(repo_root)
        except ValueError:
            pass
        else:
            raise ProofError(
                "private focused-validation reports must be outside the repository"
            )
        if VALIDATION_ID_RE.fullmatch(requested_id) is None:
            raise ProofError(f"invalid focused validation ID {requested_id!r}")
        selected = [
            item
            for item in configured_validations.values()
            if str(item["id"]) == requested_id
        ]
        if len(selected) != 1:
            raise ProofError(f"unknown focused validation ID {requested_id!r}")

        requested_lock_path = _lock_path(report_path, repo_root)
        lock_descriptor = _acquire_lock(
            requested_lock_path,
            runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
        )
        observed_start = workspace_fingerprint(repo_root)
        if observed_start != runtime["CODEX_COMPLETION_PROOF_START_FINGERPRINT"]:
            raise ProofError(
                "workspace changed between runtime authorization and focused runner startup"
            )
        if selected[0]["runner"] == "inventory-reconciliation":
            _, _, inventory_digest = load_frozen_inventory(repo_root, config)
        else:
            # The focused envelope binds the trusted policy identifier; ordinary
            # tests do not need this migration's inventory or replacement ledger.
            inventory_digest = str(config["frozen_inventory_hash"])

        with tempfile.TemporaryDirectory(prefix="kd4-completion-focused-") as temp_name:
            validation, child = _run_focused_validation(
                repo_root=repo_root,
                config_path=config_path,
                config=config,
                configured_validations=configured_validations,
                item=selected[0],
                inventory_digest=inventory_digest,
                temp_dir=Path(temp_name),
                allow_test_config=allow_test_config,
                proof_attempt_id=runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
                proof_receipt_nonce=sha256_bytes(
                    runtime["CODEX_COMPLETION_PROOF_NONCE"].encode("utf-8")
                ),
            )
            validations.append(validation)
            children.append(child)
            _write_report(
                report_path,
                focused_report(
                    "pre_result_error",
                    "focused validation attempt is incomplete",
                ),
            )

        observed_end = workspace_fingerprint(repo_root)
        if observed_end != observed_start:
            raise ProofError("workspace changed during focused validation")
    except ProofError as error:
        fatal_error = str(error)
        if not validations:
            validations.append(_focused_pre_result(requested_id, fatal_error))
        if not observed_end and repo_root.exists():
            try:
                observed_end = workspace_fingerprint(repo_root)
            except ProofError:
                observed_end = ""
    except Exception as error:  # noqa: BLE001 - trusted runner fails closed
        fatal_error = (
            f"unexpected focused runner error: {type(error).__name__}: {error}"
        )
        if not validations:
            validations.append(_focused_pre_result(requested_id, fatal_error))
        if not observed_end and repo_root.exists():
            try:
                observed_end = workspace_fingerprint(repo_root)
            except ProofError:
                observed_end = ""

    child_evidence_errors = _enforce_child_evidence_contract(validations, children)
    if child_evidence_errors:
        fatal_error = "; ".join(
            part
            for part in (fatal_error, *child_evidence_errors)
            if part
        )
    attempt_classification = (
        "pre_result_error" if fatal_error else _attempt_classification(validations)
    )
    final_runner_identity = runner_identity.finish()
    if final_runner_identity[1]:
        attempt_classification = "pre_result_error"
    try:
        _write_report(
            report_path,
            focused_report(
                attempt_classification,
                fatal_error,
                runner_identity_result=final_runner_identity,
            ),
        )
    finally:
        if lock_descriptor is not None:
            _release_lock(lock_descriptor)
    if attempt_classification == "confirmed_pass":
        print(
            f"FOCUSED VALIDATION PASSED: {requested_id}, report {report_path}",
            flush=True,
        )
        return 0
    print(
        f"FOCUSED VALIDATION {attempt_classification.upper()}: "
        f"{fatal_error or 'see structured report'}",
        file=sys.stderr,
        flush=True,
    )
    return 1 if attempt_classification == "confirmed_validation_failure" else 2


def _run_attempt(
    config_path: Path,
    *,
    allow_test_config: bool = False,
) -> int:
    runtime = _runtime_inputs()
    runner_identity = _capture_runner_process_identity()
    report_path = Path(runtime["CODEX_COMPLETION_PROOF_REPORT"]).resolve()
    _reserve_report(report_path)
    lock_descriptor: int | None = None
    validations: list[dict[str, object]] = []
    children: list[ChildProcess] = []
    exceptions: list[dict[str, Any]] = []
    overrides: list[dict[str, Any]] = []
    inventory_digest = ""
    observed_start = ""
    observed_end = ""
    repo_root = REPO_ROOT.resolve()
    policy_id = KD4_POLICY_ID
    fatal_error = ""
    accepted_configured_validation_ids: frozenset[str] | None = None

    def checkpoint() -> None:
        _write_report(
            report_path,
            _attempt_report(
                runtime=runtime,
                policy_id=policy_id,
                runner_identity=runner_identity,
                repo_root=repo_root,
                inventory_digest=inventory_digest,
                observed_start=observed_start,
                observed_end=observed_end,
                validations=validations,
                children=children,
                exceptions=exceptions,
                overrides=overrides,
                fatal_error="canonical completion-proof attempt is incomplete",
                classification="pre_result_error",
            ),
        )

    def record(
        validation: dict[str, object], child: ChildProcess | None = None
    ) -> None:
        validations.append(validation)
        if child is not None:
            children.append(child)
        checkpoint()

    try:
        _attest_runner_process(runtime, runner_identity)
        repo_root, config = load_config(
            config_path, allow_test_config=allow_test_config
        )
        policy_id = str(config["policy_id"])
        configured_validations = validation_configs(
            config,
            allow_test_config=allow_test_config,
        )
        accepted_configured_validation_ids = frozenset(
            str(item["id"]) for item in configured_validations.values()
        )
        configured_platform = str(config.get("host_platform", "")).casefold()
        active_platform = platform.system().casefold()
        if configured_platform != active_platform:
            raise ProofError(
                f"canonical proof host {active_platform!r} does not match configured host "
                f"{configured_platform!r}"
            )
        runtime_repo = Path(runtime["CODEX_COMPLETION_PROOF_REPOSITORY"]).resolve()
        if os.path.normcase(str(runtime_repo)) != os.path.normcase(str(repo_root)):
            raise ProofError(
                f"runtime repository {runtime_repo} does not match configured repository {repo_root}"
            )
        try:
            report_path.relative_to(repo_root)
        except ValueError:
            pass
        else:
            raise ProofError(
                "private completion-proof reports must be outside the repository"
            )

        requested_lock_path = _lock_path(report_path, repo_root)
        lock_descriptor = _acquire_lock(
            requested_lock_path,
            runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
        )
        observed_start = workspace_fingerprint(repo_root)
        if observed_start != runtime["CODEX_COMPLETION_PROOF_START_FINGERPRINT"]:
            raise ProofError(
                "workspace changed between runtime authorization and canonical runner startup"
            )

        with tempfile.TemporaryDirectory(prefix="kd4-completion-proof-") as temp_name:
            temp_dir = Path(temp_name)
            _audit_test_system_surface(repo_root)
            testing_rows = _testing_inventory(
                repo_root,
                config,
                allow_test_config=allow_test_config,
            )
            if testing_rows is None:
                # Discovery is mandatory preparation, not one of the configured
                # validations whose child identity is certified in the artifact.
                current_rows, _ = discover_inventory(
                    repo_root,
                    temp_dir=temp_dir,
                )
            else:
                current_rows = testing_rows
            _, _, inventory_digest = load_frozen_inventory(repo_root, config)
            known_validation_ids = {
                str(item["id"]) for item in configured_validations.values()
            }
            reconciliation = reconcile_inventory(
                repo_root,
                config,
                current_rows,
                known_validation_ids=known_validation_ids,
            )
            exceptions = reconciliation.exceptions
            overrides = reconciliation.overrides
            if "inventory-reconciliation" in configured_validations:
                report, child = _run_inventory_reconciliation(
                    repo_root=repo_root,
                    config_path=config_path,
                    current_rows=current_rows,
                    known_validation_ids=known_validation_ids,
                    reconciliation=reconciliation,
                    frozen_inventory_hash=inventory_digest,
                    temp_dir=temp_dir,
                    env=_network_disabled_env(),
                    timeout_seconds=int(
                        configured_validations["inventory-reconciliation"][
                            "timeout_seconds"
                        ]
                    ),
                    allow_test_config=allow_test_config,
                    validation_config=configured_validations[
                        "inventory-reconciliation"
                    ],
                )
                record(report, child)

            framework_ids = reconciliation.required_by_framework
            supported_frameworks = {
                "rust-nextest",
                "rust-doctest",
                "python-unittest",
                "python-pytest",
                "javascript-jest",
                "argument-comment-lint-native",
                "windows-sandbox-smoke",
            }
            unknown_frameworks = sorted(set(framework_ids) - supported_frameworks)
            if unknown_frameworks and not allow_test_config:
                raise ProofError(
                    f"unknown required test frameworks: {unknown_frameworks}"
                )

            if "rust-nextest" in configured_validations:
                report, child = _run_rust_nextest(
                    repo_root,
                    framework_ids.get("rust-nextest", []),
                    _network_disabled_env(),
                    timeout_seconds=int(
                        configured_validations["rust-nextest"]["timeout_seconds"]
                    ),
                )
                record(report, child)
            if "rust-doctest" in configured_validations:
                report, child = _run_rust_doctests(
                    repo_root,
                    framework_ids.get("rust-doctest", []),
                    _network_disabled_env(),
                    timeout_seconds=int(
                        configured_validations["rust-doctest"]["timeout_seconds"]
                    ),
                )
                record(report, child)
            if "python-unittest" in configured_validations:
                structured_path = temp_dir / "root-unittest-results.json"
                report, child = _run_structured_wrapper(
                    validation_id="maintenance.root-unittest",
                    framework="python-unittest",
                    intended=framework_ids.get("python-unittest", []),
                    command=[
                        "uv",
                        "run",
                        "--offline",
                        "--frozen",
                        "--project",
                        "scripts",
                        "python",
                        str(repo_root / "scripts" / "completion_proof_unittest.py"),
                        "run",
                        "--expected-file",
                        "{expected_file}",
                        "--output",
                        "{report_file}",
                    ],
                    report_path=structured_path,
                    cwd=repo_root,
                    env=_network_disabled_env(),
                    timeout_seconds=int(
                        configured_validations["python-unittest"]["timeout_seconds"]
                    ),
                    temp_dir=temp_dir,
                    proof_attempt_id=runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
                    proof_scope="canonical",
                )
                record(report, child)
            if "python-pytest" in configured_validations:
                structured_path = temp_dir / "sdk-python-pytest-results.json"
                report, child = _run_structured_wrapper(
                    validation_id="sdk.python.pytest",
                    framework="python-pytest",
                    intended=framework_ids.get("python-pytest", []),
                    command=[
                        "uv",
                        "run",
                        "--offline",
                        "--frozen",
                        "--directory",
                        str(repo_root / "sdk" / "python"),
                        "--group",
                        "dev",
                        "python",
                        str(repo_root / "scripts" / "completion_proof_pytest.py"),
                        "run",
                        "--expected-file",
                        "{expected_file}",
                        "--output",
                        "{report_file}",
                    ],
                    report_path=structured_path,
                    cwd=repo_root,
                    env=_network_disabled_env(),
                    timeout_seconds=int(
                        configured_validations["python-pytest"]["timeout_seconds"]
                    ),
                    temp_dir=temp_dir,
                    proof_attempt_id=runtime["CODEX_COMPLETION_PROOF_ATTEMPT_ID"],
                    proof_scope="canonical",
                )
                record(report, child)
            if "javascript-jest" in configured_validations:
                report, child = _run_jest(
                    repo_root,
                    framework_ids.get("javascript-jest", []),
                    _network_disabled_env(),
                    temp_dir,
                    timeout_seconds=int(
                        configured_validations["javascript-jest"]["timeout_seconds"]
                    ),
                )
                record(report, child)
            for native_runner in (
                "argument-comment-lint-native",
                "windows-sandbox-smoke",
            ):
                if native_runner not in configured_validations:
                    continue
                report, child = _run_native_adapter(
                    repo_root,
                    native_runner,
                    framework_ids.get(native_runner, []),
                    _network_disabled_env(),
                    temp_dir,
                    timeout_seconds=int(
                        configured_validations[native_runner]["timeout_seconds"]
                    ),
                )
                record(report, child)

            rust_gate_validations = sorted(
                (
                    item
                    for runner, item in configured_validations.items()
                    if runner.startswith("rust-gate:")
                ),
                key=lambda item: str(item["id"]),
            )
            for item in rust_gate_validations:
                report, child = _run_rust_named_gate(
                    repo_root=repo_root,
                    config=item,
                    env=_network_disabled_env(),
                    temp_dir=temp_dir,
                )
                record(report, child)

            typed_validations = sorted(
                (
                    item
                    for runner, item in configured_validations.items()
                    if runner.startswith("typed-validation:")
                ),
                key=lambda item: str(item["id"]),
            )
            for item in typed_validations:
                report, child = _run_typed_validation(
                    repo_root,
                    item,
                    _network_disabled_env(),
                    allow_test_config=allow_test_config,
                    proof_attempt_id=runtime[
                        "CODEX_COMPLETION_PROOF_ATTEMPT_ID"
                    ],
                    proof_receipt_nonce=sha256_bytes(
                        runtime["CODEX_COMPLETION_PROOF_NONCE"].encode("utf-8")
                    ),
                    proof_scope="canonical",
                )
                record(report, child)

        observed_end = workspace_fingerprint(repo_root)
        if observed_end != observed_start:
            execution_id = str(uuid.uuid4())
            record(
                _validation_report(
                    validation_id="workspace.stability",
                    execution_id=execution_id,
                    runner="infrastructure",
                    evidence_kind="infrastructure",
                    classification="pre_result_error",
                    intended=["workspace-unchanged"],
                    selected=["workspace-unchanged"],
                    executed=[],
                    outcomes=[],
                    exit_code=None,
                    diagnostic="workspace changed during canonical completion proof",
                )
            )
    except ProofError as error:
        fatal_error = str(error)
        execution_id = str(uuid.uuid4())
        validations.append(
            _validation_report(
                validation_id="completion-proof.infrastructure",
                execution_id=execution_id,
                runner="infrastructure",
                evidence_kind="infrastructure",
                classification="pre_result_error",
                intended=["canonical-attempt"],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic=fatal_error,
            )
        )
        if not observed_end and repo_root.exists():
            try:
                observed_end = workspace_fingerprint(repo_root)
            except ProofError:
                observed_end = ""
    except Exception as error:  # noqa: BLE001 - unexpected runner faults are infrastructure errors
        fatal_error = (
            f"unexpected canonical runner error: {type(error).__name__}: {error}"
        )
        execution_id = str(uuid.uuid4())
        validations.append(
            _validation_report(
                validation_id="completion-proof.infrastructure",
                execution_id=execution_id,
                runner="infrastructure",
                evidence_kind="infrastructure",
                classification="pre_result_error",
                intended=["canonical-attempt"],
                selected=[],
                executed=[],
                outcomes=[],
                exit_code=None,
                diagnostic=fatal_error,
            )
        )
        if not observed_end and repo_root.exists():
            try:
                observed_end = workspace_fingerprint(repo_root)
            except ProofError:
                observed_end = ""
    child_evidence_errors = _enforce_child_evidence_contract(validations, children)
    if child_evidence_errors:
        fatal_error = "; ".join(
            part
            for part in (fatal_error, *child_evidence_errors)
            if part
        )
    final_id_errors = (
        _canonical_validation_id_closure(
            accepted_configured_validation_ids,
            validations,
        )
        if accepted_configured_validation_ids is not None
        else []
    )
    if final_id_errors:
        fatal_error = "; ".join(
            part for part in (fatal_error, *final_id_errors) if part
        )
    attempt_classification = (
        "pre_result_error"
        if child_evidence_errors or final_id_errors
        else _attempt_classification(validations)
    )
    final_runner_identity = runner_identity.finish()
    if final_runner_identity[1]:
        attempt_classification = "pre_result_error"
    try:
        report = _attempt_report(
            runtime=runtime,
            policy_id=policy_id,
            runner_identity=runner_identity,
            repo_root=repo_root,
            inventory_digest=inventory_digest,
            observed_start=observed_start,
            observed_end=observed_end,
            validations=validations,
            children=children,
            exceptions=exceptions,
            overrides=overrides,
            fatal_error=fatal_error,
            classification=attempt_classification,
            runner_identity_result=final_runner_identity,
        )
        _write_report(report_path, report)
    finally:
        if lock_descriptor is not None:
            _release_lock(lock_descriptor)
    if attempt_classification == "confirmed_pass":
        print(
            f"COMPLETION PROOF PASSED: {len(validations)} validations, report {report_path}",
            flush=True,
        )
        return 0
    print(
        f"COMPLETION PROOF {attempt_classification.upper()}: {fatal_error or 'see structured report'}",
        file=sys.stderr,
        flush=True,
    )
    return 1 if attempt_classification == "confirmed_validation_failure" else 2


def _exception_for_row(
    config: Mapping[str, Any],
    row: Mapping[str, object],
) -> dict[str, str] | None:
    raw_rules = config.get("baseline_exception", [])
    if not isinstance(raw_rules, list):
        raise ProofError("baseline_exception entries must be a list")
    matches: list[dict[str, str]] = []
    baseline_id = str(row["baseline_id"])
    source_path = str(row["source"])
    for raw_rule in raw_rules:
        if not isinstance(raw_rule, dict):
            raise ProofError("baseline_exception contains a non-object rule")
        id_prefix = raw_rule.get("id_prefix")
        source_prefix = raw_rule.get("source_prefix")
        if (id_prefix is None) == (source_prefix is None):
            raise ProofError(
                "each baseline_exception must set exactly one of id_prefix or source_prefix"
            )
        matched = (
            baseline_id.startswith(str(id_prefix))
            if id_prefix is not None
            else source_path.startswith(str(source_prefix))
        )
        if not matched:
            continue
        kind = str(raw_rule.get("kind", ""))
        source = str(raw_rule.get("source", ""))
        text = str(raw_rule.get("text", ""))
        if kind not in {
            "protected",
            "generated",
            "live-service",
            "off-host",
            "platform-pending",
        }:
            raise ProofError(f"invalid baseline exception kind {kind!r}")
        if not source.strip() or not text.strip():
            raise ProofError("baseline exception provenance requires source and text")
        matches.append({"kind": kind, "source": source, "text": text})
    if len(matches) > 1:
        raise ProofError(
            f"baseline test {baseline_id} matches multiple exception rules"
        )
    if not matches:
        return None
    exception = matches[0]
    _validate_exception_row_semantics(
        baseline_id=baseline_id,
        kind=exception["kind"],
        row=row,
        row_role="discovered",
    )
    return exception


def _write_new_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as error:
        raise ProofError(
            f"refusing to replace immutable inventory file {path}"
        ) from error
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output_file:
            json.dump(value, output_file, indent=2, sort_keys=True)
            output_file.write("\n")
            output_file.flush()
            os.fsync(output_file.fileno())
    except BaseException:
        try:
            path.unlink()
        except OSError:
            pass
        raise


def _freeze_inventory(
    config_path: Path,
    *,
    allow_test_config: bool = False,
) -> int:
    repo_root, config = load_config(config_path, allow_test_config=allow_test_config)
    validation_configs(config, allow_test_config=allow_test_config)
    frozen_path, ledger_path = _manifest_paths(repo_root, config)
    if frozen_path.exists() or ledger_path.exists():
        raise ProofError(
            "the frozen inventory or replacement ledger already exists; baseline identity is immutable"
        )
    configured_platform = str(config.get("host_platform", "")).casefold()
    active_platform = platform.system().casefold()
    if configured_platform != active_platform:
        raise ProofError(
            f"inventory host {active_platform!r} does not match configured host {configured_platform!r}"
        )
    baseline_fingerprint = workspace_fingerprint(repo_root)
    with tempfile.TemporaryDirectory(prefix="kd4-inventory-freeze-") as temp_name:
        rows, _ = discover_inventory(repo_root, temp_dir=Path(temp_name))
    digest = inventory_hash(rows)
    baseline_commit = (
        _git(repo_root, ["rev-parse", "--verify", "HEAD"]).decode().strip()
    )
    frozen = {
        "schema_version": 1,
        "baseline_commit": baseline_commit,
        "baseline_workspace_fingerprint": baseline_fingerprint,
        "host_platform": configured_platform,
        "inventory_hash": digest,
        "tests": rows,
    }
    ledger_rows = []
    for row in rows:
        baseline_id = str(row["baseline_id"])
        exception = _exception_for_row(config, row)
        if exception is None:
            ledger_rows.append({"baseline_id": baseline_id, "resolution": "unresolved"})
        else:
            ledger_rows.append(
                {
                    "baseline_id": baseline_id,
                    "resolution": "exception",
                    "provenance": exception,
                }
            )
    ledger = {
        "schema_version": 1,
        "frozen_inventory_hash": digest,
        "rows": ledger_rows,
        "overrides": [],
    }
    _write_new_json(frozen_path, frozen)
    try:
        _write_new_json(ledger_path, ledger)
    except BaseException:
        try:
            frozen_path.unlink()
        except OSError:
            pass
        raise
    print(
        f"froze {len(rows)} tests as inventory {digest}: {frozen_path}",
        flush=True,
    )
    return 0


def _check_inventory(
    config_path: Path,
    *,
    allow_test_config: bool = False,
) -> int:
    repo_root, config = load_config(config_path, allow_test_config=allow_test_config)
    validation_configs(config, allow_test_config=allow_test_config)
    with tempfile.TemporaryDirectory(prefix="kd4-inventory-check-") as temp_name:
        rows, _ = discover_inventory(repo_root, temp_dir=Path(temp_name))
    configured = validation_configs(config, allow_test_config=allow_test_config)
    result = reconcile_inventory(
        repo_root,
        config,
        rows,
        known_validation_ids={str(item["id"]) for item in configured.values()},
    )
    _, _, digest = load_frozen_inventory(repo_root, config)
    print(
        f"inventory reconciliation passed: current={len(result.current_ids)} frozen_hash={digest}",
        flush=True,
    )
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("run")
    focused = subparsers.add_parser("focused")
    focused.add_argument("validation_id")
    subparsers.add_parser("inventory-freeze")
    subparsers.add_parser("inventory-check")
    subparsers.add_parser("fingerprint")
    reconciliation_worker = subparsers.add_parser(
        "reconciliation-worker", help=argparse.SUPPRESS
    )
    reconciliation_worker.add_argument("--input", type=Path, required=True)
    return parser


def _dispatch(
    argv: Sequence[str] | None,
    *,
    allow_test_config: bool,
) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "run":
            return _run_attempt(
                args.config,
                allow_test_config=allow_test_config,
            )
        if args.command == "focused":
            return _run_focused_attempt(
                args.config,
                args.validation_id,
                allow_test_config=allow_test_config,
            )
        if args.command == "inventory-freeze":
            return _freeze_inventory(
                args.config,
                allow_test_config=allow_test_config,
            )
        if args.command == "inventory-check":
            return _check_inventory(
                args.config,
                allow_test_config=allow_test_config,
            )
        if args.command == "reconciliation-worker":
            return _reconciliation_worker(
                args.config,
                args.input,
                allow_test_config=allow_test_config,
            )
        if args.command == "fingerprint":
            repo_root, _ = load_config(args.config, allow_test_config=allow_test_config)
            print(workspace_fingerprint(repo_root))
            return 0
        raise AssertionError(args.command)
    except ProofError as error:
        print(f"completion-proof: {error}", file=sys.stderr)
        return 2


def _unittest_main(argv: Sequence[str] | None = None) -> int:
    """Code-level integration entrypoint; production CLI never enables fixtures."""
    return _dispatch(argv, allow_test_config=True)


def main(argv: Sequence[str] | None = None) -> int:
    return _dispatch(argv, allow_test_config=False)


if __name__ == "__main__":
    raise SystemExit(main())
