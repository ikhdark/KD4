"""Closed Python contract for focused live-successor catalog V1."""

from __future__ import annotations

import hashlib
import json
import ntpath
import os
import re
import unicodedata
import uuid
from collections.abc import Mapping, Sequence
from functools import wraps
from pathlib import Path
from typing import Any

try:
    from .completion_proof_canonical import CanonicalJcsError, canonical_jcs, proof_hash
    from .completion_proof_inventory_v2 import (
        InventoryV2ContractError,
        validate_cargo_target_context_spec_v1,
        validate_execution_input_contract_v1,
        validate_resolved_executable_entry_v1,
    )
except ImportError:  # pragma: no cover - direct script import compatibility
    from completion_proof_canonical import CanonicalJcsError, canonical_jcs, proof_hash
    from completion_proof_inventory_v2 import (
        InventoryV2ContractError,
        validate_cargo_target_context_spec_v1,
        validate_execution_input_contract_v1,
        validate_resolved_executable_entry_v1,
    )


class FocusedLiveSuccessorCatalogError(ValueError):
    """Raised when a focused live-successor catalog is not trustworthy."""


FORMAT_ID = "kd4.focused-live-successor-catalog.v1"
FOCUSED_VALIDATION_ID = "inventory.current-evidence"
MAX_SAFE_INTEGER = 2**53 - 1
MAX_U64 = 2**64 - 1
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
IDENTIFIER_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
U64_DECIMAL_RE = re.compile(r"^(0|[1-9][0-9]*)$")
UUID_V7_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)

TOP_LEVEL_FIELDS = (
    "format_id",
    "schema_version",
    "attempt_id",
    "focused_validation_id",
    "frozen_inventory_hash",
    "start_fingerprint",
    "start_mutation_epoch",
    "replacement_baseline_row_count",
    "distinct_successor_count",
    "successor_ids_sha256",
    "successor_owner_map_sha256",
    "current_inventory_count",
    "current_inventory_hash",
    "current_inventory",
    "resolved_successor_entries",
    "resolved_successor_entries_sha256",
    "execution_input_contracts",
    "cargo_target_context_specs",
    "replacement_successor_catalog",
    "in_process_jest_discovery",
    "inventory_discovery_processes_sha256",
    "semantic_sha256",
)

PROCESS_ROLES = (
    "inventory.rust-nextest",
    "inventory.rust-doctest",
    "inventory.root-unittest",
    "inventory.sdk-python-pytest",
    "inventory.tools.argument-comment-lint.native",
    "inventory.windows.sandbox-smoke",
)
REPORT_FILE_ROLES = {
    "inventory.root-unittest",
    "inventory.sdk-python-pytest",
}
FRAMEWORKS = {
    "argument-comment-lint-native",
    "javascript-jest",
    "python-pytest",
    "python-unittest",
    "rust-doctest",
    "rust-nextest",
    "windows-sandbox-smoke",
}
FRAMEWORK_ROUTE_SELECTOR = {
    "argument-comment-lint-native": (
        "test-route.argument-comment-lint-native.v1",
        "argument-comment-lint-native",
    ),
    "javascript-jest": ("test-route.javascript-jest.v1", "javascript-jest"),
    "python-pytest": ("test-route.python-pytest.v1", "python-pytest"),
    "python-unittest": ("test-route.python-unittest.v1", "python-unittest"),
    "rust-doctest": ("test-route.rust-doctest.v1", "rust-doctest"),
    "rust-nextest": ("test-route.rust-nextest.v1", "rust-nextest"),
    "windows-sandbox-smoke": (
        "test-route.windows-sandbox-smoke-native.v1",
        "windows-sandbox-smoke-native",
    ),
}


def _fail(message: str) -> None:
    raise FocusedLiveSuccessorCatalogError(message)


def _public_contract(function: Any) -> Any:
    @wraps(function)
    def wrapped(*args: Any, **kwargs: Any) -> Any:
        try:
            return function(*args, **kwargs)
        except FocusedLiveSuccessorCatalogError:
            raise
        except (
            CanonicalJcsError,
            InventoryV2ContractError,
            KeyError,
            OverflowError,
            TypeError,
            ValueError,
        ) as exc:
            raise FocusedLiveSuccessorCatalogError(str(exc)) from exc

    return wrapped


