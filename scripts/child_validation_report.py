#!/usr/bin/env python3
"""Private append-only evidence and one-shot typed-validation broker.

The completion-proof parent owns every launch choice.  The broker receives one
precommitted launch over authenticated IPC, executes it once, and reports the
same observation both over IPC and through the append-only journal.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import importlib.util
import json
import os
import pathlib
import re
import sys
import time
import uuid
from collections.abc import Mapping, Sequence
from multiprocessing.connection import Client
from typing import Final, Literal


_BOUNDED_PROCESS_MODULE_NAME = "_kd4_child_validation_bounded_process"
_BOUNDED_PROCESS_PATH = pathlib.Path(__file__).resolve().with_name(
    "bounded_process.py"
)
_bounded_process_module = sys.modules.get(_BOUNDED_PROCESS_MODULE_NAME)
if _bounded_process_module is None:
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
    _bounded_process_spec.loader.exec_module(_bounded_process_module)
run_bounded_process = _bounded_process_module.run_bounded_process


REPORT_TYPE: Final = "CompletionProofChildValidationJournalV1"
SCHEMA_VERSION: Final = 1
BROKER_PROTOCOL: Final = "CompletionProofTypedValidationBrokerV1"
BROKER_AUTHKEY_ENV: Final = "KD4_TYPED_VALIDATION_BROKER_AUTHKEY"
BROKER_MAX_REQUEST_BYTES: Final = 64 * 1024 * 1024
CLASSIFICATIONS: Final = frozenset(
    {"confirmed_pass", "confirmed_validation_failure", "pre_result_error"}
)
_HEX_64 = re.compile(r"^[0-9a-f]{64}$")
_IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:$@/+-]*$")
_ABSOLUTE_EXECUTABLE_PATH = re.compile(
    r"^(?:/|[A-Za-z]:[\\/]|\\\\[^\\/]+[\\/][^\\/]+(?:[\\/]|$))"
)
_COMMON_KEYS = frozenset(
    {
        "schema_version",
        "report_type",
        "record_type",
        "sequence",
        "previous_record_sha256",
        "record_sha256",
        "proof_attempt_id",
        "proof_execution_id",
        "proof_receipt_nonce",
        "proof_scope",
        "validation_id",
        "validation_type",
        "input_contract_digest",
        "emitted_at_unix_ns",
    }
)
_RECORD_KEYS = {
    "header": frozenset(
        {
            "producer",
            "intended_ids",
            "intended_count",
            "selected_ids",
            "selected_count",
        }
    ),
    "action_started": frozenset(
        {
            "action_id",
            "action_execution_id",
            "subject_count",
            "subjects_sha256",
            "process",
        }
    ),
    "action_result": frozenset(
        {
            "action_id",
            "action_execution_id",
            "subject_count",
            "subjects_sha256",
            "classification",
            "actually_executed",
            "result_code",
            "exit_code",
            "diagnostic",
            "ended_at_unix_ns",
            "process_executable_sha256_after",
        }
    ),
    "infrastructure_error": frozenset({"phase", "diagnostic"}),
    "seal": frozenset(
        {
            "intended_ids",
            "intended_count",
            "selected_ids",
            "selected_count",
            "started_ids",
            "started_count",
            "terminal_ids",
            "terminal_count",
            "executed_ids",
            "executed_count",
            "outcomes",
            "sealed_prefix_sha256",
            "producer_ended_at_unix_ns",
            "producer_executable_sha256_after",
        }
    ),
}


class JournalContractError(ValueError):
    """The journal is malformed, unbound, incomplete, or contradictory."""


def canonical_json(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def hash_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def hash_arguments(arguments: Sequence[str]) -> str:
    return sha256_bytes(canonical_json({"arguments": list(arguments)}))


def hash_subjects(subjects: Sequence[str]) -> str:
    return sha256_bytes(canonical_json({"subjects": list(subjects)}))


def record_sha256(record: Mapping[str, object]) -> str:
    unsigned = dict(record)
    unsigned.pop("record_sha256", None)
    return sha256_bytes(canonical_json(unsigned))


def _canonical_uuid(value: str, label: str) -> str:
    try:
        parsed = uuid.UUID(value)
    except (ValueError, AttributeError) as error:
        raise JournalContractError(f"{label} is not a UUID") from error
    canonical = str(parsed)
    if value != canonical:
        raise JournalContractError(f"{label} is not a canonical UUID")
    return canonical


def _identifier(value: object, label: str) -> str:
    if not isinstance(value, str) or not _IDENTIFIER.fullmatch(value):
        raise JournalContractError(f"{label} is not a valid identifier")
    return value


def _hex_digest(value: object, label: str) -> str:
    if not isinstance(value, str) or not _HEX_64.fullmatch(value):
        raise JournalContractError(f"{label} is not a lowercase SHA-256 digest")
    return value


def _positive_int(value: object, label: str, *, allow_zero: bool = False) -> int:
    minimum = 0 if allow_zero else 1
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        qualifier = "nonnegative" if allow_zero else "positive"
        raise JournalContractError(f"{label} is not a {qualifier} integer")
    return value


def _identifier_list(value: object, label: str, *, nonempty: bool) -> list[str]:
    if not isinstance(value, list):
        raise JournalContractError(f"{label} is not a list")
    normalized = [_identifier(item, f"{label} item") for item in value]
    if nonempty and not normalized:
        raise JournalContractError(f"{label} is empty")
    if len(normalized) != len(set(normalized)):
        raise JournalContractError(f"{label} contains duplicate IDs")
    return normalized


def _nonempty_string_list(value: object, label: str) -> list[str]:
    if not isinstance(value, list):
        raise JournalContractError(f"{label} is not a list")
    if not value or not all(isinstance(item, str) and item for item in value):
        raise JournalContractError(f"{label} contains an empty or non-string value")
    if len(value) != len(set(value)):
        raise JournalContractError(f"{label} contains duplicate values")
    return value


@dataclasses.dataclass(frozen=True)
class JournalInvocationBinding:
    proof_attempt_id: str
    proof_execution_id: str
    proof_receipt_nonce: str
    proof_scope: str
    validation_id: str
    validation_type: str
    input_contract_digest: str

    def __post_init__(self) -> None:
        _canonical_uuid(self.proof_attempt_id, "proof_attempt_id")
        _canonical_uuid(self.proof_execution_id, "proof_execution_id")
        _hex_digest(self.proof_receipt_nonce, "proof_receipt_nonce")
        _identifier(self.proof_scope, "proof_scope")
        _identifier(self.validation_id, "validation_id")
        _identifier(self.validation_type, "validation_type")
        _hex_digest(self.input_contract_digest, "input_contract_digest")

    def as_record_fields(self) -> dict[str, object]:
        return dataclasses.asdict(self)

    @classmethod
    def from_mapping(cls, raw: Mapping[str, object]) -> JournalInvocationBinding:
        expected = {field.name for field in dataclasses.fields(cls)}
        if set(raw) != expected:
            raise JournalContractError("invocation binding fields do not match")
        return cls(**{key: str(raw[key]) for key in expected})


@dataclasses.dataclass(frozen=True)
class ProcessIdentity:
    pid: int
    executable_path: str
    executable_sha256: str
    argv_sha256: str
    started_at_unix_ns: int

    def __post_init__(self) -> None:
        _positive_int(self.pid, "process pid")
        if not isinstance(self.executable_path, str) or not (
            _ABSOLUTE_EXECUTABLE_PATH.match(self.executable_path)
        ):
            raise JournalContractError("process executable path is not absolute")
        _hex_digest(self.executable_sha256, "process executable_sha256")
        _hex_digest(self.argv_sha256, "process argv_sha256")
        _positive_int(self.started_at_unix_ns, "process started_at_unix_ns")

    def as_record_fields(self) -> dict[str, object]:
        return dataclasses.asdict(self)

    @classmethod
    def current(
        cls,
        *,
        started_at_unix_ns: int,
        arguments: Sequence[str] | None = None,
    ) -> ProcessIdentity:
        executable = pathlib.Path(sys.executable).resolve()
        return cls(
            pid=os.getpid(),
            executable_path=str(executable),
            executable_sha256=hash_file(executable),
            argv_sha256=hash_arguments(sys.argv if arguments is None else arguments),
            started_at_unix_ns=started_at_unix_ns,
        )

    @classmethod
    def from_mapping(cls, raw: Mapping[str, object]) -> ProcessIdentity:
        expected = {field.name for field in dataclasses.fields(cls)}
        if set(raw) != expected:
            raise JournalContractError("process identity fields do not match")
        return cls(
            pid=_positive_int(raw["pid"], "process pid"),
            executable_path=str(raw["executable_path"]),
            executable_sha256=str(raw["executable_sha256"]),
            argv_sha256=str(raw["argv_sha256"]),
            started_at_unix_ns=_positive_int(
                raw["started_at_unix_ns"], "process started_at_unix_ns"
            ),
        )


@dataclasses.dataclass(frozen=True)
class ActionProcessObservation:
    """The parent's independent in-memory view of one inner action."""

    action_id: str
    action_execution_id: str
    subjects: tuple[str, ...]
    process: ProcessIdentity
    classification: Literal[
        "confirmed_pass", "confirmed_validation_failure", "pre_result_error"
    ]
    actually_executed: bool
    result_code: Literal["passed", "failed", "error"]
    exit_code: int | None
    diagnostic: str
    ended_at_unix_ns: int
    process_executable_sha256_after: str

    def __post_init__(self) -> None:
        _identifier(self.action_id, "action observation ID")
        _canonical_uuid(self.action_execution_id, "action observation execution ID")
        _nonempty_string_list(list(self.subjects), "action observation subjects")
        if not isinstance(self.process, ProcessIdentity):
            raise JournalContractError("action observation process is invalid")
        if self.classification not in CLASSIFICATIONS:
            raise JournalContractError("action observation classification is invalid")
        if not isinstance(self.actually_executed, bool):
            raise JournalContractError("action observation executed state is invalid")
        if self.classification != "pre_result_error" and not self.actually_executed:
            raise JournalContractError("confirmed action observation did not execute")
        expected_result = {
            "confirmed_pass": "passed",
            "confirmed_validation_failure": "failed",
            "pre_result_error": "error",
        }[self.classification]
        if self.result_code != expected_result:
            raise JournalContractError(
                "action observation result contradicts classification"
            )
        if self.exit_code is not None and (
            isinstance(self.exit_code, bool) or not isinstance(self.exit_code, int)
        ):
            raise JournalContractError("action observation exit code is invalid")
        if not isinstance(self.diagnostic, str):
            raise JournalContractError("action observation diagnostic is invalid")
        _positive_int(self.ended_at_unix_ns, "action observation ended_at_unix_ns")
        if self.ended_at_unix_ns < self.process.started_at_unix_ns:
            raise JournalContractError("action observation ended before it started")
        _hex_digest(
            self.process_executable_sha256_after,
            "action observation process_executable_sha256_after",
        )

    def started_record_fields(self) -> dict[str, object]:
        return {
            "action_id": self.action_id,
            "action_execution_id": self.action_execution_id,
            "subject_count": len(self.subjects),
            "subjects_sha256": hash_subjects(self.subjects),
            "process": self.process.as_record_fields(),
        }

    def result_record_fields(self) -> dict[str, object]:
        return {
            "action_id": self.action_id,
            "action_execution_id": self.action_execution_id,
            "subject_count": len(self.subjects),
            "subjects_sha256": hash_subjects(self.subjects),
            "classification": self.classification,
            "actually_executed": self.actually_executed,
            "result_code": self.result_code,
            "exit_code": self.exit_code,
            "diagnostic": self.diagnostic,
            "ended_at_unix_ns": self.ended_at_unix_ns,
            "process_executable_sha256_after": (
                self.process_executable_sha256_after
            ),
        }

    def as_ipc_fields(self) -> dict[str, object]:
        return {
            "action_id": self.action_id,
            "action_execution_id": self.action_execution_id,
            "subjects": list(self.subjects),
            "process": self.process.as_record_fields(),
            "classification": self.classification,
            "actually_executed": self.actually_executed,
            "result_code": self.result_code,
            "exit_code": self.exit_code,
            "diagnostic": self.diagnostic,
            "ended_at_unix_ns": self.ended_at_unix_ns,
            "process_executable_sha256_after": (
                self.process_executable_sha256_after
            ),
        }

    @classmethod
    def from_mapping(cls, raw: Mapping[str, object]) -> ActionProcessObservation:
        expected = {
            "action_id",
            "action_execution_id",
            "subjects",
            "process",
            "classification",
            "actually_executed",
            "result_code",
            "exit_code",
            "diagnostic",
            "ended_at_unix_ns",
            "process_executable_sha256_after",
        }
        if set(raw) != expected:
            raise JournalContractError("action observation fields do not match")
        process = raw["process"]
        if not isinstance(process, dict):
            raise JournalContractError("action observation process is not an object")
        subjects = _nonempty_string_list(
            raw["subjects"], "action observation subjects"
        )
        classification = raw["classification"]
        result_code = raw["result_code"]
        if not isinstance(classification, str) or not isinstance(result_code, str):
            raise JournalContractError("action observation result fields are invalid")
        exit_code = raw["exit_code"]
        if exit_code is not None and (
            isinstance(exit_code, bool) or not isinstance(exit_code, int)
        ):
            raise JournalContractError("action observation exit code is invalid")
        return cls(
            action_id=str(raw["action_id"]),
            action_execution_id=str(raw["action_execution_id"]),
            subjects=tuple(subjects),
            process=ProcessIdentity.from_mapping(process),
            classification=classification,  # type: ignore[arg-type]
            actually_executed=raw["actually_executed"],  # type: ignore[arg-type]
            result_code=result_code,  # type: ignore[arg-type]
            exit_code=exit_code,
            diagnostic=raw["diagnostic"],  # type: ignore[arg-type]
            ended_at_unix_ns=_positive_int(
                raw["ended_at_unix_ns"], "action observation ended_at_unix_ns"
            ),
            process_executable_sha256_after=str(
                raw["process_executable_sha256_after"]
            ),
        )


