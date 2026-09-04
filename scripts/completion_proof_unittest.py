#!/usr/bin/env python3
"""Discover and run root unittests with machine-verifiable execution results."""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import unittest
import uuid
from pathlib import Path
from typing import Iterable, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))


def _targets() -> list[str]:
    from scripts.root_maintenance import python_unittest_targets

    converted = []
    for target in python_unittest_targets():
        path = Path(target)
        if path.is_file() and target.lower().endswith(".py"):
            # Match ``python -m unittest`` file-name conversion.  The root
            # inventory intentionally contains one file target whose parent
            # directory has a dash, and importlib still accepts that converted
            # name when loading it explicitly.
            target = os.path.normpath(target)[:-3].replace("\\", ".").replace("/", ".")
        converted.append(target)
    return converted


def _flatten(suite: unittest.TestSuite) -> Iterable[unittest.TestCase]:
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from _flatten(item)
        else:
            yield item


def _load() -> tuple[list[unittest.TestCase], list[str]]:
    suite = unittest.defaultTestLoader.loadTestsFromNames(_targets())
    tests = list(_flatten(suite))
    discovery_errors = [
        test.id()
        for test in tests
        if test.__class__.__name__ == "_FailedTest"
        or test.__class__.__module__ == "unittest.loader"
    ]
    return tests, discovery_errors


