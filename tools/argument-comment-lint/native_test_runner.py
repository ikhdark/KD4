#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Sequence


SCHEMA_VERSION = 1
REPORT_TYPE = "ArgumentCommentLintNativeTestReportV1"
UI_CASE_ENV = "KD4_ARGUMENT_COMMENT_LINT_UI_CASE"
PINNED_TOOLCHAIN = "nightly-2025-09-18"
UI_CASES = (
    "allow_char_literals",
    "allow_self_documenting_methods",
    "allow_string_literals",
    "comment_matches",
    "comment_matches_multiline",
    "comment_mismatch",
    "ignore_external_methods",
    "multiple_method_arguments",
    "uncommented_literal",
)
UI_STDERR_CASES = frozenset(
    {"comment_mismatch", "multiple_method_arguments", "uncommented_literal"}
)


@dataclass(frozen=True)
class TestSpec:
    id: str
    kind: str
    cargo_target: tuple[str, ...]
    native_id: str | None = None
    ui_case: str | None = None
    doctest_item: str | None = None
    doctest_ordinal: int | None = None

    def listed(self, *, native_id: str | None = None) -> dict[str, Any]:
        result = asdict(self)
        result["cargo_target"] = list(self.cargo_target)
        if native_id is not None:
            result["native_id"] = native_id
        return result


@dataclass(frozen=True)
class DiscoveredTest:
    kind: str
    cargo_target: tuple[str, ...]
    native_id: str


@dataclass(frozen=True)
class ResolvedTest:
    spec: TestSpec
    native_id: str
    selection: str
    exact: bool

    def listed(self) -> dict[str, Any]:
        return self.spec.listed(native_id=self.native_id)


def _rust_test(
    *, test_id: str, kind: str, cargo_target: tuple[str, ...], native_id: str
) -> TestSpec:
    return TestSpec(
        id=f"argument-comment-lint::{kind}::{test_id}",
        kind=kind,
        cargo_target=cargo_target,
        native_id=native_id,
    )


TESTS: tuple[TestSpec, ...] = (
    _rust_test(
        test_id="comment_parser.parses_prefix_comment",
        kind="rust-lib",
        cargo_target=("--lib",),
        native_id="comment_parser::tests::parses_prefix_comment",
    ),
    _rust_test(
        test_id="comment_parser.parses_trailing_comment",
        kind="rust-lib",
        cargo_target=("--lib",),
        native_id="comment_parser::tests::parses_trailing_comment",
    ),
    _rust_test(
        test_id="comment_parser.rejects_non_matching_shapes",
        kind="rust-lib",
        cargo_target=("--lib",),
        native_id="comment_parser::tests::rejects_non_matching_shapes",
    ),
    _rust_test(
        test_id="workspace_crate_filter_accepts_first_party_names_only",
        kind="rust-lib",
        cargo_target=("--lib",),
        native_id="workspace_crate_filter_accepts_first_party_names_only",
    ),
    _rust_test(
        test_id="uses_windows_cargo_dylint_binary_name",
        kind="rust-bin",
        cargo_target=("--bin", "argument-comment-lint"),
        native_id="tests::uses_windows_cargo_dylint_binary_name",
    ),
    _rust_test(
        test_id="strips_host_triple_from_nightly_filename",
        kind="rust-bin",
        cargo_target=("--bin", "argument-comment-lint"),
        native_id="tests::strips_host_triple_from_nightly_filename",
    ),
    _rust_test(
        test_id="leaves_unqualified_nightly_filename_alone",
        kind="rust-bin",
        cargo_target=("--bin", "argument-comment-lint"),
        native_id="tests::leaves_unqualified_nightly_filename_alone",
    ),
    _rust_test(
        test_id="strict_rustflags_promotes_both_enforced_lints",
        kind="rust-bin",
        cargo_target=("--bin", "argument-comment-lint"),
        native_id="tests::strict_rustflags_promotes_both_enforced_lints",
    ),
    TestSpec(
        id="argument-comment-lint::rust-doctest::argument_comment_mismatch.example",
        kind="rust-doctest",
        cargo_target=("--doc",),
        doctest_item="ARGUMENT_COMMENT_MISMATCH",
        doctest_ordinal=0,
    ),
    TestSpec(
        id="argument-comment-lint::rust-doctest::argument_comment_mismatch.use_instead",
        kind="rust-doctest",
        cargo_target=("--doc",),
        doctest_item="ARGUMENT_COMMENT_MISMATCH",
        doctest_ordinal=1,
    ),
    TestSpec(
        id="argument-comment-lint::rust-doctest::uncommented_literal.example",
        kind="rust-doctest",
        cargo_target=("--doc",),
        doctest_item="UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT",
        doctest_ordinal=0,
    ),
    TestSpec(
        id="argument-comment-lint::rust-doctest::uncommented_literal.use_instead",
        kind="rust-doctest",
        cargo_target=("--doc",),
        doctest_item="UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT",
        doctest_ordinal=1,
    ),
    *(
        TestSpec(
            id=f"argument-comment-lint::dylint-ui::{case}",
            kind="dylint-ui",
            cargo_target=("--lib",),
            native_id="ui",
            ui_case=case,
        )
        for case in UI_CASES
    ),
)