@dataclasses.dataclass(frozen=True)
class JournalVerdict:
    classification: Literal[
        "confirmed_pass", "confirmed_validation_failure", "pre_result_error"
    ]
    intended_ids: tuple[str, ...]
    selected_ids: tuple[str, ...]
    started_ids: tuple[str, ...]
    terminal_ids: tuple[str, ...]
    executed_ids: tuple[str, ...]
    confirmed_failure_ids: tuple[str, ...]
    sealed: bool
    valid_record_count: int
    journal_sha256: str | None
    diagnostics: tuple[str, ...]

    def to_dict(self) -> dict[str, object]:
        return dataclasses.asdict(self)


@dataclasses.dataclass
class _StartedAction:
    action_execution_id: str
    subject_count: int
    subjects_sha256: str
    process: ProcessIdentity


@dataclasses.dataclass
class _TerminalAction:
    action_execution_id: str
    classification: str
    actually_executed: bool
    result_code: str


class ChildValidationJournalWriter:
    """Exclusively creates and synchronously appends one bound journal."""

    def __init__(
        self,
        path: pathlib.Path,
        *,
        binding: JournalInvocationBinding,
        producer: ProcessIdentity,
        intended_ids: Sequence[str],
        selected_ids: Sequence[str],
    ) -> None:
        intended = _identifier_list(list(intended_ids), "intended_ids", nonempty=True)
        selected = _identifier_list(list(selected_ids), "selected_ids", nonempty=True)
        if any(action_id not in intended for action_id in selected):
            raise JournalContractError("selected_ids is not a subset of intended_ids")
        path.parent.mkdir(parents=True, exist_ok=True)
        self._handle = path.open("x", encoding="utf-8", newline="\n")
        self._binding = binding
        self._producer = producer
        self._intended = intended
        self._selected = selected
        self._sequence = 0
        self._previous_hash: str | None = None
        self._started: dict[str, _StartedAction] = {}
        self._terminal: dict[str, _TerminalAction] = {}
        self._sealed = False
        try:
            self._append(
                "header",
                producer=producer.as_record_fields(),
                intended_ids=intended,
                intended_count=len(intended),
                selected_ids=selected,
                selected_count=len(selected),
            )
        except BaseException:
            self._handle.close()
            raise

    def __enter__(self) -> ChildValidationJournalWriter:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def close(self) -> None:
        if not self._handle.closed:
            self._handle.close()

    def _append(self, record_type: str, **fields: object) -> str:
        if self._sealed:
            raise JournalContractError("journal is already sealed")
        record: dict[str, object] = {
            "schema_version": SCHEMA_VERSION,
            "report_type": REPORT_TYPE,
            "record_type": record_type,
            "sequence": self._sequence,
            "previous_record_sha256": self._previous_hash,
            **self._binding.as_record_fields(),
            "emitted_at_unix_ns": time.time_ns(),
            **fields,
        }
        digest = record_sha256(record)
        record["record_sha256"] = digest
        self._handle.write(canonical_json(record).decode("utf-8") + "\n")
        self._handle.flush()
        os.fsync(self._handle.fileno())
        self._sequence += 1
        self._previous_hash = digest
        if record_type == "seal":
            self._sealed = True
        return digest

    def start_action(
        self,
        action_id: str,
        *,
        subjects: Sequence[str],
        process: ProcessIdentity,
        action_execution_id: str | None = None,
    ) -> str:
        action_id = _identifier(action_id, "action_id")
        if action_id not in self._selected:
            raise JournalContractError("action_id was not selected")
        if action_id in self._started:
            raise JournalContractError("action_id was already started")
        subject_ids = _nonempty_string_list(list(subjects), "subjects")
        execution_id = action_execution_id or str(uuid.uuid4())
        _canonical_uuid(execution_id, "action_execution_id")
        if any(
            started.action_execution_id == execution_id
            for started in self._started.values()
        ):
            raise JournalContractError("action_execution_id is duplicated")
        started = _StartedAction(
            action_execution_id=execution_id,
            subject_count=len(subject_ids),
            subjects_sha256=hash_subjects(subject_ids),
            process=process,
        )
        self._append(
            "action_started",
            action_id=action_id,
            action_execution_id=execution_id,
            subject_count=started.subject_count,
            subjects_sha256=started.subjects_sha256,
            process=process.as_record_fields(),
        )
        self._started[action_id] = started
        return execution_id

    def finish_action(
        self,
        action_id: str,
        *,
        action_execution_id: str,
        classification: str,
        actually_executed: bool,
        result_code: str,
        exit_code: int | None,
        diagnostic: str = "",
        ended_at_unix_ns: int | None = None,
        process_executable_sha256_after: str | None = None,
    ) -> None:
        action_id = _identifier(action_id, "action_id")
        started = self._started.get(action_id)
        if started is None:
            raise JournalContractError("action_id was not started")
        if action_id in self._terminal:
            raise JournalContractError("action_id already has a terminal result")
        if action_execution_id != started.action_execution_id:
            raise JournalContractError("action_execution_id does not match its start")
        if classification not in CLASSIFICATIONS:
            raise JournalContractError("unknown action classification")
        if not isinstance(actually_executed, bool):
            raise JournalContractError("actually_executed is not a boolean")
        if classification != "pre_result_error" and not actually_executed:
            raise JournalContractError("confirmed results require actual execution")
        if result_code not in {"passed", "failed", "error"}:
            raise JournalContractError("unknown result_code")
        if classification == "confirmed_pass" and result_code != "passed":
            raise JournalContractError("confirmed pass requires passed result_code")
        if classification == "confirmed_validation_failure" and result_code != "failed":
            raise JournalContractError("confirmed failure requires failed result_code")
        if classification == "pre_result_error" and result_code != "error":
            raise JournalContractError("pre-result error requires error result_code")
        if exit_code is not None and (
            isinstance(exit_code, bool) or not isinstance(exit_code, int)
        ):
            raise JournalContractError("exit_code is not an integer or null")
        ended = time.time_ns() if ended_at_unix_ns is None else ended_at_unix_ns
        _positive_int(ended, "ended_at_unix_ns")
        if ended < started.process.started_at_unix_ns:
            raise JournalContractError("action ended before it started")
        executable_after = (
            started.process.executable_sha256
            if process_executable_sha256_after is None
            else process_executable_sha256_after
        )
        _hex_digest(executable_after, "process_executable_sha256_after")
        self._append(
            "action_result",
            action_id=action_id,
            action_execution_id=action_execution_id,
            subject_count=started.subject_count,
            subjects_sha256=started.subjects_sha256,
            classification=classification,
            actually_executed=actually_executed,
            result_code=result_code,
            exit_code=exit_code,
            diagnostic=diagnostic,
            ended_at_unix_ns=ended,
            process_executable_sha256_after=executable_after,
        )
        self._terminal[action_id] = _TerminalAction(
            action_execution_id=action_execution_id,
            classification=classification,
            actually_executed=actually_executed,
            result_code=result_code,
        )

    def infrastructure_error(self, *, phase: str, diagnostic: str) -> None:
        self._append(
            "infrastructure_error",
            phase=_identifier(phase, "phase"),
            diagnostic=diagnostic,
        )

    def seal(
        self,
        *,
        producer_ended_at_unix_ns: int | None = None,
        producer_executable_sha256_after: str | None = None,
    ) -> str:
        ended = (
            time.time_ns()
            if producer_ended_at_unix_ns is None
            else producer_ended_at_unix_ns
        )
        _positive_int(ended, "producer_ended_at_unix_ns")
        executable_after = (
            self._producer.executable_sha256
            if producer_executable_sha256_after is None
            else producer_executable_sha256_after
        )
        _hex_digest(executable_after, "producer_executable_sha256_after")
        started_ids = list(self._started)
        terminal_ids = list(self._terminal)
        executed_ids = [
            action_id
            for action_id, terminal in self._terminal.items()
            if terminal.actually_executed
        ]
        outcomes = [
            {
                "action_id": action_id,
                "action_execution_id": terminal.action_execution_id,
                "classification": terminal.classification,
                "actually_executed": terminal.actually_executed,
                "result_code": terminal.result_code,
            }
            for action_id, terminal in self._terminal.items()
        ]
        assert self._previous_hash is not None
        return self._append(
            "seal",
            intended_ids=self._intended,
            intended_count=len(self._intended),
            selected_ids=self._selected,
            selected_count=len(self._selected),
            started_ids=started_ids,
            started_count=len(started_ids),
            terminal_ids=terminal_ids,
            terminal_count=len(terminal_ids),
            executed_ids=executed_ids,
            executed_count=len(executed_ids),
            outcomes=outcomes,
            sealed_prefix_sha256=self._previous_hash,
            producer_ended_at_unix_ns=ended,
            producer_executable_sha256_after=executable_after,
        )


