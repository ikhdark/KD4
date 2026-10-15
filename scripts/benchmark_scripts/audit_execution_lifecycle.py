"""Capture execution evidence from explicit rollouts without inferring missing timings.

By default the report retains complete selected payloads and source record numbers.
Use --compact for lifecycle projections and content-addressed source references. All
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


def _projection(payload):
    """Allowlisted lifecycle data; never retain arbitrary message/tool bodies."""
    fields = {
        "type", "turn_id", "thread_id", "session_id", "id", "call_id", "item_id",
        "sampling_request_id", "samplingRequestId", "request_id", "generation_index",
        "generationIndex", "timestamp", "started_at", "completed_at", "status",
        "name", "resolved_tool_name", "tool_purpose", "role", "channel",
        "success", "exit_code", "exitCode", "isError", "outcome",
    }
    result = {key: value for key, value in payload.items()
              if key in fields and isinstance(value, (str, int, float, bool, type(None)))}
    for key in ("counters", "milestones"):
        if isinstance(payload.get(key), dict):
            result[key] = {name: value for name, value in payload[key].items()
                           if type(value) in (int, float)}
    if isinstance(payload.get("timing"), dict):
        timing = payload["timing"]
        result["timing"] = {key: timing[key] for key in ("schemaVersion", "profileValid", "classificationComplete")
                            if key in timing and type(timing[key]) in (int, bool)}
        result["timing"].update({key: value for key, value in _projection({
            key: timing[key] for key in ("milestones", "counters") if key in timing
        }).items()})
    if isinstance(payload.get("item"), dict):
        # Project one item envelope, not recursively nested arbitrary payloads.
        result["item"] = _projection({key: value for key, value in payload["item"].items() if key != "item"})
    return result


def capture(path, *, compact=False, error_example_limit=20):
    if type(error_example_limit) is not int or error_example_limit < 0:
        raise ValueError("error_example_limit must be a nonnegative integer")
    counts, observations, errors = Counter(), [], []
    error_count, error_lines = 0, []
    ambiguities, turns, open_turns = [], {}, {}
    replayed_calls, conflicting_calls = 0, set()
    hydrated = 0
    current_turn = None
    call_turns = {}
    call_payloads = {}

    def record_error(line, error):
        nonlocal error_count
        error_count += 1
        if error_lines and error_lines[-1][1] == line - 1:
            error_lines[-1][1] = line
        else:
            error_lines.append([line, line])
        if len(errors) < error_example_limit:
            errors.append({"line": line, "error": str(error)[:1000]})

    with read_rollout_snapshot(path) as snapshot:
        offset = 0
        for line, row, error, line_bytes in snapshot.decoded_lines():
            source_ref = {"sha256": snapshot.sha256, "line": line,
                          "expanded_byte_offset": offset, "expanded_bytes": line_bytes}
            offset += line_bytes
            if error:
                record_error(line, error)
                continue
            external = row.get("type") == "rollout_payload_artifact"
            if external and isinstance(row.get("payload"), dict):
                source_ref["external_payload"] = {key: row["payload"].get(key) for key in ("sha256", "bytes")}
            try:
                row = hydrate_rollout_record(row, snapshot.path)
            except (OSError, ValueError, KeyError, TypeError) as error:
                record_error(line, error)
                continue
            hydrated += int(external)
            payload = row.get("payload", {})
            kind = row.get("type")
            if not isinstance(payload, dict) or not isinstance(kind, str):
                record_error(line, "record type and payload must be string/object")
                continue
            subtype = payload.get("type", "")
            if not isinstance(subtype, str):
                record_error(line, "payload type must be a string")
                continue
            counts[f"{kind}:{subtype}"] += 1
            explicit_turn = payload.get("turn_id")
            explicit_turn = explicit_turn if isinstance(explicit_turn, str) and explicit_turn else None
            if kind == "event_msg" and subtype == "task_started":
                if open_turns:
                    displaced = current_turn if current_turn in open_turns else next(reversed(open_turns))
                    ambiguities.append({"category": "overlapping_turn_starts", "line": line,
                                        "displaced_turn": dict(open_turns[displaced]),
                                        "unresolved_turn_count": len(open_turns),
                                        "incoming_turn_id": explicit_turn})
                current_turn = line
                turns[line] = {"turn_id": explicit_turn, "start_line": line, "status": "unresolved"}
                open_turns[line] = turns[line]
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
            owner = explicit_turn
            if "turn_id" not in payload and len(open_turns) == 1:
                owner = next(iter(open_turns.values()))["turn_id"]
            call_id = payload.get("call_id")
            if kind == "response_item" and subtype in ("function_call", "custom_tool_call"):
                if isinstance(call_id, str) and call_id:
                    if call_id not in call_turns:
                        call_turns[call_id] = owner
                        call_payloads[call_id] = payload
                    elif (call_turns[call_id] == owner and call_id not in conflicting_calls
                          and call_payloads[call_id] == payload):
                        replayed_calls += 1
                    else:
                        conflicting_calls.add(call_id)
                        call_turns[call_id] = None
            elif kind == "response_item" and subtype in ("function_call_output", "custom_tool_call_output"):
                owner = call_turns.get(call_id) if isinstance(call_id, str) else None
                if payload.get("turn_id", owner) != owner:
                    owner = None
            if selected:
                lifecycle = _projection(payload)
                observations.append({"line": line, "timestamp": row.get("timestamp"),
                                     "turn_id": owner,
                                     "kind": kind, "payload": lifecycle if compact else payload,
                                     "lifecycle": lifecycle, "source_ref": source_ref})
            if kind == "event_msg" and subtype in ("task_complete", "turn_aborted"):
                matches = [(key, turn) for key, turn in open_turns.items() if turn["turn_id"] == owner]
                if len(matches) == 1:
                    key, turn = matches[0]
                    turn.update(status=subtype, terminal_line=line)
                    open_turns.pop(key)
                    if key == current_turn:
                        current_turn = None
        return {"path": str(snapshot.path), "sha256": snapshot.sha256,
                "mode": "compact" if compact else "full",
                "bytes": snapshot.byte_length, "records": sum(counts.values()) + error_count,
                "hydrated_payloads": hydrated, "counts": dict(counts), "errors": errors,
                "error_count": error_count, "error_example_limit": error_example_limit,
                "error_line_ranges": error_lines, "omitted_error_examples": error_count - len(errors),
                "ambiguities": ambiguities, "turns": list(turns.values()),
                "replayed_call_ids": replayed_calls, "conflicting_call_ids": sorted(conflicting_calls),
                "observations": observations}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", type=Path, nargs="+")
    parser.add_argument("--output", type=Path, required=True, help="New JSON evidence report; never overwrite")
    parser.add_argument("--compact", action="store_true", help="Retain lifecycle projections, not selected content bodies")
    parser.add_argument("--error-example-limit", type=int, default=20, help="Maximum retained parsing error examples (default: 20)")
    args = parser.parse_args(argv)
    if args.error_example_limit < 0:
        parser.error("--error-example-limit must be nonnegative")
    # Reserve the exact new report path before any scan. A failed capture leaves
    # an explicit partial report rather than pretending all inputs were read.
    with args.output.open("x", encoding="utf-8") as output:
        sessions = []
        for path in args.paths:
            try:
                sessions.append(capture(path, compact=args.compact, error_example_limit=args.error_example_limit))
            except (OSError, ValueError) as error:
                sessions.append({"path": str(path), "errors": [{"error": str(error)}]})
        report = {"kind": "execution_lifecycle_evidence_v1", "sessions": sessions,
                  "complete": all(not session.get("error_count", len(session["errors"])) for session in sessions)}
        json.dump(report, output, ensure_ascii=False)
        output.write("\n")
    print(json.dumps({"output": str(args.output), "complete": report["complete"],
                      "sessions": [{key: value for key, value in session.items()
                                    if key != "observations"} for session in sessions]}, indent=2))
    return 0 if report["complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
