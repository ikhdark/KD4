#!/usr/bin/env python3
"""Verify and consume a Rust-produced CI decision artifact."""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import hmac
import json
import os
import re
from pathlib import Path

WORKFLOWS = {
    "blob-size-policy",
    "cargo-deny",
    "cargo-full",
    "codespell",
    "repo-checks",
    "rust-ci",
    "sdk",
}
BODY_KEYS = {
    "schema_version",
    "event",
    "comparison_mode",
    "base",
    "merge_base",
    "head",
    "changes",
    "full_fallback",
    "fallback_reasons",
    "workflows",
    "affected_packages",
    "reverse_closure",
    "matrix",
}
DECISION_ID_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
OID_RE = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--decision-id", required=True)
    parser.add_argument("--workflow", required=True)
    return parser.parse_args()


def consume(artifact: Path, expected_id: str, workflow_id: str) -> dict[str, object]:
    if DECISION_ID_RE.fullmatch(expected_id) is None:
        raise SystemExit(f"invalid decision id: {expected_id!r}")
    payload = artifact.read_bytes()
    actual_id = "sha256:" + hashlib.sha256(payload).hexdigest()
    if not hmac.compare_digest(actual_id, expected_id):
        raise SystemExit(
            f"decision artifact hash mismatch: expected {expected_id}, actual {actual_id}"
        )
    try:
        body = json.loads(payload, object_pairs_hook=unique_object)
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError) as error:
        raise SystemExit(f"decision artifact is not strict JSON: {error}") from error
    validate_body(body)
    workflows = body.get("workflows")
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
    return {
        "consumed_decision_id": expected_id,
        "rust_matrix": matrix.get("rust_packages", []),
        "rust_shards": matrix.get("rust_shards", []),
    }


def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def validate_body(body: object) -> None:
    if not isinstance(body, dict) or set(body) != BODY_KEYS:
        raise SystemExit("decision artifact has an invalid body field set")
    if body["schema_version"] != 1:
        raise SystemExit("decision artifact has an unsupported schema version")
    if not isinstance(body["event"], str) or not body["event"]:
        raise SystemExit("decision artifact event is invalid")
    if body["comparison_mode"] not in {
        "worktree",
        "explicit_paths",
        "direct_commit_diff",
        "pull_request_merge_base",
    }:
        raise SystemExit("decision artifact comparison mode is invalid")
    for name in ["base", "merge_base", "head"]:
        value = body[name]
        if value is not None and (
            not isinstance(value, str) or OID_RE.fullmatch(value) is None
        ):
            raise SystemExit(f"decision artifact {name} is invalid")
    if type(body["full_fallback"]) is not bool:
        raise SystemExit("decision artifact fallback flag is invalid")
    validate_string_list(body, "fallback_reasons")
    validate_string_list(body, "affected_packages")
    validate_string_list(body, "reverse_closure")
    if body["fallback_reasons"] != sorted(set(body["fallback_reasons"])):
        raise SystemExit("decision artifact fallback reasons are not canonical")
    for name in ["affected_packages", "reverse_closure"]:
        if body[name] != sorted(set(body[name])):
            raise SystemExit(f"decision artifact {name} is not canonical")

    changes = body["changes"]
    if not isinstance(changes, list):
        raise SystemExit("decision artifact changes are invalid")
    for change in changes:
        if not isinstance(change, dict) or set(change) != {
            "status",
            "path",
            "original_path",
            "staged",
            "unstaged",
            "submodule_state",
        }:
            raise SystemExit("decision artifact change record is invalid")
        if not isinstance(change["status"], str) or not change["status"]:
            raise SystemExit("decision artifact change status is invalid")
        validate_raw_path(change["path"])
        if change["original_path"] is not None:
            validate_raw_path(change["original_path"])
        if type(change["staged"]) is not bool or type(change["unstaged"]) is not bool:
            raise SystemExit("decision artifact change flags are invalid")
        if change["submodule_state"] is not None and not isinstance(
            change["submodule_state"], str
        ):
            raise SystemExit("decision artifact submodule state is invalid")

    workflows = body["workflows"]
    if not isinstance(workflows, list):
        raise SystemExit("decision artifact has no workflow decisions")
    workflow_ids: set[str] = set()
    for decision in workflows:
        if (
            not isinstance(decision, dict)
            or set(decision) != {"id", "run"}
            or decision.get("id") not in WORKFLOWS
            or decision["id"] in workflow_ids
            or type(decision.get("run")) is not bool
        ):
            raise SystemExit("decision artifact workflow set is invalid")
        workflow_ids.add(decision["id"])
    if workflow_ids != WORKFLOWS:
        raise SystemExit("decision artifact workflow set is incomplete")

    matrix = body["matrix"]
    if not isinstance(matrix, dict) or set(matrix) != {
        "rust_packages",
        "rust_shards",
    }:
        raise SystemExit("decision artifact has no valid matrix plan")
    validate_string_list(matrix, "rust_packages")
    validate_string_list(matrix, "rust_shards")
    if len(matrix["rust_packages"]) > 128 or len(matrix["rust_shards"]) > 32:
        raise SystemExit("decision artifact matrix exceeds entry limits")
    if (
        len(
            json.dumps(
                matrix["rust_packages"], ensure_ascii=False, separators=(",", ":")
            ).encode()
        )
        > 32 * 1024
    ):
        raise SystemExit("decision artifact Rust matrix exceeds its byte limit")
    if body["full_fallback"] and (
        not all(decision["run"] for decision in workflows)
        or matrix["rust_packages"]
        or matrix["rust_shards"] != ["workspace"]
    ):
        raise SystemExit("decision artifact fallback invariants are invalid")


def validate_string_list(container: dict[str, object], name: str) -> None:
    value = container[name]
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise SystemExit(f"decision artifact {name} is invalid")


def validate_raw_path(value: object) -> None:
    if not isinstance(value, dict) or set(value) != {"utf8", "bytes_base64"}:
        raise SystemExit("decision artifact raw path is invalid")
    utf8 = value["utf8"]
    encoded = value["bytes_base64"]
    if utf8 is not None and not isinstance(utf8, str):
        raise SystemExit("decision artifact raw path text is invalid")
    if not isinstance(encoded, str):
        raise SystemExit("decision artifact raw path bytes are invalid")
    try:
        raw = base64.b64decode(encoded, validate=True)
    except (binascii.Error, ValueError) as error:
        raise SystemExit("decision artifact raw path base64 is invalid") from error
    if utf8 is not None and raw != utf8.encode():
        raise SystemExit("decision artifact raw path encodings disagree")
    if (
        not raw
        or b"\0" in raw
        or raw.startswith((b"/", b"\\"))
        or (len(raw) > 1 and raw[1:2] == b":")
        or any(part in {b"", b".."} for part in raw.split(b"/"))
    ):
        raise SystemExit("decision artifact raw path is not repository-relative")


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