def _validate_common_record(
    raw: Mapping[str, object],
    *,
    binding: JournalInvocationBinding,
    sequence: int,
    previous_hash: str | None,
    previous_emitted_at: int | None,
    producer: ProcessIdentity,
    outer_ended_at_unix_ns: int,
) -> tuple[str, int]:
    record_type = raw.get("record_type")
    if not isinstance(record_type, str) or record_type not in _RECORD_KEYS:
        raise JournalContractError("unknown record_type")
    expected_keys = _COMMON_KEYS | _RECORD_KEYS[record_type]
    if set(raw) != expected_keys:
        raise JournalContractError(f"{record_type} fields do not match the schema")
    if raw.get("schema_version") != SCHEMA_VERSION or raw.get("report_type") != REPORT_TYPE:
        raise JournalContractError("journal schema identity mismatch")
    if raw.get("sequence") != sequence:
        raise JournalContractError("journal sequence is not contiguous")
    if raw.get("previous_record_sha256") != previous_hash:
        raise JournalContractError("journal hash chain is broken")
    claimed_hash = _hex_digest(raw.get("record_sha256"), "record_sha256")
    if claimed_hash != record_sha256(raw):
        raise JournalContractError("record_sha256 does not match the record")
    for key, expected in binding.as_record_fields().items():
        if raw.get(key) != expected:
            raise JournalContractError(f"journal {key} binding mismatch")
    emitted_at = _positive_int(raw.get("emitted_at_unix_ns"), "emitted_at_unix_ns")
    if emitted_at < producer.started_at_unix_ns or emitted_at > outer_ended_at_unix_ns:
        raise JournalContractError("record timestamp is outside the producer lifetime")
    if previous_emitted_at is not None and emitted_at < previous_emitted_at:
        raise JournalContractError("record timestamps moved backwards")
    return record_type, emitted_at