LIST_TARGETS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("rust-lib", ("--lib",)),
    ("rust-bin", ("--bin", "argument-comment-lint")),
    ("rust-doctest", ("--doc",)),
)


def _tool_root() -> Path:
    return Path(__file__).resolve().parent


def _assert_static_inventory() -> None:
    ids = [spec.id for spec in TESTS]
    if not ids:
        raise ValueError("native test inventory must not be empty")
    if len(set(ids)) != len(ids):
        raise ValueError("native test inventory contains duplicate semantic IDs")

    expected_targets = {
        "rust-lib": ("--lib",),
        "rust-bin": ("--bin", "argument-comment-lint"),
        "rust-doctest": ("--doc",),
        "dylint-ui": ("--lib",),
    }
    doctest_ordinals: dict[str, list[int]] = {}
    for spec in TESTS:
        if spec.kind not in expected_targets:
            raise ValueError(f"native test {spec.id!r} has unknown kind {spec.kind!r}")
        if spec.cargo_target != expected_targets[spec.kind]:
            raise ValueError(f"native test {spec.id!r} has an invalid Cargo target")
        if not spec.id.startswith(f"argument-comment-lint::{spec.kind}::"):
            raise ValueError(f"native test {spec.id!r} has an invalid semantic identity")
        if spec.kind == "rust-doctest":
            if (
                spec.native_id is not None
                or not isinstance(spec.doctest_item, str)
                or not spec.doctest_item
                or isinstance(spec.doctest_ordinal, bool)
                or not isinstance(spec.doctest_ordinal, int)
                or spec.doctest_ordinal < 0
                or spec.ui_case is not None
            ):
                raise ValueError(f"native doctest {spec.id!r} has an invalid mapping")
            doctest_ordinals.setdefault(spec.doctest_item, []).append(
                spec.doctest_ordinal
            )
        elif spec.kind == "dylint-ui":
            if (
                spec.native_id != "ui"
                or not isinstance(spec.ui_case, str)
                or not spec.ui_case
                or spec.doctest_item is not None
                or spec.doctest_ordinal is not None
            ):
                raise ValueError(f"Dylint UI test {spec.id!r} has an invalid mapping")
        elif (
            not isinstance(spec.native_id, str)
            or not spec.native_id
            or spec.ui_case is not None
            or spec.doctest_item is not None
            or spec.doctest_ordinal is not None
        ):
            raise ValueError(f"native test {spec.id!r} has an invalid mapping")

    for item, ordinals in doctest_ordinals.items():
        if sorted(ordinals) != list(range(len(ordinals))):
            raise ValueError(f"doctest {item!r} has incomplete or duplicate ordinals")

    mapped_ui = [spec.ui_case for spec in TESTS if spec.kind == "dylint-ui"]
    if len(mapped_ui) != len(set(mapped_ui)):
        raise ValueError("Dylint UI semantic mapping contains duplicate cases")
    expected_ui = set(UI_CASES)
    if set(mapped_ui) != expected_ui:
        raise ValueError("Dylint UI semantic mapping is incomplete")
    ui_root = _tool_root() / "ui"
    actual_ui = {path.stem for path in ui_root.glob("*.rs")}
    if actual_ui != expected_ui:
        raise ValueError(
            "Dylint UI inventory does not match ui/*.rs: "
            f"expected {sorted(expected_ui)!r}, found {sorted(actual_ui)!r}"
        )
    actual_stderr = {path.stem for path in ui_root.glob("*.stderr")}
    if actual_stderr != UI_STDERR_CASES:
        raise ValueError(
            "Dylint UI inventory does not match ui/*.stderr: "
            f"expected {sorted(UI_STDERR_CASES)!r}, found {sorted(actual_stderr)!r}"
        )


def _emit(payload: dict[str, Any]) -> None:
    print(json.dumps(payload, sort_keys=True, separators=(",", ":")))


