#!/usr/bin/env python3
"""Validate the terminal fan-in against one classifier decision."""

from __future__ import annotations

import json
import os


JOB_ENV = {
    "blob-size-policy": "RUN_BLOB_SIZE_POLICY",
    "cargo-deny": "RUN_CARGO_DENY",
    "cargo-full": "RUN_CARGO_FULL",
    "codespell": "RUN_CODESPELL",
    "repo-checks": "RUN_REPO_CHECKS",
    "rust-ci": "RUN_RUST_CI",
    "sdk": "RUN_SDK",
}


def validate(
    needs: dict[str, dict[str, object]],
    decision_id: str,
    expected: dict[str, bool],
) -> list[str]:
    failures: list[str] = []
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
    expected = {
        job: os.environ[environment].lower() == "true"
        for job, environment in JOB_ENV.items()
    }
    failures = validate(needs, decision_id, expected)
    if failures:
        print("CI terminal decision failed:")
        for failure in failures:
            print(f"- {failure}")
        raise SystemExit(1)
    print(f"All CI jobs matched {decision_id}.")


if __name__ == "__main__":
    main()
