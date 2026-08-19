#!/usr/bin/env python3
"""Audit model-side and tool execution latency in Codex rollout JSONL files."""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import io
import json
import os
import re
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

try:
    from scripts.rollout_snapshot import read_rollout_snapshot
except ImportError:
    from rollout_snapshot import read_rollout_snapshot


REPORT_SCHEMA_VERSION = 3

_NANOSECONDS_PER_SECOND = 1_000_000_000
_CHILD_WALL_TIME_PATTERN = re.compile(
    r'"wall_time_seconds"\s*:\s*([0-9]+(?:\.[0-9]+)?)'
)


def _path_key(path: str | os.PathLike[str]) -> str:
    value = os.path.normpath(os.fspath(path)).replace("\\", "/").rstrip("/")
    if ":" in value[:3] or "\\" in os.fspath(path):
        return value.casefold()
    return value


def _population(cwd: str, repo_root: str) -> str:
    cwd_key = _path_key(cwd)
    root_key = _path_key(repo_root)
    if cwd_key == root_key:
        return "repository_root"
    if cwd_key.startswith(f"{root_key}/.codex/evals/"):
        return "eval"
    return "other"


def _timestamp_ns(value: Any) -> int | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return int(parsed.timestamp() * _NANOSECONDS_PER_SECOND)


def _tool_output_text(payload: dict[str, Any]) -> str:
    output = payload.get("output")
    if isinstance(output, str):
        return output
    if not isinstance(output, list):
        return ""
    return "\n".join(
        str(item.get("text", ""))
        for item in output
        if isinstance(item, dict) and item.get("text") is not None
    )


def _command_orchestration_report(records: list[dict[str, Any]]) -> dict[str, Any]:
    covered = [record for record in records if record["reportedChildCalls"] > 0]
    round_trip_ns = sum(record["roundTripNs"] for record in covered)
    reported_child_work_ns = sum(record["reportedChildWorkNs"] for record in covered)
    orchestration_gap_lower_bound_ns = sum(
        record["orchestrationGapLowerBoundNs"] for record in covered
    )
    orchestration_gap_upper_bound_ns = sum(
        record["orchestrationGapUpperBoundNs"] for record in covered
    )
    return {
        "pairedToolCalls": len(records),
        "reportedChildRuntimeCalls": len(covered),
        "coverage": len(covered) / len(records) if records else None,
        "roundTripNs": round_trip_ns,
        "reportedChildWorkNs": reported_child_work_ns,
        "orchestrationGapLowerBoundNs": orchestration_gap_lower_bound_ns,
        "orchestrationGapUpperBoundNs": orchestration_gap_upper_bound_ns,
        "orchestrationShareLowerBound": orchestration_gap_lower_bound_ns / round_trip_ns
        if round_trip_ns
        else None,
        "orchestrationShareUpperBound": orchestration_gap_upper_bound_ns / round_trip_ns
        if round_trip_ns
        else None,
        "reportedChildCalls": sum(record["reportedChildCalls"] for record in covered),
        "parallelBatches": sum(record["reportedChildCalls"] > 1 for record in covered),
    }


def _terminal_record(
    file: Path, line_number: int, timestamp: Any, cwd: str, payload: dict[str, Any]
) -> dict[str, Any]:
    return {
        "file": str(file),
        "line": line_number,
        "timestamp": timestamp,
        "turn_id": payload.get("turn_id"),
        "status": payload.get("type"),
        "cwd": cwd,
        "timing": payload.get("timing"),
    }


def _selected_requests(timing: dict[str, Any]) -> list[dict[str, Any]]:
    return [item for item in timing.get("modelRequests", []) if isinstance(item, dict)]


def _request_metric(
    requests: Iterable[dict[str, Any]],
    includes: Callable[[dict[str, Any]], bool],
) -> dict[str, int]:
    request_list = list(requests)
    generation_ids = {
        request["generationIndex"]
        for request in request_list
        if request.get("attemptKind", "primary") == "primary"
        and request.get("generationIndex") is not None
        and includes(request)
    }
    matching = [
        request
        for request in request_list
        if request.get("generationIndex") in generation_ids
    ]
    decision_ready = [
        request for request in matching if request.get("decisionLatencyNs") is not None
    ]
    return {
        "logicalGenerations": len(generation_ids),
        "physicalAttempts": len(matching),
        "modelStreamWaitNs": sum(
            int(request.get("modelStreamWaitNs", 0)) for request in matching
        ),
        "decisionReadyAttempts": len(decision_ready),
        "decisionLatencyNs": sum(
            int(request["decisionLatencyNs"]) for request in decision_ready
        ),
        "toolCalls": sum(int(request.get("toolCallCount", 0)) for request in matching),
        "toolActiveUnionNs": sum(
            int(request.get("toolActiveUnionNs", 0)) for request in matching
        ),
    }


