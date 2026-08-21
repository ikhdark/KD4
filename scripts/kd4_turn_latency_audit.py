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


REPORT_SCHEMA_VERSION = 7

_NANOSECONDS_PER_SECOND = 1_000_000_000
_SLOW_TOOL_CALL_NS = 5 * _NANOSECONDS_PER_SECOND
_MAX_SLOW_TOOL_CALLS = 8
_MAX_RENDERED_TURNS = 10
_MAX_SUMMARY_TURNS = 20
_MAX_SUMMARY_TOKEN_INTERVALS = 16
_SAMPLING_PASS_TARGET_PER_COMPLETED_TURN = 8
_MAX_OPEN_TURN_DETAILS = 100
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
                "turnId": record.get("turnId"),
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
        "model_tool": int(population.get("modelPlusToolNs", 0)),
        "retry": int(population.get("retryOnlyNs", 0)),
        "interactive_machine": int(population.get("interactivePlusMachineNs", 0)),
        "standalone": int(population.get("standaloneWorkNs", 0)),
        "finalization": int(population.get("finalizationNs", 0)),
        "unclassified": int(population.get("unclassifiedNs", 0)),
    }
    dominant_phase, dominant_ns = max(phases.items(), key=lambda item: item[1])
    agent_active_ns = int(population.get("machineDurationNs", 0))
    dominant_share = dominant_ns / agent_active_ns if agent_active_ns else None

    reasons: list[str] = []
    blockers: list[str] = []
    if coverage["validCompleteProfiles"] == 0:
        blockers.append("no_valid_complete_timing_profile")
    if coverage["parseErrorCount"]:
        blockers.append("rollout_parse_errors")
    if coverage["startedTurnsWithoutTerminal"]:
        reasons.append("active_tail_excluded")
        reasons.extend(
            f"open_turn_{state}"
            for state, count in coverage.get("openTurnStateCounts", {}).items()
            if count
        )
    if dominant_share is None or dominant_share < 0.5:
        reasons.append("mixed_timing_attribution")
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
        reasons.append("limited_representative_evidence")

    ready = not blockers
    return {
        "readyToFinalize": ready,
        "dominantPhase": dominant_phase,
        "dominantNs": dominant_ns,
        "dominantShare": dominant_share,
        "agentActiveNs": agent_active_ns,
        "humanWaitNs": int(population.get("interactiveOnlyWaitNs", 0)),
        "humanWaitUnionNs": int(population.get("interactiveWaitUnionNs", 0)),
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
    payload_type = payload.get("type")
    abort_reason = str(payload.get("reason") or "").casefold()
    if payload_type == "turn_aborted":
        lifecycle = "abandoned" if abort_reason == "replaced" else "canceled"
    elif payload.get("error"):
        lifecycle = "failed"
    else:
        lifecycle = "completed"
    return {
        "file": str(file),
        "line": line_number,
        "timestamp": timestamp,
        "turn_id": payload.get("turn_id"),
        "status": payload.get("type"),
        "lifecycle": lifecycle,
        "cwd": cwd,
        "timing": payload.get("timing"),
    }


def _selected_requests(timing: dict[str, Any]) -> list[dict[str, Any]]:
    return [item for item in timing.get("modelRequests", []) if isinstance(item, dict)]


def _physical_attempt_count(request: dict[str, Any]) -> int:
    attempt_ids = request.get("physicalAttemptIds")
    if isinstance(attempt_ids, list):
        distinct_ids = {
            attempt_id
            for attempt_id in attempt_ids
            if isinstance(attempt_id, str) and attempt_id
        }
        if distinct_ids:
            return len(distinct_ids)
    return 1


def _token_report(requests: Iterable[dict[str, Any]]) -> dict[str, Any]:
    request_list = list(requests)
    physical_attempts = sum(_physical_attempt_count(request) for request in request_list)
    totals = collections.Counter()
    prompt_categories = collections.Counter()
    covered_attempts = 0
    categorized_attempts = 0
    for request in request_list:
        usage = request.get("tokenUsage")
        if isinstance(usage, dict):
            covered_attempts += 1
            input_tokens = max(0, int(usage.get("inputTokens", 0)))
            cached_input_tokens = max(0, int(usage.get("cachedInputTokens", 0)))
            visible_output_tokens = max(0, int(usage.get("visibleOutputTokens", 0)))
            reasoning_tokens = max(0, int(usage.get("reasoningTokens", 0)))
            total_tokens = max(
                0,
                int(
                    usage.get(
                        "totalTokens",
                        input_tokens + visible_output_tokens + reasoning_tokens,
                    )
                ),
            )
        else:
            input_tokens = 0
            cached_input_tokens = 0
            output_tokens = max(0, int(request.get("outputTokens", 0)))
            reasoning_tokens = max(0, int(request.get("reasoningOutputTokens", 0)))
            visible_output_tokens = max(0, output_tokens - reasoning_tokens)
            total_tokens = output_tokens
        totals["inputTokens"] += input_tokens
        totals["cachedInputTokens"] += min(input_tokens, cached_input_tokens)
        totals["visibleOutputTokens"] += visible_output_tokens
        totals["reasoningTokens"] += reasoning_tokens
        totals["outputTokens"] += visible_output_tokens + reasoning_tokens
        totals["totalTokens"] += total_tokens
        categories = request.get("requestTokenCategories")
        if isinstance(categories, dict):
            categorized_attempts += 1
            for key in (
                "baseInstructions",
                "toolSchemas",
                "conversationHistory",
                "currentInput",
                "repositoryContext",
                "skills",
                "otherInjectedContext",
                "logicalTotal",
                "localInputEstimate",
                "repeatedUnchangedContext",
            ):
                prompt_categories[key] += max(0, int(categories.get(key, 0)))

    for key in (
        "inputTokens",
        "cachedInputTokens",
        "visibleOutputTokens",
        "reasoningTokens",
        "outputTokens",
        "totalTokens",
    ):
        totals[key] += 0
    totals["nonCachedInputTokens"] = max(
        0, totals["inputTokens"] - totals["cachedInputTokens"]
    )
    observed_blended_tokens = totals["nonCachedInputTokens"] + totals["outputTokens"]
    observed_billable_tokens = totals["inputTokens"] + totals["outputTokens"]
    input_tokens = totals["inputTokens"]
    usage_complete = covered_attempts == physical_attempts
    return {
        "physicalAttempts": physical_attempts,
        "providerUsageAttempts": covered_attempts,
        "coverage": covered_attempts / physical_attempts if physical_attempts else None,
        "complete": usage_complete,
        **dict(totals),
        "billableTokens": observed_billable_tokens if usage_complete else None,
        "observedBillableTokens": observed_billable_tokens,
        "billableDefinition": "provider_input_including_cached_plus_output",
        "blendedTokens": observed_blended_tokens if usage_complete else None,
        "observedBlendedTokens": observed_blended_tokens,
        "blendedDefinition": "non_cached_input_plus_output",
        "promptCategoryAttempts": categorized_attempts,
        "promptCategories": dict(prompt_categories),
        "cacheShare": totals["cachedInputTokens"] / input_tokens
        if input_tokens
        else None,
    }


def _token_intervals(
    requests: Iterable[dict[str, Any]], tool_calls: Iterable[dict[str, Any]]
) -> list[dict[str, Any]]:
    """Attribute provider usage to each logical generation between tool batches.

    Retry and fallback attempts share a generation index. Keeping one interval
    per physical request would attach the same emitted tool batch to every
    attempt, inflating the apparent number of model/tool handoffs.
    """
    request_list = list(requests)
    calls_by_generation: dict[int, list[dict[str, Any]]] = collections.defaultdict(list)
    for call in tool_calls:
        generation_index = call.get("generationIndex")
        if isinstance(generation_index, int):
            calls_by_generation[generation_index].append(call)

    request_groups: dict[tuple[str, int], list[tuple[int, dict[str, Any]]]] = {}
    for request_index, request in enumerate(request_list):
        generation_index = request.get("generationIndex")
        group_key = (
            ("generation", generation_index)
            if isinstance(generation_index, int)
            else ("request", request_index)
        )
        request_groups.setdefault(group_key, []).append((request_index, request))

    intervals: list[dict[str, Any]] = []
    for grouped_requests in request_groups.values():
        request_indexes = [request_index for request_index, _ in grouped_requests]
        interval_requests = [request for _, request in grouped_requests]
        request = interval_requests[0]
        generation_index = request.get("generationIndex")
        prior_generation = None
        emitted_calls: list[dict[str, Any]] = []
        preceding_calls: list[dict[str, Any]] = []
        if isinstance(generation_index, int):
            emitted_calls = calls_by_generation.get(generation_index, [])
            prior_generation = max(
                (
                    candidate
                    for candidate in calls_by_generation
                    if candidate < generation_index
                ),
                default=None,
            )
            if prior_generation is not None:
                preceding_calls = calls_by_generation[prior_generation]

        preceding_delivery_ms = [
            int(call["deliveredAtMs"])
            for call in preceding_calls
            if isinstance(call.get("deliveredAtMs"), int)
        ]
        emitted_acceptance_ms = [
            int(call["acceptedAtMs"])
            for call in emitted_calls
            if isinstance(call.get("acceptedAtMs"), int)
        ]
        dispatch_ms = [
            int(item["dispatchMs"])
            for item in interval_requests
            if isinstance(item.get("dispatchMs"), int)
        ]
        completed_ms = [
            int(item["completedMs"])
            for item in interval_requests
            if isinstance(item.get("completedMs"), int)
        ]
        attempt_kinds = [str(item.get("attemptKind", "primary")) for item in interval_requests]
        intervals.append(
            {
                "requestIndex": request_indexes[0],
                "requestIndexes": request_indexes,
                "generationIndex": generation_index,
                "attemptKind": attempt_kinds[0] if len(attempt_kinds) == 1 else "multiple",
                "attemptKinds": attempt_kinds,
                "physicalAttempts": sum(
                    _physical_attempt_count(item) for item in interval_requests
                ),
                "generationPurpose": request.get("generationPurpose"),
                "dispatchMs": min(dispatch_ms) if dispatch_ms else None,
                "completedMs": max(completed_ms) if completed_ms else None,
                "precedingToolGenerationIndex": prior_generation,
                "precedingToolCallIds": [
                    str(call.get("callId") or "") for call in preceding_calls
                ],
                "precedingResultsDeliveredAtMs": (
                    max(preceding_delivery_ms) if preceding_delivery_ms else None
                ),
                "emittedToolCallIds": [
                    str(call.get("callId") or "") for call in emitted_calls
                ],
                "emittedToolsAcceptedAtMs": (
                    min(emitted_acceptance_ms) if emitted_acceptance_ms else None
                ),
                "tokens": _token_report(interval_requests),
            }
        )
    return intervals


def _iso_from_unix_ms(value: int | None) -> str | None:
    if value is None:
        return None
    return dt.datetime.fromtimestamp(value / 1000, dt.timezone.utc).isoformat()


def _turn_boundaries(timing: dict[str, Any], terminal_timestamp: Any) -> dict[str, Any]:
    raw_started = timing.get("startedAtUnixMs")
    raw_completed = timing.get("completedAtUnixMs")
    started_ms = int(raw_started) if isinstance(raw_started, int) else None
    completed_ms = int(raw_completed) if isinstance(raw_completed, int) else None
    source = (
        "timing" if started_ms is not None and completed_ms is not None else "derived"
    )
    if completed_ms is None:
        terminal_ns = _timestamp_ns(terminal_timestamp)
        completed_ms = terminal_ns // 1_000_000 if terminal_ns is not None else None
    if started_ms is None and completed_ms is not None:
        started_ms = (
            completed_ms - int(timing.get("inclusiveDurationNs", 0)) // 1_000_000
        )
    return {
        "startedAtUnixMs": started_ms,
        "completedAtUnixMs": completed_ms,
        "startedAt": _iso_from_unix_ms(started_ms),
        "completedAt": _iso_from_unix_ms(completed_ms),
        "boundarySource": source,
    }


def _diagnostic_token_report(aggregates: Iterable[dict[str, Any]]) -> dict[str, int]:
    totals = collections.Counter()
    for aggregate in aggregates:
        for key in (
            "logicalGenerations",
            "inputTokens",
            "cachedInputTokens",
            "visibleOutputTokens",
            "reasoningTokens",
            "totalTokens",
        ):
            totals[key] += max(0, int(aggregate.get(key, 0)))
    totals["outputTokens"] = totals["visibleOutputTokens"] + totals["reasoningTokens"]
    totals["nonCachedInputTokens"] = max(
        0, totals["inputTokens"] - totals["cachedInputTokens"]
    )
    totals["observedBillableTokens"] = (
        totals["nonCachedInputTokens"] + totals["outputTokens"]
    )
    return dict(totals)


def _tool_call_end_to_end_duration_ms(call: dict[str, Any]) -> int:
    accepted_at = call.get("acceptedAtMs")
    delivered_at = call.get("deliveredAtMs")
    if isinstance(accepted_at, int) and isinstance(delivered_at, int):
        return max(0, delivered_at - accepted_at)

    relay_ms = max(0, int(call.get("totalDurationMs") or 0))
    queued_ms = max(0, int(call.get("itemToFirstPollMs") or 0))
    return queued_ms + relay_ms


def _tool_relay_report(
    records: Iterable[dict[str, Any]], overflow_count: int = 0
) -> dict[str, Any]:
    calls = [record for record in records if isinstance(record, dict)]
    totals = collections.Counter()
    generation_counts: collections.Counter[tuple[Any, int]] = collections.Counter()
    incomplete = 0
    for call in calls:
        generation_index = call.get("generationIndex")
        if isinstance(generation_index, int):
            generation_counts[(call.get("_turnId"), generation_index)] += 1
        totals["endToEndDurationMs"] += _tool_call_end_to_end_duration_ms(call)
        for key in (
            "itemToFirstPollMs",
            "parallelGateWaitMs",
            "authorizationStateCoordinationMs",
            "handlerDurationMs",
            "workspaceEvidenceBeforeMs",
            "workspaceEvidenceAfterMs",
            "preToolHookMs",
            "postToolHookMs",
            "outputProjectionMs",
            "historyPersistenceMs",
            "postHandlerMs",
            "totalDurationMs",
        ):
            totals[key] += max(0, int(call.get(key) or 0))
        for key, start_key, end_key in (
            ("requestToProcessSpawnMs", "acceptedAtMs", "processSpawnedAtMs"),
            ("firstPollToHandlerEntryMs", "firstPollAtMs", "handlerEntryAtMs"),
            ("handlerEntryToProcessSpawnMs", "handlerEntryAtMs", "processSpawnedAtMs"),
            ("processRuntimeMs", "processSpawnedAtMs", "processExitedAtMs"),
            ("processExitToDeliveryMs", "processExitedAtMs", "deliveredAtMs"),
            ("deliveryToProcessExitMs", "deliveredAtMs", "processExitedAtMs"),
            (
                "processExitToOutputCollectedMs",
                "processExitedAtMs",
                "outputCollectedAtMs",
            ),
            (
                "outputCollectedToDeliveryMs",
                "outputCollectedAtMs",
                "deliveredAtMs",
            ),
            (
                "deliveryToModelResumeMs",
                "deliveredAtMs",
                "modelResumedAtMs",
            ),
        ):
            start = call.get(start_key)
            end = call.get(end_key)
            if isinstance(start, int) and isinstance(end, int):
                totals[key] += max(0, end - start)
        if (
            call.get("acceptedAtMs") is None
            or call.get("outputCollectedAtMs") is None
            or call.get("deliveredAtMs") is None
        ):
            incomplete += 1

    slow_calls = sorted(
        (
            {
                "callId": str(call.get("callId") or ""),
                "tool": str(call.get("toolName") or "unknown"),
                "source": str(call.get("source") or "direct"),
                "totalDurationMs": max(0, int(call.get("totalDurationMs") or 0)),
                "endToEndDurationMs": _tool_call_end_to_end_duration_ms(call),
                "processAliveAtDelivery": bool(call.get("processAliveAtDelivery")),
            }
            for call in calls
            if _tool_call_end_to_end_duration_ms(call)
            >= _SLOW_TOOL_CALL_NS // 1_000_000
        ),
        key=lambda call: call["endToEndDurationMs"],
        reverse=True,
    )
    batch_groups = sum(count > 1 for count in generation_counts.values())
    batched_calls = sum(count for count in generation_counts.values() if count > 1)
    return {
        "calls": len(calls),
        "timingOverflowCalls": max(0, int(overflow_count)),
        "directCalls": sum(call.get("source", "direct") == "direct" for call in calls),
        "nestedCalls": sum(call.get("source") == "code_mode" for call in calls),
        "eagerCalls": sum(bool(call.get("eager")) for call in calls),
        "processAliveAtDeliveryCalls": sum(
            bool(call.get("processAliveAtDelivery")) for call in calls
        ),
        "incompleteLifecycleCalls": incomplete,
        "generationGroups": len(generation_counts),
        "batchGroups": batch_groups,
        "batchedCalls": batched_calls,
        "singleCallGroups": sum(count == 1 for count in generation_counts.values()),
        "phaseTotalsMs": dict(totals),
        "slowCallThresholdMs": _SLOW_TOOL_CALL_NS // 1_000_000,
        "slowCallCount": len(slow_calls),
        "topSlowCalls": slow_calls[:_MAX_SLOW_TOOL_CALLS],
        "omittedSlowCalls": max(0, len(slow_calls) - _MAX_SLOW_TOOL_CALLS),
    }


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
        "physicalAttempts": sum(_physical_attempt_count(request) for request in matching),
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
    all_requests: list[dict[str, Any]] = []
    all_tool_calls: list[dict[str, Any]] = []
    nonprogress_token_aggregates: list[dict[str, Any]] = []
    tool_call_timing_overflow = 0

    for record in records:
        timing = record["timing"]
        exclusive = timing.get("exclusive", {})
        unions = timing.get("unions", {})
        counters = timing.get("counters", {})
        requests = _selected_requests(timing)
        all_requests.extend(requests)
        all_tool_calls.extend(
            {**call, "_turnId": record["turn_id"]}
            for call in timing.get("toolCalls", [])
            if isinstance(call, dict)
        )
        tool_call_timing_overflow += max(
            0, int(timing.get("toolCallTimingOverflow", 0))
        )
        recorded_nonprogress_tokens = timing.get("observationalNonprogressTokens")
        if isinstance(recorded_nonprogress_tokens, dict):
            nonprogress_token_aggregates.append(recorded_nonprogress_tokens)
        status_counts[record["status"]] += 1
        totals["inclusiveDurationNs"] += int(timing.get("inclusiveDurationNs", 0))
        totals["machineDurationNs"] += int(
            timing.get(
                "machineDurationNs",
                max(
                    0,
                    int(timing.get("inclusiveDurationNs", 0))
                    - int(exclusive.get("interactiveOnlyWaitNs", 0)),
                ),
            )
        )
        totals["modelOnlyNs"] += int(exclusive.get("modelOnlyNs", 0))
        totals["toolOnlyNs"] += int(exclusive.get("toolOnlyNs", 0))
        totals["modelPlusToolNs"] += int(exclusive.get("modelPlusToolNs", 0))
        totals["orchestrationNs"] += int(exclusive.get("orchestrationNs", 0))
        totals["retryOnlyNs"] += int(exclusive.get("retryOnlyNs", 0))
        totals["interactiveOnlyWaitNs"] += int(
            exclusive.get("interactiveOnlyWaitNs", 0)
        )
        totals["interactivePlusMachineNs"] += int(
            exclusive.get("interactivePlusMachineNs", 0)
        )
        totals["interactiveWaitUnionNs"] += int(
            unions.get(
                "interactiveWaitUnionNs",
                int(exclusive.get("interactiveOnlyWaitNs", 0))
                + int(exclusive.get("interactivePlusMachineNs", 0)),
            )
        )
        totals["finalizationNs"] += int(exclusive.get("finalizationNs", 0))
        totals["standaloneWorkNs"] += int(exclusive.get("standaloneWorkNs", 0))
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
        for key in (
            "residualDeterministicGenerationCount",
            "ownerDrainedContinuationCount",
            "executedValidationCount",
            "reusedValidationCount",
            "duplicateValidationCount",
            "forcedFreshValidationCount",
            "exactRepeatedWaitCount",
            "waitOnlyGenerationCount",
            "internallyDrainedWaitCount",
            "noProgressDirectiveCount",
            "provenLoopActivationCount",
        ):
            totals[key] += int(counters.get(key, 0))
        request_count += sum(_physical_attempt_count(request) for request in requests)
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
    machine = totals["machineDurationNs"]
    model = totals["modelOnlyNs"]
    tool = totals["toolOnlyNs"]
    return {
        "turns": len(records),
        "statusCounts": dict(sorted(status_counts.items())),
        **dict(totals),
        "modelShare": model / machine if machine else None,
        "toolShare": tool / machine if machine else None,
        "agentActiveShareOfWall": machine / inclusive if inclusive else None,
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
        "tokens": _token_report(all_requests),
        "observationalNonprogressTokens": _diagnostic_token_report(
            nonprogress_token_aggregates
        ),
        "toolRelay": _tool_relay_report(all_tool_calls, tool_call_timing_overflow),
        "observationalNonprogressLatency": nonprogress,
        "deterministicToolContinuationLatency": deterministic,
    }


def _turn_report(
    record: dict[str, Any],
    repo_root: Path,
    command_records: list[dict[str, Any]],
) -> dict[str, Any]:
    timing = record["timing"]
    exclusive = timing.get("exclusive", {})
    unions = timing.get("unions", {})
    counters = timing.get("counters", {})
    requests = _selected_requests(timing)
    tool_calls = [
        call for call in timing.get("toolCalls", []) if isinstance(call, dict)
    ]
    tokens = _token_report(requests)
    token_intervals = _token_intervals(requests, tool_calls)
    relay = _tool_relay_report(tool_calls, int(timing.get("toolCallTimingOverflow", 0)))
    orchestration = _command_orchestration_report(command_records)
    inclusive_ns = int(timing.get("inclusiveDurationNs", 0))
    human_wait_ns = int(exclusive.get("interactiveOnlyWaitNs", 0))
    human_wait_union_ns = int(
        unions.get(
            "interactiveWaitUnionNs",
            human_wait_ns + int(exclusive.get("interactivePlusMachineNs", 0)),
        )
    )
    machine_ns = int(
        timing.get("machineDurationNs", max(0, inclusive_ns - human_wait_ns))
    )
    nonprogress = timing.get("observationalNonprogressLatency")
    if not isinstance(nonprogress, dict):
        nonprogress = _request_metric(
            requests,
            lambda request: (
                bool(request.get("unchangedRelevantState"))
                and not bool(request.get("nextStructuredActionChanged"))
            ),
        )

    signals: list[str] = []
    if record["status"] == "turn_aborted":
        signals.append("turn_aborted")
    if timing.get("profileValid") is not True:
        signals.append("invalid_timing_profile")
    if timing.get("classificationComplete") is not True:
        signals.append("incomplete_timing_classification")
    if orchestration["failedToolCalls"]:
        signals.append("failed_tool_call")
    if orchestration["runningToolCalls"]:
        signals.append("tool_reported_running")
    if relay["processAliveAtDeliveryCalls"]:
        signals.append("process_alive_at_delivery")
    if relay["incompleteLifecycleCalls"]:
        signals.append("incomplete_tool_lifecycle")
    if requests and tokens["providerUsageAttempts"] < len(requests):
        signals.append("partial_token_coverage")
    if int(nonprogress.get("logicalGenerations", 0)):
        signals.append("observational_nonprogress")
    sampling_passes = int(timing.get("counters", {}).get("logicalGenerationCount", 0))
    if sampling_passes > _SAMPLING_PASS_TARGET_PER_COMPLETED_TURN:
        signals.append("sampling_pass_target_exceeded")

    return {
        "turnId": record["turn_id"],
        "status": record["status"],
        "lifecycle": record["lifecycle"],
        "timestamp": record["timestamp"],
        "file": record["file"],
        "line": record["line"],
        "cwd": record["cwd"],
        "population": _population(record["cwd"], str(repo_root)),
        "timingSchemaVersion": timing.get("schemaVersion"),
        "profileValid": timing.get("profileValid") is True,
        "classificationComplete": timing.get("classificationComplete") is True,
        "inclusiveDurationNs": inclusive_ns,
        "agentActiveDurationNs": machine_ns,
        "humanWaitNs": human_wait_ns,
        "humanOnlyWaitNs": human_wait_ns,
        "humanWaitUnionNs": human_wait_union_ns,
        "humanWaitDefinition": "interactive_only_additive_partition",
        "humanWaitCounts": {
            key: int(counters.get(key, 0))
            for key in (
                "approvalWaitCount",
                "permissionWaitCount",
                "userInputWaitCount",
                "mcpElicitationWaitCount",
            )
        },
        **_turn_boundaries(timing, record["timestamp"]),
        "exclusive": {
            key: int(exclusive.get(key, 0))
            for key in (
                "modelOnlyNs",
                "toolOnlyNs",
                "modelPlusToolNs",
                "orchestrationNs",
                "retryOnlyNs",
                "interactiveOnlyWaitNs",
                "interactivePlusMachineNs",
                "standaloneWorkNs",
                "finalizationNs",
                "unclassifiedNs",
            )
        },
        "counters": dict(counters),
        "samplingPasses": sampling_passes,
        "samplingPassTarget": _SAMPLING_PASS_TARGET_PER_COMPLETED_TURN,
        "tokens": tokens,
        "tokenIntervals": token_intervals,
        "observationalNonprogressTokens": _diagnostic_token_report(
            [timing.get("observationalNonprogressTokens", {})]
        ),
        "observationalNonprogressLatency": nonprogress,
        "toolRelay": relay,
        "commandOrchestration": orchestration,
        "signals": signals,
    }


def _behavior_report(
    per_turn: list[dict[str, Any]],
    coverage: dict[str, Any],
    command_orchestration: dict[str, Any],
    tool_relay: dict[str, Any],
) -> dict[str, Any]:
    return {
        "activeTurnsExcluded": coverage["startedTurnsWithoutTerminal"],
        "openTurnStateCounts": coverage.get("openTurnStateCounts", {}),
        "terminalLifecycleStateCounts": coverage.get(
            "terminalLifecycleStateCounts", {}
        ),
        "abortedTurns": sum(turn["status"] == "turn_aborted" for turn in per_turn),
        "canceledTurns": sum(turn["lifecycle"] == "canceled" for turn in per_turn),
        "failedTurns": sum(turn["lifecycle"] == "failed" for turn in per_turn),
        "abandonedTurns": sum(turn["lifecycle"] == "abandoned" for turn in per_turn),
        "runningProcessTurns": coverage.get("openTurnStateCounts", {}).get(
            "running_process", 0
        ),
        "userWaitingTurns": coverage.get("openTurnStateCounts", {}).get(
            "user_waiting", 0
        ),
        "activeWithoutPendingToolTurns": coverage.get("openTurnStateCounts", {}).get(
            "active_without_pending_tool", 0
        ),
        "invalidTimingProfiles": coverage["invalidProfiles"],
        "incompleteTimingClassifications": coverage["classificationIncompleteProfiles"],
        "terminalTurnsWithoutTiming": coverage["terminalTurnsWithoutTiming"],
        "failedToolCalls": command_orchestration["failedToolCalls"],
        "runningToolCalls": command_orchestration["runningToolCalls"],
        "unpairedToolCalls": coverage.get("unpairedToolCalls", 0),
        "processAliveAtDeliveryCalls": tool_relay["processAliveAtDeliveryCalls"],
        "incompleteToolLifecycleCalls": tool_relay["incompleteLifecycleCalls"],
        "turnsWithObservationalNonprogress": sum(
            "observational_nonprogress" in turn["signals"] for turn in per_turn
        ),
        "turnsWithPartialTokenCoverage": sum(
            "partial_token_coverage" in turn["signals"] for turn in per_turn
        ),
        "samplingPassTarget": _SAMPLING_PASS_TARGET_PER_COMPLETED_TURN,
        "turnsOverSamplingPassTarget": sum(
            "sampling_pass_target_exceeded" in turn["signals"] for turn in per_turn
        ),
    }


def analyze_session_path(source: Path, repo_root: Path) -> dict[str, Any]:
    files = [source] if source.is_file() else sorted(source.rglob("*.jsonl"))
    started_turns: set[str] = set()
    turn_starts: dict[str, dict[str, Any]] = {}
    unresolved_tools_by_turn: dict[str, list[str]] = collections.defaultdict(list)
    terminal_turns: set[str] = set()
    terminal_without_timing: set[str] = set()
    timed_records: dict[str, dict[str, Any]] = {}
    duplicate_timed_terminal_events = 0
    parse_error_count = 0
    parse_errors: list[dict[str, Any]] = []
    status_counts: collections.Counter[str] = collections.Counter()
    terminal_lifecycle_counts: collections.Counter[str] = collections.Counter()
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
        active_turn_id: str | None = None
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
                    if calls_since_sampling_boundary:
                        execution_loop_counts["samplingPassesWithTools"] += 1
                        if calls_since_sampling_boundary > 1:
                            execution_loop_counts["multiToolCallSamplingPasses"] += 1
                            execution_loop_counts["batchedToolCalls"] += (
                                calls_since_sampling_boundary
                            )
                        else:
                            execution_loop_counts["singleToolCallSamplingPasses"] += 1
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
                            "turnId": active_turn_id,
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
                                "turnId": pending["turnId"],
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
                    active_turn_id = turn_id
                    started_turns.add(turn_id)
                    turn_starts[turn_id] = {
                        "turnId": turn_id,
                        "file": str(file),
                        "timestamp": item.get("timestamp"),
                        "cwd": cwd,
                    }
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
                if active_turn_id == turn_id:
                    active_turn_id = None
                status_counts[str(payload_type)] += 1
                record = _terminal_record(
                    file, line_number, item.get("timestamp"), cwd, payload
                )
                terminal_lifecycle_counts[record["lifecycle"]] += 1
                timing = record["timing"]
                if not isinstance(timing, dict):
                    terminal_without_timing.add(turn_id)
                    continue
                if turn_id in timed_records:
                    duplicate_timed_terminal_events += 1
                timed_records[turn_id] = record
                schema_versions[str(timing.get("schemaVersion", "missing"))] += 1
        for pending in pending_tool_calls.values():
            pending_turn_id = pending.get("turnId")
            if pending_turn_id is not None:
                unresolved_tools_by_turn[str(pending_turn_id)].append(
                    str(pending.get("tool") or "unknown")
                )
        if calls_since_sampling_boundary:
            execution_loop_counts["samplingPassesWithTools"] += 1
            if calls_since_sampling_boundary > 1:
                execution_loop_counts["multiToolCallSamplingPasses"] += 1
                execution_loop_counts["batchedToolCalls"] += (
                    calls_since_sampling_boundary
                )
            else:
                execution_loop_counts["singleToolCallSamplingPasses"] += 1

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
    command_orchestration = _command_orchestration_report(command_orchestration_records)
    valid_tool_calls = [
        {**call, "_turnId": record["turn_id"]}
        for record in valid
        for call in record["timing"].get("toolCalls", [])
        if isinstance(call, dict)
    ]
    tool_relay = _tool_relay_report(
        valid_tool_calls,
        sum(
            max(0, int(record["timing"].get("toolCallTimingOverflow", 0)))
            for record in valid
        ),
    )
    per_turn = sorted(
        (
            _turn_report(
                record,
                repo_root,
                [
                    command
                    for command in command_orchestration_records
                    if command.get("turnId") == record["turn_id"]
                ],
            )
            for record in records
        ),
        key=lambda turn: turn["inclusiveDurationNs"],
        reverse=True,
    )
    open_turns: list[dict[str, Any]] = []
    open_turn_state_counts: collections.Counter[str] = collections.Counter()
    for turn_id in sorted(started_turns - terminal_turns):
        pending_tools = unresolved_tools_by_turn.get(turn_id, [])
        folded_tools = [tool.casefold() for tool in pending_tools]
        if any("request_user_input" in tool for tool in folded_tools):
            state = "user_waiting"
        elif any(
            "request_permissions" in tool or "approval" in tool for tool in folded_tools
        ):
            state = "user_waiting"
        elif pending_tools:
            state = "running_process"
        else:
            state = "active_without_pending_tool"
        open_turn_state_counts[state] += 1
        start = turn_starts.get(turn_id, {"turnId": turn_id})
        open_turns.append(
            {
                **start,
                "state": state,
                "pendingToolCount": len(pending_tools),
                "pendingTools": sorted(set(pending_tools)),
            }
        )

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
        "openTurnStateCounts": dict(sorted(open_turn_state_counts.items())),
        "openTurns": open_turns[:_MAX_OPEN_TURN_DETAILS],
        "omittedOpenTurns": max(0, len(open_turns) - _MAX_OPEN_TURN_DETAILS),
        "terminalLifecycleStateCounts": dict(sorted(terminal_lifecycle_counts.items())),
        "timedTerminalTurnsWithoutStart": len(set(timed_records) - started_turns),
        "statusCounts": dict(sorted(status_counts.items())),
        "timingSchemaVersions": dict(sorted(schema_versions.items())),
        "unpairedToolCalls": execution_loop["unpairedToolCalls"],
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
        "commandOrchestration": command_orchestration,
        "toolRelay": tool_relay,
        "perTurn": per_turn,
        "behaviorSignals": _behavior_report(
            per_turn,
            coverage,
            command_orchestration,
            tool_relay,
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
            f"multi-call passes={execution_loop.get('multiToolCallSamplingPasses', 0)}/"
            f"{execution_loop.get('samplingPassesWithTools', 0)}; "
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
    relay = report["toolRelay"]
    if relay["calls"] or relay["timingOverflowCalls"]:
        phases = relay["phaseTotalsMs"]
        lines.append(
            "persisted tool relay: "
            f"{relay['calls']} calls ({relay['directCalls']} direct/"
            f"{relay['nestedCalls']} nested; "
            f"{relay['timingOverflowCalls']} overflow); "
            f"batched={relay['batchedCalls']} calls in {relay['batchGroups']} generations; "
            f"queue={phases.get('itemToFirstPollMs', 0) / 1e3:.1f}s "
            f"gate={phases.get('parallelGateWaitMs', 0) / 1e3:.1f}s "
            f"handler={phases.get('handlerDurationMs', 0) / 1e3:.1f}s "
            f"process={phases.get('processRuntimeMs', 0) / 1e3:.1f}s "
            f"collect-to-delivery="
            f"{phases.get('outputCollectedToDeliveryMs', 0) / 1e3:.1f}s "
            f"delivery-to-model="
            f"{phases.get('deliveryToModelResumeMs', 0) / 1e3:.1f}s"
        )
    for name in ("all", "eval", "repository_root", "other"):
        population = report["populations"].get(name)
        if not population or not population["turns"]:
            continue
        ratio = population["modelToolRatio"]
        nonprogress = population["observationalNonprogressLatency"]
        decision = population["decisionLatency"]
        tokens = population["tokens"]
        nonprogress_tokens = population["observationalNonprogressTokens"]
        ratio_text = f"{ratio:.2f}x" if ratio is not None else "n/a"
        lines.append(
            f"{name}: {population['turns']} turns; "
            f"agent-active={population['machineDurationNs'] / 1e9:.1f}s "
            f"human-only={population['interactiveOnlyWaitNs'] / 1e9:.1f}s "
            f"human-union={population['interactiveWaitUnionNs'] / 1e9:.1f}s; "
            f"model={population['modelOnlyNs'] / 1e9:.1f}s "
            f"tool={population['toolOnlyNs'] / 1e9:.1f}s "
            f"ratio={ratio_text}; "
            f"decision coverage={decision['decisionReadyAttempts']}/"
            f"{decision['physicalAttempts']}; "
            f"tokens input/cached/output/reasoning="
            f"{tokens['inputTokens']}/{tokens['cachedInputTokens']}/"
            f"{tokens['outputTokens']}/{tokens['reasoningTokens']} "
            f"billable={tokens['billableTokens']} "
            f"(coverage {tokens['providerUsageAttempts']}/"
            f"{tokens['physicalAttempts']}); "
            f"unchanged-state/action stream={nonprogress['modelStreamWaitNs'] / 1e9:.1f}s "
            f"tokens={nonprogress_tokens.get('totalTokens', 0)}; "
            f"wait-only/drained/loop="
            f"{population.get('waitOnlyGenerationCount', 0)}/"
            f"{population.get('internallyDrainedWaitCount', 0)}/"
            f"{population.get('provenLoopActivationCount', 0)}"
        )
    behavior = report["behaviorSignals"]
    lines.append(
        "behavior signals: "
        f"active-excluded={behavior['activeTurnsExcluded']} "
        f"canceled/failed/abandoned="
        f"{behavior['canceledTurns']}/{behavior['failedTurns']}/"
        f"{behavior['abandonedTurns']} "
        f"running/user-waiting/no-pending="
        f"{behavior['runningProcessTurns']}/{behavior['userWaitingTurns']}/"
        f"{behavior['activeWithoutPendingToolTurns']} "
        f"invalid={behavior['invalidTimingProfiles']} "
        f"classification-incomplete={behavior['incompleteTimingClassifications']} "
        f"failed/running tools={behavior['failedToolCalls']}/"
        f"{behavior['runningToolCalls']} "
        f"sampling-target={behavior['samplingPassTarget']} "
        f"over-target={behavior['turnsOverSamplingPassTarget']} "
        f"process-alive-at-delivery={behavior['processAliveAtDeliveryCalls']}"
    )
    if coverage.get("openTurnStateCounts"):
        lines.append(
            "open turn states: "
            + ", ".join(
                f"{state}={count}"
                for state, count in coverage["openTurnStateCounts"].items()
            )
        )
    rendered_turns = report["perTurn"][:_MAX_RENDERED_TURNS]
    for turn in rendered_turns:
        exclusive = turn["exclusive"]
        tokens = turn["tokens"]
        peak_interval = max(
            turn["tokenIntervals"],
            key=lambda interval: interval["tokens"]["observedBillableTokens"],
            default=None,
        )
        peak_interval_text = (
            f"g{peak_interval['generationIndex']}:"
            f"{peak_interval['tokens']['observedBillableTokens']}"
            if peak_interval is not None
            else "none"
        )
        signals = ",".join(turn["signals"]) or "none"
        lines.append(
            f"turn {turn['turnId']}: status={turn['status']} "
            f"boundary={turn['startedAt'] or 'unknown'}.."
            f"{turn['completedAt'] or 'unknown'} "
            f"wall={turn['inclusiveDurationNs'] / 1e9:.1f}s "
            f"agent={turn['agentActiveDurationNs'] / 1e9:.1f}s "
            f"human-only/union={turn['humanOnlyWaitNs'] / 1e9:.1f}/"
            f"{turn['humanWaitUnionNs'] / 1e9:.1f}s "
            f"model/tool/orchestration="
            f"{exclusive['modelOnlyNs'] / 1e9:.1f}/"
            f"{exclusive['toolOnlyNs'] / 1e9:.1f}/"
            f"{exclusive['orchestrationNs'] / 1e9:.1f}s "
            f"tokens={tokens['inputTokens']}/{tokens['cachedInputTokens']}/"
            f"{tokens['outputTokens']}/{tokens['reasoningTokens']} "
            f"billable={tokens['billableTokens']} "
            f"peak-between-tools={peak_interval_text} "
            f"signals={signals}"
        )
    omitted_turns = len(report["perTurn"]) - len(rendered_turns)
    if omitted_turns:
        lines.append(
            f"per-turn detail: {omitted_turns} additional turns in JSON output"
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
        "unpairedToolCalls",
        "openTurnStateCounts",
        "openTurns",
        "omittedOpenTurns",
        "terminalLifecycleStateCounts",
    )
    bounded_turns = []
    for turn in report["perTurn"][:_MAX_SUMMARY_TURNS]:
        bounded_turn = {
            key: turn[key]
            for key in (
                "turnId",
                "status",
                "lifecycle",
                "timestamp",
                "startedAtUnixMs",
                "completedAtUnixMs",
                "startedAt",
                "completedAt",
                "boundarySource",
                "population",
                "timingSchemaVersion",
                "profileValid",
                "classificationComplete",
                "inclusiveDurationNs",
                "agentActiveDurationNs",
                "humanWaitNs",
                "humanOnlyWaitNs",
                "humanWaitUnionNs",
                "humanWaitDefinition",
                "humanWaitCounts",
                "exclusive",
                "samplingPasses",
                "samplingPassTarget",
                "tokens",
                "observationalNonprogressTokens",
                "observationalNonprogressLatency",
                "signals",
            )
        }
        bounded_turn["tokenIntervals"] = turn["tokenIntervals"][
            :_MAX_SUMMARY_TOKEN_INTERVALS
        ]
        bounded_turn["omittedTokenIntervals"] = max(
            0, len(turn["tokenIntervals"]) - _MAX_SUMMARY_TOKEN_INTERVALS
        )
        bounded_turns.append(bounded_turn)
    bounded_coverage = {key: coverage[key] for key in coverage_keys}
    bounded_coverage["openTurns"] = coverage["openTurns"][:_MAX_SUMMARY_TURNS]
    bounded_coverage["omittedOpenTurns"] = coverage["omittedOpenTurns"] + max(
        0, len(coverage["openTurns"]) - _MAX_SUMMARY_TURNS
    )
    return {
        "schemaVersion": report["schemaVersion"],
        "observedAt": report["observedAt"],
        "source": report["source"],
        "repoRoot": report["repoRoot"],
        "coverage": bounded_coverage,
        "executionLoop": report["executionLoop"],
        "commandOrchestration": report["commandOrchestration"],
        "toolRelay": report["toolRelay"],
        "perTurn": bounded_turns,
        "omittedPerTurnRecords": max(0, len(report["perTurn"]) - len(bounded_turns)),
        "behaviorSignals": report["behaviorSignals"],
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