def _write(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


class ProofResult(unittest.TextTestResult):
    def __init__(
        self,
        *args: object,
        expected: list[str],
        output: Path,
        proof_binding: dict[str, str],
        **kwargs: object,
    ) -> None:
        super().__init__(*args, **kwargs)
        self.expected = expected
        self.output = output
        self.proof_binding = proof_binding
        self.started: list[str] = []
        self.terminal: list[str] = []
        self.outcomes: dict[str, str] = {}
        self.started_at: dict[str, float] = {}
        self.durations_ms: dict[str, int] = {}

    def checkpoint(self) -> None:
        outcomes = [
            {
                "id": test_id,
                "outcome": self.outcomes.get(test_id, "unknown"),
                "duration_ms": self.durations_ms.get(test_id, 0),
            }
            for test_id in self.terminal
            if test_id in self.outcomes
        ]
        _write(
            self.output,
            {
                "schema_version": 2,
                "report_type": "CompletionProofStructuredTestReportV2",
                "framework": "python-unittest",
                **self.proof_binding,
                "classification": "pre_result_error",
                "intended_ids": self.expected,
                "selected_ids": self.expected,
                "started_ids": self.started,
                "terminal_ids": self.terminal,
                "executed_ids": [str(item["id"]) for item in outcomes],
                "outcomes": outcomes,
                "selection_confirmed": True,
                "checkpoint": True,
            },
        )

    def startTest(self, test: unittest.TestCase) -> None:  # noqa: N802
        test_id = test.id()
        self.started.append(test_id)
        self.started_at[test_id] = time.monotonic()
        super().startTest(test)

    def stopTest(self, test: unittest.TestCase) -> None:  # noqa: N802
        test_id = test.id()
        started_at = self.started_at.pop(test_id, time.monotonic())
        self.durations_ms[test_id] = max(
            0, round((time.monotonic() - started_at) * 1000)
        )
        super().stopTest(test)
        self.checkpoint()

    def record_outcome(self, test: unittest.TestCase, outcome: str) -> None:
        test_id = test.id()
        self.outcomes[test_id] = outcome
        if test_id not in self.terminal:
            self.terminal.append(test_id)

    def addSuccess(self, test: unittest.TestCase) -> None:  # noqa: N802
        self.record_outcome(test, "passed")
        super().addSuccess(test)
        self.checkpoint()

    def addFailure(self, test: unittest.TestCase, err: object) -> None:  # noqa: N802
        self.record_outcome(test, "failed")
        super().addFailure(test, err)
        self.checkpoint()

    def addError(self, test: unittest.TestCase, err: object) -> None:  # noqa: N802
        self.record_outcome(test, "failed")
        super().addError(test, err)
        self.checkpoint()

    def addSkip(self, test: unittest.TestCase, reason: str) -> None:  # noqa: N802
        self.record_outcome(test, "skipped")
        super().addSkip(test, reason)
        self.checkpoint()

    def addExpectedFailure(self, test: unittest.TestCase, err: object) -> None:  # noqa: N802
        self.record_outcome(test, "passed")
        super().addExpectedFailure(test, err)
        self.checkpoint()

    def addUnexpectedSuccess(self, test: unittest.TestCase) -> None:  # noqa: N802
        self.record_outcome(test, "failed")
        super().addUnexpectedSuccess(test)
        self.checkpoint()

    def addSubTest(  # noqa: N802
        self,
        test: unittest.TestCase,
        subtest: unittest.TestCase,
        err: object | None,
    ) -> None:
        if err is not None:
            self.record_outcome(test, "failed")
        super().addSubTest(test, subtest, err)
        if err is not None:
            self.checkpoint()


def _collect(output: Path) -> int:
    tests, discovery_errors = _load()
    rows = []
    seen: set[str] = set()
    duplicates: list[str] = []
    for test in tests:
        test_id = test.id()
        if test_id in seen:
            duplicates.append(test_id)
        seen.add(test_id)
        test_method = getattr(test, getattr(test, "_testMethodName", ""), None)
        rows.append(
            {
                "id": test_id,
                "skipped_at_discovery": bool(
                    getattr(test.__class__, "__unittest_skip__", False)
                    or getattr(test, "__unittest_skip__", False)
                    or getattr(test_method, "__unittest_skip__", False)
                ),
                "skip_reason": str(
                    getattr(test, "__unittest_skip_why__", "")
                    or getattr(test.__class__, "__unittest_skip_why__", "")
                    or getattr(test_method, "__unittest_skip_why__", "")
                ),
            }
        )
    classification = (
        "pre_result_error"
        if not rows or discovery_errors or duplicates
        else "discovered"
    )
    _write(
        output,
        {
            "schema_version": 1,
            "framework": "python-unittest",
            "classification": classification,
            "tests": rows,
            "selected_count": len(rows),
            "discovery_errors": discovery_errors,
            "duplicate_ids": sorted(set(duplicates)),
        },
    )
    return 0 if classification == "discovered" else 2


def _run(
    expected_file: Path,
    output: Path,
    *,
    proof_binding: dict[str, str],
) -> int:
    expected_value = json.loads(expected_file.read_text(encoding="utf-8"))
    expected = (
        expected_value if isinstance(expected_value, list) else expected_value["ids"]
    )
    expected = [str(item) for item in expected]
    tests, discovery_errors = _load()
    discovered = [test.id() for test in tests]
    tests_by_id = {test.id(): test for test in tests}
    selection_confirmed = (
        bool(expected)
        and not discovery_errors
        and len(discovered) == len(set(discovered))
        and len(expected) == len(set(expected))
        and all(test_id in tests_by_id for test_id in expected)
    )
    if not selection_confirmed:
        _write(
            output,
            {
                "schema_version": 2,
                "report_type": "CompletionProofStructuredTestReportV2",
                "framework": "python-unittest",
                **proof_binding,
                "classification": "pre_result_error",
                "intended_ids": expected,
                "selected_ids": [
                    test_id for test_id in expected if test_id in tests_by_id
                ],
                "started_ids": [],
                "terminal_ids": [],
                "executed_ids": [],
                "outcomes": [],
                "discovery_errors": discovery_errors,
                "selection_confirmed": False,
            },
        )
        return 2

    selected_tests = [tests_by_id[test_id] for test_id in expected]
    suite = unittest.TestSuite(selected_tests)
    _write(
        output,
        {
            "schema_version": 2,
            "report_type": "CompletionProofStructuredTestReportV2",
            "framework": "python-unittest",
            **proof_binding,
            "classification": "pre_result_error",
            "intended_ids": expected,
            "selected_ids": expected,
            "started_ids": [],
            "terminal_ids": [],
            "executed_ids": [],
            "outcomes": [],
            "selection_confirmed": True,
            "checkpoint": True,
        },
    )

    def result_factory(*args: object, **kwargs: object) -> ProofResult:
        return ProofResult(
            *args,
            expected=expected,
            output=output,
            proof_binding=proof_binding,
            **kwargs,
        )

    runner = unittest.TextTestRunner(
        stream=sys.stderr,
        verbosity=2,
        resultclass=result_factory,
    )
    result = runner.run(suite)
    assert isinstance(result, ProofResult)
    executed = result.terminal
    outcomes = [
        {
            "id": test_id,
            "outcome": result.outcomes.get(test_id, "unknown"),
            "duration_ms": result.durations_ms.get(test_id, 0),
        }
        for test_id in executed
    ]
    complete_execution = (
        result.started == expected
        and result.terminal == expected
        and executed == expected
        and all(
            result.outcomes.get(test_id) in {"passed", "failed", "skipped"}
            for test_id in expected
        )
    )
    has_failure = any(item["outcome"] == "failed" for item in outcomes)
    has_non_result = any(item["outcome"] in {"skipped", "unknown"} for item in outcomes)
    fixture_problem_ids = {
        problem.id()
        for problem, _ in [*result.errors, *result.failures]
        if problem.id() not in expected
    }
    if not complete_execution or has_non_result or fixture_problem_ids:
        classification = "pre_result_error"
    elif has_failure or not result.wasSuccessful():
        classification = "confirmed_validation_failure"
    else:
        classification = "confirmed_pass"
    _write(
        output,
        {
            "schema_version": 2,
            "report_type": "CompletionProofStructuredTestReportV2",
            "framework": "python-unittest",
            **proof_binding,
            "classification": classification,
            "intended_ids": expected,
            "selected_ids": expected,
            "started_ids": result.started,
            "terminal_ids": result.terminal,
            "executed_ids": executed,
            "outcomes": outcomes,
            "selection_confirmed": selection_confirmed,
            "intended_count": len(expected),
            "selected_count": len(expected),
            "executed_count": len(executed),
            "fixture_problem_ids": sorted(fixture_problem_ids),
        },
    )
    return 0 if classification == "confirmed_pass" else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    collect = subparsers.add_parser("collect")
    collect.add_argument("--output", required=True, type=Path)
    run = subparsers.add_parser("run")
    run.add_argument("--expected-file", required=True, type=Path)
    run.add_argument("--output", required=True, type=Path)
    run.add_argument("--proof-attempt-id", required=True)
    run.add_argument("--proof-execution-id", required=True)
    run.add_argument("--proof-receipt-nonce", required=True)
    run.add_argument("--proof-scope", required=True, choices=("canonical", "focused"))
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.command == "collect":
        return _collect(args.output)
    if args.command == "run":
        return _run(
            args.expected_file,
            args.output,
            proof_binding={
                "proof_attempt_id": args.proof_attempt_id,
                "proof_execution_id": args.proof_execution_id,
                "proof_receipt_nonce": args.proof_receipt_nonce,
                "proof_scope": args.proof_scope,
            },
        )
    raise AssertionError(args.command)


if __name__ == "__main__":
    raise SystemExit(main())