def _sum_metric(target: dict[str, int], source: dict[str, Any]) -> None:
    for key in target:
        target[key] += int(source.get(key, 0))


def _population_report(records: list[dict[str, Any]]) -> dict[str, Any]:
    totals = collections.Counter()
    nonprogress = {
        "logicalGenerations": 0,
        "physicalAttempts": 0,
        "modelStreamWaitNs": 0,
        "decisionReadyAttempts": 0,
        "decisionLatencyNs": 0,
        "toolCalls": 0,
        "toolActiveUnionNs": 0,
    }
    deterministic = dict(nonprogress)
    decision_ready_attempts = 0
    decision_latency_ns = 0
    request_count = 0
    status_counts: collections.Counter[str] = collections.Counter()

    for record in records:
        timing = record["timing"]
        exclusive = timing.get("exclusive", {})
        unions = timing.get("unions", {})
        counters = timing.get("counters", {})
        requests = _selected_requests(timing)
        status_counts[record["status"]] += 1
        totals["inclusiveDurationNs"] += int(timing.get("inclusiveDurationNs", 0))
        totals["modelOnlyNs"] += int(exclusive.get("modelOnlyNs", 0))
        totals["toolOnlyNs"] += int(exclusive.get("toolOnlyNs", 0))
        totals["modelPlusToolNs"] += int(exclusive.get("modelPlusToolNs", 0))
        totals["modelRequestWaitNs"] += int(unions.get("modelRequestWaitUnionNs", 0))
        totals["modelStreamWaitNs"] += int(unions.get("modelStreamWaitUnionNs", 0))
        totals["logicalGenerations"] += int(counters.get("logicalGenerationCount", 0))
        totals["toolCallCount"] += int(counters.get("toolCallCount", 0))
        totals["samePurposeContinuationCount"] += int(
            counters.get("samePurposeContinuationCount", 0)
        )
        totals["suppressedDeterministicContinuationCount"] += int(
            counters.get("suppressedDeterministicContinuationCount", 0)
        )
        request_count += len(requests)
        decision_ready = [
            request
            for request in requests
            if request.get("decisionLatencyNs") is not None
        ]
        decision_ready_attempts += len(decision_ready)
        decision_latency_ns += sum(
            int(request["decisionLatencyNs"]) for request in decision_ready
        )

        recorded_nonprogress = timing.get("observationalNonprogressLatency")
        if isinstance(recorded_nonprogress, dict):
            _sum_metric(nonprogress, recorded_nonprogress)
        else:
            _sum_metric(
                nonprogress,
                _request_metric(
                    requests,
                    lambda request: (
                        bool(request.get("unchangedRelevantState"))
                        and not bool(request.get("nextStructuredActionChanged"))
                    ),
                ),
            )
        _sum_metric(
            deterministic,
            _request_metric(
                requests,
                lambda request: (
                    request.get("generationPurpose")
                    == "deterministic_tool_continuation"
                ),
            ),
        )

    inclusive = totals["inclusiveDurationNs"]
    model = totals["modelOnlyNs"]
    tool = totals["toolOnlyNs"]
    return {
        "turns": len(records),
        "statusCounts": dict(sorted(status_counts.items())),
        **dict(totals),
        "modelShare": model / inclusive if inclusive else None,
        "toolShare": tool / inclusive if inclusive else None,
        "modelToolRatio": model / tool if tool else None,
        "modelDominatedTurns": sum(
            int(record["timing"].get("exclusive", {}).get("modelOnlyNs", 0))
            > int(record["timing"].get("exclusive", {}).get("toolOnlyNs", 0))
            for record in records
        ),
        "modelOverFiveTimesToolTurns": sum(
            int(record["timing"].get("exclusive", {}).get("modelOnlyNs", 0))
            > 5 * int(record["timing"].get("exclusive", {}).get("toolOnlyNs", 0))
            for record in records
        ),
        "decisionLatency": {
            "physicalAttempts": request_count,
            "decisionReadyAttempts": decision_ready_attempts,
            "coverage": decision_ready_attempts / request_count
            if request_count
            else None,
            "totalNs": decision_latency_ns,
        },
        "observationalNonprogressLatency": nonprogress,
        "deterministicToolContinuationLatency": deterministic,
    }