def _validate_header(
    raw: Mapping[str, object],
    expected_producer: ProcessIdentity,
    expected_intended: Sequence[str],
    expected_selected: Sequence[str],
) -> tuple[list[str], list[str]]:
    producer_raw = raw.get("producer")
    if not isinstance(producer_raw, dict):
        raise JournalContractError("header producer is not an object")
    producer = ProcessIdentity.from_mapping(producer_raw)
    if producer != expected_producer:
        raise JournalContractError("journal producer identity mismatch")
    intended = _identifier_list(raw.get("intended_ids"), "intended_ids", nonempty=True)
    selected = _identifier_list(raw.get("selected_ids"), "selected_ids", nonempty=True)
    if raw.get("intended_count") != len(intended):
        raise JournalContractError("intended_count does not match intended_ids")
    if raw.get("selected_count") != len(selected):
        raise JournalContractError("selected_count does not match selected_ids")
    if any(action_id not in intended for action_id in selected):
        raise JournalContractError("selected_ids is not a subset of intended_ids")
    if intended != list(expected_intended):
        raise JournalContractError("intended_ids does not match the external selection")
    if selected != list(expected_selected):
        raise JournalContractError("selected_ids does not match the external selection")
    return intended, selected


def _validate_started(
    raw: Mapping[str, object],
    *,
    selected: Sequence[str],
    started: Mapping[str, _StartedAction],
    execution_ids: set[str],
    producer: ProcessIdentity,
    expected_processes: Mapping[str, ProcessIdentity],
    expected_observations: Mapping[str, ActionProcessObservation] | None,
    outer_ended_at_unix_ns: int,
) -> tuple[str, _StartedAction]:
    action_id = _identifier(raw.get("action_id"), "action_id")
    if action_id not in selected:
        raise JournalContractError("started action was not selected")
    if action_id in started:
        raise JournalContractError("duplicate action start")
    execution_id = _canonical_uuid(
        str(raw.get("action_execution_id")), "action_execution_id"
    )
    if execution_id in execution_ids:
        raise JournalContractError("duplicate action_execution_id")
    subject_count = _positive_int(raw.get("subject_count"), "subject_count")
    subjects_sha256 = _hex_digest(raw.get("subjects_sha256"), "subjects_sha256")
    process_raw = raw.get("process")
    if not isinstance(process_raw, dict):
        raise JournalContractError("action process is not an object")
    process = ProcessIdentity.from_mapping(process_raw)
    if process != expected_processes[action_id]:
        raise JournalContractError("action process identity mismatch")
    if expected_observations is not None:
        expected_fields = expected_observations[action_id].started_record_fields()
        for key, expected in expected_fields.items():
            if raw.get(key) != expected:
                raise JournalContractError(
                    f"action start {key} does not match the parent observation"
                )
    if (
        process.started_at_unix_ns < producer.started_at_unix_ns
        or process.started_at_unix_ns > outer_ended_at_unix_ns
    ):
        raise JournalContractError("action process is outside the producer lifetime")
    return action_id, _StartedAction(
        action_execution_id=execution_id,
        subject_count=subject_count,
        subjects_sha256=subjects_sha256,
        process=process,
    )