def _object(value: Any, fields: Sequence[str] | set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail(f"{label} must be an object")
    expected = set(fields)
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        _fail(f"{label} fields mismatch (missing={missing}, extra={extra})")
    return value


def _nfc_string(value: Any, label: str, *, nonempty: bool = True) -> str:
    if not isinstance(value, str) or (nonempty and not value):
        _fail(f"{label} must be {'a nonempty ' if nonempty else ''}string")
    if any(0xD800 <= ord(character) <= 0xDFFF for character in value):
        _fail(f"{label} contains a Unicode surrogate")
    if unicodedata.normalize("NFC", value) != value:
        _fail(f"{label} must be NFC")
    return value


def _sha256(value: Any, label: str) -> str:
    value = _nfc_string(value, label)
    if SHA256_RE.fullmatch(value) is None:
        _fail(f"{label} must be 64 lowercase hexadecimal characters")
    return value


def _identifier(value: Any, label: str) -> str:
    value = _nfc_string(value, label)
    if IDENTIFIER_RE.fullmatch(value) is None:
        _fail(f"{label} must be a strict identifier")
    return value


def _safe_uint(value: Any, label: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        _fail(f"{label} must be an integer")
    if not minimum <= value <= MAX_SAFE_INTEGER:
        _fail(f"{label} is outside its permitted range")
    return value


def _u64_decimal(value: Any, label: str) -> int:
    if not isinstance(value, str) or U64_DECIMAL_RE.fullmatch(value) is None:
        _fail(f"{label} must be a canonical unsigned decimal string")
    parsed = int(value)
    if parsed == 0 or parsed > MAX_U64:
        _fail(f"{label} exceeds unsigned 64-bit range")
    return parsed


def _uuid_v7(value: Any, label: str) -> str:
    if not isinstance(value, str) or len(value) != 36 or UUID_V7_RE.fullmatch(value) is None:
        _fail(f"{label} must be canonical lowercase RFC UUIDv7 text")
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError) as exc:
        raise FocusedLiveSuccessorCatalogError(f"{label} is not a UUID") from exc
    if str(parsed) != value or parsed.version != 7 or parsed.variant != uuid.RFC_4122:
        _fail(f"{label} must be canonical lowercase RFC UUIDv7 text")
    return value


def _repository_path(value: Any, label: str) -> str:
    value = _nfc_string(value, label)
    if (
        value.startswith(("/", "\\"))
        or "\\" in value
        or ":" in value
        or any(part in {"", ".", ".."} for part in value.split("/"))
    ):
        _fail(f"{label} must be a normalized repository-relative path")
    return value


def _windows_absolute_path(value: Any, label: str) -> str:
    value = _nfc_string(value, label)
    if any(ord(character) < 0x20 or ord(character) == 0x7F for character in value):
        _fail(f"{label} contains a control character")
    if "/" in value or not ntpath.isabs(value) or ntpath.normpath(value) != value:
        _fail(f"{label} must be an absolute Windows path")
    drive, tail = ntpath.splitdrive(value)
    if re.fullmatch(r"[A-Za-z]:", drive) is None or not tail.startswith("\\"):
        _fail(f"{label} must be an absolute Windows path")
    components = tail[1:].split("\\") if len(tail) > 1 else []
    reserved = re.compile(
        r"^(?:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9]|CONIN\$|CONOUT\$|CLOCK\$)(?:\..*)?$",
        re.IGNORECASE,
    )
    for component in components:
        if (
            not component
            or component.endswith((".", " "))
            or ":" in component
            or any(character in '<>"|?*' for character in component)
            or reserved.fullmatch(component) is not None
        ):
            _fail(f"{label} contains a non-canonical Windows path component")
    return value


def _windows_absolute_file_path(value: Any, label: str) -> str:
    value = _windows_absolute_path(value, label)
    _drive, tail = ntpath.splitdrive(value)
    if tail == "\\":
        _fail(f"{label} must identify a file below the drive root")
    return value


def _sorted_unique_strings(value: Any, label: str, *, nonempty: bool = False) -> list[str]:
    if not isinstance(value, list) or (nonempty and not value):
        _fail(f"{label} must be {'a nonempty ' if nonempty else 'an '}array")
    result = [_nfc_string(item, f"{label}[{index}]") for index, item in enumerate(value)]
    if result != sorted(set(result)):
        _fail(f"{label} must be sorted and unique")
    return result


def _sorted_unique_jcs(value: Any, label: str, *, nonempty: bool = False) -> list[Any]:
    if not isinstance(value, list) or (nonempty and not value):
        _fail(f"{label} must be {'a nonempty ' if nonempty else 'an '}array")
    encodings = [canonical_jcs(item) for item in value]
    if any(left >= right for left, right in zip(encodings, encodings[1:])):
        _fail(f"{label} must be JCS-sorted and unique")
    return value


def _raw_sha256(value: Any) -> str:
    return hashlib.sha256(canonical_jcs(value)).hexdigest()


def _file_sha256(path: str, label: str) -> str:
    try:
        hasher = hashlib.sha256()
        with Path(path).open("rb") as handle:
            while chunk := handle.read(1024 * 1024):
                hasher.update(chunk)
        return hasher.hexdigest()
    except OSError as exc:
        raise FocusedLiveSuccessorCatalogError(f"cannot read {label}") from exc


def _current_windows_file_identity(path: str, label: str) -> dict[str, str]:
    if os.name != "nt":
        _fail(f"{label} stable identity can only be checked on Windows")
    import ctypes
    from ctypes import wintypes

    class FILE_ID_128(ctypes.Structure):
        _fields_ = [("Identifier", ctypes.c_ubyte * 16)]

    class FILE_ID_INFO(ctypes.Structure):
        _fields_ = [("VolumeSerialNumber", ctypes.c_ulonglong), ("FileId", FILE_ID_128)]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    create_file = kernel32.CreateFileW
    create_file.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
        wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE,
    ]
    create_file.restype = wintypes.HANDLE
    handle = create_file(path, 0x80, 0x7, None, 3, 0x02000000, None)
    invalid_handle = wintypes.HANDLE(-1).value
    if handle == invalid_handle:
        _fail(f"cannot open {label} for stable identity")
    try:
        info = FILE_ID_INFO()
        get_info = kernel32.GetFileInformationByHandleEx
        get_info.argtypes = [wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD]
        get_info.restype = wintypes.BOOL
        if not get_info(handle, 18, ctypes.byref(info), ctypes.sizeof(info)):
            _fail(f"cannot query {label} stable identity")
        return {
            "kind": "windows-file-id-info-v1",
            "volume_serial_number_hex": f"{info.VolumeSerialNumber:016x}",
            "file_id_hex": bytes(info.FileId.Identifier).hex(),
        }
    finally:
        kernel32.CloseHandle(handle)