def analyze_session_path(source: Path, repo_root: Path) -> dict[str, Any]:
    files = [source] if source.is_file() else sorted(source.rglob("*.jsonl"))
    started_turns: set[str] = set()
    terminal_turns: set[str] = set()
    terminal_without_timing: set[str] = set()
    timed_records: dict[str, dict[str, Any]] = {}
    duplicate_timed_terminal_events = 0
    parse_error_count = 0
    parse_errors: list[dict[str, Any]] = []
    status_counts: collections.Counter[str] = collections.Counter()
    schema_versions: collections.Counter[str] = collections.Counter()
    line_count = 0
    byte_count = 0
    snapshots: list[dict[str, str | int]] = []
    command_orchestration_records: list[dict[str, Any]] = []

    for file in files:
        pending_tool_calls: dict[str, tuple[int, str]] = {}
        snapshot = read_rollout_snapshot(file)
        snapshots.append(snapshot.metadata())
        byte_count += snapshot.byte_length
        cwd = ""
        with io.StringIO(snapshot.data.decode("utf-8")) as handle:
            for line_number, line in enumerate(handle, 1):
                line_count += 1
                try:
                    item = json.loads(line)
                except (json.JSONDecodeError, UnicodeDecodeError) as error:
                    parse_error_count += 1
                    if len(parse_errors) < 100:
                        parse_errors.append(
                            {
                                "file": str(file),
                                "line": line_number,
                                "error": str(error),
                            }
                        )
                    continue
                payload = item.get("payload") or {}
                if item.get("type") == "session_meta":
                    cwd = str(payload.get("cwd") or cwd)
                payload_type = payload.get("type")
                if item.get("type") == "response_item" and payload_type in (
                    "custom_tool_call",
                    "function_call",
                ):
                    call_id = payload.get("call_id")
                    timestamp_ns = _timestamp_ns(item.get("timestamp"))
                    if call_id and timestamp_ns is not None:
                        pending_tool_calls[str(call_id)] = (timestamp_ns, cwd)
                elif item.get("type") == "response_item" and payload_type in (
                    "custom_tool_call_output",
                    "function_call_output",
                ):
                    call_id = payload.get("call_id")
                    timestamp_ns = _timestamp_ns(item.get("timestamp"))
                    pending = pending_tool_calls.pop(str(call_id), None)
                    if pending is not None and timestamp_ns is not None:
                        started_ns, call_cwd = pending
                        round_trip_ns = max(0, timestamp_ns - started_ns)
                        child_wall_seconds = [
                            float(value)
                            for value in _CHILD_WALL_TIME_PATTERN.findall(
                                _tool_output_text(payload)
                            )
                        ]
                        reported_child_work_ns = int(
                            sum(child_wall_seconds) * _NANOSECONDS_PER_SECOND
                        )
                        reported_child_critical_path_ns = int(
                            max(child_wall_seconds, default=0.0)
                            * _NANOSECONDS_PER_SECOND
                        )
                        command_orchestration_records.append(
                            {
                                "cwd": call_cwd,
                                "roundTripNs": round_trip_ns,
                                "reportedChildCalls": len(child_wall_seconds),
                                "reportedChildWorkNs": reported_child_work_ns,
                                "orchestrationGapLowerBoundNs": max(
                                    0, round_trip_ns - reported_child_work_ns
                                ),
                                "orchestrationGapUpperBoundNs": max(
                                    0,
                                    round_trip_ns - reported_child_critical_path_ns,
                                ),
                            }
                        )
                turn_id = payload.get("turn_id")
                if payload_type == "task_started" and turn_id:
                    started_turns.add(str(turn_id))
                if payload_type not in ("task_complete", "turn_aborted") or not turn_id:
                    continue
                turn_id = str(turn_id)
                terminal_turns.add(turn_id)
                status_counts[str(payload_type)] += 1
                record = _terminal_record(
                    file, line_number, item.get("timestamp"), cwd, payload
                )
                timing = record["timing"]
                if not isinstance(timing, dict):
                    terminal_without_timing.add(turn_id)
                    continue
                if turn_id in timed_records:
                    duplicate_timed_terminal_events += 1
                timed_records[turn_id] = record
                schema_versions[str(timing.get("schemaVersion", "missing"))] += 1

    records = list(timed_records.values())
    valid = [
        record
        for record in records
        if record["timing"].get("profileValid") is True
        and record["timing"].get("classificationComplete") is True
    ]
    populations = {
        name: [
            record
            for record in valid
            if name == "all" or _population(record["cwd"], str(repo_root)) == name
        ]
        for name in ("all", "eval", "repository_root", "other")
    }
    invalid = [record for record in records if record not in valid]
    coverage = {
        "files": len(files),
        "lines": line_count,
        "bytes": byte_count,
        "snapshots": snapshots,
        "parseErrorCount": parse_error_count,
        "parseErrors": parse_errors,
        "uniqueTaskStarts": len(started_turns),
        "uniqueTerminalTurns": len(terminal_turns),
        "uniqueTimedTerminalTurns": len(timed_records),
        "duplicateTimedTerminalEvents": duplicate_timed_terminal_events,
        "validCompleteProfiles": len(valid),
        "invalidProfiles": sum(
            record["timing"].get("profileValid") is not True for record in records
        ),
        "classificationIncompleteProfiles": sum(
            record["timing"].get("classificationComplete") is not True
            for record in records
        ),
        "terminalTurnsWithoutTiming": len(terminal_without_timing - set(timed_records)),
        "startedTurnsWithoutTerminal": len(started_turns - terminal_turns),
        "timedTerminalTurnsWithoutStart": len(set(timed_records) - started_turns),
        "statusCounts": dict(sorted(status_counts.items())),
        "timingSchemaVersions": dict(sorted(schema_versions.items())),
        "excludedInvalidOrIncompleteTurns": [
            {
                "turnId": record["turn_id"],
                "file": record["file"],
                "profileValid": record["timing"].get("profileValid"),
                "classificationComplete": record["timing"].get(
                    "classificationComplete"
                ),
            }
            for record in invalid
        ],
    }
    return {
        "schemaVersion": REPORT_SCHEMA_VERSION,
        "observedAt": dt.datetime.now(dt.timezone.utc).isoformat(),
        "source": str(source.resolve()),
        "repoRoot": str(repo_root.resolve()),
        "coverage": coverage,
        "commandOrchestration": _command_orchestration_report(
            command_orchestration_records
        ),
        "populations": {
            name: _population_report(population_records)
            for name, population_records in populations.items()
        },
    }