def _validate_result(
    raw: Mapping[str, object],
    *,
    started: Mapping[str, _StartedAction],
    terminal: Mapping[str, _TerminalAction],
    expected_observations: Mapping[str, ActionProcessObservation] | None,
    outer_ended_at_unix_ns: int,
) -> tuple[str, _TerminalAction]:
    action_id = _identifier(raw.get("action_id"), "action_id")
    start = started.get(action_id)
    if start is None:
        raise JournalContractError("terminal action was not started")
    if action_id in terminal:
        raise JournalContractError("duplicate action terminal")
    execution_id = _canonical_uuid(
        str(raw.get("action_execution_id")), "action_execution_id"
    )
    if execution_id != start.action_execution_id:
        raise JournalContractError("terminal action_execution_id does not match its start")
    if raw.get("subject_count") != start.subject_count:
        raise JournalContractError("terminal subject_count does not match its start")
    if raw.get("subjects_sha256") != start.subjects_sha256:
        raise JournalContractError("terminal subjects_sha256 does not match its start")
    classification = raw.get("classification")
    if classification not in CLASSIFICATIONS:
        raise JournalContractError("unknown action classification")
    actually_executed = raw.get("actually_executed")
    if not isinstance(actually_executed, bool):
        raise JournalContractError("actually_executed is not a boolean")
    if classification != "pre_result_error" and not actually_executed:
        raise JournalContractError("confirmed result did not actually execute")
    result_code = raw.get("result_code")
    expected_result = {
        "confirmed_pass": "passed",
        "confirmed_validation_failure": "failed",
        "pre_result_error": "error",
    }[str(classification)]
    if result_code != expected_result:
        raise JournalContractError("result_code contradicts classification")
    exit_code = raw.get("exit_code")
    if exit_code is not None and (
        isinstance(exit_code, bool) or not isinstance(exit_code, int)
    ):
        raise JournalContractError("exit_code is not an integer or null")
    if not isinstance(raw.get("diagnostic"), str):
        raise JournalContractError("diagnostic is not a string")
    ended_at = _positive_int(raw.get("ended_at_unix_ns"), "ended_at_unix_ns")
    if ended_at < start.process.started_at_unix_ns or ended_at > outer_ended_at_unix_ns:
        raise JournalContractError("action result is outside the process lifetime")
    executable_after = _hex_digest(
        raw.get("process_executable_sha256_after"),
        "process_executable_sha256_after",
    )
    if executable_after != start.process.executable_sha256:
        raise JournalContractError("action executable changed during execution")
    if expected_observations is not None:
        expected_fields = expected_observations[action_id].result_record_fields()
        for key, expected in expected_fields.items():
            if raw.get(key) != expected:
                raise JournalContractError(
                    f"action result {key} does not match the parent observation"
                )
    return action_id, _TerminalAction(
        action_execution_id=execution_id,
        classification=str(classification),
        actually_executed=actually_executed,
        result_code=str(result_code),
    )


def _expected_outcomes(
    terminal: Mapping[str, _TerminalAction],
) -> list[dict[str, object]]:
    return [
        {
            "action_id": action_id,
            "action_execution_id": result.action_execution_id,
            "classification": result.classification,
            "actually_executed": result.actually_executed,
            "result_code": result.result_code,
        }
        for action_id, result in terminal.items()
    ]


def _validate_seal(
    raw: Mapping[str, object],
    *,
    intended: Sequence[str],
    selected: Sequence[str],
    started: Mapping[str, _StartedAction],
    terminal: Mapping[str, _TerminalAction],
    previous_hash: str,
    expected_producer: ProcessIdentity,
    outer_ended_at_unix_ns: int,
    outer_executable_sha256_after: str,
) -> None:
    expected_lists = {
        "intended_ids": list(intended),
        "selected_ids": list(selected),
        "started_ids": list(started),
        "terminal_ids": list(terminal),
        "executed_ids": [
            action_id
            for action_id, result in terminal.items()
            if result.actually_executed
        ],
    }
    for key, expected in expected_lists.items():
        actual = _identifier_list(raw.get(key), key, nonempty=False)
        if actual != expected:
            raise JournalContractError(f"seal {key} does not match the journal")
        count_key = key.removesuffix("_ids") + "_count"
        if raw.get(count_key) != len(expected):
            raise JournalContractError(f"seal {count_key} does not match {key}")
    if raw.get("outcomes") != _expected_outcomes(terminal):
        raise JournalContractError("seal outcomes do not match action terminals")
    if raw.get("sealed_prefix_sha256") != previous_hash:
        raise JournalContractError("seal does not bind the journal prefix")
    ended_at = _positive_int(
        raw.get("producer_ended_at_unix_ns"), "producer_ended_at_unix_ns"
    )
    if ended_at < expected_producer.started_at_unix_ns or ended_at > outer_ended_at_unix_ns:
        raise JournalContractError("seal is outside the producer lifetime")
    executable_after = _hex_digest(
        raw.get("producer_executable_sha256_after"),
        "producer_executable_sha256_after",
    )
    if executable_after != outer_executable_sha256_after:
        raise JournalContractError("producer executable identity changed")


