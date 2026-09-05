#!/usr/bin/env python3
"""Instrument unittest parents and their successful subtest occurrences.

The private worker emits non-authoritative execution observations. The public
controller requires the exact approved five-source and 29-parent exceptions before recapturing
859 authenticated frozen bodies. All 893 frozen identities are retained; the
excepted 34 and 16 hidden-at-freeze replacements never become executions.
"""

from __future__ import annotations

import argparse
import ast
import base64
from collections import ChainMap
from contextlib import contextmanager
import dis
import hashlib
import inspect
import ipaddress
import json
import math
import os
from pathlib import Path, PurePosixPath
import shutil
import socket
import subprocess
import struct
import sys
import sysconfig
import tempfile
from typing import Any, Iterable, Iterator
import unicodedata
import unittest
import uuid


BASELINE_COMMIT = "60bb133fa0a4f25e83851ab16d8c462e5f42ff95"
BASELINE_TREE_OBJECT = "c2d4bf589a595f6d451c50333317c675d7781958"
SOURCE_TREE_SHA256 = "654591dd1ddda7a77312172ec7c70e60e80990590c7c74a7c3b08a445279d90e"
FROZEN_INVENTORY_RAW_SHA256 = "df230a7683f0f31f1aae4d3f7644af39cec67b09fadf8f3f1e6c60729d18196a"
REPOSITORY_IDENTITY_SHA256 = "f386e4786f3a61829ecdd61e764fa9d65eddbd08f2902745c6480bce448573cc"
EXPECTED_PARENT_COUNT = 893
REPORT_FORMAT_ID = "kd4.unittest-execution-report.v1"
PARENT_MANIFEST_FORMAT_ID = "kd4.unittest-parent-manifest.v1"
PARENT_RECORDS_SHA256 = "a46a941721c872655dcb1c4ca55c070b9f48008a451d0df283f2d69957c2dd07"
SOURCE_EXCEPTIONS_SHA256 = "63fd50f8f5838408cf19b9b18abdc5353cc52412dbf36a275eb77f31c8a452f9"
PROXY_ENVIRONMENT_NAMES = (
    "ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY",
    "all_proxy", "https_proxy", "http_proxy", "no_proxy",
)
EXPECTED_DIRTY_OVERLAY_GAP = (
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_executed_failure_is_confirmed_and_reported",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_existing_report_is_rejected_without_reusing_cached_content",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_real_cli_records_fresh_execution_for_each_attempt",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_unmapped_current_inventory_blocks_before_validation",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_zero_selection_is_a_pre_result_error",
)
_IJSON_MAX_INTEGER = (1 << 53) - 1


class RecaptureError(RuntimeError):
    """A fail-closed unittest recapture error."""


