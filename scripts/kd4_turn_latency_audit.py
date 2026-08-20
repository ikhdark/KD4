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
import uuid
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any, Sequence

try:
    from scripts.rollout_snapshot import read_rollout_snapshot
except ImportError:
    from rollout_snapshot import read_rollout_snapshot


REPORT_SCHEMA_VERSION = 4

_NANOSECONDS_PER_SECOND = 1_000_000_000
_SLOW_TOOL_CALL_NS = 5 * _NANOSECONDS_PER_SECOND
_MAX_SLOW_TOOL_CALLS = 8
_CHILD_WALL_TIME_PATTERN = re.compile(
    r'"wall_time_seconds"\s*:\s*([0-9]+(?:\.[0-9]+)?)'
)
_EXEC_WALL_TIME_PATTERN = re.compile(
    r"\bWall time\s+([0-9]+(?:\.[0-9]+)?)\s+seconds\b",
    re.IGNORECASE,
)
_NESTED_TOOL_PATTERN = re.compile(r"\btools\.([A-Za-z_][A-Za-z0-9_]*)\s*\(")


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


def _tool_label(payload: dict[str, Any]) -> str:
    name = str(payload.get("name") or "unknown")
    if payload.get("type") != "custom_tool_call":
        return name
    source = payload.get("input")
    if not isinstance(source, str):
        return name
    nested = list(dict.fromkeys(_NESTED_TOOL_PATTERN.findall(source)))
    if not nested:
        return name
    return f"{name}>{'+'.join(nested[:3])}"


def _tool_status(output: str) -> str:
    if re.search(
        r"\b(?:Script failed|exec cancelled|Traceback \(most recent call last\))\b",
        output,
        re.IGNORECASE,
    ) or re.search(r"\bExit code:\s*[1-9][0-9]*\b", output):
        return "failed"
    if "Command is still running" in output:
        return "running"
    return "completed"


def _reported_runtime_seconds(output: str) -> tuple[list[float], float | None]:
    child_runtime = [float(value) for value in _CHILD_WALL_TIME_PATTERN.findall(output)]
    exec_wall_matches = _EXEC_WALL_TIME_PATTERN.findall(output)
    exec_wall = float(exec_wall_matches[0]) if exec_wall_matches else None
    return child_runtime, exec_wall