def parse_child_validation_journal(
    path: pathlib.Path,
    *,
    expected_journal_path: pathlib.Path,
    expected_binding: JournalInvocationBinding,
    expected_intended_ids: Sequence[str],
    expected_selected_ids: Sequence[str],
    expected_producer: ProcessIdentity,
    expected_action_processes: Mapping[str, ProcessIdentity],
    outer_ended_at_unix_ns: int,
    outer_executable_sha256_after: str,
    outer_exit_code: int | None,
    expected_action_observations: Mapping[
        str, ActionProcessObservation
    ] | None = None,
) -> JournalVerdict:
    """Parse a strict valid prefix and classify only authenticated evidence."""

    intended_binding = _identifier_list(
        list(expected_intended_ids), "expected_intended_ids", nonempty=True
    )
    selected_binding = _identifier_list(
        list(expected_selected_ids), "expected_selected_ids", nonempty=True
    )
    if selected_binding != intended_binding:
        raise JournalContractError(
            "external selected_ids must exactly match external intended_ids"
        )
    if not isinstance(expected_action_processes, Mapping):
        raise JournalContractError("expected_action_processes is not a mapping")
    normalized_processes: dict[str, ProcessIdentity] = {}
    for raw_action_id, process in expected_action_processes.items():
        action_id = _identifier(raw_action_id, "expected action process ID")
        if not isinstance(process, ProcessIdentity):
            raise JournalContractError(
                f"expected action process {action_id} is not a ProcessIdentity"
            )
        normalized_processes[action_id] = process
    if set(normalized_processes) != set(selected_binding):
        raise JournalContractError(
            "expected action process IDs do not match external selected_ids"
        )
    normalized_observations: dict[str, ActionProcessObservation] | None = None
    if expected_action_observations is not None:
        if not isinstance(expected_action_observations, Mapping):
            raise JournalContractError("expected_action_observations is not a mapping")
        normalized_observations = {}
        for raw_action_id, observation in expected_action_observations.items():
            action_id = _identifier(raw_action_id, "expected action observation ID")
            if not isinstance(observation, ActionProcessObservation):
                raise JournalContractError(
                    f"expected action observation {action_id} is invalid"
                )
            if observation.action_id != action_id:
                raise JournalContractError(
                    "expected action observation ID does not match its mapping key"
                )
            normalized_observations[action_id] = observation
        if set(normalized_observations) != set(selected_binding):
            raise JournalContractError(
                "expected action observation IDs do not match external selected_ids"
            )
        if any(
            normalized_processes[action_id] != observation.process
            for action_id, observation in normalized_observations.items()
        ):
            raise JournalContractError(
                "expected action processes do not match parent observations"
            )
    try:
        actual_journal_path = path.resolve(strict=False)
        bound_journal_path = expected_journal_path.resolve(strict=False)
    except OSError as error:
        raise JournalContractError(f"journal path could not be resolved: {error}") from error
    if actual_journal_path != bound_journal_path:
        raise JournalContractError("journal path does not match the external invocation")
    _positive_int(outer_ended_at_unix_ns, "outer_ended_at_unix_ns")
    _hex_digest(
        outer_executable_sha256_after, "outer_executable_sha256_after"
    )
    if outer_exit_code is not None and (
        isinstance(outer_exit_code, bool) or not isinstance(outer_exit_code, int)
    ):
        raise JournalContractError("outer_exit_code is not an integer or null")
    diagnostics: list[str] = []
    external_identity_valid = True
    if outer_executable_sha256_after != expected_producer.executable_sha256:
        external_identity_valid = False
        diagnostics.append("producer executable changed during the outer execution")
    try:
        contents = path.read_bytes()
    except OSError as error:
        return JournalVerdict(
            classification="pre_result_error",
            intended_ids=(),
            selected_ids=(),
            started_ids=(),
            terminal_ids=(),
            executed_ids=(),
            confirmed_failure_ids=(),
            sealed=False,
            valid_record_count=0,
            journal_sha256=None,
            diagnostics=(f"journal could not be read: {error}",),
        )

    intended: list[str] = []
    selected: list[str] = []
    started: dict[str, _StartedAction] = {}
    terminal: dict[str, _TerminalAction] = {}
    execution_ids: set[str] = set()
    previous_hash: str | None = None
    previous_emitted_at: int | None = None
    valid_record_count = 0
    header_seen = False
    seal_seen = False
    lines = contents.splitlines(keepends=True)
    if not lines:
        diagnostics.append("journal is empty")
    for index, physical_line in enumerate(lines):
        if not physical_line.endswith(b"\n"):
            diagnostics.append(f"journal record {index} is truncated")
            break
        line = physical_line[:-1]
        try:
            decoded = line.decode("utf-8")
            raw = json.loads(decoded)
            if not isinstance(raw, dict):
                raise JournalContractError("journal record is not an object")
            if canonical_json(raw) != line:
                raise JournalContractError("journal record is not canonical JSON")
            record_type, emitted_at = _validate_common_record(
                raw,
                binding=expected_binding,
                sequence=valid_record_count,
                previous_hash=previous_hash,
                previous_emitted_at=previous_emitted_at,
                producer=expected_producer,
                outer_ended_at_unix_ns=outer_ended_at_unix_ns,
            )
            if not header_seen and record_type != "header":
                raise JournalContractError("journal does not begin with a header")
            if seal_seen:
                raise JournalContractError("journal contains data after its seal")
            if record_type == "header":
                if header_seen or valid_record_count != 0:
                    raise JournalContractError("journal contains a duplicate header")
                intended, selected = _validate_header(
                    raw,
                    expected_producer,
                    intended_binding,
                    selected_binding,
                )
                header_seen = True
            elif record_type == "action_started":
                action_id, action = _validate_started(
                    raw,
                    selected=selected,
                    started=started,
                    execution_ids=execution_ids,
                    producer=expected_producer,
                    expected_processes=normalized_processes,
                    expected_observations=normalized_observations,
                    outer_ended_at_unix_ns=outer_ended_at_unix_ns,
                )
                started[action_id] = action
                execution_ids.add(action.action_execution_id)
            elif record_type == "action_result":
                action_id, result = _validate_result(
                    raw,
                    started=started,
                    terminal=terminal,
                    expected_observations=normalized_observations,
                    outer_ended_at_unix_ns=outer_ended_at_unix_ns,
                )
                terminal[action_id] = result
            elif record_type == "infrastructure_error":
                _identifier(raw.get("phase"), "infrastructure error phase")
                if not isinstance(raw.get("diagnostic"), str):
                    raise JournalContractError(
                        "infrastructure error diagnostic is not a string"
                    )
                diagnostics.append(
                    "runner reported infrastructure error: "
                    f"{raw['phase']}: {raw['diagnostic']}"
                )
            elif record_type == "seal":
                if previous_hash is None:
                    raise JournalContractError("seal has no journal prefix")
                _validate_seal(
                    raw,
                    intended=intended,
                    selected=selected,
                    started=started,
                    terminal=terminal,
                    previous_hash=previous_hash,
                    expected_producer=expected_producer,
                    outer_ended_at_unix_ns=outer_ended_at_unix_ns,
                    outer_executable_sha256_after=outer_executable_sha256_after,
                )
                seal_seen = True
            previous_hash = str(raw["record_sha256"])
            previous_emitted_at = emitted_at
            valid_record_count += 1
        except (JournalContractError, UnicodeDecodeError, json.JSONDecodeError) as error:
            diagnostics.append(f"journal record {index} is invalid: {error}")
            break

    authenticated_failure_ids = [
        action_id
        for action_id, result in terminal.items()
        if result.classification == "confirmed_validation_failure"
    ]
    failure_ids = authenticated_failure_ids if external_identity_valid else []
    executed_ids = [
        action_id for action_id, result in terminal.items() if result.actually_executed
    ]
    complete_pass = (
        seal_seen
        and not diagnostics
        and external_identity_valid
        and intended == selected
        and list(started) == intended
        and list(terminal) == intended
        and executed_ids == intended
        and all(
            result.classification == "confirmed_pass" for result in terminal.values()
        )
    )
    if not external_identity_valid:
        classification = "pre_result_error"
    elif failure_ids and outer_exit_code == 0:
        classification = "pre_result_error"
        diagnostics.append("confirmed failure contradicts successful outer exit")
    elif complete_pass and outer_exit_code == 0:
        classification = "confirmed_pass"
    elif complete_pass:
        classification = "pre_result_error"
        diagnostics.append("complete pass contradicts unsuccessful outer execution")
    elif failure_ids and outer_exit_code != 0:
        classification = "confirmed_validation_failure"
    else:
        classification = "pre_result_error"
        if not diagnostics:
            diagnostics.append("journal did not establish a complete confirmed result")
    return JournalVerdict(
        classification=classification,  # type: ignore[arg-type]
        intended_ids=tuple(intended),
        selected_ids=tuple(selected),
        started_ids=tuple(started),
        terminal_ids=tuple(terminal),
        executed_ids=tuple(executed_ids),
        confirmed_failure_ids=tuple(failure_ids),
        sealed=seal_seen and not diagnostics,
        valid_record_count=valid_record_count,
        journal_sha256=sha256_bytes(contents),
        diagnostics=tuple(diagnostics),
    )


def _canonical_ipc_object(payload: bytes, label: str) -> dict[str, object]:
    try:
        value = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise JournalContractError(f"{label} is not valid UTF-8 JSON") from error
    if not isinstance(value, dict) or canonical_json(value) != payload:
        raise JournalContractError(f"{label} is not a canonical JSON object")
    return value


def _portable_exit_code(value: int | None) -> int | None:
    if value is None:
        return None
    if 0x80000000 <= value <= 0xFFFFFFFF:
        return value - 0x100000000
    if -(2**31) <= value <= 2**31 - 1:
        return value
    return None