def _canonical_json(value: Any) -> bytes:
    try:
        return json.dumps(
            value,
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise RecaptureError(f"value is not canonical JSON: {exc}") from exc


def _sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _atomic_write(path: Path, raw: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(handle, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _strict_repository_path(value: str) -> str:
    if not isinstance(value, str) or not value or value != unicodedata.normalize("NFC", value):
        raise RecaptureError("repository path must be a nonempty NFC string")
    if "\\" in value or value.startswith("/") or value.endswith("/"):
        raise RecaptureError(f"invalid repository path: {value!r}")
    parts = PurePosixPath(value).parts
    if not parts or any(part in {"", ".", ".."} for part in parts):
        raise RecaptureError(f"invalid repository path: {value!r}")
    return value


def _relative_repository_path(path: Path, source_root: Path) -> str:
    root = source_root.resolve(strict=True)
    resolved = path.resolve(strict=False)
    try:
        relative = resolved.relative_to(root)
    except ValueError as exc:
        raise RecaptureError(f"path escapes source root: {path}") from exc
    value = relative.as_posix()
    return _strict_repository_path(value)


def _require_source_root_cwd(source_root: Path) -> str:
    execution_working_directory = Path.cwd().resolve(strict=True)
    if execution_working_directory != source_root:
        raise RecaptureError("worker current directory does not equal source root")
    return execution_working_directory.relative_to(source_root).as_posix()


def _project_parameter(value: Any, source_root: Path, active: set[int] | None = None) -> dict[str, Any]:
    """Project the deliberately small, lossless parameter value vocabulary."""

    active = set() if active is None else active
    value_type = type(value)
    if value is None:
        return {"kind": "null"}
    if value_type is bool:
        return {"kind": "boolean", "value": value}
    if value_type is int:
        if not -_IJSON_MAX_INTEGER <= value <= _IJSON_MAX_INTEGER:
            raise RecaptureError("integer parameter exceeds the exact I-JSON range")
        return {"kind": "integer", "value": value}
    if value_type is float:
        if not math.isfinite(value):
            raise RecaptureError("non-finite float parameter is unsupported")
        return {"kind": "float64", "bits": struct.pack(">d", value).hex()}
    if value_type is str:
        if value != unicodedata.normalize("NFC", value):
            raise RecaptureError("string parameter is not NFC")
        return {"kind": "string", "value": value}
    if value_type is bytes:
        encoded = base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")
        return {"base64url": encoded, "kind": "bytes"}
    if isinstance(value, Path):
        return {
            "kind": "repository-path",
            "value": _relative_repository_path(value, source_root),
        }

    if value_type not in {list, tuple, set, dict}:
        raise RecaptureError(f"unsupported subtest parameter type: {value_type.__module__}.{value_type.__qualname__}")
    identity = id(value)
    if identity in active:
        raise RecaptureError("recursive subtest parameter container is unsupported")
    active.add(identity)
    try:
        if value_type in {list, tuple}:
            kind = "list" if value_type is list else "tuple"
            return {
                "items": [_project_parameter(item, source_root, active) for item in value],
                "kind": kind,
            }
        if value_type is set:
            items = [_project_parameter(item, source_root, active) for item in value]
            items.sort(key=_canonical_json)
            encoded = [_canonical_json(item) for item in items]
            if len(encoded) != len(set(encoded)):
                raise RecaptureError("set parameter projection is not unique")
            return {"items": items, "kind": "set"}
        entries = [
            {
                "key": _project_parameter(key, source_root, active),
                "value": _project_parameter(item, source_root, active),
            }
            for key, item in value.items()
        ]
        entries.sort(key=_canonical_json)
        encoded = [_canonical_json(entry) for entry in entries]
        if len(encoded) != len(set(encoded)):
            raise RecaptureError("mapping parameter projection is not unique")
        return {"entries": entries, "kind": "mapping"}
    finally:
        active.remove(identity)


def _context_projection(message: Any, params: Any, source_root: Path) -> dict[str, Any]:
    if isinstance(params, ChainMap):
        flattened = dict(params)
    else:
        try:
            flattened = dict(params)
        except (TypeError, ValueError) as exc:
            raise RecaptureError("subtest parameters are not a mapping") from exc
    return {
        "items": [
            _project_parameter(message, source_root),
            _project_parameter(flattened, source_root),
        ],
        "kind": "tuple",
    }


def _proof_hash(domain: str, value: Any) -> str:
    return _sha256(domain.encode("ascii") + b"\0" + _canonical_json(value))


@contextmanager
def _retained_audit_workspace() -> Iterator[str]:
    # A failed test may deliberately leave a child alive. Retain this owned
    # checkout instead of deleting a partial tree or hiding the primary failure.
    name = tempfile.mkdtemp(prefix="kd4-unittest-baseline-audit-")
    try:
        yield name
    finally:
        print(f"Retained isolated baseline workspace: {name}", file=sys.stderr, flush=True)


def _load_manifest(path: Path, *, require_frozen: bool) -> tuple[bytes, str, list[dict[str, str]]]:
    try:
        raw = path.read_bytes()
    except OSError as exc:
        raise RecaptureError(f"cannot read parent manifest: {exc}") from exc
    raw_sha256 = _sha256(raw)
    try:
        document = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise RecaptureError("parent manifest is invalid JSON") from exc
    expected_fields = {
        "baseline_commit", "format_id", "frozen_inventory_raw_sha256",
        "parent_records", "schema_version", "source_tree_sha256",
    }
    if not isinstance(document, dict) or set(document) != expected_fields:
        raise RecaptureError("parent manifest has missing or unknown fields")
    if _canonical_json(document) != raw:
        raise RecaptureError("parent manifest is not exact canonical JSON")
    if document.get("format_id") != PARENT_MANIFEST_FORMAT_ID:
        raise RecaptureError("parent manifest format mismatch")
    if not isinstance(document.get("parent_records"), list):
        raise RecaptureError("parent manifest must contain a parent_records array")
    if require_frozen:
        if (
            document.get("schema_version") != 1
            or document["baseline_commit"] != BASELINE_COMMIT
            or document.get("source_tree_sha256") != SOURCE_TREE_SHA256
            or document.get("frozen_inventory_raw_sha256") != FROZEN_INVENTORY_RAW_SHA256
        ):
            raise RecaptureError("frozen V1 inventory authority mismatch")

    parents: list[dict[str, str]] = []
    full_records: list[dict[str, str]] = []
    for row in document["parent_records"]:
        if not isinstance(row, dict) or set(row) != {
            "baseline_id", "native_id", "predecessor_entry_sha256"
        }:
            raise RecaptureError("unittest parent record is malformed")
        baseline_id = row.get("baseline_id")
        native_id = row.get("native_id")
        predecessor = row.get("predecessor_entry_sha256")
        if (
            not isinstance(baseline_id, str)
            or not isinstance(native_id, str)
            or not isinstance(predecessor, str)
            or len(predecessor) != 64
            or any(char not in "0123456789abcdef" for char in predecessor)
        ):
            raise RecaptureError("unittest parent identity must be a string")
        if not baseline_id.endswith("python-unittest::" + native_id):
            raise RecaptureError("unittest baseline/native identity mismatch")
        parents.append({"baseline_id": baseline_id, "native_id": native_id})
        full_records.append(row)
    expected_count = EXPECTED_PARENT_COUNT if require_frozen else len(parents)
    if expected_count == 0 or len(parents) != expected_count:
        raise RecaptureError(f"unittest parent manifest must select exactly {expected_count} parents")
    native_ids = [item["native_id"] for item in parents]
    baseline_ids = [item["baseline_id"] for item in parents]
    if len(set(native_ids)) != len(native_ids) or len(set(baseline_ids)) != len(baseline_ids):
        raise RecaptureError("unittest parent manifest contains duplicates")
    if require_frozen and baseline_ids != sorted(baseline_ids):
        raise RecaptureError("frozen unittest parents are not canonically sorted")
    if require_frozen and _proof_hash(
        "kd4.unittest-recapture-parent-record-set.v1", full_records
    ) != PARENT_RECORDS_SHA256:
        raise RecaptureError(
            "parent manifest is not the exact frozen 893 executable freeze-parent set"
        )
    return raw, raw_sha256, parents


def _flatten_suite(suite: unittest.TestSuite) -> Iterator[unittest.TestCase]:
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from _flatten_suite(item)
        elif isinstance(item, unittest.TestCase):
            yield item
        else:
            raise RecaptureError(f"foreign unittest selection object: {type(item).__name__}")


def _is_subtest_call(node: ast.AST) -> bool:
    return isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "subTest"


class _SiteResolver:
    def __init__(self, source_root: Path, selected: dict[str, unittest.TestCase]):
        self.source_root = source_root.resolve(strict=True)
        self._trees: dict[Path, ast.Module] = {}
        self._calls: dict[Path, list[ast.Call]] = {}
        self._declared_by_parent: dict[str, list[dict[str, Any]]] = {}
        for native_id, test in selected.items():
            self._declared_by_parent[native_id] = self._declared_sites(test)

    def _tree(self, path: Path) -> ast.Module:
        tree = self._trees.get(path)
        if tree is None:
            try:
                source = path.read_text(encoding="utf-8")
                tree = ast.parse(source, filename=str(path))
            except (OSError, UnicodeError, SyntaxError) as exc:
                raise RecaptureError(f"cannot parse unittest source {path}: {exc}") from exc
            self._trees[path] = tree
            self._calls[path] = [node for node in ast.walk(tree) if _is_subtest_call(node)]
        return tree

    def _source_path(self, filename: str) -> Path:
        path = Path(filename).resolve(strict=True)
        _relative_repository_path(path, self.source_root)
        return path

    def _method_node(self, test: unittest.TestCase, path: Path) -> ast.AST:
        method = inspect.unwrap(getattr(test, test._testMethodName, None))
        code = getattr(method, "__code__", None)
        if code is None:
            raise RecaptureError(f"selected unittest is not a method: {test.id()}")
        candidates: list[ast.AST] = []
        for node in ast.walk(self._tree(path)):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            if node.name != test._testMethodName or node.end_lineno is None:
                continue
            earliest = min([node.lineno, *[item.lineno for item in node.decorator_list]])
            if earliest <= code.co_firstlineno <= node.end_lineno:
                candidates.append(node)
        if len(candidates) != 1:
            raise RecaptureError(f"cannot uniquely locate selected unittest method AST: {test.id()}")
        return candidates[0]

    def _declared_sites(self, test: unittest.TestCase) -> list[dict[str, Any]]:
        method = inspect.unwrap(getattr(test, test._testMethodName, None))
        filename = inspect.getsourcefile(method)
        if filename is None:
            raise RecaptureError(f"selected unittest method has no source file: {test.id()}")
        path = self._source_path(filename)
        method_node = self._method_node(test, path)
        relative = _relative_repository_path(path, self.source_root)
        sites = [
            {"column": node.col_offset + 1, "line": node.lineno, "path": relative}
            for node in ast.walk(method_node)
            if _is_subtest_call(node)
        ]
        unique = {(item["path"], item["line"], item["column"]): item for item in sites}
        if len(unique) != len(sites):
            raise RecaptureError(f"duplicate declared subtest AST site: {test.id()}")
        return sorted(sites, key=lambda item: (item["path"], item["line"], item["column"]))

    def manifest(self, parent_by_native: dict[str, str]) -> list[dict[str, Any]]:
        result = []
        for native_id, baseline_id in parent_by_native.items():
            for site in self._declared_by_parent[native_id]:
                result.append({"parent_baseline_id": baseline_id, **site})
        return sorted(
            result,
            key=lambda item: (
                item["parent_baseline_id"], item["path"], item["line"], item["column"]
            ),
        )

    def resolve(self, native_id: str, frame: Any) -> dict[str, Any]:
        path = self._source_path(frame.f_code.co_filename)
        relative = _relative_repository_path(path, self.source_root)
        instruction = None
        for item in dis.get_instructions(frame.f_code):
            if item.offset <= frame.f_lasti:
                instruction = item
            else:
                break
        line = frame.f_lineno
        column = None
        if instruction is not None and instruction.positions is not None:
            line = instruction.positions.lineno or line
            if instruction.positions.col_offset is not None:
                column = instruction.positions.col_offset

        candidates: list[ast.Call] = []
        for node in self._calls.get(path, []):
            if node.end_lineno is None or node.end_col_offset is None:
                continue
            if not node.lineno <= line <= node.end_lineno:
                continue
            if column is not None:
                if line == node.lineno and column < node.col_offset:
                    continue
                if line == node.end_lineno and column >= node.end_col_offset:
                    continue
            candidates.append(node)
        if len(candidates) != 1:
            raise RecaptureError(
                f"cannot uniquely resolve subtest call site for {native_id} at {relative}:{line}"
            )
        site = {
            "column": candidates[0].col_offset + 1,
            "line": candidates[0].lineno,
            "path": relative,
        }
        if site not in self._declared_by_parent.get(native_id, []):
            raise RecaptureError(f"subtest call is outside selected method body: {native_id}")
        return site


def _loopback_host(host: Any) -> bool:
    if isinstance(host, bytes):
        try:
            host = host.decode("ascii")
        except UnicodeDecodeError:
            return False
    if not isinstance(host, str):
        return False
    if host.casefold() == "localhost":
        return True
    try:
        return ipaddress.ip_address(host.split("%", 1)[0]).is_loopback
    except ValueError:
        return False


def _address_is_loopback(sock: socket.socket, address: Any) -> bool:
    if sock.family not in {socket.AF_INET, socket.AF_INET6}:
        return False
    return isinstance(address, tuple) and bool(address) and _loopback_host(address[0])


@contextmanager
def _loopback_socket_policy() -> Iterator[None]:
    originals = {
        "bind": socket.socket.bind,
        "connect": socket.socket.connect,
        "connect_ex": socket.socket.connect_ex,
    }
    original_sendto = socket.socket.sendto

    def guard(method_name: str):
        original = originals[method_name]

        def checked(sock: socket.socket, address: Any, *args: Any, **kwargs: Any):
            if not _address_is_loopback(sock, address):
                if method_name == "connect_ex":
                    raise RecaptureError(f"non-loopback socket {method_name} blocked")
                raise RecaptureError(f"non-loopback socket {method_name} blocked")
            return original(sock, address, *args, **kwargs)

        return checked

    for name in originals:
        setattr(socket.socket, name, guard(name))

    def checked_sendto(sock: socket.socket, data: Any, *args: Any, **kwargs: Any):
        address = args[-1] if args else kwargs.get("address")
        if not _address_is_loopback(sock, address):
            raise RecaptureError("non-loopback socket sendto blocked")
        return original_sendto(sock, data, *args, **kwargs)

    socket.socket.sendto = checked_sendto
    try:
        yield
    finally:
        for name, original in originals.items():
            setattr(socket.socket, name, original)
        socket.socket.sendto = original_sendto


class _ProofResult(unittest.TestResult):
    def __init__(
        self,
        expected: list[dict[str, str]],
        source_root: Path,
        resolver: _SiteResolver,
        active_subtests: dict[int, tuple[dict[str, Any], Any]],
    ) -> None:
        super().__init__()
        self.expected = expected
        self.source_root = source_root
        self.resolver = resolver
        self.active_subtests = active_subtests
        self.parent_by_native = {item["native_id"]: item["baseline_id"] for item in expected}
        self.started: list[str] = []
        self.terminal: list[str] = []
        self.outcomes: dict[str, tuple[str, str | None]] = {}
        self.occurrences: list[dict[str, Any]] = []
        self._ordinals: dict[tuple[str, str, int, int], int] = {}
        self.fatal: list[str] = []

    def _native(self, test: Any) -> str:
        if not isinstance(test, unittest.TestCase):
            raise RecaptureError(f"foreign unittest outcome object: {type(test).__name__}")
        native_id = test.id()
        if native_id not in self.parent_by_native:
            raise RecaptureError(f"foreign unittest outcome: {native_id}")
        return native_id

    def _outcome(self, test: Any, outcome: str, reason: str | None = None) -> None:
        native_id = self._native(test)
        if native_id in self.outcomes:
            self.fatal.append(f"duplicate terminal outcome: {native_id}")
            return
        self.outcomes[native_id] = (outcome, reason)
        if outcome != "passed":
            self.fatal.append(f"unittest parent {outcome}: {native_id}")

    def startTest(self, test: Any) -> None:  # noqa: N802 - unittest API
        native_id = self._native(test)
        if native_id in self.started:
            raise RecaptureError(f"duplicate unittest start: {native_id}")
        self.started.append(native_id)
        super().startTest(test)

    def stopTest(self, test: Any) -> None:  # noqa: N802 - unittest API
        native_id = self._native(test)
        if native_id in self.terminal:
            raise RecaptureError(f"duplicate unittest terminal: {native_id}")
        if native_id not in self.outcomes:
            self.fatal.append(f"missing unittest terminal outcome: {native_id}")
        self.terminal.append(native_id)
        super().stopTest(test)

    def addSuccess(self, test: Any) -> None:  # noqa: N802
        self._outcome(test, "passed")
        super().addSuccess(test)

    def addFailure(self, test: Any, err: Any) -> None:  # noqa: N802
        self._outcome(test, "failed")
        super().addFailure(test, err)

    def addError(self, test: Any, err: Any) -> None:  # noqa: N802
        try:
            self._outcome(test, "error")
        except RecaptureError as exc:
            self.fatal.append(str(exc))
        super().addError(test, err)

    def addSkip(self, test: Any, reason: str) -> None:  # noqa: N802
        self._outcome(test, "skipped", reason)
        super().addSkip(test, reason)

    def addExpectedFailure(self, test: Any, err: Any) -> None:  # noqa: N802
        self._outcome(test, "expected-failure")
        super().addExpectedFailure(test, err)

    def addUnexpectedSuccess(self, test: Any) -> None:  # noqa: N802
        self._outcome(test, "unexpected-success")
        super().addUnexpectedSuccess(test)

    def addSubTest(self, test: Any, subtest: Any, err: Any) -> None:  # noqa: N802
        native_id = self._native(test)
        metadata = self.active_subtests.get(id(subtest))
        if metadata is None:
            raise RecaptureError(f"subtest lacks an exact AST site: {native_id}")
        site, message = metadata
        if err is not None:
            self.fatal.append(f"subtest failure/error: {native_id}")
            super().addSubTest(test, subtest, err)
            return
        baseline_id = self.parent_by_native[native_id]
        key = (baseline_id, site["path"], site["line"], site["column"])
        ordinal = self._ordinals.get(key, 0)
        self._ordinals[key] = ordinal + 1
        try:
            projection = _context_projection(message, subtest.params, self.source_root)
        except RecaptureError as exc:
            self.fatal.append(str(exc))
            raise
        self.occurrences.append(
            {
                "canonical_context_projection": projection,
                "occurrence_ordinal": ordinal,
                "parent_baseline_id": baseline_id,
                "site": site,
            }
        )
        super().addSubTest(test, subtest, err)


@contextmanager
def _instrument_subtests(
    resolver: _SiteResolver,
    selected: dict[str, unittest.TestCase],
    active: dict[int, tuple[dict[str, Any], Any]],
) -> Iterator[None]:
    original = unittest.TestCase.subTest
    sentinel = unittest.case._subtest_msg_sentinel

    def instrumented(test_case: unittest.TestCase, msg: Any = sentinel, **params: Any):
        native_id = test_case.id()
        if native_id not in selected or selected[native_id] is not test_case:
            raise RecaptureError(f"foreign subtest parent: {native_id}")
        frame = inspect.currentframe()
        if frame is None or frame.f_back is None:
            raise RecaptureError("subtest caller frame is unavailable")
        site = resolver.resolve(native_id, frame.f_back)
        public_message = None if msg is sentinel else msg
        original_context = original(test_case, msg, **params)

        @contextmanager
        def observed() -> Iterator[None]:
            key = None
            try:
                with original_context:
                    subtest = test_case._subtest
                    if subtest is None:
                        raise RecaptureError("unittest did not create a subtest object")
                    key = id(subtest)
                    if key in active:
                        raise RecaptureError("duplicate active subtest identity")
                    active[key] = (site, public_message)
                    yield
            finally:
                if key is not None:
                    active.pop(key, None)

        return observed()

    unittest.TestCase.subTest = instrumented
    try:
        yield
    finally:
        unittest.TestCase.subTest = original


def _git_environment(empty_global_config: Path | None = None) -> dict[str, str]:
    environment = os.environ.copy()
    for name in ("GIT_CONFIG", "GIT_CONFIG_PARAMETERS", "GIT_DIR", "GIT_WORK_TREE"):
        environment.pop(name, None)
    for name in PROXY_ENVIRONMENT_NAMES:
        environment.pop(name, None)
    environment["GIT_ALLOW_PROTOCOL"] = "file"
    environment["GIT_CONFIG_NOSYSTEM"] = "1"
    environment["GIT_NO_REPLACE_OBJECTS"] = "1"
    environment["GIT_TERMINAL_PROMPT"] = "0"
    environment["GIT_OPTIONAL_LOCKS"] = "0"
    if empty_global_config is not None:
        environment["GIT_CONFIG_GLOBAL"] = str(empty_global_config)
    return environment


def _git(
    args: Iterable[str],
    *,
    environment: dict[str, str],
    hooks: Path | None = None,
    executable: str = "git",
    check: bool = True,
) -> subprocess.CompletedProcess[bytes]:
    command = [executable]
    if hooks is not None:
        command.extend(["-c", "core.autocrlf=false", "-c", f"core.hooksPath={hooks}"])
    command.extend(args)
    try:
        result = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
            check=False,
            timeout=600,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RecaptureError(f"cannot execute Git: {exc}") from exc
    if check and result.returncode != 0:
        detail = result.stderr.decode("utf-8", "replace").strip()
        raise RecaptureError(f"Git command failed ({result.returncode}): {detail}")
    return result


def _resolved_git_executable() -> str:
    candidate = shutil.which("git")
    if candidate is None:
        raise RecaptureError("Git executable is unavailable")
    try:
        return str(Path(candidate).resolve(strict=True))
    except OSError as exc:
        raise RecaptureError(f"cannot resolve Git executable: {exc}") from exc


def _require_no_proxy_environment() -> None:
    present = sorted(name for name in PROXY_ENVIRONMENT_NAMES if os.environ.get(name) is not None)
    if present:
        raise RecaptureError(
            "controller proxy environment must be cleared: " + ", ".join(present)
        )


def _require_local_repository_root(
    repo_root: Path,
    *,
    environment: dict[str, str],
    git_executable: str,
) -> None:
    # A UNC path would turn the supposedly local clone into network access.
    if repo_root.anchor.startswith("\\\\"):
        raise RecaptureError("controller repository root must not be a network path")
    top_level_raw = _git(
        ["-C", str(repo_root), "rev-parse", "--show-toplevel"],
        environment=environment,
        executable=git_executable,
    ).stdout
    try:
        top_level = Path(top_level_raw.decode("utf-8", "strict").strip()).resolve(strict=True)
    except (OSError, UnicodeError) as exc:
        raise RecaptureError("Git returned an invalid repository root") from exc
    if top_level != repo_root:
        raise RecaptureError("controller repository root is not the Git worktree root")


def _verify_frozen_repository(
    repo_root: Path,
    *,
    environment: dict[str, str],
    hooks: Path | None,
    git_executable: str,
) -> None:
    object_result = _git(
        ["-C", str(repo_root), "cat-file", "-t", BASELINE_COMMIT],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
        check=False,
    )
    if object_result.returncode != 0:
        raise RecaptureError(
            f"frozen baseline commit is unavailable: {BASELINE_COMMIT}"
        )
    object_type = object_result.stdout.decode("ascii", "strict").strip()
    if object_type != "commit":
        raise RecaptureError(
            f"frozen baseline object is not an exact commit: {BASELINE_COMMIT}"
        )
    commit_body = _git(
        ["-C", str(repo_root), "cat-file", "-p", BASELINE_COMMIT],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    ).stdout
    first_line = commit_body.partition(b"\n")[0]
    if not first_line.startswith(b"tree "):
        raise RecaptureError("frozen baseline commit has no exact tree header")
    try:
        tree_object = first_line.removeprefix(b"tree ").decode("ascii", "strict")
    except UnicodeError as exc:
        raise RecaptureError("frozen baseline commit has an invalid tree header") from exc
    if tree_object != BASELINE_TREE_OBJECT:
        raise RecaptureError(f"frozen baseline tree object mismatch: {tree_object}")
    actual_tree_sha256 = _sha256(
        _git(
            ["-C", str(repo_root), "ls-tree", "-r", "--full-tree", BASELINE_COMMIT],
            environment=environment,
            hooks=hooks,
            executable=git_executable,
        ).stdout
    )
    if actual_tree_sha256 != SOURCE_TREE_SHA256:
        raise RecaptureError(
            f"frozen baseline source-tree SHA-256 mismatch: {actual_tree_sha256}"
        )
    _git(
        [
            "-C", str(repo_root), "fsck", "--strict", "--no-reflogs",
            "--connectivity-only", BASELINE_COMMIT,
        ],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    )
    repository_identity = _proof_hash(
        "kd4.frozen-repository-identity.v1",
        {
            "baseline_commit": BASELINE_COMMIT,
            "inventory_raw_sha256": FROZEN_INVENTORY_RAW_SHA256,
        },
    )
    if repository_identity != REPOSITORY_IDENTITY_SHA256:
        raise RecaptureError("frozen repository identity mismatch")


def _git_object_directory(
    repo_root: Path,
    *,
    environment: dict[str, str],
    hooks: Path | None,
    git_executable: str,
) -> Path:
    raw = _git(
        [
            "-C", str(repo_root), "rev-parse", "--path-format=absolute",
            "--git-path", "objects",
        ],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    ).stdout
    try:
        return Path(raw.decode("utf-8", "strict").strip()).resolve(strict=True)
    except (OSError, UnicodeError) as exc:
        raise RecaptureError("Git returned an invalid object directory") from exc


def _verify_no_borrowed_or_hardlinked_objects(
    source_root: Path,
    checkout_root: Path,
    *,
    environment: dict[str, str],
    hooks: Path,
    git_executable: str,
) -> None:
    source_objects = _git_object_directory(
        source_root,
        environment=environment,
        hooks=None,
        git_executable=git_executable,
    )
    checkout_objects = _git_object_directory(
        checkout_root,
        environment=environment,
        hooks=hooks,
        git_executable=git_executable,
    )
    alternates = checkout_objects / "info" / "alternates"
    if alternates.exists() and alternates.read_bytes().strip():
        raise RecaptureError("isolated clone borrows a Git object store")

    compared = 0
    for checkout_object in checkout_objects.rglob("*"):
        if not checkout_object.is_file() or checkout_object == alternates:
            continue
        relative = checkout_object.relative_to(checkout_objects)
        source_object = source_objects / relative
        if not source_object.is_file():
            continue
        compared += 1
        try:
            same_file = os.path.samefile(source_object, checkout_object)
        except OSError as exc:
            raise RecaptureError(f"cannot compare cloned Git object isolation: {exc}") from exc
        if same_file:
            raise RecaptureError(f"isolated clone contains a hardlinked Git object: {relative}")
    if compared == 0:
        raise RecaptureError("isolated clone object independence could not be verified")


def _verify_clone_configuration(
    checkout_root: Path,
    *,
    environment: dict[str, str],
    hooks: Path,
    git_executable: str,
) -> None:
    autocrlf = _git(
        ["-C", str(checkout_root), "config", "--local", "--get", "core.autocrlf"],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    ).stdout.decode("utf-8", "strict").strip()
    if autocrlf != "false":
        raise RecaptureError("isolated clone core.autocrlf is not false")
    hooks_value = _git(
        ["-C", str(checkout_root), "config", "--local", "--get", "core.hooksPath"],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    ).stdout.decode("utf-8", "strict").strip()
    try:
        configured_hooks = Path(hooks_value).resolve(strict=True)
    except OSError as exc:
        raise RecaptureError("isolated clone has an invalid hooks path") from exc
    if configured_hooks != hooks.resolve(strict=True):
        raise RecaptureError("isolated clone hooks are not disabled")


def _audit_parent_source_definitions(
    source_root: Path,
    parents: list[dict[str, str]],
) -> tuple[str, ...]:
    parsed_modules: dict[str, ast.Module | None] = {}
    missing: list[str] = []
    for parent in parents:
        native_id = parent["native_id"]
        try:
            module_name, class_name, method_name = native_id.rsplit(".", 2)
        except ValueError as exc:
            raise RecaptureError(f"invalid frozen unittest native identity: {native_id}") from exc
        tree = parsed_modules.get(module_name)
        if module_name not in parsed_modules:
            relative = _strict_repository_path(module_name.replace(".", "/") + ".py")
            module_path = source_root / PurePosixPath(relative)
            if not module_path.is_file():
                tree = None
            else:
                try:
                    tree = ast.parse(
                        module_path.read_text(encoding="utf-8"), filename=relative
                    )
                except (OSError, UnicodeError, SyntaxError) as exc:
                    raise RecaptureError(f"cannot audit frozen unittest source {relative}: {exc}") from exc
            parsed_modules[module_name] = tree
        if tree is None:
            missing.append(native_id)
            continue
        classes = [
            node for node in ast.walk(tree)
            if isinstance(node, ast.ClassDef) and node.name == class_name
        ]
        methods = [] if len(classes) != 1 else [
            node for node in classes[0].body
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
            and node.name == method_name
        ]
        if len(methods) != 1:
            missing.append(native_id)
    return tuple(sorted(missing))


def _source_tree_sha256(
    source_root: Path,
    environment: dict[str, str],
    hooks: Path | None,
    git_executable: str = "git",
) -> str:
    result = _git(
        ["-C", str(source_root), "ls-tree", "-r", "--full-tree", "HEAD"],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    )
    return _sha256(result.stdout)


def _verify_checkout(
    source_root: Path,
    expected_head: str,
    expected_tree: str,
    environment: dict[str, str],
    hooks: Path | None,
    git_executable: str = "git",
) -> None:
    head = _git(
        ["-C", str(source_root), "rev-parse", "HEAD"],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    ).stdout.decode("ascii", "strict").strip()
    if head != expected_head:
        raise RecaptureError(f"checkout HEAD mismatch: {head}")
    status = _git(
        ["-C", str(source_root), "status", "--porcelain=v1", "--untracked-files=all"],
        environment=environment,
        hooks=hooks,
        executable=git_executable,
    ).stdout
    if status:
        raise RecaptureError("checkout is not clean")
    actual_tree = _source_tree_sha256(
        source_root, environment, hooks, git_executable
    )
    if actual_tree != expected_tree:
        raise RecaptureError(f"checkout source-tree SHA-256 mismatch: {actual_tree}")


def _load_exact_suite(parents: list[dict[str, str]]) -> tuple[unittest.TestSuite, list[unittest.TestCase]]:
    native_ids = [item["native_id"] for item in parents]
    loader = unittest.TestLoader()
    suite = loader.loadTestsFromNames(native_ids)
    if loader.errors:
        raise RecaptureError("unittest discovery errors: " + " | ".join(loader.errors))
    selected = list(_flatten_suite(suite))
    selected_ids = [test.id() for test in selected]
    if not selected_ids:
        raise RecaptureError("unittest selection is empty")
    if selected_ids != native_ids:
        raise RecaptureError("unittest selection is missing, foreign, reordered, or duplicated")
    if len(set(selected_ids)) != len(selected_ids):
        raise RecaptureError("unittest selection contains duplicate methods")
    for test in selected:
        if not test._testMethodName or test._testMethodName == "runTest":
            raise RecaptureError(f"unittest parent is not a test method: {test.id()}")
    return suite, selected


def _run_worker(args: argparse.Namespace) -> int:
    output = Path(args.output).resolve()
    source_root = Path(args.source_root).resolve(strict=True)
    observed_repository_relative_cwd = _require_source_root_cwd(source_root)
    sys.dont_write_bytecode = True
    manifest_path = Path(args.manifest).resolve(strict=True)
    worker_path = Path(__file__).resolve(strict=True)
    raw, manifest_sha256, parents = _load_manifest(manifest_path, require_frozen=args.require_frozen)
    manifest_document = json.loads(raw)
    if manifest_sha256 != args.manifest_sha256:
        raise RecaptureError("injected parent manifest SHA-256 mismatch")
    worker_sha256 = _sha256(worker_path.read_bytes())
    if worker_sha256 != args.worker_sha256:
        raise RecaptureError("injected worker SHA-256 mismatch")
    python_path = Path(sys.executable).resolve(strict=True)
    python_sha256 = _sha256(python_path.read_bytes())
    proxy_names = (
        "ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY",
        "all_proxy", "https_proxy", "http_proxy", "no_proxy",
    )
    proxy_environment = {name: os.environ.get(name) for name in proxy_names}
    if any(value is not None for value in proxy_environment.values()):
        raise RecaptureError("proxy environment was not cleared before worker execution")
    local_binding = os.environ.get("CODEX_NETWORK_ALLOW_LOCAL_BINDING")
    if local_binding != "1":
        raise RecaptureError("CODEX_NETWORK_ALLOW_LOCAL_BINDING must be exactly one")

    environment = _git_environment()
    _verify_checkout(source_root, args.expected_head, args.expected_tree, environment, None)
    if str(source_root) not in sys.path:
        sys.path.insert(0, str(source_root))
    with _loopback_socket_policy():
        suite, selected_tests = _load_exact_suite(parents)
        observed_repository_relative_cwd = _require_source_root_cwd(source_root)
        selected = {test.id(): test for test in selected_tests}
        resolver = _SiteResolver(source_root, selected)
        parent_by_native = {item["native_id"]: item["baseline_id"] for item in parents}
        sites = resolver.manifest(parent_by_native)
        active: dict[int, tuple[dict[str, Any], Any]] = {}
        result = _ProofResult(parents, source_root, resolver, active)
        with _instrument_subtests(resolver, selected, active):
            suite.run(result)
        observed_repository_relative_cwd = _require_source_root_cwd(source_root)
    for test, detail in [*result.errors, *result.failures]:
        print(f"RECAPTURE FAILURE: {test}\n{detail}", file=sys.stderr, flush=True)
    _atomic_write(output.with_suffix(".diagnostics.json"), _canonical_json({
        "format_id": "kd4.unittest-recapture-diagnostics.v1",
        "authoritative": False,
        "selected_native_ids": [parent["native_id"] for parent in parents],
        "started_native_ids": result.started,
        "terminal_native_ids": result.terminal,
        "outcomes": [{"native_id": native, "outcome": outcome, "skip_reason": reason} for native, (outcome, reason) in sorted(result.outcomes.items())],
        "fatal_errors": result.fatal,
    }))
    _verify_checkout(source_root, args.expected_head, args.expected_tree, environment, None)

    expected_ids = [item["native_id"] for item in parents]
    if result.fatal:
        raise RecaptureError("; ".join(result.fatal))
    if result.started != expected_ids or result.terminal != expected_ids:
        raise RecaptureError("unittest start/terminal coverage is incomplete or reordered")
    if set(result.outcomes) != set(expected_ids):
        raise RecaptureError("unittest terminal outcomes are incomplete")
    if any(value != ("passed", None) for value in result.outcomes.values()):
        raise RecaptureError("unittest run did not completely pass")
    declared_keys = {
        (site["parent_baseline_id"], site["path"], site["line"], site["column"])
        for site in sites
    }
    if any(
        (
            occurrence["parent_baseline_id"],
            occurrence["site"]["path"],
            occurrence["site"]["line"],
            occurrence["site"]["column"],
        ) not in declared_keys
        for occurrence in result.occurrences
    ):
        raise RecaptureError("subtest occurrence references an invalid AST site")

    parent_results = [
        {
            "parent_baseline_id": item["baseline_id"],
            "selected": True,
            "skip_reason": None,
            "started": True,
            "terminal_result": "passed",
        }
        for item in parents
    ]
    report_sites = []
    site_ids: dict[tuple[str, str, int, int], str] = {}
    for site in sites:
        site_id = "unittest-site." + _proof_hash(
            "kd4.unittest-recapture-source-site.v1", site
        )
        key = (
            site["parent_baseline_id"], site["path"], site["line"], site["column"]
        )
        site_ids[key] = site_id
        report_sites.append({**site, "declared_site_id": site_id})
    report_sites.sort(key=lambda item: item["declared_site_id"])
    report_occurrences = []
    for occurrence in result.occurrences:
        site = occurrence["site"]
        site_id = site_ids[
            (
                occurrence["parent_baseline_id"], site["path"],
                site["line"], site["column"],
            )
        ]
        report_occurrences.append(
            {
                "canonical_context_projection": occurrence["canonical_context_projection"],
                "declared_site_id": site_id,
                "occurrence_ordinal": occurrence["occurrence_ordinal"],
                "parent_baseline_id": occurrence["parent_baseline_id"],
            }
        )
    report_occurrences.sort(
        key=lambda item: (
            item["parent_baseline_id"], item["declared_site_id"],
            item["occurrence_ordinal"],
        )
    )
    output_binding_results = [
        {
            "parent_baseline_id": item["parent_baseline_id"],
            "parent_result_sha256": _proof_hash(
                "kd4.unittest-parent-result.v1", item
            ),
        }
        for item in parent_results
    ]
    report = {
        "format_id": REPORT_FORMAT_ID,
        "frozen_inventory_raw_sha256": manifest_document[
            "frozen_inventory_raw_sha256"
        ],
        "output_binding_results": output_binding_results,
        "parent_manifest_sha256": manifest_sha256,
        "parent_results": parent_results,
        "schema_version": 1,
        "selection": {
            "intended_count": len(expected_ids),
            "intended_native_ids": expected_ids,
            "selected_count": len(expected_ids),
            "selected_native_ids": expected_ids,
        },
        "source_site_manifest": report_sites,
        "subtest_occurrences": report_occurrences,
        "total_counts": {
            "selected_parent_count": len(parents),
            "started_parent_count": len(result.started),
            "subtest_occurrence_count": len(result.occurrences),
            "terminal_parent_count": len(result.terminal),
        },
        "untrusted_observations": {
            "socket_policy": "loopback-only",
            "checkout": {
                "clean_after": True,
                "clean_before": True,
                "execution_working_directory": observed_repository_relative_cwd,
                "head_commit": args.expected_head,
                "source_tree_sha256": args.expected_tree,
            },
            "environment": {
                "codex_network_allow_local_binding": local_binding,
                "proxy_environment": proxy_environment,
            },
            "process": {
                "command_argv": [str(python_path), str(worker_path), *sys.argv[1:]],
                "python_executable_path": str(python_path),
                "python_executable_sha256": python_sha256,
                "worker_sha256": worker_sha256,
            },
        },
    }
    _atomic_write(output, _canonical_json(report))
    return 0


def _approved_source_exceptions(path: str) -> dict[str, Any]:
    raw = Path(path).resolve(strict=True).read_bytes()
    if _sha256(raw) != SOURCE_EXCEPTIONS_SHA256:
        raise RecaptureError("source exceptions do not match the exact approved five-source and 29-parent amendments")
    return json.loads(raw)


def _audit_authenticated_subtest_sites(
    checkout_root: Path, parents: list[dict[str, str]], environment: dict[str, str],
    git_executable: str,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """Derive source evidence without importing or executing baseline code."""
    modules: dict[str, ast.Module] = {}
    sites = []
    for parent in parents:
        module, class_name, method_name = parent["native_id"].rsplit(".", 2)
        path = _strict_repository_path(module.replace(".", "/") + ".py")
        if path not in modules:
            modules[path] = ast.parse((checkout_root / path).read_bytes(), filename=path)
        classes = [node for node in ast.walk(modules[path]) if isinstance(node, ast.ClassDef) and node.name == class_name]
        methods = [] if len(classes) != 1 else [node for node in classes[0].body if isinstance(node, ast.FunctionDef) and node.name == method_name]
        if len(methods) != 1:
            raise RecaptureError("authenticated source audit cannot resolve selected method")
        for node in ast.walk(methods[0]):
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "subTest":
                projection = {"column": node.col_offset + 1, "line": node.lineno, "parent_baseline_id": parent["baseline_id"], "path": path}
                sites.append({**projection, "declared_site_id": "unittest-site." + _proof_hash("kd4.unittest-recapture-source-site.v1", projection)})
    sites.sort(key=lambda site: site["declared_site_id"])
    tracked = _git(["-C", str(checkout_root), "ls-tree", "-r", "--name-only", "HEAD"], environment=environment, executable=git_executable).stdout.decode("utf-8").splitlines()
    textual_count = ast_count = 0
    for path in tracked:
        if not path.endswith(".py"):
            continue
        source = (checkout_root / _strict_repository_path(path)).read_text(encoding="utf-8")
        if "subTest" not in source:
            continue
        textual_count += source.count(".subTest(")
        ast_count += sum(isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) and node.func.attr == "subTest" for node in ast.walk(ast.parse(source, filename=path)))
    if ast_count != 68 or ast_count - len(sites) != 9 or textual_count != 69:
        raise RecaptureError(f"authenticated source audit mismatch: {len(sites)} selected, {ast_count} AST, {textual_count} textual markers")
    audit = {
        "embedded_non_ast_marker_count": textual_count - ast_count,
        "executable_ast_site_count": len(sites),
        "excepted_ast_site_count": ast_count - len(sites),
        "source_file_count": len({site["path"] for site in sites}),
        "source_site_manifest_sha256": _proof_hash("kd4.unittest-recapture-source-site-manifest.v1", sites),
        "textual_marker_count": textual_count,
    }
    return sites, audit


def _run_approved_recapture(
    args: argparse.Namespace,
    checkout_root: Path,
    hooks: Path,
    environment: dict[str, str],
    git_executable: str,
    manifest_document: dict[str, Any],
    exceptions: dict[str, Any],
) -> int:
    # All worker-writable files live under the baseline's ignored target directory.
    # The controller retains raw observations even if execution or validation fails.
    from completion_proof_inventory_v2 import (
        UNITTEST_APPROVED_SOURCE_SITE_MANIFEST_SHA256,
        validate_unittest_recapture_packet_v1,
    )
    if Path(args.output).exists():
        raise RecaptureError("recapture output already exists; refusing to reuse or overwrite evidence")

    attempt_id = str(uuid.uuid4())
    evidence = Path(args.output).resolve().parent / ("unittest-attempt-" + attempt_id)
    evidence.mkdir(parents=True, exist_ok=False)
    work = checkout_root / "target" / ("unittest-recapture-" + attempt_id)
    work.mkdir(parents=True, exist_ok=False)
    worker_path = work / "worker.py"
    worker_path.write_bytes(Path(__file__).read_bytes())
    worker_sha256 = _sha256(worker_path.read_bytes())
    excepted = set(exceptions["baseline_ids"]) | set(exceptions["historical_execution_extension"]["baseline_ids"])
    executable_records = [
        row for row in manifest_document["parent_records"]
        if row["baseline_id"] not in excepted
    ]
    if len(executable_records) != 859:
        raise RecaptureError("approved exception must leave exactly 859 executable parents")
    audited_sites, source_audit = _audit_authenticated_subtest_sites(
        checkout_root, executable_records, environment, git_executable
    )
    if source_audit["source_site_manifest_sha256"] != UNITTEST_APPROVED_SOURCE_SITE_MANIFEST_SHA256:
        raise RecaptureError("authenticated source-site manifest differs from the reviewed baseline")
    _atomic_write(evidence / "source-audit.json", _canonical_json({"audit": source_audit, "sites": audited_sites}))
    worker_manifest = {**manifest_document, "parent_records": executable_records}
    manifest_raw = _canonical_json(worker_manifest)
    manifest_path = work / "parents.json"
    manifest_path.write_bytes(manifest_raw)
    report_path = work / "report.json"
    python_path = Path(sys.executable).resolve(strict=True)
    python_sha256 = _sha256(python_path.read_bytes())
    codex_path = Path(args.codex_executable).resolve(strict=True)
    codex_sha256 = _sha256(codex_path.read_bytes())
    command = [
        str(python_path), str(worker_path), "_worker",
        "--source-root", str(checkout_root), "--manifest", str(manifest_path),
        "--output", str(report_path), "--expected-head", BASELINE_COMMIT,
        "--expected-tree", SOURCE_TREE_SHA256,
        "--manifest-sha256", _sha256(manifest_raw),
        "--worker-sha256", worker_sha256,
    ]
    sandbox_command = [
        str(codex_path), "-c", 'windows.sandbox="elevated"',
        "sandbox", "-P", ":workspace", "-C", str(checkout_root),
        "--", *command,
    ]
    worker_environment = dict(environment)
    worker_environment["CODEX_NETWORK_ALLOW_LOCAL_BINDING"] = "1"
    worker_environment["PYTHONDONTWRITEBYTECODE"] = "1"
    worker_environment.pop("PYTHONPATH", None)
    worker_environment.pop("PYTHONHOME", None)
    _atomic_write(evidence / "parent-manifest.json", manifest_raw)
    _atomic_write(evidence / "invocation.json", _canonical_json({
        "attempt_id": attempt_id, "command_argv": sandbox_command,
        "source_provenance_exception": exceptions,
        "worker_sha256": worker_sha256, "codex_sha256": codex_sha256,
    }))
    print(f"Executing 859 authenticated parents; evidence: {evidence}", flush=True)
    with (evidence / "stdout.bin").open("wb") as stdout, (evidence / "stderr.bin").open("wb") as stderr:
        process = subprocess.Popen(
            sandbox_command, cwd=checkout_root, env=worker_environment,
            stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
        )
        try:
            returncode = process.wait(timeout=1800)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], stdin=subprocess.DEVNULL, stdout=stderr, stderr=stderr, timeout=30, check=False)
            else:
                process.kill()
            process.wait(timeout=30)
            _atomic_write(evidence / "exit.json", _canonical_json({"returncode": process.returncode, "timed_out": True}))
            raise RecaptureError(f"sandbox worker exceeded 1800 seconds; retained evidence: {evidence}")
    if report_path.is_file():
        _atomic_write(evidence / "worker-report.json", report_path.read_bytes())
    if report_path.with_suffix(".diagnostics.json").is_file():
        _atomic_write(evidence / "worker-diagnostics.json", report_path.with_suffix(".diagnostics.json").read_bytes())
    _atomic_write(evidence / "exit.json", _canonical_json({"returncode": returncode}))
    if returncode != 0:
        raise RecaptureError(f"sandbox worker exited {returncode}; retained evidence: {evidence}")
    _verify_checkout(checkout_root, BASELINE_COMMIT, SOURCE_TREE_SHA256, environment, None, git_executable)
    if (
        worker_sha256 != _sha256(worker_path.read_bytes())
        or python_sha256 != _sha256(python_path.read_bytes())
        or codex_sha256 != _sha256(codex_path.read_bytes())
        or manifest_raw != manifest_path.read_bytes()
    ):
        raise RecaptureError("recapture executable or manifest changed during execution")
    report_raw = report_path.read_bytes()
    report = json.loads(report_raw)
    if _canonical_json(report) != report_raw:
        raise RecaptureError("worker report is not canonical JSON")
    if report["source_site_manifest"] != audited_sites:
        raise RecaptureError("worker source sites differ from independent authenticated source audit")
    artifacts = {}
    for name, raw in {
        "parent_manifest": manifest_raw, "report": report_raw,
        "stdout": (evidence / "stdout.bin").read_bytes(),
        "stderr": (evidence / "stderr.bin").read_bytes(),
    }.items():
        artifacts[name] = {"base64": base64.b64encode(raw).decode("ascii"), "byte_count": len(raw), "sha256": _sha256(raw)}
    occurrences = report["subtest_occurrences"]
    parent_manifests = []
    for record in executable_records:
        parent = record["baseline_id"]
        selected_occurrences = [item for item in occurrences if item["parent_baseline_id"] == parent]
        projection = {
            "method_body_observed": True, "parent_baseline_id": parent,
            "site_ids": sorted({item["declared_site_id"] for item in selected_occurrences}),
            "subtest_occurrence_count": len(selected_occurrences),
        }
        parent_manifests.append({**projection, "manifest_sha256": _proof_hash(
            "kd4.unittest-subtest-manifest.v1", {"occurrences": selected_occurrences, **projection}
        )})
    version_raw = sys.version.encode("utf-8")
    sites = report["source_site_manifest"]
    packet = {
        "schema_version": 1, "format_id": "kd4.unittest-recapture.v1",
        "attempt_id": attempt_id, "baseline_commit": BASELINE_COMMIT,
        "frozen_inventory_raw_sha256": FROZEN_INVENTORY_RAW_SHA256,
        "repository_identity_sha256": REPOSITORY_IDENTITY_SHA256,
        "source_tree_sha256": SOURCE_TREE_SHA256,
        "source_provenance_exception": exceptions,
        "parent_records": manifest_document["parent_records"],
        "parent_results": report["parent_results"], "parent_manifests": parent_manifests,
        "subtest_occurrences": occurrences, "source_site_manifest": sites,
        "artifacts": artifacts,
        "output_bindings": [
            {**binding, **{f"{name}_sha256": artifacts[name]["sha256"] for name in ("report", "stderr", "stdout")}}
            for binding in report["output_binding_results"]
        ],
        "source_audit": source_audit,
        "source_isolation": {
            "kind": "detached-checkout", "clone_kind": "local-no-hardlinks-no-checkout",
            "clone_source_path": str(Path(args.repo_root).resolve()),
            "isolated_checkout_path": str(checkout_root),
            "clone_command": ["git", "clone", "--local", "--no-hardlinks", "--no-checkout", "--config", "core.autocrlf=false", "--config", f"core.hooksPath={hooks}", str(Path(args.repo_root).resolve()), str(checkout_root)],
            "checkout_command": ["git", "-C", str(checkout_root), "checkout", "--detach", "--force", BASELINE_COMMIT],
            "clone_config": {"core_autocrlf": False, "git_hooks_path": str(hooks), "hardlinks": False, "local": True, "no_checkout": True},
            "clean_before": True, "clean_after": True, "core_autocrlf": False,
            "git_hooks_disabled": True, "global_git_config_disabled": True,
            "system_git_config_disabled": True, "execution_working_directory": ".",
            "head_commit": BASELINE_COMMIT, "source_repository_identity_sha256": REPOSITORY_IDENTITY_SHA256,
            "source_tree_sha256": SOURCE_TREE_SHA256,
        },
        "python_identity": {
            "executable_path": str(python_path), "executable_sha256": python_sha256,
            "implementation": sys.implementation.name.replace("cpython", "CPython"),
            "major": sys.version_info.major, "minor": sys.version_info.minor, "micro": sys.version_info.micro,
            "soabi": sysconfig.get_config_var("SOABI"),
            "version_base64": base64.b64encode(version_raw).decode("ascii"), "version_sha256": _sha256(version_raw),
        },
        "worker_identity": {"command_argv": command, "environment_sha256": _sha256(_canonical_json(worker_environment)), "worker_id": "kd4.unittest-recapture.worker.v1", "worker_sha256": worker_sha256},
        "network_isolation": {
            "codex_executable_path": str(codex_path), "codex_executable_sha256": codex_sha256,
            "codex_network_allow_local_binding": "1", "command_argv": sandbox_command,
            "fail_closed": True, "kind": "codex-windows-sandbox", "profile": ":workspace",
            "proxy_environment": {name: None for name in PROXY_ENVIRONMENT_NAMES}, "sandbox_available": True,
        },
        "total_counts": {**report["total_counts"], "parent_record_count": 893, "method_body_child_count": 859, "recovered_child_count": 859 + len(occurrences)},
    }
    packet["receipt_sha256"] = _proof_hash("kd4.unittest-recapture-receipt.v1", packet)
    _atomic_write(evidence / "candidate-packet.json", _canonical_json(packet))
    validate_unittest_recapture_packet_v1(packet)
    _atomic_write(Path(args.output).resolve(), _canonical_json(packet))
    return 0


def _run_controller(args: argparse.Namespace) -> int:
    Path(args.output).resolve()
    repo_root = Path(args.repo_root).resolve(strict=True)
    manifest = Path(args.manifest).resolve(strict=True)
    manifest_raw, _, parents = _load_manifest(manifest, require_frozen=True)
    exceptions = _approved_source_exceptions(args.source_exceptions) if args.source_exceptions else None
    if exceptions is not None and not args.codex_executable:
        raise RecaptureError("approved recapture requires --codex-executable")
    if len(parents) != EXPECTED_PARENT_COUNT:
        raise RecaptureError(
            "authenticated exact 893 executable freeze-parent workspace unavailable: "
            "selection count mismatch"
        )
    _require_no_proxy_environment()
    git_executable = _resolved_git_executable()
    with _retained_audit_workspace() as name:
        audit_root = Path(name).resolve(strict=True)
        empty_global_config = audit_root / "empty-global-git-config"
        empty_global_config.write_bytes(b"")
        hooks = audit_root / "disabled-hooks"
        hooks.mkdir()
        checkout_root = audit_root / "checkout"
        environment = _git_environment(empty_global_config)

        _require_local_repository_root(
            repo_root,
            environment=environment,
            git_executable=git_executable,
        )
        _verify_frozen_repository(
            repo_root,
            environment=environment,
            hooks=None,
            git_executable=git_executable,
        )
        _git(
            [
                "clone", "--local", "--no-hardlinks", "--no-checkout",
                "--config", "core.autocrlf=false",
                "--config", f"core.hooksPath={hooks}",
                str(repo_root), str(checkout_root),
            ],
            environment=environment,
            executable=git_executable,
        )
        _git(
            [
                "-C", str(checkout_root), "checkout", "--detach", "--force",
                BASELINE_COMMIT,
            ],
            environment=environment,
            executable=git_executable,
        )
        _verify_clone_configuration(
            checkout_root,
            environment=environment,
            hooks=hooks,
            git_executable=git_executable,
        )
        _verify_no_borrowed_or_hardlinked_objects(
            repo_root,
            checkout_root,
            environment=environment,
            hooks=hooks,
            git_executable=git_executable,
        )
        _verify_checkout(
            checkout_root,
            BASELINE_COMMIT,
            SOURCE_TREE_SHA256,
            environment,
            None,
            git_executable,
        )

        _verify_frozen_repository(
            checkout_root,
            environment=environment,
            hooks=None,
            git_executable=git_executable,
        )
        missing = _audit_parent_source_definitions(checkout_root, parents)
        _verify_checkout(
            checkout_root,
            BASELINE_COMMIT,
            SOURCE_TREE_SHA256,
            environment,
            None,
            git_executable,
        )

        if exceptions is not None:
            if missing != EXPECTED_DIRTY_OVERLAY_GAP:
                raise RecaptureError("approved recapture source gap does not match the exact five exceptions")
            return _run_approved_recapture(
                args, checkout_root, hooks, environment, git_executable,
                json.loads(manifest_raw), exceptions,
            )

    if missing != EXPECTED_DIRTY_OVERLAY_GAP:
        raise RecaptureError(
            "authenticated clean baseline source audit did not reproduce the exact "
            f"dirty-overlay gap: {json.dumps(list(missing), separators=(',', ':'))}"
        )
    raise RecaptureError(
        "authenticated exact 893 executable freeze-parent workspace unavailable: "
        "clean baseline reconstruction contains 888 parents and has the exact "
        "five-parent dirty-overlay source gap; recapture worker was not launched: "
        + json.dumps(list(missing), separators=(",", ":"))
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    controller = commands.add_parser(
        "controller", help="validate frozen authority and fail closed until workspace audit completes"
    )
    controller.add_argument("--repo-root", required=True)
    controller.add_argument("--manifest", required=True)
    controller.add_argument("--output", required=True)
    controller.add_argument("--source-exceptions")
    controller.add_argument("--codex-executable")
    worker = commands.add_parser("_worker", help=argparse.SUPPRESS)
    worker.add_argument("--source-root", required=True)
    worker.add_argument("--manifest", required=True)
    worker.add_argument("--output", required=True)
    worker.add_argument("--expected-head", required=True)
    worker.add_argument("--expected-tree", required=True)
    worker.add_argument("--manifest-sha256", required=True)
    worker.add_argument("--worker-sha256", required=True)
    worker.add_argument("--require-frozen", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "controller":
            return _run_controller(args)
        return _run_worker(args)
    except (RecaptureError, OSError) as exc:
        print(f"unittest recapture failed: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