def _command_orchestration_report(records: list[dict[str, Any]]) -> dict[str, Any]:
    covered = [record for record in records if record["reportedChildCalls"] > 0]
    all_paired_round_trip_ns = sum(record["roundTripNs"] for record in records)
    round_trip_ns = sum(record["roundTripNs"] for record in covered)
    reported_child_work_ns = sum(record["reportedChildWorkNs"] for record in covered)
    orchestration_gap_lower_bound_ns = sum(
        record["orchestrationGapLowerBoundNs"] for record in covered
    )
    orchestration_gap_upper_bound_ns = sum(
        record["orchestrationGapUpperBoundNs"] for record in covered
    )
    slow_calls = sorted(
        (
            {
                "tool": record["tool"],
                "status": record["status"],
                "roundTripNs": record["roundTripNs"],
                "reportedExecWallNs": record["reportedExecWallNs"],
                "reportedChildWorkNs": record["reportedChildWorkNs"],
                "orchestrationGapLowerBoundNs": record["orchestrationGapLowerBoundNs"],
            }
            for record in records
            if record["roundTripNs"] >= _SLOW_TOOL_CALL_NS
        ),
        key=lambda record: record["roundTripNs"],
        reverse=True,
    )
    return {
        "pairedToolCalls": len(records),
        "allPairedRoundTripNs": all_paired_round_trip_ns,
        "failedToolCalls": sum(record["status"] == "failed" for record in records),
        "runningToolCalls": sum(record["status"] == "running" for record in records),
        "slowToolCallThresholdNs": _SLOW_TOOL_CALL_NS,
        "slowToolCallCount": len(slow_calls),
        "topSlowToolCalls": slow_calls[:_MAX_SLOW_TOOL_CALLS],
        "omittedSlowToolCalls": max(0, len(slow_calls) - _MAX_SLOW_TOOL_CALLS),
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


def _audit_decision(report: dict[str, Any]) -> dict[str, Any]:
    coverage = report["coverage"]
    population = report["populations"]["all"]
    orchestration = report["commandOrchestration"]
    phases = {
        "orchestration": int(population.get("orchestrationNs", 0)),
        "model": int(population.get("modelOnlyNs", 0)),
        "tool": int(population.get("toolOnlyNs", 0)),
        "retry": int(population.get("retryOnlyNs", 0)),
        "interactive_wait": int(population.get("interactiveOnlyWaitNs", 0)),
        "finalization": int(population.get("finalizationNs", 0)),
        "unclassified": int(population.get("unclassifiedNs", 0)),
    }
    dominant_phase, dominant_ns = max(phases.items(), key=lambda item: item[1])
    inclusive_ns = int(population.get("inclusiveDurationNs", 0))
    dominant_share = dominant_ns / inclusive_ns if inclusive_ns else None

    reasons: list[str] = []
    blockers: list[str] = []
    if coverage["validCompleteProfiles"] == 0:
        blockers.append("no_valid_complete_timing_profile")
    if coverage["parseErrorCount"]:
        blockers.append("rollout_parse_errors")
    if coverage["startedTurnsWithoutTerminal"]:
        blockers.append("turn_still_non_terminal")
    if dominant_share is None or dominant_share < 0.5:
        blockers.append("no_majority_timing_attribution")
    else:
        reasons.append(f"majority_{dominant_phase}_attribution")

    representative_evidence = False
    if dominant_phase == "orchestration":
        representative_evidence = orchestration["slowToolCallCount"] >= 2
        if representative_evidence:
            reasons.append("repeated_slow_tool_round_trips")
    elif dominant_phase == "model":
        representative_evidence = (
            population["decisionLatency"]["decisionReadyAttempts"] >= 2
        )
        if representative_evidence:
            reasons.append("repeated_model_decision_latency")
    elif dominant_phase == "tool":
        representative_evidence = orchestration["slowToolCallCount"] >= 2
        if representative_evidence:
            reasons.append("repeated_slow_tool_execution")
    else:
        representative_evidence = dominant_share is not None and dominant_share >= 0.75
        if representative_evidence:
            reasons.append("strong_terminal_timing_attribution")
    if not representative_evidence:
        blockers.append("representative_evidence_missing")

    ready = not blockers
    return {
        "readyToFinalize": ready,
        "dominantPhase": dominant_phase,
        "dominantNs": dominant_ns,
        "dominantShare": dominant_share,
        "reasonCodes": reasons,
        "blockerCodes": blockers,
        "instruction": (
            "Stop rollout inspection and answer from this report."
            if ready
            else "Continue only to resolve the listed blockers."
        ),
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
        totals["orchestrationNs"] += int(exclusive.get("orchestrationNs", 0))
        totals["retryOnlyNs"] += int(exclusive.get("retryOnlyNs", 0))
        totals["interactiveOnlyWaitNs"] += int(
            exclusive.get("interactiveOnlyWaitNs", 0)
        )
        totals["finalizationNs"] += int(exclusive.get("finalizationNs", 0))
        totals["unclassifiedNs"] += int(exclusive.get("unclassifiedNs", 0))
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
    execution_loop_counts: collections.Counter[str] = collections.Counter()
    execution_loop_ns: collections.Counter[str] = collections.Counter()
    first_timestamp_ns: int | None = None
    last_timestamp_ns: int | None = None

    for file in files:
        pending_tool_calls: dict[str, dict[str, Any]] = {}
        task_started_at: dict[str, int] = {}
        current_sampling_boundary_ns: int | None = None
        calls_since_sampling_boundary = 0
        last_tool_output_ns: int | None = None
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
                timestamp_ns = _timestamp_ns(item.get("timestamp"))
                if timestamp_ns is not None:
                    first_timestamp_ns = (
                        timestamp_ns
                        if first_timestamp_ns is None
                        else min(first_timestamp_ns, timestamp_ns)
                    )
                    last_timestamp_ns = (
                        timestamp_ns
                        if last_timestamp_ns is None
                        else max(last_timestamp_ns, timestamp_ns)
                    )
                if item.get("type") == "sampling_boundary":
                    execution_loop_counts["samplingPasses"] += 1
                    if (
                        timestamp_ns is not None
                        and last_tool_output_ns is not None
                        and timestamp_ns >= last_tool_output_ns
                    ):
                        execution_loop_ns["postToolHandoffNs"] += (
                            timestamp_ns - last_tool_output_ns
                        )
                    current_sampling_boundary_ns = timestamp_ns
                    calls_since_sampling_boundary = 0
                    last_tool_output_ns = None
                if item.get("type") == "session_meta":
                    cwd = str(payload.get("cwd") or cwd)
                payload_type = payload.get("type")
                if item.get("type") == "response_item" and payload_type in (
                    "custom_tool_call",
                    "function_call",
                ):
                    execution_loop_counts["toolCalls"] += 1
                    call_id = payload.get("call_id")
                    if (
                        timestamp_ns is not None
                        and current_sampling_boundary_ns is not None
                        and calls_since_sampling_boundary == 0
                        and timestamp_ns >= current_sampling_boundary_ns
                    ):
                        execution_loop_ns["samplingToFirstToolCallNs"] += (
                            timestamp_ns - current_sampling_boundary_ns
                        )
                    calls_since_sampling_boundary += 1
                    if call_id and timestamp_ns is not None:
                        pending_tool_calls[str(call_id)] = {
                            "startedNs": timestamp_ns,
                            "cwd": cwd,
                            "tool": _tool_label(payload),
                        }
                elif item.get("type") == "response_item" and payload_type in (
                    "custom_tool_call_output",
                    "function_call_output",
                ):
                    call_id = payload.get("call_id")
                    pending = pending_tool_calls.pop(str(call_id), None)
                    if pending is not None and timestamp_ns is not None:
                        started_ns = int(pending["startedNs"])
                        round_trip_ns = max(0, timestamp_ns - started_ns)
                        execution_loop_counts["pairedToolCalls"] += 1
                        execution_loop_ns["pairedToolRoundTripNs"] += round_trip_ns
                        last_tool_output_ns = max(
                            last_tool_output_ns or timestamp_ns, timestamp_ns
                        )
                        output = _tool_output_text(payload)
                        child_wall_seconds, exec_wall_seconds = (
                            _reported_runtime_seconds(output)
                        )
                        reported_child_work_ns = int(
                            sum(child_wall_seconds) * _NANOSECONDS_PER_SECOND
                        )
                        reported_child_critical_path_ns = int(
                            max(child_wall_seconds, default=0.0)
                            * _NANOSECONDS_PER_SECOND
                        )
                        command_orchestration_records.append(
                            {
                                "cwd": pending["cwd"],
                                "tool": pending["tool"],
                                "status": _tool_status(output),
                                "roundTripNs": round_trip_ns,
                                "reportedExecWallNs": (
                                    int(exec_wall_seconds * _NANOSECONDS_PER_SECOND)
                                    if exec_wall_seconds is not None
                                    else None
                                ),
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
                    turn_id = str(turn_id)
                    started_turns.add(turn_id)
                    if timestamp_ns is not None:
                        task_started_at[turn_id] = timestamp_ns
                if payload_type not in ("task_complete", "turn_aborted") or not turn_id:
                    continue
                turn_id = str(turn_id)
                if timestamp_ns is not None and turn_id in task_started_at:
                    execution_loop_counts["terminalTasks"] += 1
                    execution_loop_ns["taskElapsedNs"] += max(
                        0, timestamp_ns - task_started_at[turn_id]
                    )
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
    execution_loop = {
        **dict(execution_loop_counts),
        **dict(execution_loop_ns),
        "unpairedToolCalls": max(
            0,
            execution_loop_counts["toolCalls"]
            - execution_loop_counts["pairedToolCalls"],
        ),
        "recordSpanNs": (
            last_timestamp_ns - first_timestamp_ns
            if first_timestamp_ns is not None and last_timestamp_ns is not None
            else None
        ),
    }
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
    report = {
        "schemaVersion": REPORT_SCHEMA_VERSION,
        "observedAt": dt.datetime.now(dt.timezone.utc).isoformat(),
        "source": str(source.resolve()),
        "repoRoot": str(repo_root.resolve()),
        "coverage": coverage,
        "executionLoop": execution_loop,
        "commandOrchestration": _command_orchestration_report(
            command_orchestration_records
        ),
        "populations": {
            name: _population_report(population_records)
            for name, population_records in populations.items()
        },
    }
    report["auditDecision"] = _audit_decision(report)
    return report


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
    execution_loop = report["executionLoop"]
    if execution_loop.get("samplingPasses") or execution_loop.get("toolCalls"):
        lines.append(
            "execution loop: "
            f"{execution_loop.get('samplingPasses', 0)} sampling passes; "
            f"{execution_loop.get('pairedToolCalls', 0)}/"
            f"{execution_loop.get('toolCalls', 0)} tool calls paired; "
            f"sampling-to-call={execution_loop.get('samplingToFirstToolCallNs', 0) / 1e9:.1f}s "
            f"tool-round-trip={execution_loop.get('pairedToolRoundTripNs', 0) / 1e9:.1f}s "
            f"handoff={execution_loop.get('postToolHandoffNs', 0) / 1e9:.1f}s"
        )
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
    if orchestration["slowToolCallCount"]:
        rendered_slow_calls = ", ".join(
            f"{record['tool']}={record['roundTripNs'] / 1e9:.1f}s/{record['status']}"
            for record in orchestration["topSlowToolCalls"][:3]
        )
        omitted = orchestration["slowToolCallCount"] - min(
            3, len(orchestration["topSlowToolCalls"])
        )
        omitted_text = f" (+{omitted} more)" if omitted else ""
        lines.append(
            "slow tool round-trips: "
            f"{orchestration['slowToolCallCount']} >= "
            f"{orchestration['slowToolCallThresholdNs'] / 1e9:.0f}s; "
            f"{rendered_slow_calls}{omitted_text}"
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
    decision = report["auditDecision"]
    dominant_share = decision["dominantShare"]
    dominant_share_text = (
        f"{dominant_share:.1%}" if dominant_share is not None else "unavailable"
    )
    outcome = "finalize" if decision["readyToFinalize"] else "continue"
    codes = (
        decision["reasonCodes"]
        if decision["readyToFinalize"]
        else decision["blockerCodes"]
    )
    lines.append(
        f"audit decision: {outcome}; dominant={decision['dominantPhase']} "
        f"{decision['dominantNs'] / 1e9:.1f}s/{dominant_share_text}; "
        f"codes={','.join(codes) or 'none'}. {decision['instruction']}"
    )
    return "\n".join(lines)


def bounded_summary(report: dict[str, Any]) -> dict[str, Any]:
    coverage = report["coverage"]
    coverage_keys = (
        "files",
        "lines",
        "bytes",
        "snapshots",
        "parseErrorCount",
        "uniqueTaskStarts",
        "uniqueTerminalTurns",
        "uniqueTimedTerminalTurns",
        "validCompleteProfiles",
        "startedTurnsWithoutTerminal",
        "terminalTurnsWithoutTiming",
        "statusCounts",
        "timingSchemaVersions",
    )
    return {
        "schemaVersion": report["schemaVersion"],
        "observedAt": report["observedAt"],
        "source": report["source"],
        "repoRoot": report["repoRoot"],
        "coverage": {key: coverage[key] for key in coverage_keys},
        "executionLoop": report["executionLoop"],
        "commandOrchestration": report["commandOrchestration"],
        "populations": {
            name: population
            for name, population in report["populations"].items()
            if population["turns"]
        },
        "auditDecision": report["auditDecision"],
    }


def _canonical_session_uuid(value: str) -> str | None:
    try:
        parsed = uuid.UUID(value)
    except ValueError:
        return None
    canonical = str(parsed)
    return canonical if value.casefold() == canonical else None


def resolve_rollout_source(source: str, sessions_root: Path | None = None) -> Path:
    path = Path(source).expanduser()
    if path.exists():
        return path.resolve()

    session_id = _canonical_session_uuid(source)
    if session_id is None:
        raise FileNotFoundError(f"source does not exist: {source}")
    if sessions_root is None:
        codex_home = os.environ.get("CODEX_HOME")
        if not codex_home:
            raise FileNotFoundError(
                "session UUID lookup requires --sessions-root or CODEX_HOME"
            )
        sessions_root = Path(codex_home) / "sessions"
    sessions_root = sessions_root.expanduser().resolve(strict=True)
    matches = sorted(
        path.resolve()
        for path in sessions_root.rglob(f"*-{session_id}.jsonl")
        if path.is_file()
    )
    if not matches:
        raise FileNotFoundError(
            f"no rollout found for session {session_id} under {sessions_root}"
        )
    if len(matches) > 1:
        raise FileNotFoundError(
            f"multiple rollouts found for session {session_id} under {sessions_root}"
        )
    return matches[0]


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "source", help="Rollout JSONL path, session directory, or exact session UUID"
    )
    parser.add_argument(
        "--sessions-root",
        type=Path,
        help="Sessions directory for UUID lookup; defaults to CODEX_HOME/sessions",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path.cwd(),
        help="Repository root used for workload population segmentation",
    )
    output = parser.add_mutually_exclusive_group()
    output.add_argument(
        "--json", action="store_true", help="Emit the complete JSON report"
    )
    output.add_argument(
        "--summary-json",
        action="store_true",
        help="Emit bounded JSON without per-record diagnostic arrays",
    )
    args = parser.parse_args(argv)
    try:
        source = resolve_rollout_source(args.source, args.sessions_root)
    except (FileNotFoundError, OSError) as error:
        parser.error(str(error))
    report = analyze_session_path(source, args.repo_root)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    elif args.summary_json:
        print(json.dumps(bounded_summary(report), sort_keys=True))
    else:
        print(render_report(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
