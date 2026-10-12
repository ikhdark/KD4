"""Capture execution evidence from explicit rollouts without inferring missing timings.

The report retains complete selected payloads and source record numbers. All
records are decoded and all external payload references are hydrated; omitted
record classes (instructions, manifests, private reasoning) are counted only.
Unlike timing aggregation, this does not require every checkpoint to carry a
sampling identity and does not treat missing fields as zero.
"""
from __future__ import annotations

import argparse
from collections import Counter
import json
from pathlib import Path
import sys

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.benchmark_scripts.rollout_snapshot import hydrate_rollout_record, read_rollout_snapshot


def capture(path):
    counts, observations, errors = Counter(), [], []
    hydrated = 0
    current_turn = None
    with read_rollout_snapshot(path) as snapshot:
        for line, row, error, _ in snapshot.decoded_lines():
            if error:
                errors.append({"line": line, "error": error})
                continue
            external = row.get("type") == "rollout_payload_artifact"
            try:
                row = hydrate_rollout_record(row, snapshot.path)
            except (OSError, ValueError, KeyError, TypeError) as error:
                errors.append({"line": line, "error": str(error)})
                continue
            hydrated += int(external)
            payload = row.get("payload", {})
            kind = row.get("type")
            subtype = payload.get("type", "")
            counts[f"{kind}:{subtype}"] += 1
            if subtype == "task_started":
                current_turn = payload.get("turn_id")
            selected = (
                kind in {"session_meta", "sampling_boundary"}
                or kind == "event_msg" and subtype in {
                    "task_started", "task_complete", "turn_aborted", "user_message",
                    "item_started", "item_completed", "warning", "error", "patch_apply_end",
                }
                or kind == "response_item" and (
                    subtype in {"custom_tool_call", "custom_tool_call_output", "function_call",
                                "function_call_output", "agent_message"}
                    or subtype == "message" and payload.get("role") == "assistant"
                )
            )
            if kind == "session_meta":
                payload = {key: payload.get(key) for key in (
                    "session_id", "id", "timestamp", "cwd", "originator", "cli_version",
                    "harness_build", "source", "thread_source",
                )}
            if selected:
                observations.append({"line": line, "timestamp": row.get("timestamp"),
                                     "turn_id": payload.get("turn_id", current_turn),
                                     "kind": kind, "payload": payload})
        return {"path": str(snapshot.path), "sha256": snapshot.sha256,
                "bytes": snapshot.byte_length, "records": sum(counts.values()) + len(errors),
                "hydrated_payloads": hydrated, "counts": dict(counts), "errors": errors,
                "observations": observations}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", type=Path, nargs="+")
    parser.add_argument("--output", type=Path, required=True, help="New JSON evidence report; never overwrite")
    args = parser.parse_args(argv)
    # Reserve the exact new report path before any scan. A failed capture leaves
    # an explicit partial report rather than pretending all inputs were read.
    with args.output.open("x", encoding="utf-8") as output:
        sessions = []
        for path in args.paths:
            try:
                sessions.append(capture(path))
            except (OSError, ValueError) as error:
                sessions.append({"path": str(path), "errors": [{"error": str(error)}]})
        report = {"kind": "execution_lifecycle_evidence_v1", "sessions": sessions,
                  "complete": all(not session["errors"] for session in sessions)}
        json.dump(report, output, ensure_ascii=False)
        output.write("\n")
    print(json.dumps({"output": str(args.output), "complete": report["complete"],
                      "sessions": [{key: value for key, value in session.items()
                                    if key != "observations"} for session in sessions]}, indent=2))
    return 0 if report["complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