def _selection_error(intended: Sequence[str], message: str) -> int:
    _emit(
        {
            "schema_version": SCHEMA_VERSION,
            "report_type": REPORT_TYPE,
            "intended_validation_ids": list(intended),
            "selected_validation_ids": [],
            "actually_executed_validation_ids": [],
            "outcomes": [],
            "result": "pre_result_error",
            "error": message,
        }
    )
    return 2


def _run_command(
    command: Sequence[str], *, env: dict[str, str]
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            list(command),
            cwd=_tool_root(),
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
    except OSError as error:
        return subprocess.CompletedProcess(
            list(command),
            127,
            stdout="",
            stderr=f"failed to launch {command[0]}: {error}",
        )


def _resolve_discovery_linker(env: dict[str, str]) -> str:
    search_path = env.get("PATH")
    if not search_path:
        raise ValueError("Cargo discovery requires PATH to resolve an ordinary linker")
    candidates = ("lld-link.exe", "lld-link") if os.name == "nt" else ("cc",)
    for candidate in candidates:
        resolved = shutil.which(candidate, path=search_path)
        if resolved is not None:
            return str(Path(resolved).resolve(strict=True))
    raise ValueError(
        "Cargo discovery could not resolve an ordinary linker: "
        + ", ".join(candidates)
    )


def _discovery_linker_override(linker: str) -> str:
    return (
        "target.'cfg(all())'.linker="
        + json.dumps(linker, ensure_ascii=False)
    )


def _parse_native_list(
    stdout: str, *, kind: str, cargo_target: tuple[str, ...]
) -> list[DiscoveredTest]:
    discovered: list[DiscoveredTest] = []
    for line in stdout.splitlines():
        native_id, separator, test_type = line.rpartition(": ")
        if separator == "" or test_type.strip() != "test":
            continue
        native_id = native_id.strip()
        if not native_id:
            raise ValueError(f"{kind} discovery returned an empty native identity")
        discovered.append(
            DiscoveredTest(
                kind=kind,
                cargo_target=cargo_target,
                native_id=native_id,
            )
        )
    return discovered


def _discover_native_tests(
    cargo: Sequence[str], env: dict[str, str]
) -> list[DiscoveredTest]:
    linker = _resolve_discovery_linker(env)
    discovery_cargo = [
        *cargo,
        "--config",
        _discovery_linker_override(linker),
    ]
    discovered: list[DiscoveredTest] = []
    for kind, cargo_target in LIST_TARGETS:
        command = [
            *discovery_cargo,
            "test",
            "--offline",
            *cargo_target,
            "--",
            "--list",
        ]
        completed = _run_command(command, env=env)
        if completed.returncode != 0:
            diagnostic = completed.stderr.strip() or completed.stdout.strip()
            raise ValueError(
                f"{kind} discovery failed before validation with exit "
                f"{completed.returncode}: {diagnostic}"
            )
        target_tests = _parse_native_list(
            completed.stdout,
            kind=kind,
            cargo_target=cargo_target,
        )
        if not target_tests:
            raise ValueError(f"{kind} discovery selected zero native tests")
        discovered.extend(target_tests)
    return discovered


def _parse_test_events(stdout: str) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for line in stdout.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("type") == "test":
            events.append(value)
    return events


def _doctest_filter(native_id: str, discovered: Sequence[str]) -> str | None:
    match = re.search(r"\(line ([0-9]+)\)$", native_id)
    if match is None:
        return None
    candidate = match.group(1)
    matching = [test_id for test_id in discovered if candidate in test_id]
    return candidate if matching == [native_id] else None


def _reconcile_inventory(
    cargo: Sequence[str], env: dict[str, str]
) -> list[ResolvedTest]:
    _assert_static_inventory()
    discovered = _discover_native_tests(cargo, env)
    discovered_keys = [
        (item.cargo_target, item.native_id) for item in discovered
    ]
    duplicate_keys = sorted(
        {
            f"{' '.join(cargo_target)}::{native_id}"
            for cargo_target, native_id in discovered_keys
            if discovered_keys.count((cargo_target, native_id)) > 1
        }
    )
    if duplicate_keys:
        raise ValueError(f"Cargo discovery returned duplicate native IDs: {duplicate_keys!r}")

    discovered_by_key = {
        (item.cargo_target, item.native_id): index
        for index, item in enumerate(discovered)
    }
    consumed: set[int] = set()
    resolved: dict[str, ResolvedTest] = {}

    for spec in TESTS:
        if spec.kind not in {"rust-lib", "rust-bin"}:
            continue
        assert spec.native_id is not None
        key = (spec.cargo_target, spec.native_id)
        index = discovered_by_key.get(key)
        if index is None:
            raise ValueError(
                f"Cargo discovery omitted ordinary native test {spec.native_id!r}"
            )
        if index in consumed:
            raise ValueError(
                f"ordinary native test {spec.native_id!r} has an ambiguous mapping"
            )
        consumed.add(index)
        resolved[spec.id] = ResolvedTest(
            spec=spec,
            native_id=spec.native_id,
            selection=spec.native_id,
            exact=True,
        )

    ui_specs = [spec for spec in TESTS if spec.kind == "dylint-ui"]
    if not ui_specs:
        raise ValueError("Dylint UI semantic mapping selected zero cases")
    ui_native_keys = {(spec.cargo_target, spec.native_id) for spec in ui_specs}
    if len(ui_native_keys) != 1:
        raise ValueError("Dylint UI semantic cases do not share exactly one native test")
    ui_target, ui_native_id = next(iter(ui_native_keys))
    assert ui_native_id is not None
    ui_index = discovered_by_key.get((ui_target, ui_native_id))
    if ui_index is None:
        raise ValueError("Cargo discovery omitted the Dylint UI native test")
    if ui_index in consumed:
        raise ValueError("the Dylint UI native test has an ambiguous mapping")
    consumed.add(ui_index)
    for spec in ui_specs:
        resolved[spec.id] = ResolvedTest(
            spec=spec,
            native_id=ui_native_id,
            selection=ui_native_id,
            exact=True,
        )

    doc_specs = [spec for spec in TESTS if spec.kind == "rust-doctest"]
    doc_discovered = [
        (index, item)
        for index, item in enumerate(discovered)
        if item.kind == "rust-doctest"
    ]
    discovered_doc_ids = [item.native_id for _, item in doc_discovered]
    docs_by_item: dict[str, list[tuple[int, int, str]]] = {}
    for index, item in doc_discovered:
        match = re.fullmatch(
            r"src[\\/]lib\.rs - ([A-Z][A-Z0-9_]*) \(line ([1-9][0-9]*)\)",
            item.native_id,
        )
        if match is None:
            raise ValueError(
                f"Cargo discovery returned an unmappable doctest {item.native_id!r}"
            )
        docs_by_item.setdefault(match.group(1), []).append(
            (int(match.group(2)), index, item.native_id)
        )

    expected_docs: dict[str, list[TestSpec]] = {}
    for spec in doc_specs:
        assert spec.doctest_item is not None
        expected_docs.setdefault(spec.doctest_item, []).append(spec)
    if set(docs_by_item) != set(expected_docs):
        missing = sorted(set(expected_docs) - set(docs_by_item))
        extra = sorted(set(docs_by_item) - set(expected_docs))
        raise ValueError(
            f"doctest discovery did not match the semantic mapping; "
            f"missing={missing!r}, extra={extra!r}"
        )
    for item, specs in expected_docs.items():
        native_tests = sorted(docs_by_item[item])
        ordered_specs = sorted(specs, key=lambda spec: int(spec.doctest_ordinal))
        if len(native_tests) != len(ordered_specs):
            raise ValueError(
                f"doctest {item!r} discovery count did not match its semantic mapping"
            )
        for spec, (_, index, native_id) in zip(ordered_specs, native_tests):
            selection = _doctest_filter(native_id, discovered_doc_ids)
            if selection is None:
                raise ValueError(
                    f"doctest {native_id!r} has no complete unique native filter"
                )
            consumed.add(index)
            resolved[spec.id] = ResolvedTest(
                spec=spec,
                native_id=native_id,
                selection=selection,
                exact=False,
            )

    unconsumed = [
        f"{item.kind}::{item.native_id}"
        for index, item in enumerate(discovered)
        if index not in consumed
    ]
    if unconsumed:
        raise ValueError(f"Cargo discovery returned extra native tests: {unconsumed!r}")
    if len(resolved) != len(TESTS):
        raise ValueError("native test reconciliation did not resolve every semantic test")
    return [resolved[spec.id] for spec in TESTS]


def _run_one(
    resolved: ResolvedTest,
    *,
    cargo: Sequence[str],
    env: dict[str, str],
) -> dict[str, Any]:
    spec = resolved.spec
    native_id = resolved.native_id
    exact_argument = ["--exact"] if resolved.exact else []
    command = [
        *cargo,
        "test",
        "--offline",
        *spec.cargo_target,
        "--",
        resolved.selection,
        *exact_argument,
        "--format=json",
        "-Z",
        "unstable-options",
        "--show-output",
    ]
    child_env = dict(env)
    if spec.ui_case is not None:
        child_env[UI_CASE_ENV] = spec.ui_case
    else:
        child_env.pop(UI_CASE_ENV, None)

    completed = _run_command(command, env=child_env)
    events = _parse_test_events(completed.stdout)
    started = [event for event in events if event.get("event") == "started"]
    finished = [
        event for event in events if event.get("event") in {"ok", "failed", "ignored"}
    ]
    exactly_started = len(started) == 1 and started[0].get("name") == native_id
    exactly_finished = len(finished) == 1 and finished[0].get("name") == native_id
    dylint_infrastructure_error = spec.kind == "dylint-ui" and any(
        "could not load library" in str(event.get("stdout", "")) for event in finished
    )
    passed = (
        completed.returncode == 0
        and exactly_started
        and exactly_finished
        and finished[0].get("event") == "ok"
    )
    confirmed_failure = (
        exactly_started
        and exactly_finished
        and finished[0].get("event") == "failed"
        and not dylint_infrastructure_error
    )
    if passed:
        classification = "confirmed_pass"
    elif confirmed_failure:
        classification = "confirmed_validation_failure"
    else:
        classification = "pre_result_error"

    return {
        "id": spec.id,
        "native_id": native_id,
        "command": command,
        "exit_code": completed.returncode,
        "executed": exactly_started,
        "classification": classification,
        "stdout": completed.stdout,
        "stderr": completed.stderr,
    }


def _list_tests(*, cargo: Sequence[str], base_env: dict[str, str]) -> int:
    try:
        resolved = _reconcile_inventory(cargo, base_env)
    except ValueError as error:
        return _selection_error([], str(error))
    _emit(
        {
            "schema_version": SCHEMA_VERSION,
            "report_type": "ArgumentCommentLintNativeTestInventoryV1",
            "count": len(resolved),
            "tests": [test.listed() for test in resolved],
        }
    )
    return 0


def _run_tests(
    intended: Sequence[str], *, cargo: Sequence[str], base_env: dict[str, str]
) -> int:
    try:
        _assert_static_inventory()
    except ValueError as error:
        return _selection_error(intended, str(error))
    if not intended:
        return _selection_error(intended, "run requires at least one --test ID")
    if len(set(intended)) != len(intended):
        return _selection_error(intended, "duplicate --test IDs are not allowed")

    by_id = {spec.id: spec for spec in TESTS}
    unknown = [test_id for test_id in intended if test_id not in by_id]
    if unknown:
        return _selection_error(intended, f"unknown native test IDs: {unknown!r}")

    try:
        reconciled = _reconcile_inventory(cargo, base_env)
    except ValueError as error:
        return _selection_error(intended, str(error))
    resolved_by_id = {test.spec.id: test for test in reconciled}
    selected = [resolved_by_id[test_id] for test_id in intended]
    outcomes = [
        _run_one(
            test,
            cargo=cargo,
            env=base_env,
        )
        for test in selected
    ]

    classifications = {outcome["classification"] for outcome in outcomes}
    if "confirmed_validation_failure" in classifications:
        result = "confirmed_validation_failure"
        exit_code = 1
    elif "pre_result_error" in classifications:
        result = "pre_result_error"
        exit_code = 2
    else:
        result = "confirmed_pass"
        exit_code = 0
    executed = [outcome["id"] for outcome in outcomes if outcome["executed"]]
    _emit(
        {
            "schema_version": SCHEMA_VERSION,
            "report_type": REPORT_TYPE,
            "intended_validation_ids": list(intended),
            "selected_validation_ids": [test.spec.id for test in selected],
            "actually_executed_validation_ids": executed,
            "outcomes": outcomes,
            "result": result,
        }
    )
    return exit_code


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="List or run exact native argument-comment-lint test leaves."
    )
    parser.add_argument("--cargo")
    parser.add_argument("--cargo-arg", action="append", default=[])
    subparsers = parser.add_subparsers(dest="operation", required=True)
    subparsers.add_parser("list")
    run = subparsers.add_parser("run")
    run.add_argument("--test", action="append", default=[])
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    cargo = (
        [args.cargo, *args.cargo_arg]
        if args.cargo is not None
        else ["rustup", "run", PINNED_TOOLCHAIN, "cargo", *args.cargo_arg]
    )
    base_env = os.environ.copy()
    if args.operation == "list":
        return _list_tests(cargo=cargo, base_env=base_env)
    return _run_tests(
        args.test,
        cargo=cargo,
        base_env=base_env,
    )


if __name__ == "__main__":
    raise SystemExit(main())
