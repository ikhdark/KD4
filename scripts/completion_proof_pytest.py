#!/usr/bin/env python3
"""Discover and run the Python SDK pytest suite with structured proof output."""

from __future__ import annotations

import argparse
import json
import os
import time
import uuid
from pathlib import Path
from typing import Any, Sequence

import pytest


def _write(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


class ProofPlugin:
    def __init__(
        self,
        *,
        checkpoint_output: Path | None = None,
        intended: list[str] | None = None,
        proof_binding: dict[str, str] | None = None,
    ) -> None:
        self.collected: list[dict[str, object]] = []
        self.collection_errors: list[str] = []
        self.reports: dict[str, list[dict[str, object]]] = {}
        self.started_at: dict[str, float] = {}
        self.started: list[str] = []
        self.terminal: list[str] = []
        self.checkpoint_output = checkpoint_output
        self.intended = intended or []
        self.proof_binding = proof_binding or {}

    def pytest_collection_finish(self, session: pytest.Session) -> None:
        for item in session.items:
            skip_markers = []
            for marker in item.iter_markers():
                if marker.name in {"skip", "skipif"}:
                    skip_markers.append(
                        {
                            "name": marker.name,
                            "reason": str(marker.kwargs.get("reason", "")),
                        }
                    )
            self.collected.append({"id": item.nodeid, "skip_markers": skip_markers})
        self.checkpoint()

    def pytest_collectreport(self, report: pytest.CollectReport) -> None:
        if report.failed:
            self.collection_errors.append(str(report.longrepr))

    def pytest_runtest_logstart(self, nodeid: str, location: object) -> None:
        self.started.append(nodeid)
        self.started_at[nodeid] = time.monotonic()

    def pytest_runtest_logfinish(self, nodeid: str, location: object) -> None:
        self.terminal.append(nodeid)
        self.checkpoint()

    def pytest_runtest_logreport(self, report: pytest.TestReport) -> None:
        self.reports.setdefault(report.nodeid, []).append(
            {
                "phase": report.when,
                "outcome": report.outcome,
                "wasxfail": bool(getattr(report, "wasxfail", False)),
            }
        )
        self.checkpoint()

    def outcome_for(self, nodeid: str) -> str:
        reports = self.reports.get(nodeid, [])
        if any(
            report["phase"] == "call" and report["outcome"] == "failed"
            for report in reports
        ):
            return "failed"
        if any(report["outcome"] == "skipped" for report in reports):
            return "skipped"
        if any(
            report["phase"] == "call" and report["outcome"] == "passed"
            for report in reports
        ):
            return "passed"
        return "unknown"

    def has_phase_error(self, nodeid: str) -> bool:
        reports = self.reports.get(nodeid, [])
        phases = [str(report["phase"]) for report in reports]
        if sorted(phases) != ["call", "setup", "teardown"]:
            return True
        return any(
            report["phase"] in {"setup", "teardown"} and report["outcome"] != "passed"
            for report in reports
        )

    def checkpoint(self) -> None:
        if self.checkpoint_output is None:
            return
        selected = [str(item["id"]) for item in self.collected]
        outcomes = [
            {"id": test_id, "outcome": self.outcome_for(test_id)}
            for test_id in self.terminal
            if test_id in self.reports
        ]
        _write(
            self.checkpoint_output,
            {
                "schema_version": 2,
                "report_type": "CompletionProofStructuredTestReportV2",
                "framework": "python-pytest",
                **self.proof_binding,
                "classification": "pre_result_error",
                "intended_ids": self.intended,
                "selected_ids": selected,
                "started_ids": self.started,
                "terminal_ids": self.terminal,
                "executed_ids": [str(item["id"]) for item in outcomes],
                "outcomes": outcomes,
                "selection_confirmed": selected == self.intended,
                "checkpoint": True,
            },
        )


def _collect(output: Path) -> int:
    plugin = ProofPlugin()
    exit_code = int(pytest.main(["--collect-only", "-q", "tests"], plugins=[plugin]))
    ids = [str(item["id"]) for item in plugin.collected]
    duplicates = sorted({test_id for test_id in ids if ids.count(test_id) > 1})
    classification = (
        "discovered"
        if exit_code == 0 and ids and not duplicates and not plugin.collection_errors
        else "pre_result_error"
    )
    _write(
        output,
        {
            "schema_version": 1,
            "framework": "python-pytest",
            "classification": classification,
            "tests": plugin.collected,
            "selected_count": len(ids),
            "duplicate_ids": duplicates,
            "collection_errors": plugin.collection_errors,
            "pytest_exit_code": exit_code,
        },
    )
    return 0 if classification == "discovered" else 2


def _run(
    expected_file: Path,
    output: Path,
    *,
    proof_binding: dict[str, str],
) -> int:
    value: Any = json.loads(expected_file.read_text(encoding="utf-8"))
    expected = value if isinstance(value, list) else value["ids"]
    expected = [str(item) for item in expected]
    if not expected:
        _write(
            output,
            {
                "schema_version": 2,
                "report_type": "CompletionProofStructuredTestReportV2",
                "framework": "python-pytest",
                **proof_binding,
                "classification": "pre_result_error",
                "intended_ids": [],
                "selected_ids": [],
                "started_ids": [],
                "terminal_ids": [],
                "executed_ids": [],
                "outcomes": [],
                "selection_confirmed": False,
                "error": "zero intended pytest selection",
            },
        )
        return 2

    plugin = ProofPlugin(
        checkpoint_output=output,
        intended=expected,
        proof_binding=proof_binding,
    )
    exit_code = int(pytest.main(["-q", *expected], plugins=[plugin]))
    selected = [str(item["id"]) for item in plugin.collected]
    selection_confirmed = (
        not plugin.collection_errors
        and selected == expected
        and len(selected) == len(set(selected))
    )
    outcomes = [
        {"id": test_id, "outcome": plugin.outcome_for(test_id)}
        for test_id in plugin.terminal
        if test_id in plugin.reports
    ]
    executed = list(plugin.terminal)
    has_failure = any(item["outcome"] == "failed" for item in outcomes)
    has_non_result = any(item["outcome"] in {"skipped", "unknown"} for item in outcomes)
    has_phase_error = any(plugin.has_phase_error(test_id) for test_id in selected)
    complete_execution = (
        plugin.started == expected
        and plugin.terminal == expected
        and executed == expected
    )
    if (
        not selection_confirmed
        or not complete_execution
        or has_non_result
        or has_phase_error
        or exit_code not in {0, 1}
    ):
        classification = "pre_result_error"
    elif has_failure and exit_code == 1:
        classification = "confirmed_validation_failure"
    elif not has_failure and exit_code == 0:
        classification = "confirmed_pass"
    else:
        classification = "pre_result_error"
    _write(
        output,
        {
            "schema_version": 2,
            "report_type": "CompletionProofStructuredTestReportV2",
            "framework": "python-pytest",
            **proof_binding,
            "classification": classification,
            "intended_ids": expected,
            "selected_ids": selected,
            "started_ids": plugin.started,
            "terminal_ids": plugin.terminal,
            "executed_ids": executed,
            "outcomes": outcomes,
            "selection_confirmed": selection_confirmed,
            "intended_count": len(expected),
            "selected_count": len(selected),
            "executed_count": len(executed),
            "collection_errors": plugin.collection_errors,
            "pytest_exit_code": exit_code,
            "phase_error_ids": sorted(
                test_id for test_id in selected if plugin.has_phase_error(test_id)
            ),
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