def _validated_broker_request(raw: Mapping[str, object]) -> dict[str, object]:
    expected = {
        "protocol",
        "binding",
        "action_id",
        "action_execution_id",
        "subjects",
        "command",
        "cwd",
        "env",
        "timeout_seconds",
        "stdout_limit_bytes",
        "stderr_limit_bytes",
        "validation_failure_exit_codes",
        "journal_path",
        "launch_target_identity",
    }
    if set(raw) != expected or raw.get("protocol") != BROKER_PROTOCOL:
        raise JournalContractError("broker request fields do not match")
    binding_raw = raw["binding"]
    if not isinstance(binding_raw, dict):
        raise JournalContractError("broker binding is not an object")
    binding = JournalInvocationBinding.from_mapping(binding_raw)
    action_id = _identifier(raw["action_id"], "broker action_id")
    if action_id != binding.validation_id:
        raise JournalContractError("broker action_id does not match its binding")
    action_execution_id = _canonical_uuid(
        str(raw["action_execution_id"]), "broker action_execution_id"
    )
    subjects = _nonempty_string_list(raw["subjects"], "broker subjects")

    command = raw["command"]
    if (
        not isinstance(command, list)
        or not command
        or not all(isinstance(item, str) and item for item in command)
    ):
        raise JournalContractError("broker command is invalid")
    executable = pathlib.Path(command[0])
    if not executable.is_absolute():
        raise JournalContractError("broker command executable is not absolute")
    cwd = raw["cwd"]
    journal_path = raw["journal_path"]
    if not isinstance(cwd, str) or not pathlib.Path(cwd).is_absolute():
        raise JournalContractError("broker cwd is not absolute")
    if not isinstance(journal_path, str) or not pathlib.Path(journal_path).is_absolute():
        raise JournalContractError("broker journal path is not absolute")
    env = raw["env"]
    if not isinstance(env, dict) or not all(
        isinstance(key, str) and isinstance(value, str)
        for key, value in env.items()
    ):
        raise JournalContractError("broker environment is invalid")
    timeout_seconds = _positive_int(raw["timeout_seconds"], "broker timeout_seconds")
    stdout_limit = _positive_int(
        raw["stdout_limit_bytes"],
        "broker stdout_limit_bytes",
        allow_zero=True,
    )
    stderr_limit = _positive_int(
        raw["stderr_limit_bytes"],
        "broker stderr_limit_bytes",
        allow_zero=True,
    )
    failure_codes = raw["validation_failure_exit_codes"]
    if not isinstance(failure_codes, list) or not all(
        not isinstance(value, bool) and isinstance(value, int)
        for value in failure_codes
    ):
        raise JournalContractError("broker failure exit codes are invalid")
    if failure_codes != sorted(set(failure_codes)) or 0 in failure_codes:
        raise JournalContractError("broker failure exit codes are not canonical")
    launch_raw = raw["launch_target_identity"]
    if not isinstance(launch_raw, dict) or set(launch_raw) != {
        "requested",
        "resolved_path",
        "sha256_before",
    }:
        raise JournalContractError("broker launch identity fields do not match")
    if (
        not isinstance(launch_raw["requested"], str)
        or not isinstance(launch_raw["resolved_path"], str)
        or pathlib.Path(launch_raw["resolved_path"]).resolve(strict=False)
        != executable.resolve(strict=False)
        or command[0] != launch_raw["resolved_path"]
    ):
        raise JournalContractError("broker command contradicts its launch identity")
    expected_executable_hash = _hex_digest(
        launch_raw["sha256_before"], "broker launch executable hash"
    )
    if hash_file(executable.resolve(strict=True)) != expected_executable_hash:
        raise JournalContractError("broker launch executable changed before execution")
    return {
        "binding": binding,
        "action_id": action_id,
        "action_execution_id": action_execution_id,
        "subjects": tuple(subjects),
        "command": command,
        "cwd": pathlib.Path(cwd),
        "env": env,
        "timeout_seconds": timeout_seconds,
        "stdout_limit_bytes": stdout_limit,
        "stderr_limit_bytes": stderr_limit,
        "validation_failure_exit_codes": frozenset(failure_codes),
        "journal_path": pathlib.Path(journal_path),
        "expected_executable_hash": expected_executable_hash,
    }


def _run_broker_request(
    request_payload: bytes,
    *,
    producer: ProcessIdentity,
) -> tuple[dict[str, object], dict[str, object] | None, int]:
    request_sha256 = sha256_bytes(request_payload)
    request = _validated_broker_request(
        _canonical_ipc_object(request_payload, "broker request")
    )
    binding = request["binding"]
    assert isinstance(binding, JournalInvocationBinding)
    journal_path = request["journal_path"]
    assert isinstance(journal_path, pathlib.Path)
    action_id = str(request["action_id"])
    action_execution_id = str(request["action_execution_id"])
    subjects = request["subjects"]
    assert isinstance(subjects, tuple)
    command = request["command"]
    cwd = request["cwd"]
    env = request["env"]
    assert isinstance(command, list)
    assert isinstance(cwd, pathlib.Path)
    assert isinstance(env, dict)
    failure_codes = request["validation_failure_exit_codes"]
    assert isinstance(failure_codes, frozenset)

    with ChildValidationJournalWriter(
        journal_path,
        binding=binding,
        producer=producer,
        intended_ids=[action_id],
        selected_ids=[action_id],
    ) as writer:
        started_at = time.time_ns()
        bounded = run_bounded_process(
            command,
            cwd=cwd,
            env=env,
            timeout_seconds=int(request["timeout_seconds"]),
            stdout_limit_bytes=int(request["stdout_limit_bytes"]),
            stderr_limit_bytes=int(request["stderr_limit_bytes"]),
        )
        sys.stdout.buffer.write(bounded.stdout)
        sys.stdout.buffer.flush()
        sys.stderr.buffer.write(bounded.stderr)
        sys.stderr.buffer.flush()
        returncode = _portable_exit_code(bounded.returncode)
        supervision_error = bounded.supervision_error
        if bounded.returncode is not None and returncode is None:
            supervision_error = (
                f"{supervision_error}; inner validation returned an exit code outside "
                "the signed 32-bit report contract"
                if supervision_error
                else "inner validation returned an exit code outside the signed "
                "32-bit report contract"
            )
        if bounded.pid <= 0:
            writer.infrastructure_error(
                phase="inner-launch",
                diagnostic=supervision_error or "inner validation did not launch",
            )
            producer_ended = time.time_ns()
            producer_after = hash_file(pathlib.Path(sys.executable).resolve())
            writer.seal(
                producer_ended_at_unix_ns=producer_ended,
                producer_executable_sha256_after=producer_after,
            )
            return (
                {
                    "protocol": BROKER_PROTOCOL,
                    "request_sha256": request_sha256,
                    "status": "infrastructure_error",
                    "producer": producer.as_record_fields(),
                    "producer_ended_at_unix_ns": producer_ended,
                    "producer_executable_sha256_after": producer_after,
                    "diagnostic": supervision_error
                    or "inner validation did not launch",
                },
                None,
                2,
            )

        executable = pathlib.Path(command[0]).resolve(strict=True)
        executable_after = hash_file(executable)
        process = ProcessIdentity(
            pid=bounded.pid,
            executable_path=str(executable),
            executable_sha256=str(request["expected_executable_hash"]),
            argv_sha256=hash_arguments(command),
            started_at_unix_ns=started_at,
        )
        if supervision_error or executable_after != process.executable_sha256:
            classification = "pre_result_error"
        elif returncode == 0:
            classification = "confirmed_pass"
        elif returncode in failure_codes:
            classification = "confirmed_validation_failure"
        else:
            classification = "pre_result_error"
        result_code = {
            "confirmed_pass": "passed",
            "confirmed_validation_failure": "failed",
            "pre_result_error": "error",
        }[classification]
        diagnostic = supervision_error or bounded.stderr.decode(
            "utf-8", errors="replace"
        )
        if executable_after != process.executable_sha256:
            diagnostic = "; ".join(
                part
                for part in (
                    diagnostic,
                    "inner validation executable changed during execution",
                )
                if part
            )
        ended_at = time.time_ns()
        observation = ActionProcessObservation(
            action_id=action_id,
            action_execution_id=action_execution_id,
            subjects=subjects,
            process=process,
            classification=classification,  # type: ignore[arg-type]
            actually_executed=classification
            in {"confirmed_pass", "confirmed_validation_failure"},
            result_code=result_code,  # type: ignore[arg-type]
            exit_code=returncode,
            diagnostic=diagnostic,
            ended_at_unix_ns=ended_at,
            process_executable_sha256_after=executable_after,
        )
        writer.start_action(
            action_id,
            subjects=subjects,
            process=process,
            action_execution_id=action_execution_id,
        )
        writer.finish_action(
            action_id,
            action_execution_id=action_execution_id,
            classification=classification,
            actually_executed=observation.actually_executed,
            result_code=result_code,
            exit_code=returncode,
            diagnostic=diagnostic,
            ended_at_unix_ns=ended_at,
            process_executable_sha256_after=executable_after,
        )
        if supervision_error:
            writer.infrastructure_error(
                phase="inner-supervision", diagnostic=supervision_error
            )
        producer_ended = time.time_ns()
        producer_after = hash_file(pathlib.Path(sys.executable).resolve())
        writer.seal(
            producer_ended_at_unix_ns=producer_ended,
            producer_executable_sha256_after=producer_after,
        )
    response = {
        "protocol": BROKER_PROTOCOL,
        "request_sha256": request_sha256,
        "status": "observed",
        "producer": producer.as_record_fields(),
        "observation": observation.as_ipc_fields(),
    }
    completion = {
        "protocol": BROKER_PROTOCOL,
        "request_sha256": request_sha256,
        "status": "completed",
        "producer_ended_at_unix_ns": producer_ended,
        "producer_executable_sha256_after": producer_after,
    }
    return response, completion, returncode if returncode is not None else 2


