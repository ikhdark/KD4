#!/usr/bin/env python3
"""Validate the terminal fan-in against one classifier decision."""

from __future__ import annotations

import json
import os
import re
from collections.abc import Mapping


JOB_ENV = {
    "blob-size-policy": "RUN_BLOB_SIZE_POLICY",
    "cargo-deny": "RUN_CARGO_DENY",
    "cargo-full": "RUN_CARGO_FULL",
    "codespell": "RUN_CODESPELL",
    "repo-checks": "RUN_REPO_CHECKS",
    "rust-ci": "RUN_RUST_CI",
    "sdk": "RUN_SDK",
}
DECISION_ID_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")


def parse_expected_values(environment: Mapping[str, str]) -> dict[str, bool]:
    expected: dict[str, bool] = {}
    for job, name in JOB_ENV.items():
        value = environment.get(name)
        if value not in {"true", "false"}:
            raise ValueError(f"{name} must be exactly true or false, got {value!r}")
        expected[job] = value == "true"
    return expected


def validate(
    needs: dict[str, dict[str, object]],
    decision_id: str,
    expected: dict[str, bool],
) -> list[str]:
    failures: list[str] = []
    if DECISION_ID_RE.fullmatch(decision_id) is None:
        failures.append(f"invalid decision id: {decision_id!r}")
    classify = needs.get("classify")
    if not isinstance(classify, dict) or classify.get("result") != "success":
        failures.append("classify did not succeed")
    else:
        outputs = classify.get("outputs")
        classified_id = outputs.get("decision_id") if isinstance(outputs, dict) else None
        if classified_id != decision_id:
            failures.append(
                f"classify decision mismatch: expected {decision_id}, got {classified_id}"
            )
        if isinstance(outputs, dict):
            expected_output_names = {
                f"run_{job.replace('-', '_')}" for job in expected
            }
            actual_output_names = {
                name for name in outputs if name.startswith("run_")
            }
            for extra in sorted(actual_output_names - expected_output_names):
                failures.append(f"classify exposed unexpected output {extra}")
            for job, should_run in expected.items():
                name = f"run_{job.replace('-', '_')}"
                value = outputs.get(name)
                if value not in {"true", "false"}:
                    failures.append(
                        f"classify output {name} must be true or false, got {value!r}"
                    )
                elif (value == "true") != should_run:
                    failures.append(
                        f"classify output {name} disagrees with terminal expectation"
                    )
        else:
            failures.append("classify outputs are missing")

    for job, should_run in expected.items():
        dependency = needs.get(job)
        if not isinstance(dependency, dict):
            failures.append(f"{job}: missing dependency result")
            continue
        result = dependency.get("result")
        outputs = dependency.get("outputs")
        consumed = (
            outputs.get("consumed_decision_id") if isinstance(outputs, dict) else None
        )
        if should_run:
            if result != "success":
                failures.append(f"{job}: expected success, got {result}")
            if consumed != decision_id:
                failures.append(
                    f"{job}: stale or missing decision id (expected {decision_id}, got {consumed})"
                )
        elif result != "skipped":
            failures.append(f"{job}: expected skipped, got {result}")
    return failures


def main() -> None:
    needs = json.loads(os.environ["NEEDS"])
    decision_id = os.environ["DECISION_ID"]
    try:
        expected = parse_expected_values(os.environ)
    except ValueError as error:
        print(f"CI terminal decision failed:\n- {error}")
        raise SystemExit(1) from error
    failures = validate(needs, decision_id, expected)
    if failures:
        print("CI terminal decision failed:")
        for failure in failures:
            print(f"- {failure}")
        raise SystemExit(1)
    print(f"All CI jobs matched {decision_id}.")


if __name__ == "__main__":
    main()
