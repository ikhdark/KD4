#!/usr/bin/env python3
"""Verify and consume a Rust-produced CI decision artifact."""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--decision-id", required=True)
    parser.add_argument("--workflow", required=True)
    return parser.parse_args()


def consume(artifact: Path, expected_id: str, workflow_id: str) -> dict[str, object]:
    payload = artifact.read_bytes()
    actual_id = "sha256:" + hashlib.sha256(payload).hexdigest()
    if not hmac.compare_digest(actual_id, expected_id):
        raise SystemExit(
            f"decision artifact hash mismatch: expected {expected_id}, actual {actual_id}"
        )
    body = json.loads(payload)
    if not isinstance(body, dict) or "decision_id" in body:
        raise SystemExit("decision artifact has an invalid body contract")
    workflows = body.get("workflows")
    if not isinstance(workflows, list):
        raise SystemExit("decision artifact has no workflow decisions")
    decision = next(
        (
            entry
            for entry in workflows
            if isinstance(entry, dict) and entry.get("id") == workflow_id
        ),
        None,
    )
    if decision is None:
        raise SystemExit(f"decision artifact does not define workflow {workflow_id}")
    if decision.get("run") is not True:
        raise SystemExit(f"workflow {workflow_id} was not marked to run")
    matrix = body.get("matrix")
    if not isinstance(matrix, dict):
        raise SystemExit("decision artifact has no matrix plan")
    return {
        "consumed_decision_id": expected_id,
        "rust_matrix": matrix.get("rust_packages", []),
        "rust_shards": matrix.get("rust_shards", []),
    }


def append_output(name: str, value: str) -> None:
    output = os.environ.get("GITHUB_OUTPUT")
    if not output:
        return
    if "\n" in value or "\r" in value:
        raise SystemExit(f"output {name} contains a newline")
    with Path(output).open("a", encoding="utf-8", newline="\n") as handle:
        handle.write(f"{name}={value}\n")


def main() -> None:
    args = parse_args()
    consumed = consume(args.artifact, args.decision_id, args.workflow)
    append_output("consumed_decision_id", str(consumed["consumed_decision_id"]))
    append_output(
        "rust_matrix",
        json.dumps(consumed["rust_matrix"], separators=(",", ":")),
    )
    append_output(
        "rust_shards",
        json.dumps(consumed["rust_shards"], separators=(",", ":")),
    )


if __name__ == "__main__":
    main()