def _broker_main(*, host: str, port: int) -> int:
    started_at = time.time_ns()
    launch_arguments = globals().get("_BROKER_LAUNCH_ARGUMENTS")
    if not (
        isinstance(launch_arguments, list)
        and launch_arguments
        and all(isinstance(item, str) for item in launch_arguments)
    ):
        launch_arguments = [str(pathlib.Path(sys.executable).resolve()), *sys.argv]
    producer = ProcessIdentity.current(
        started_at_unix_ns=started_at,
        arguments=launch_arguments,
    )
    authkey_value = os.environ.pop(BROKER_AUTHKEY_ENV, "")
    try:
        authkey = bytes.fromhex(authkey_value)
    except ValueError as error:
        raise JournalContractError("broker auth key is not hexadecimal") from error
    if len(authkey) != 32:
        raise JournalContractError("broker auth key has invalid length")
    with Client((host, port), family="AF_INET", authkey=authkey) as connection:
        request_payload = connection.recv_bytes(BROKER_MAX_REQUEST_BYTES)
        try:
            response, completion, returncode = _run_broker_request(
                request_payload,
                producer=producer,
            )
        except Exception as error:  # noqa: BLE001 - private broker fails closed
            response = {
                "protocol": BROKER_PROTOCOL,
                "request_sha256": sha256_bytes(request_payload),
                "status": "infrastructure_error",
                "producer": producer.as_record_fields(),
                "producer_ended_at_unix_ns": time.time_ns(),
                "producer_executable_sha256_after": hash_file(
                    pathlib.Path(sys.executable).resolve()
                ),
                "diagnostic": f"{type(error).__name__}: {error}",
            }
            completion = None
            returncode = 2
        connection.send_bytes(canonical_json(response))
        if completion is not None:
            connection.send_bytes(canonical_json(completion))
    return returncode


def _load_json_object(path: pathlib.Path, label: str) -> dict[str, object]:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise JournalContractError(f"{label} could not be loaded: {error}") from error
    if not isinstance(raw, dict):
        raise JournalContractError(f"{label} is not an object")
    return raw


def _load_external_selection(path: pathlib.Path) -> tuple[list[str], list[str]]:
    raw = _load_json_object(path, "selection")
    if set(raw) != {"intended_ids", "selected_ids"}:
        raise JournalContractError("selection fields do not match")
    intended = _identifier_list(raw["intended_ids"], "intended_ids", nonempty=True)
    selected = _identifier_list(raw["selected_ids"], "selected_ids", nonempty=True)
    return intended, selected


def _load_action_processes(path: pathlib.Path) -> dict[str, ProcessIdentity]:
    raw = _load_json_object(path, "action processes")
    processes: dict[str, ProcessIdentity] = {}
    for raw_action_id, raw_process in raw.items():
        action_id = _identifier(raw_action_id, "action process ID")
        if not isinstance(raw_process, dict):
            raise JournalContractError(f"action process {action_id} is not an object")
        processes[action_id] = ProcessIdentity.from_mapping(raw_process)
    return processes


def _parse_exit_code(value: str) -> int | None:
    if value == "none":
        return None
    try:
        return int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected an integer or 'none'") from error


def _main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    broker = subparsers.add_parser(
        "typed-validation-broker", help=argparse.SUPPRESS
    )
    broker.add_argument("--host", required=True)
    broker.add_argument("--port", required=True, type=int)
    validate = subparsers.add_parser("validate", help="validate one completed journal")
    validate.add_argument("--journal", required=True, type=pathlib.Path)
    validate.add_argument("--expected-journal", required=True, type=pathlib.Path)
    validate.add_argument("--binding", required=True, type=pathlib.Path)
    validate.add_argument("--selection", required=True, type=pathlib.Path)
    validate.add_argument("--producer", required=True, type=pathlib.Path)
    validate.add_argument("--action-processes", required=True, type=pathlib.Path)
    validate.add_argument("--outer-ended-at-unix-ns", required=True, type=int)
    validate.add_argument("--outer-executable-sha256-after", required=True)
    validate.add_argument("--outer-exit-code", required=True, type=_parse_exit_code)
    args = parser.parse_args(argv)
    try:
        if args.command == "typed-validation-broker":
            return _broker_main(host=args.host, port=args.port)
        binding = JournalInvocationBinding.from_mapping(
            _load_json_object(args.binding, "binding")
        )
        producer = ProcessIdentity.from_mapping(
            _load_json_object(args.producer, "producer")
        )
        intended_ids, selected_ids = _load_external_selection(args.selection)
        action_processes = _load_action_processes(args.action_processes)
        verdict = parse_child_validation_journal(
            args.journal,
            expected_journal_path=args.expected_journal,
            expected_binding=binding,
            expected_intended_ids=intended_ids,
            expected_selected_ids=selected_ids,
            expected_producer=producer,
            expected_action_processes=action_processes,
            outer_ended_at_unix_ns=args.outer_ended_at_unix_ns,
            outer_executable_sha256_after=args.outer_executable_sha256_after,
            outer_exit_code=args.outer_exit_code,
        )
    except (JournalContractError, OSError, ValueError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print(canonical_json(verdict.to_dict()).decode("utf-8"))
    return {
        "confirmed_pass": 0,
        "confirmed_validation_failure": 1,
        "pre_result_error": 2,
    }[verdict.classification]


if __name__ == "__main__":
    raise SystemExit(_main())