def _strict_json_loads(raw: bytes) -> Any:
    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as exc:
        raise FocusedLiveSuccessorCatalogError("catalog is not valid UTF-8") from exc

    def pairs_hook(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                _fail(f"duplicate JSON object key: {key!r}")
            result[key] = value
        return result

    def reject_constant(token: str) -> Any:
        _fail(f"non-finite JSON number is forbidden: {token}")

    try:
        return json.loads(
            text,
            object_pairs_hook=pairs_hook,
            parse_constant=reject_constant,
        )
    except FocusedLiveSuccessorCatalogError:
        raise
    except (json.JSONDecodeError, ValueError) as exc:
        raise FocusedLiveSuccessorCatalogError("catalog is not valid JSON") from exc


@_public_contract
def parse_focused_live_successor_catalog_v1(raw: bytes) -> dict[str, Any]:
    """Parse exact UTF-8 JCS bytes and validate the complete wire contract."""

    if not isinstance(raw, bytes):
        _fail("catalog raw input must be bytes")
    value = _strict_json_loads(raw)
    if not isinstance(value, dict):
        _fail("catalog root must be an object")
    try:
        encoded = canonical_jcs(value)
    except CanonicalJcsError as exc:
        raise FocusedLiveSuccessorCatalogError(str(exc)) from exc
    if raw != encoded:
        _fail("catalog bytes are not exact canonical JCS")
    validate_focused_live_successor_catalog_wire_v1(value)
    return value


def _validate_inventory_row(value: Any, index: int) -> None:
    row = _object(
        value,
        {"baseline_id", "framework", "native_id", "source", "ignored", "platforms"},
        f"current_inventory[{index}]",
    )
    for field in ("baseline_id", "framework", "native_id"):
        _nfc_string(row[field], f"current_inventory[{index}].{field}")
    if row["framework"] not in FRAMEWORKS:
        _fail(f"current_inventory[{index}].framework is unknown")
    _repository_path(row["source"], f"current_inventory[{index}].source")
    if not isinstance(row["ignored"], bool):
        _fail(f"current_inventory[{index}].ignored must be boolean")
    platforms = _sorted_unique_strings(
        row["platforms"], f"current_inventory[{index}].platforms", nonempty=True
    )
    if any(platform not in {"darwin", "linux", "windows"} for platform in platforms):
        _fail(f"current_inventory[{index}].platforms contains an unknown host")


def _validate_replacement_catalog(value: Any) -> list[str]:
    catalog = _object(
        value, {"format_id", "schema_version", "successors"}, "replacement_successor_catalog"
    )
    if catalog["format_id"] != "kd4.replacement-successor-catalog.v1":
        _fail("replacement successor catalog format mismatch")
    if isinstance(catalog["schema_version"], bool) or catalog["schema_version"] != 1:
        _fail("replacement successor catalog schema version mismatch")
    if not isinstance(catalog["successors"], list):
        _fail("replacement successor catalog successors must be an array")
    ids: list[str] = []
    fields = {
        "test_id", "framework", "native_id", "source_path", "test_route_id",
        "validation_id", "runner_selector_sha256", "executable_identity_sha256",
        "execution_input_contract_sha256", "platform_applicability_sha256",
    }
    for index, raw_successor in enumerate(catalog["successors"]):
        successor = _object(raw_successor, fields, f"replacement_successor_catalog.successors[{index}]")
        for field in ("test_id", "framework", "native_id"):
            _nfc_string(successor[field], f"replacement_successor_catalog.successors[{index}].{field}")
        for field in ("test_route_id", "validation_id"):
            _identifier(successor[field], f"replacement_successor_catalog.successors[{index}].{field}")
        _repository_path(successor["source_path"], f"replacement_successor_catalog.successors[{index}].source_path")
        for field in fields - {"test_id", "framework", "native_id", "source_path", "test_route_id", "validation_id"}:
            _sha256(successor[field], f"replacement_successor_catalog.successors[{index}].{field}")
        ids.append(successor["test_id"])
    if ids != sorted(set(ids)):
        _fail("replacement successor catalog must be sorted by unique test_id")
    return ids


def _validate_stable_file_identity(value: Any, label: str) -> None:
    identity = _object(
        value,
        {"kind", "volume_serial_number_hex", "file_id_hex"},
        label,
    )
    if identity["kind"] != "windows-file-id-info-v1":
        _fail(f"{label}.kind mismatch")
    volume = identity["volume_serial_number_hex"]
    file_id = identity["file_id_hex"]
    if not isinstance(volume, str) or re.fullmatch(r"[0-9a-f]{16}", volume) is None:
        _fail(f"{label}.volume_serial_number_hex must be 16 lowercase hexadecimal characters")
    if not isinstance(file_id, str) or re.fullmatch(r"[0-9a-f]{32}", file_id) is None:
        _fail(f"{label}.file_id_hex must be 32 lowercase hexadecimal characters")
    if file_id in {"0" * 32, "f" * 32}:
        _fail(f"{label}.file_id_hex uses a forbidden sentinel")


def _validate_process(
    process: Any,
    index: int,
    expected_role: str,
) -> None:
    label = f"inventory_discovery_processes[{index}]"
    process = _object(process, {"role", "child_process", "argv", "cwd", "output"}, label)
    if process["role"] != expected_role:
        _fail(f"{label}.role is out of order")
    argv = process["argv"]
    if not isinstance(argv, list) or not argv:
        _fail(f"{label}.argv must be a nonempty array")
    for argument_index, argument in enumerate(argv):
        _nfc_string(argument, f"{label}.argv[{argument_index}]", nonempty=False)
    cwd = _windows_absolute_path(process["cwd"], f"{label}.cwd")
    child = _object(
        process["child_process"],
        {
            "validation_id", "execution_id", "pid", "executable",
            "launch_target_identity", "args_hash", "started_at", "ended_at", "exit_code",
        },
        f"{label}.child_process",
    )
    _identifier(child["validation_id"], f"{label}.child_process.validation_id")
    if child["validation_id"] != expected_role:
        _fail(f"{label}.child_process.validation_id must equal role")
    execution_id = _nfc_string(child["execution_id"], f"{label}.child_process.execution_id")
    try:
        parsed_execution = uuid.UUID(execution_id)
    except ValueError as exc:
        raise FocusedLiveSuccessorCatalogError(f"{label}.child_process.execution_id is invalid") from exc
    if str(parsed_execution) != execution_id or parsed_execution.version != 4 or parsed_execution.variant != uuid.RFC_4122:
        _fail(f"{label}.child_process.execution_id must be canonical UUIDv4 text")
    if isinstance(child["pid"], bool) or not isinstance(child["pid"], int) or not 1 <= child["pid"] <= 2**32 - 1:
        _fail(f"{label}.child_process.pid must be a nonzero u32")
    executable = _nfc_string(child["executable"], f"{label}.child_process.executable")
    _windows_absolute_file_path(executable, f"{label}.child_process.executable")
    _sha256(child["args_hash"], f"{label}.child_process.args_hash")
    started = _u64_decimal(child["started_at"], f"{label}.child_process.started_at")
    ended = _u64_decimal(child["ended_at"], f"{label}.child_process.ended_at")
    if started > ended:
        _fail(f"{label}.child_process timestamps are reversed")
    if isinstance(child["exit_code"], bool) or not isinstance(child["exit_code"], int) or not -(2**31) <= child["exit_code"] <= 2**31 - 1:
        _fail(f"{label}.child_process.exit_code must be an i32")
    if child["exit_code"] != 0:
        _fail(f"{label}.child_process.exit_code must be zero")
    launch = _object(
        child["launch_target_identity"],
        {"requested", "resolved_path", "sha256_before", "sha256_after"},
        f"{label}.child_process.launch_target_identity",
    )
    _nfc_string(launch["requested"], f"{label}.child_process.launch_target_identity.requested")
    resolved = _windows_absolute_file_path(
        launch["resolved_path"],
        f"{label}.child_process.launch_target_identity.resolved_path",
    )
    before = _sha256(launch["sha256_before"], f"{label}.child_process.launch_target_identity.sha256_before")
    after = _sha256(launch["sha256_after"], f"{label}.child_process.launch_target_identity.sha256_after")
    if executable != resolved or before != after:
        _fail(f"{label}.child_process launch identity is not stable")
    if argv[0] != executable:
        _fail(f"{label}.argv[0] must equal the launched executable")
    if child["args_hash"] != _raw_sha256(argv):
        _fail(f"{label}.child_process.args_hash does not bind argv")

    if not isinstance(process["output"], dict):
        _fail(f"{label}.output must be an object")
    output = process["output"]
    if expected_role in REPORT_FILE_ROLES:
        output = _object(
            output,
            {"kind", "report_path", "report_identity", "report_sha256"},
            f"{label}.output",
        )
        if output["kind"] != "report-file":
            _fail(f"{label}.output kind mismatch")
        report_path = _windows_absolute_file_path(output["report_path"], f"{label}.output.report_path")
        _validate_stable_file_identity(output["report_identity"], f"{label}.output.report_identity")
        _sha256(output["report_sha256"], f"{label}.output.report_sha256")
        output_indexes = [position for position, argument in enumerate(argv) if argument == "--output"]
        if (
            len(output_indexes) != 1
            or output_indexes[0] + 1 >= len(argv)
            or argv[output_indexes[0] + 1] != report_path
        ):
            _fail(f"{label}.argv must bind exactly one --output to report_path")
    else:
        output = _object(output, {"kind", "stdout_sha256"}, f"{label}.output")
        if output["kind"] != "stdout":
            _fail(f"{label}.output kind mismatch")
        _sha256(output["stdout_sha256"], f"{label}.output.stdout_sha256")


def _validate_process_authority(
    process: Mapping[str, Any],
    index: int,
    expected_role: str,
    authority: Mapping[str, Any],
    attempt_started: int,
    attempt_ended: int,
    reconciliation_started: int,
) -> None:
    label = f"inventory_discovery_processes[{index}]"
    argv = process["argv"]
    cwd = process["cwd"]
    child = process["child_process"]
    executable = child["executable"]
    output = process["output"]
    started = _u64_decimal(child["started_at"], f"{label}.child_process.started_at")
    ended = _u64_decimal(child["ended_at"], f"{label}.child_process.ended_at")
    if started < attempt_started or ended > reconciliation_started or reconciliation_started > attempt_ended:
        _fail(f"{label}.child_process interval is outside the attempt bounds")
    before = child["launch_target_identity"]["sha256_before"]
    if _file_sha256(executable, f"{label}.child_process.executable") != before:
        _fail(f"{label}.child_process executable identity is stale")
    if output["kind"] == "report-file":
        report_path = output["report_path"]
        if _file_sha256(report_path, f"{label}.output.report_path") != output["report_sha256"]:
            _fail(f"{label}.output report hash is stale")
        if _current_windows_file_identity(report_path, f"{label}.output.report_path") != output["report_identity"]:
            _fail(f"{label}.output report stable identity is stale")

    authority = _object(
        dict(authority),
        {"role", "executable", "argv", "cwd", "output_kind", "report_path"},
        f"invocation_authority.expected_processes[{index}]",
    )
    expected_output_kind = "report-file" if expected_role in REPORT_FILE_ROLES else "stdout"
    if authority["role"] != expected_role or authority["output_kind"] != expected_output_kind:
        _fail(f"invocation authority process {index} role or output kind mismatch")
    expected_executable = _windows_absolute_file_path(authority["executable"], f"invocation_authority.expected_processes[{index}].executable")
    expected_cwd = _windows_absolute_path(authority["cwd"], f"invocation_authority.expected_processes[{index}].cwd")
    if not isinstance(authority["argv"], (list, tuple)) or not authority["argv"]:
        _fail(f"invocation_authority.expected_processes[{index}].argv must be nonempty")
    expected_argv = list(authority["argv"])
    for argument_index, argument in enumerate(expected_argv):
        _nfc_string(argument, f"invocation_authority.expected_processes[{index}].argv[{argument_index}]", nonempty=False)
    expected_report_path = authority["report_path"]
    if expected_output_kind == "stdout":
        if expected_report_path is not None:
            _fail(f"invocation_authority.expected_processes[{index}].report_path must be null")
    else:
        expected_report_path = _windows_absolute_file_path(expected_report_path, f"invocation_authority.expected_processes[{index}].report_path")
    observed_report_path = output.get("report_path") if output["kind"] == "report-file" else None
    if (
        executable != expected_executable
        or argv != expected_argv
        or cwd != expected_cwd
        or output["kind"] != expected_output_kind
        or observed_report_path != expected_report_path
    ):
        _fail(f"{label} does not exactly match invocation authority")


@_public_contract
def validate_inventory_discovery_process_set_v1(
    processes: Any,
) -> None:
    """Validate the intrinsic exact six-process discovery evidence vector."""

    try:
        canonical_jcs(processes)
        if not isinstance(processes, list) or len(processes) != len(PROCESS_ROLES):
            _fail("inventory discovery process set must contain exactly six processes")
        execution_ids: list[str] = []
        for index, (process, role) in enumerate(zip(processes, PROCESS_ROLES)):
            _validate_process(process, index, role)
            execution_ids.append(process["child_process"]["execution_id"])
        if len(execution_ids) != len(set(execution_ids)):
            _fail("inventory discovery execution IDs must be unique")
    except (CanonicalJcsError, InventoryV2ContractError) as exc:
        raise FocusedLiveSuccessorCatalogError(str(exc)) from exc


@_public_contract
def validate_inventory_discovery_process_authority_v1(
    processes: Any,
    *,
    invocation_authority: Mapping[str, Any],
    attempt_bounds: Mapping[str, Any],
) -> None:
    """Validate discovery evidence against trusted invocation and attempt authority."""

    validate_inventory_discovery_process_set_v1(processes)
    try:
        authority = _object(dict(invocation_authority), {"expected_processes"}, "invocation_authority")
        expected_processes = authority["expected_processes"]
        if not isinstance(expected_processes, (list, tuple)) or len(expected_processes) != len(PROCESS_ROLES):
            _fail("invocation_authority.expected_processes must contain six entries")
        bounds = _object(
            dict(attempt_bounds),
            {"attempt_id", "runner_pid", "started_at", "reconciliation_started_at", "ended_at"},
            "attempt_bounds",
        )
        _uuid_v7(bounds["attempt_id"], "attempt_bounds.attempt_id")
        if isinstance(bounds["runner_pid"], bool) or not isinstance(bounds["runner_pid"], int) or not 1 <= bounds["runner_pid"] <= 2**32 - 1:
            _fail("attempt_bounds.runner_pid must be a nonzero u32")
        attempt_started = _u64_decimal(bounds["started_at"], "attempt_bounds.started_at")
        attempt_ended = _u64_decimal(bounds["ended_at"], "attempt_bounds.ended_at")
        reconciliation_started = _u64_decimal(
            bounds["reconciliation_started_at"],
            "attempt_bounds.reconciliation_started_at",
        )
        if not attempt_started <= reconciliation_started <= attempt_ended:
            _fail("attempt_bounds timestamps are reversed")
        for index, (process, role) in enumerate(zip(processes, PROCESS_ROLES)):
            _validate_process_authority(
                process,
                index,
                role,
                expected_processes[index],
                attempt_started,
                attempt_ended,
                reconciliation_started,
            )
    except (CanonicalJcsError, InventoryV2ContractError) as exc:
        raise FocusedLiveSuccessorCatalogError(str(exc)) from exc


def _validate_jest(value: Any) -> None:
    value = _object(
        value,
        {"observation_id", "execution_id", "runner_pid", "started_at", "ended_at", "discovered_count", "discovered_test_ids_sha256"},
        "in_process_jest_discovery",
    )
    if value["observation_id"] != "inventory.sdk.typescript.jest":
        _fail("in-process Jest observation ID mismatch")
    execution_id = _nfc_string(value["execution_id"], "in_process_jest_discovery.execution_id")
    try:
        parsed = uuid.UUID(execution_id)
    except ValueError as exc:
        raise FocusedLiveSuccessorCatalogError("in-process Jest execution ID is invalid") from exc
    if str(parsed) != execution_id or parsed.version != 4 or parsed.variant != uuid.RFC_4122:
        _fail("in-process Jest execution ID must be canonical UUIDv4 text")
    if isinstance(value["runner_pid"], bool) or not isinstance(value["runner_pid"], int) or not 1 <= value["runner_pid"] <= 2**32 - 1:
        _fail("in-process Jest runner_pid must be a nonzero u32")
    started = _u64_decimal(value["started_at"], "in_process_jest_discovery.started_at")
    ended = _u64_decimal(value["ended_at"], "in_process_jest_discovery.ended_at")
    if started > ended:
        _fail("in-process Jest timestamps are reversed")
    _safe_uint(value["discovered_count"], "in_process_jest_discovery.discovered_count")
    _sha256(value["discovered_test_ids_sha256"], "in_process_jest_discovery.discovered_test_ids_sha256")


@_public_contract
def validate_focused_live_successor_catalog_wire_v1(value: Any) -> None:
    """Validate intrinsic V1 wire shape, ordering, and self-contained hashes."""

    try:
        value = _object(value, TOP_LEVEL_FIELDS, "FocusedLiveSuccessorCatalogV1")
        canonical_jcs(value)
        if value["format_id"] != FORMAT_ID or value["schema_version"] != 1 or isinstance(value["schema_version"], bool):
            _fail("catalog format or schema version mismatch")
        _uuid_v7(value["attempt_id"], "attempt_id")
        if value["focused_validation_id"] != FOCUSED_VALIDATION_ID:
            _fail("focused_validation_id mismatch")
        _sha256(value["frozen_inventory_hash"], "frozen_inventory_hash")
        _sha256(value["start_fingerprint"], "start_fingerprint")
        _safe_uint(value["start_mutation_epoch"], "start_mutation_epoch")
        _safe_uint(value["replacement_baseline_row_count"], "replacement_baseline_row_count")
        _safe_uint(value["distinct_successor_count"], "distinct_successor_count")
        _sha256(value["successor_ids_sha256"], "successor_ids_sha256")
        _sha256(value["successor_owner_map_sha256"], "successor_owner_map_sha256")
        _safe_uint(value["current_inventory_count"], "current_inventory_count")
        _sha256(value["current_inventory_hash"], "current_inventory_hash")

        current_inventory = value["current_inventory"]
        if not isinstance(current_inventory, list):
            _fail("current_inventory must be an array")
        for index, row in enumerate(current_inventory):
            _validate_inventory_row(row, index)
        inventory_ids = [row["baseline_id"] for row in current_inventory]
        if inventory_ids != sorted(set(inventory_ids)):
            _fail("current_inventory must be sorted by unique baseline_id")
        if value["current_inventory_count"] != len(current_inventory):
            _fail("current_inventory_count mismatch")
        if value["current_inventory_hash"] != _raw_sha256({"schema_version": 1, "tests": current_inventory}):
            _fail("current_inventory_hash mismatch")

        resolved = value["resolved_successor_entries"]
        if not isinstance(resolved, list):
            _fail("resolved_successor_entries must be an array")
        for entry in resolved:
            validate_resolved_executable_entry_v1(entry)
        resolved_keys = [canonical_jcs(entry) for entry in resolved]
        if any(left >= right for left, right in zip(resolved_keys, resolved_keys[1:])):
            _fail("resolved_successor_entries must be sorted by unique executable identity")
        if value["resolved_successor_entries_sha256"] != proof_hash("kd4.resolved-executable-entry-set.v1", resolved):
            _fail("resolved_successor_entries_sha256 mismatch")

        contracts = value["execution_input_contracts"]
        if not isinstance(contracts, list):
            _fail("execution_input_contracts must be an array")
        for contract in contracts:
            validate_execution_input_contract_v1(contract)
        contract_encodings = [canonical_jcs(contract) for contract in contracts]
        if any(left >= right for left, right in zip(contract_encodings, contract_encodings[1:])):
            _fail("execution_input_contracts must be JCS-sorted and unique")

        cargo_specs = value["cargo_target_context_specs"]
        if not isinstance(cargo_specs, list):
            _fail("cargo_target_context_specs must be an array")
        for spec in cargo_specs:
            validate_cargo_target_context_spec_v1(spec)
        context_hashes = [spec["context_sha256"] for spec in cargo_specs]
        if context_hashes != sorted(set(context_hashes)):
            _fail("cargo_target_context_specs must be sorted by unique context_sha256")

        successor_ids = _validate_replacement_catalog(value["replacement_successor_catalog"])
        if value["distinct_successor_count"] != len(successor_ids):
            _fail("distinct_successor_count mismatch")
        if value["successor_ids_sha256"] != proof_hash("kd4.focused-live-successor-id-set.v1", successor_ids):
            _fail("successor_ids_sha256 mismatch")
        _validate_jest(value["in_process_jest_discovery"])

        # The wire stores only the process-set digest; process evidence is supplied
        # independently to the builder and semantic validator.
        _sha256(value["inventory_discovery_processes_sha256"], "inventory_discovery_processes_sha256")
        semantic_projection = {field: value[field] for field in TOP_LEVEL_FIELDS[:-1]}
        if value["semantic_sha256"] != proof_hash("kd4.focused-live-successor-catalog.v1.semantic", semantic_projection):
            _fail("semantic_sha256 mismatch")
    except (CanonicalJcsError, InventoryV2ContractError) as exc:
        raise FocusedLiveSuccessorCatalogError(str(exc)) from exc


def _validate_successor_owner_map(value: Any) -> tuple[list[dict[str, Any]], list[str]]:
    if not isinstance(value, list):
        _fail("successor_owner_map must be an array")
    rows: list[dict[str, Any]] = []
    for index, raw_row in enumerate(value):
        row = _object(raw_row, {"successor_id", "baseline_ids"}, f"successor_owner_map[{index}]")
        successor_id = _nfc_string(row["successor_id"], f"successor_owner_map[{index}].successor_id")
        baseline_ids = _sorted_unique_strings(
            row["baseline_ids"], f"successor_owner_map[{index}].baseline_ids", nonempty=True
        )
        rows.append({"successor_id": successor_id, "baseline_ids": baseline_ids})
    successor_ids = [row["successor_id"] for row in rows]
    if successor_ids != sorted(set(successor_ids)):
        _fail("successor_owner_map must be sorted by unique successor_id")
    return rows, successor_ids


def _validate_catalog_resource_closure(
    current_inventory: list[dict[str, Any]],
    resolved_entries: list[dict[str, Any]],
    contracts: list[dict[str, Any]],
    contexts: list[dict[str, Any]],
    successor_catalog: dict[str, Any],
) -> None:
    successor_by_id = {
        successor["test_id"]: successor
        for successor in successor_catalog["successors"]
    }
    current_by_id = {row["baseline_id"]: row for row in current_inventory}
    resolved_by_id: dict[str, dict[str, Any]] = {}
    for resolved in resolved_entries:
        entry = resolved["inventory_entry"]
        identity = entry["executable_identity"]
        if identity.get("kind") != "test":
            _fail("resolved successor entry must carry a test identity")
        test_id = identity["test_id"]
        if test_id in resolved_by_id:
            _fail("resolved successor identities are duplicated")
        resolved_by_id[test_id] = entry
    if set(resolved_by_id) != set(successor_by_id):
        _fail("resolved entries do not cover the exact successor catalog")
    for test_id, successor in successor_by_id.items():
        entry = resolved_by_id[test_id]
        expected_bindings = {
            "test_route_id": entry["test_route_id"],
            "validation_id": entry["validation_id"],
            "runner_selector_sha256": entry["runner_selector_sha256"],
            "executable_identity_sha256": entry["executable_identity_sha256"],
            "execution_input_contract_sha256": entry["execution_input_contract_sha256"],
            "platform_applicability_sha256": entry["platform_applicability_sha256"],
        }
        if any(successor[field] != expected for field, expected in expected_bindings.items()):
            _fail(f"successor catalog bindings disagree with resolved entry: {test_id}")
        current = current_by_id.get(test_id)
        if current is None or current != {
            "baseline_id": test_id,
            "framework": successor["framework"],
            "native_id": successor["native_id"],
            "source": successor["source_path"],
            "ignored": current.get("ignored") if current is not None else False,
            "platforms": current.get("platforms") if current is not None else [],
        }:
            _fail(f"successor catalog identity disagrees with current inventory: {test_id}")
        expected_route, expected_selector_kind = FRAMEWORK_ROUTE_SELECTOR[current["framework"]]
        if (
            entry["test_route_id"] != expected_route
            or entry["runner_selector"]["kind"] != expected_selector_kind
        ):
            _fail(
                f"successor framework disagrees with resolved route or selector kind: {test_id}"
            )
    contract_hashes = {contract["contract_sha256"] for contract in contracts}
    referenced_contracts = {
        entry["execution_input_contract_sha256"] for entry in resolved_by_id.values()
    }
    if contract_hashes != referenced_contracts:
        _fail("execution input contracts are not the exact resolved resource set")
    context_hashes = {context["context_sha256"] for context in contexts}
    referenced_contexts = {
        digest
        for entry in resolved_by_id.values()
        if (digest := entry["cargo_target_context_spec_sha256"]) is not None
    }
    if context_hashes != referenced_contexts:
        _fail("Cargo target contexts are not the exact resolved resource set")


@_public_contract
def build_focused_live_successor_catalog_v1(
    *,
    attempt_id: str,
    focused_validation_id: str,
    frozen_inventory_hash: str,
    start_fingerprint: str,
    start_mutation_epoch: int,
    replacement_baseline_row_count: int,
    current_inventory: Sequence[Mapping[str, Any]],
    resolved_successor_entries: Sequence[Mapping[str, Any]],
    execution_input_contracts: Sequence[Mapping[str, Any]],
    cargo_target_context_specs: Sequence[Mapping[str, Any]],
    replacement_successor_catalog: Mapping[str, Any],
    successor_owner_map: Sequence[Mapping[str, Any]],
    inventory_discovery_processes: Sequence[Mapping[str, Any]],
    invocation_authority: Mapping[str, Any],
    attempt_bounds: Mapping[str, Any],
    jest_observation: Mapping[str, Any],
    jest_discovered_ids: Sequence[str],
) -> dict[str, Any]:
    """Build a catalog while deriving every count, projection, and digest."""

    if focused_validation_id != FOCUSED_VALIDATION_ID:
        _fail("focused_validation_id mismatch")
    owner_projection, owner_successor_ids = _validate_successor_owner_map(list(successor_owner_map))
    replacement_successor_catalog = dict(replacement_successor_catalog)
    catalog_successor_ids = _validate_replacement_catalog(replacement_successor_catalog)
    if owner_successor_ids != catalog_successor_ids:
        _fail("successor_owner_map does not cover the exact successor catalog ID set")
    baseline_union = {
        baseline_id
        for row in owner_projection
        for baseline_id in row["baseline_ids"]
    }
    _safe_uint(replacement_baseline_row_count, "replacement_baseline_row_count")
    if replacement_baseline_row_count != len(baseline_union):
        _fail("replacement_baseline_row_count does not match the owner-map baseline union")
    discovered_jest_ids = _sorted_unique_strings(
        list(jest_discovered_ids),
        "jest_discovered_ids",
    )
    jest = dict(jest_observation)
    jest["discovered_count"] = len(discovered_jest_ids)
    jest["discovered_test_ids_sha256"] = proof_hash(
        "kd4.in-process-jest-discovered-id-set.v1", discovered_jest_ids
    )
    inventory_discovery_processes = [dict(item) for item in inventory_discovery_processes]
    validate_inventory_discovery_process_authority_v1(
        inventory_discovery_processes,
        invocation_authority=invocation_authority,
        attempt_bounds=attempt_bounds,
    )

    bounds = dict(attempt_bounds)
    if attempt_id != bounds.get("attempt_id"):
        _fail("attempt_id does not match trusted attempt bounds")

    current_inventory = sorted((dict(row) for row in current_inventory), key=lambda row: row.get("baseline_id", ""))
    resolved_successor_entries = sorted(
        (dict(row) for row in resolved_successor_entries), key=canonical_jcs
    )
    execution_input_contracts = sorted((dict(row) for row in execution_input_contracts), key=canonical_jcs)
    cargo_target_context_specs = sorted((dict(row) for row in cargo_target_context_specs), key=lambda item: item.get("context_sha256", ""))
    _validate_catalog_resource_closure(
        current_inventory,
        resolved_successor_entries,
        execution_input_contracts,
        cargo_target_context_specs,
        replacement_successor_catalog,
    )
    _validate_jest(jest)
    attempt_started = _u64_decimal(bounds["started_at"], "attempt_bounds.started_at")
    attempt_ended = _u64_decimal(bounds["ended_at"], "attempt_bounds.ended_at")
    reconciliation_started = _u64_decimal(
        bounds["reconciliation_started_at"],
        "attempt_bounds.reconciliation_started_at",
    )
    if jest["runner_pid"] != bounds["runner_pid"]:
        _fail("Jest runner_pid does not match the attempt runner")
    jest_started = _u64_decimal(jest["started_at"], "in_process_jest_discovery.started_at")
    jest_ended = _u64_decimal(jest["ended_at"], "in_process_jest_discovery.ended_at")
    if jest_started < attempt_started or jest_ended > reconciliation_started or reconciliation_started > attempt_ended:
        _fail("Jest observation interval is outside the attempt bounds")
    process_execution_ids = {
        process["child_process"]["execution_id"] for process in inventory_discovery_processes
    }
    if jest["execution_id"] in process_execution_ids:
        _fail("Jest and child-process execution IDs must be unique")
    expected_jest_ids = sorted(
        row["baseline_id"]
        for row in current_inventory
        if row["framework"] == "javascript-jest"
    )
    if discovered_jest_ids != expected_jest_ids:
        _fail("Jest discovered IDs do not match current JavaScript inventory")
    catalog: dict[str, Any] = {
        "format_id": FORMAT_ID,
        "schema_version": 1,
        "attempt_id": attempt_id,
        "focused_validation_id": focused_validation_id,
        "frozen_inventory_hash": frozen_inventory_hash,
        "start_fingerprint": start_fingerprint,
        "start_mutation_epoch": start_mutation_epoch,
        "replacement_baseline_row_count": replacement_baseline_row_count,
        "distinct_successor_count": len(catalog_successor_ids),
        "successor_ids_sha256": proof_hash("kd4.focused-live-successor-id-set.v1", catalog_successor_ids),
        "successor_owner_map_sha256": proof_hash("kd4.focused-live-successor-owner-map.v1", owner_projection),
        "current_inventory_count": len(current_inventory),
        "current_inventory_hash": _raw_sha256({"schema_version": 1, "tests": current_inventory}),
        "current_inventory": current_inventory,
        "resolved_successor_entries": resolved_successor_entries,
        "resolved_successor_entries_sha256": proof_hash("kd4.resolved-executable-entry-set.v1", resolved_successor_entries),
        "execution_input_contracts": execution_input_contracts,
        "cargo_target_context_specs": cargo_target_context_specs,
        "replacement_successor_catalog": replacement_successor_catalog,
        "in_process_jest_discovery": jest,
        "inventory_discovery_processes_sha256": proof_hash("kd4.inventory-discovery-process-set.v1", inventory_discovery_processes),
    }
    catalog["semantic_sha256"] = proof_hash(
        "kd4.focused-live-successor-catalog.v1.semantic", catalog
    )
    validate_focused_live_successor_catalog_wire_v1(catalog)
    return catalog


@_public_contract
def validate_focused_live_successor_catalog_semantics_v1(
    value: Mapping[str, Any],
    *,
    successor_owner_map: Sequence[Mapping[str, Any]],
    inventory_discovery_processes: Sequence[Mapping[str, Any]],
    invocation_authority: Mapping[str, Any],
    attempt_bounds: Mapping[str, Any],
    current_inventory: Sequence[Mapping[str, Any]],
    resolved_successor_entries: Sequence[Mapping[str, Any]],
    execution_input_contracts: Sequence[Mapping[str, Any]],
    cargo_target_context_specs: Sequence[Mapping[str, Any]],
    replacement_successor_catalog: Mapping[str, Any],
    jest_observation: Mapping[str, Any],
    jest_discovered_ids: Sequence[str],
    expected_replacement_baseline_row_count: int,
    expected_frozen_inventory_hash: str,
    expected_start_fingerprint: str,
    expected_start_mutation_epoch: int,
) -> None:
    """Close all catalog values over independently trusted runtime inputs."""

    validate_focused_live_successor_catalog_wire_v1(value)
    trusted_catalog_collections = {
        "current_inventory": list(current_inventory),
        "resolved_successor_entries": list(resolved_successor_entries),
        "execution_input_contracts": list(execution_input_contracts),
        "cargo_target_context_specs": list(cargo_target_context_specs),
    }
    for field, trusted_projection in trusted_catalog_collections.items():
        if canonical_jcs(trusted_projection) != canonical_jcs(value[field]):
            _fail(f"catalog {field} does not exactly match its trusted projection")
    if canonical_jcs(dict(replacement_successor_catalog)) != canonical_jcs(
        value["replacement_successor_catalog"]
    ):
        _fail("catalog replacement_successor_catalog does not exactly match its trusted projection")
    if canonical_jcs(dict(jest_observation)) != canonical_jcs(
        value["in_process_jest_discovery"]
    ):
        _fail("catalog in_process_jest_discovery does not exactly match its trusted observation")
    expected = build_focused_live_successor_catalog_v1(
        attempt_id=value["attempt_id"],
        focused_validation_id=value["focused_validation_id"],
        frozen_inventory_hash=expected_frozen_inventory_hash,
        start_fingerprint=expected_start_fingerprint,
        start_mutation_epoch=expected_start_mutation_epoch,
        replacement_baseline_row_count=expected_replacement_baseline_row_count,
        current_inventory=current_inventory,
        resolved_successor_entries=resolved_successor_entries,
        execution_input_contracts=execution_input_contracts,
        cargo_target_context_specs=cargo_target_context_specs,
        replacement_successor_catalog=replacement_successor_catalog,
        successor_owner_map=successor_owner_map,
        inventory_discovery_processes=inventory_discovery_processes,
        invocation_authority=invocation_authority,
        attempt_bounds=attempt_bounds,
        jest_observation=jest_observation,
        jest_discovered_ids=jest_discovered_ids,
    )
    if canonical_jcs(value) != canonical_jcs(expected):
        _fail("catalog does not match independently trusted semantic inputs")


__all__ = [
    "FocusedLiveSuccessorCatalogError",
    "parse_focused_live_successor_catalog_v1",
    "validate_focused_live_successor_catalog_wire_v1",
    "validate_inventory_discovery_process_set_v1",
    "validate_inventory_discovery_process_authority_v1",
    "build_focused_live_successor_catalog_v1",
    "validate_focused_live_successor_catalog_semantics_v1",
]