def render_report(report: dict[str, Any]) -> str:
    coverage = report["coverage"]
    lines = [
        f"source: {report['source']}",
        (
            "coverage: "
            f"{coverage['validCompleteProfiles']} valid complete / "
            f"{coverage['uniqueTimedTerminalTurns']} timed terminal / "
            f"{coverage['uniqueTaskStarts']} started turns; "
            f"{coverage['startedTurnsWithoutTerminal']} starts still non-terminal; "
            f"{coverage['parseErrorCount']} parse errors"
        ),
    ]
    orchestration = report["commandOrchestration"]
    if orchestration["reportedChildRuntimeCalls"]:
        lines.append(
            "command orchestration: "
            f"{orchestration['reportedChildRuntimeCalls']}/"
            f"{orchestration['pairedToolCalls']} paired calls with child runtime; "
            f"round-trip={orchestration['roundTripNs'] / 1e9:.1f}s "
            f"child-work={orchestration['reportedChildWorkNs'] / 1e9:.1f}s "
            f"gap={orchestration['orchestrationGapLowerBoundNs'] / 1e9:.1f}-"
            f"{orchestration['orchestrationGapUpperBoundNs'] / 1e9:.1f}s"
        )
    for name in ("all", "eval", "repository_root", "other"):
        population = report["populations"][name]
        if not population["turns"]:
            continue
        ratio = population["modelToolRatio"]
        nonprogress = population["observationalNonprogressLatency"]
        decision = population["decisionLatency"]
        ratio_text = f"{ratio:.2f}x" if ratio is not None else "n/a"
        lines.append(
            f"{name}: {population['turns']} turns; "
            f"model={population['modelOnlyNs'] / 1e9:.1f}s "
            f"tool={population['toolOnlyNs'] / 1e9:.1f}s "
            f"ratio={ratio_text}; "
            f"decision coverage={decision['decisionReadyAttempts']}/"
            f"{decision['physicalAttempts']}; "
            f"unchanged-state/action stream={nonprogress['modelStreamWaitNs'] / 1e9:.1f}s"
        )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "source", type=Path, help="Rollout JSONL file or session directory"
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path.cwd(),
        help="Repository root used for workload population segmentation",
    )
    parser.add_argument(
        "--json", action="store_true", help="Emit the complete JSON report"
    )
    args = parser.parse_args()
    if not args.source.exists():
        parser.error(f"source does not exist: {args.source}")
    report = analyze_session_path(args.source, args.repo_root)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render_report(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
