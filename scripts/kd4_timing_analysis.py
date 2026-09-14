"""Canonical runtime timing analysis shared by KD4 measurement commands.

This library consumes terminal timing records only. Capture, rollout discovery,
harness observations, experiment scoring, and report rendering stay in callers.
Nanosecond unions and exclusive ownership are distinct from summed diagnostics.
"""

from __future__ import annotations

import collections
import re
from collections.abc import Callable, Iterable
from typing import Any

try:
    from scripts.kd4_first_useful_action_analysis import canonical_milestones
except ImportError:
    from kd4_first_useful_action_analysis import canonical_milestones


_OUTPUT_COLLECTED_LIFECYCLE_SCHEMA_VERSION = 25

_CORRELATED_NESTED_LIFECYCLE_SCHEMA_VERSION = 25

_NANOSECONDS_PER_SECOND = 1_000_000_000

_SLOW_TOOL_CALL_NS = 5 * _NANOSECONDS_PER_SECOND

_MAX_SLOW_TOOL_CALLS = 8

_MAX_EXCLUSIVE_GATE_CONVOYS = 8

_TOOL_PHASE_OWNERS = {
    "itemToFirstPollMs": "ToolDispatchQueue",
    "parallelGateWaitMs": "ExclusiveGate",
    "authorizationStateCoordinationMs": "AuthorizationStateCoordination",
    "workspaceEvidenceBeforeMs": "WorkspaceEvidenceBefore",
    "preToolHookMs": "PreToolUse",
    "processRuntimeMs": "ProcessExecution",
    "workspaceEvidenceAfterMs": "WorkspaceEvidenceAfter",
    "postToolHookMs": "PostToolUse",
    "outputProjectionMs": "OutputProjection",
    "historyPersistenceMs": "HistoryPersistence",
}


def _selected_requests(timing: dict[str, Any]) -> list[dict[str, Any]]:
    rows = timing.get("modelRequests")
    return (
        [item for item in rows if isinstance(item, dict)]
        if isinstance(rows, list)
        else []
    )


def timing_profile_valid(timing: Any) -> bool:
    return isinstance(timing, dict) and timing.get("profileValid") is True


def turn_measurements(event: dict[str, Any]) -> tuple[float | None, int | None]:
    """Return union wait and explicitly flagged continuations, not retry count."""
    timing = event.get("timing")
    if not timing_profile_valid(timing):
        return None, None
    unions = timing.get("unions")
    if not isinstance(unions, dict) or not isinstance(
        timing.get("modelRequests"), list
    ):
        return None, None
    wait_ns = unions.get("modelStreamWaitUnionNs")
    if type(wait_ns) is not int or wait_ns < 0:
        return None, None
    return round(wait_ns / 1_000_000, 3), continuation_count(timing)


def continuation_count(timing: dict[str, Any]) -> int | None:
    if not isinstance(timing.get("modelRequests"), list):
        return None
    return sum(row.get("isContinuation") is True for row in _selected_requests(timing))


def analyze_timing(
    timing: dict[str, Any], *, status: str = "task_complete", include_tokens: bool = True
) -> dict[str, Any]:
    """Analyze one terminal profile before a caller applies trace retention limits.

    Callers decide which profiles enter comparisons. Invalid profiles remain
    identifiable; absent milestone evidence never becomes a zero latency.
    """
    report = _population_report([{"timing": timing, "turn_id": None, "status": status}], include_tokens=include_tokens)
    milestones = canonical_milestones(timing)
    report.update(
        {
            "schemaVersion": 1,
            "profileValid": timing.get("profileValid") is True,
            "classificationComplete": timing.get("classificationComplete") is True,
            "continuationCount": continuation_count(timing),
            "firstUsefulActionMs": milestones["firstUsefulActionMs"]
            if milestones
            else None,
        }
    )
    return report


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
    physical_attempts = sum(
        _physical_attempt_count(request) for request in request_list
    )
    totals = collections.Counter()
    prompt_categories = collections.Counter()
    covered_attempts = 0
    categorized_attempts = 0
    for request in request_list:
        usage = request.get("tokenUsage")
        if isinstance(usage, dict) and all(
            type(usage.get(key)) is int and usage[key] >= 0
            for key in ("inputTokens", "cachedInputTokens", "visibleOutputTokens", "reasoningTokens")
        ):
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
        if isinstance(categories, dict) and type(categories.get("logicalTotal")) is int:
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
                if type(categories.get(key)) is int:
                    prompt_categories[key] += max(0, categories[key])

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
    usage_complete = physical_attempts > 0 and covered_attempts == physical_attempts
    return {
        "physicalAttempts": physical_attempts,
        "providerUsageAttempts": covered_attempts,
        "coverage": covered_attempts / physical_attempts if physical_attempts else None,
        "complete": usage_complete,
        "providerTotals": dict(totals) if usage_complete else None,
        **dict(totals),
        "billableTokens": observed_billable_tokens if usage_complete else None,
        "observedBillableTokens": observed_billable_tokens,
        "billableDefinition": "provider_input_including_cached_plus_output",
        "blendedTokens": observed_blended_tokens if usage_complete else None,
        "observedBlendedTokens": observed_blended_tokens,
        "blendedDefinition": "non_cached_input_plus_output",
        "promptCategoryAttempts": categorized_attempts,
        "promptCategories": dict(prompt_categories) if categorized_attempts else None,
        "promptCategoryBasis": "native_estimate",
        "promptCategoryCoverage": categorized_attempts / len(request_list) if request_list else None,
        "promptCategoryEvidence": {
            "accountingBases": sorted({str(request["requestTokenCategories"].get("accountingBasis", "unknown")) for request in request_list if isinstance(request.get("requestTokenCategories"), dict)}),
            **{key: sum(request["requestTokenCategories"][key] for request in request_list if isinstance(request.get("requestTokenCategories"), dict) and type(request["requestTokenCategories"].get(key)) is int)
               if any(isinstance(request.get("requestTokenCategories"), dict) and type(request["requestTokenCategories"].get(key)) is int for request in request_list) else None
               for key in ("localReconciliationResidual", "providerReconciliationResidual", "providerInputTokens")},
        },
        "rankedPromptConsumers": [
            {"category": key, "tokens": value, "share": value / prompt_categories["logicalTotal"] if prompt_categories["logicalTotal"] else None, "denominator": "covered_logical_prompt_estimate"}
            for key, value in sorted(prompt_categories.items(), key=lambda pair: pair[1], reverse=True)
            if key not in ("logicalTotal", "localInputEstimate", "repeatedUnchangedContext")
        ],
        "accountingNote": "Counts are observed subtotals when coverage is partial; cached input and reasoning output are subsets.",
        "available": bool(covered_attempts or any("outputTokens" in request for request in request_list)),
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

        preceding_model_visible_ms = [
            _tool_model_visible_at_ms(call)
            for call in preceding_calls
            if _tool_model_visible_at_ms(call) is not None
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
        attempt_kinds = [
            str(item.get("attemptKind", "primary")) for item in interval_requests
        ]
        intervals.append(
            {
                "requestIndex": request_indexes[0],
                "requestIndexes": request_indexes,
                "generationIndex": generation_index,
                "attemptKind": attempt_kinds[0]
                if len(attempt_kinds) == 1
                else "multiple",
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
                "precedingResultsModelVisibleAtMs": (
                    max(preceding_model_visible_ms)
                    if preceding_model_visible_ms
                    else None
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
    totals["observedBillableTokens"] = totals["inputTokens"] + totals["outputTokens"]
    return dict(totals)


def _tool_model_visible_at_ms(call: dict[str, Any]) -> int | None:
    model_visible_at = call.get("outputModelVisibleAtMs")
    if isinstance(model_visible_at, int):
        return model_visible_at
    delivered_at = call.get("deliveredAtMs")
    return delivered_at if isinstance(delivered_at, int) else None


def _tool_call_end_to_end_duration_ms(call: dict[str, Any]) -> int:
    accepted_at = call.get("acceptedAtMs")
    model_visible_at = _tool_model_visible_at_ms(call)
    if isinstance(accepted_at, int) and model_visible_at is not None:
        return max(0, model_visible_at - accepted_at)

    relay_ms = max(0, int(call.get("totalDurationMs") or 0))
    queued_ms = max(0, int(call.get("itemToFirstPollMs") or 0))
    return queued_ms + relay_ms


def _tool_phase_durations_ms(call: dict[str, Any]) -> dict[str, int]:
    phases = {
        key: max(0, int(call.get(key) or 0))
        for key in _TOOL_PHASE_OWNERS
        if key != "processRuntimeMs"
    }
    process_spawned_at = call.get("processSpawnedAtMs")
    process_exited_at = call.get("processExitedAtMs")
    phases["processRuntimeMs"] = (
        max(0, process_exited_at - process_spawned_at)
        if isinstance(process_spawned_at, int) and isinstance(process_exited_at, int)
        else 0
    )
    return phases


def _dominant_tool_phase(phases: dict[str, int]) -> tuple[str | None, str | None, int]:
    if not phases:
        return None, None, 0
    phase, duration_ms = max(phases.items(), key=lambda item: item[1])
    if duration_ms <= 0:
        return None, None, 0
    return phase, _TOOL_PHASE_OWNERS[phase], duration_ms


def _tool_lifecycle_missing_boundaries(call: dict[str, Any]) -> list[str]:
    source = call.get("source", "direct")
    timing_schema_version = call.get("_timingSchemaVersion")
    legacy_nested_lifecycle = (
        source != "direct"
        and isinstance(timing_schema_version, int)
        and 0 < timing_schema_version < _OUTPUT_COLLECTED_LIFECYCLE_SCHEMA_VERSION
    )
    required = ["acceptedAtMs"]
    if not legacy_nested_lifecycle:
        required.append("outputCollectedAtMs")
    missing = [field for field in required if not isinstance(call.get(field), int)]
    if source == "direct":
        direct_required = ["deliveredAtMs"]
        if call.get("_turnStatus") != "turn_aborted":
            direct_required.append("outputModelVisibleAtMs")
        missing.extend(
            field for field in direct_required if not isinstance(call.get(field), int)
        )
    elif (
        isinstance(timing_schema_version, int)
        and timing_schema_version >= _CORRELATED_NESTED_LIFECYCLE_SCHEMA_VERSION
    ):
        missing.extend(
            field
            for field in ("parentCallId", "parentCellId", "runtimeToolCallId")
            if not isinstance(call.get(field), str) or not call[field]
        )
    return missing


def _expected_terminal_abort_model_visibility_truncation(
    call: dict[str, Any],
) -> bool:
    return (
        call.get("source", "direct") == "direct"
        and call.get("_turnStatus") == "turn_aborted"
        and not isinstance(call.get("outputModelVisibleAtMs"), int)
        and all(
            isinstance(call.get(field), int)
            for field in ("acceptedAtMs", "outputCollectedAtMs", "deliveredAtMs")
        )
    )


def _tool_relay_report(
    records: Iterable[dict[str, Any]], overflow_count: int = 0
) -> dict[str, Any]:
    calls = [record for record in records if isinstance(record, dict)]
    totals = collections.Counter()
    generation_calls: dict[tuple[Any, int], list[dict[str, Any]]] = (
        collections.defaultdict(list)
    )
    incomplete = 0
    incomplete_reasons = collections.Counter()
    incomplete_direct = 0
    incomplete_nested = 0
    expected_terminal_abort_truncations = 0
    for call in calls:
        model_visible_at = _tool_model_visible_at_ms(call)
        generation_index = call.get("generationIndex")
        if isinstance(generation_index, int):
            generation_calls[(call.get("_turnId"), generation_index)].append(call)
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
            (
                "processExitToOutputCollectedMs",
                "processExitedAtMs",
                "outputCollectedAtMs",
            ),
        ):
            start = call.get(start_key)
            end = call.get(end_key)
            if isinstance(start, int) and isinstance(end, int):
                totals[key] += max(0, end - start)
        process_exited_at = call.get("processExitedAtMs")
        output_collected_at = call.get("outputCollectedAtMs")
        model_resumed_at = call.get("modelResumedAtMs")
        if isinstance(process_exited_at, int) and model_visible_at is not None:
            totals["processExitToModelVisibleMs"] += max(
                0, model_visible_at - process_exited_at
            )
            totals["modelVisibleToProcessExitMs"] += max(
                0, process_exited_at - model_visible_at
            )
        if isinstance(output_collected_at, int) and model_visible_at is not None:
            totals["outputCollectedToModelVisibleMs"] += max(
                0, model_visible_at - output_collected_at
            )
        if isinstance(model_resumed_at, int) and model_visible_at is not None:
            totals["modelVisibleToModelResumeMs"] += max(
                0, model_resumed_at - model_visible_at
            )
        missing_boundaries = _tool_lifecycle_missing_boundaries(call)
        if _expected_terminal_abort_model_visibility_truncation(call):
            expected_terminal_abort_truncations += 1
        if missing_boundaries:
            incomplete += 1
            incomplete_reasons.update(missing_boundaries)
            if call.get("source", "direct") == "direct":
                incomplete_direct += 1
            else:
                incomplete_nested += 1

    dominant_phase, dominant_owner, dominant_phase_ms = _dominant_tool_phase(
        {phase: int(totals.get(phase, 0)) for phase in _TOOL_PHASE_OWNERS}
    )
    slow_calls = sorted(
        (
            {
                "callId": str(call.get("callId") or ""),
                "tool": str(call.get("toolName") or "unknown"),
                "source": str(call.get("source") or "direct"),
                "totalDurationMs": max(0, int(call.get("totalDurationMs") or 0)),
                "endToEndDurationMs": _tool_call_end_to_end_duration_ms(call),
                "processAliveAtDelivery": bool(call.get("processAliveAtDelivery")),
                "outputModelVisibilityRecorded": isinstance(
                    call.get("outputModelVisibleAtMs"), int
                ),
                "dominantPhase": _dominant_tool_phase(_tool_phase_durations_ms(call))[
                    0
                ],
                "dominantPhaseOwner": _dominant_tool_phase(
                    _tool_phase_durations_ms(call)
                )[1],
                "dominantPhaseMs": _dominant_tool_phase(_tool_phase_durations_ms(call))[
                    2
                ],
            }
            for call in calls
            if _tool_call_end_to_end_duration_ms(call)
            >= _SLOW_TOOL_CALL_NS // 1_000_000
        ),
        key=lambda call: call["endToEndDurationMs"],
        reverse=True,
    )
    generation_counts = {
        key: len(group_calls) for key, group_calls in generation_calls.items()
    }
    batch_groups = sum(count > 1 for count in generation_counts.values())
    batched_calls = sum(count for count in generation_counts.values() if count > 1)
    convoys = sorted(
        (
            {
                "turnId": str(turn_id) if turn_id is not None else None,
                "generationIndex": generation_index,
                "callIds": [str(call.get("callId") or "") for call in group_calls],
                "waitingCallIds": [
                    str(call.get("callId") or "")
                    for call in group_calls
                    if max(0, int(call.get("parallelGateWaitMs") or 0)) > 0
                ],
                "parallelGateWaitMs": sum(
                    max(0, int(call.get("parallelGateWaitMs") or 0))
                    for call in group_calls
                ),
            }
            for (turn_id, generation_index), group_calls in generation_calls.items()
            if len(group_calls) > 1
            and sum(
                max(0, int(call.get("parallelGateWaitMs") or 0)) for call in group_calls
            )
            >= _SLOW_TOOL_CALL_NS // 1_000_000
        ),
        key=lambda convoy: convoy["parallelGateWaitMs"],
        reverse=True,
    )
    return {
        "evidenceSource": "toolCalls" if calls else "none",
        "calls": len(calls),
        "timingOverflowCalls": max(0, int(overflow_count)),
        "directCalls": sum(call.get("source", "direct") == "direct" for call in calls),
        "nestedCalls": sum(call.get("source") == "code_mode" for call in calls),
        "eagerCalls": sum(bool(call.get("eager")) for call in calls),
        "processAliveAtDeliveryCalls": sum(
            bool(call.get("processAliveAtDelivery")) for call in calls
        ),
        "outputModelVisibilityRecordedCalls": sum(
            isinstance(call.get("outputModelVisibleAtMs"), int) for call in calls
        ),
        "incompleteLifecycleCalls": incomplete,
        "incompleteDirectLifecycleCalls": incomplete_direct,
        "incompleteNestedLifecycleCalls": incomplete_nested,
        "incompleteLifecycleReasonCounts": dict(sorted(incomplete_reasons.items())),
        "expectedTerminalAbortModelVisibilityTruncations": (
            expected_terminal_abort_truncations
        ),
        "generationGroups": len(generation_counts),
        "batchGroups": batch_groups,
        "batchedCalls": batched_calls,
        "singleCallGroups": sum(count == 1 for count in generation_counts.values()),
        "phaseTotalsMs": dict(totals),
        "dominantPhase": dominant_phase,
        "dominantPhaseOwner": dominant_owner,
        "dominantPhaseMs": dominant_phase_ms,
        "exclusiveGateConvoyCount": len(convoys),
        "topExclusiveGateConvoys": convoys[:_MAX_EXCLUSIVE_GATE_CONVOYS],
        "omittedExclusiveGateConvoys": max(
            0, len(convoys) - _MAX_EXCLUSIVE_GATE_CONVOYS
        ),
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
        "physicalAttempts": sum(
            _physical_attempt_count(request) for request in matching
        ),
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


def _generation_purpose_latency_report(
    requests: Iterable[dict[str, Any]],
) -> dict[str, dict[str, int]]:
    by_purpose: dict[str, collections.Counter[str]] = collections.defaultdict(
        collections.Counter
    )
    for request in requests:
        purpose = str(request.get("generationPurpose") or "unknown")
        metrics = by_purpose[purpose]
        metrics["logicalGenerations"] += int(
            request.get("attemptKind", "primary") == "primary"
        )
        metrics["physicalAttempts"] += _physical_attempt_count(request)
        metrics["modelStreamWaitNs"] += max(0, int(request.get("modelStreamWaitNs", 0)))
        decision_latency = request.get("decisionLatencyNs")
        if decision_latency is not None:
            metrics["decisionReadyAttempts"] += 1
            metrics["decisionLatencyNs"] += max(0, int(decision_latency))
        metrics["toolCalls"] += max(0, int(request.get("toolCallCount", 0)))

    report: dict[str, dict[str, int]] = {}
    for purpose, metrics in sorted(
        by_purpose.items(),
        key=lambda item: (-item[1]["modelStreamWaitNs"], item[0]),
    ):
        metrics["retryAttempts"] = max(
            0, metrics["physicalAttempts"] - metrics["logicalGenerations"]
        )
        report[purpose] = dict(metrics)
    return report


def _population_report(records: list[dict[str, Any]], *, include_tokens: bool = True) -> dict[str, Any]:
    totals = collections.Counter()
    local_totals = collections.Counter()
    pre_first_output_totals = collections.Counter()
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
        local = timing.get("local", {})
        counters = timing.get("counters", {})
        requests = _selected_requests(timing)
        all_requests.extend(requests)
        all_tool_calls.extend(
            {
                **call,
                "_turnId": record["turn_id"],
                "_timingSchemaVersion": timing.get("schemaVersion"),
                "_turnStatus": record["status"],
            }
            for call in timing.get("toolCalls", [])
            if isinstance(call, dict)
        )
        tool_call_timing_overflow += max(
            0, int(timing.get("toolCallTimingOverflow", 0))
        )
        recorded_nonprogress_tokens = timing.get("observationalNonprogressTokens")
        if include_tokens and isinstance(recorded_nonprogress_tokens, dict):
            nonprogress_token_aggregates.append(recorded_nonprogress_tokens)
        status_counts[record["status"]] += 1
        inclusive_ns = int(timing.get("inclusiveDurationNs", 0))
        machine_ns = int(
            timing.get(
                "machineDurationNs",
                max(
                    0,
                    inclusive_ns - int(exclusive.get("interactiveOnlyWaitNs", 0)),
                ),
            )
        )
        orchestration_ns = int(exclusive.get("orchestrationNs", 0))
        totals["inclusiveDurationNs"] += inclusive_ns
        totals["machineDurationNs"] += machine_ns
        totals["modelOnlyNs"] += int(exclusive.get("modelOnlyNs", 0))
        totals["toolOnlyNs"] += int(exclusive.get("toolOnlyNs", 0))
        totals["modelPlusToolNs"] += int(exclusive.get("modelPlusToolNs", 0))
        totals["orchestrationNs"] += orchestration_ns
        totals["orchestrationMajorityTurns"] += int(
            machine_ns > 0 and orchestration_ns * 2 >= machine_ns
        )
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
        totals["modelActiveUnionNs"] += int(unions.get("modelActiveUnionNs", 0))
        totals["modelRequestWaitNs"] += int(unions.get("modelRequestWaitUnionNs", 0))
        totals["modelStreamWaitNs"] += int(unions.get("modelStreamWaitUnionNs", 0))
        totals["modelStreamProcessingNs"] += int(
            unions.get("modelStreamProcessingUnionNs", 0)
        )
        for key in (
            "preparationUnionNs",
            "planningUnionNs",
            "planningExclusiveUnionNs",
            "planningCompactionOverlapUnionNs",
            "compactionUnionNs",
            "persistenceUnionNs",
            "serializationUnionNs",
            "routerBuildUnionNs",
            "startupPrewarmWaitUnionNs",
            "executorReadinessWaitUnionNs",
        ):
            local_totals[key] += int(local.get(key, 0))
        pre_first_output = timing.get("preFirstModelOutput")
        if isinstance(pre_first_output, dict):
            pre_first_output_totals["profiles"] += 1
            for key in (
                "clientCriticalPathNs",
                "attributedClientUnionNs",
                "unattributedPreOutputNs",
                "historySnapshotNs",
                "normalizationNs",
                "promptConstructionNs",
                "requestTransformationNs",
                "serializationNs",
                "transportReadinessNs",
            ):
                pre_first_output_totals[key] += int(pre_first_output.get(key, 0))
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
        "localActivityUnionsNs": {
            "preparationNs": local_totals["preparationUnionNs"],
            "planningNs": local_totals["planningUnionNs"],
            "planningExclusiveNs": local_totals["planningExclusiveUnionNs"],
            "planningCompactionOverlapNs": local_totals[
                "planningCompactionOverlapUnionNs"
            ],
            "compactionNs": local_totals["compactionUnionNs"],
            "persistenceNs": local_totals["persistenceUnionNs"],
            "serializationNs": local_totals["serializationUnionNs"],
            "routerBuildNs": local_totals["routerBuildUnionNs"],
            "startupPrewarmWaitNs": local_totals["startupPrewarmWaitUnionNs"],
            "executorReadinessWaitNs": local_totals["executorReadinessWaitUnionNs"],
        },
        "preFirstModelOutput": {
            "profiles": pre_first_output_totals["profiles"],
            **{
                key: pre_first_output_totals[key]
                for key in (
                    "clientCriticalPathNs",
                    "attributedClientUnionNs",
                    "unattributedPreOutputNs",
                    "historySnapshotNs",
                    "normalizationNs",
                    "promptConstructionNs",
                    "requestTransformationNs",
                    "serializationNs",
                    "transportReadinessNs",
                )
            },
        },
        "generationPurposeLatency": _generation_purpose_latency_report(all_requests),
        "tokens": _token_report(all_requests) if include_tokens else disabled_tokens(),
        "observationalNonprogressTokens": _diagnostic_token_report(
            nonprogress_token_aggregates
        ) if include_tokens else {},
        "toolRelay": _tool_relay_report(all_tool_calls, tool_call_timing_overflow),
        "observationalNonprogressLatency": nonprogress,
        "deterministicToolContinuationLatency": deterministic,
    }


def disabled_tokens() -> dict[str, Any]:
    """A sentinel with no counting or inspection of token-bearing evidence."""
    return {
        "enabled": False, "available": False, "complete": False,
        **dict.fromkeys(("inputTokens", "cachedInputTokens", "nonCachedInputTokens",
                        "outputTokens", "visibleOutputTokens", "reasoningTokens",
                        "totalTokens", "billableTokens", "observedBillableTokens",
                        "cacheShare", "physicalAttempts", "providerUsageAttempts")),
    }


def _native_usage(usage: Any) -> dict[str, Any] | None:
    """Normalize native usage without adding cached/reasoning subsets twice."""
    if not isinstance(usage, dict):
        return None
    fields = {
        "inputTokens": ("inputTokens", "input_tokens"),
        "cachedInputTokens": ("cachedInputTokens", "cached_input_tokens"),
        "outputTokens": ("outputTokens", "output_tokens"),
        "reasoningTokens": ("reasoningOutputTokens", "reasoning_output_tokens", "reasoningTokens"),
    }
    counts = {
        key: next((usage[name] for name in names if type(usage.get(name)) is int and usage[name] >= 0), None)
        for key, names in fields.items()
    }
    if counts["inputTokens"] is None or counts["outputTokens"] is None:
        return None
    counts["totalTokens"] = counts["inputTokens"] + counts["outputTokens"]
    counts["nonCachedInputTokens"] = (
        max(0, counts["inputTokens"] - counts["cachedInputTokens"])
        if counts["cachedInputTokens"] is not None else None
    )
    counts["visibleOutputTokens"] = (
        max(0, counts["outputTokens"] - counts["reasoningTokens"])
        if counts["reasoningTokens"] is not None else None
    )
    return counts


def analyze_runner_evidence(evidence: dict[str, Any], *, include_tokens: bool = True) -> dict[str, Any]:
    """Analyze one attempt's captured native events; never schedule or compare runs.

    Event indices refer to preserved inputs. Terminal profiles are deduplicated by
    turn, and streamed item updates by item id. Missing native details stay unknown.
    Text matches report evidence of a symptom, never a proven reason for a retry.
    """
    if not isinstance(evidence, dict) or evidence.get("schemaVersion") != 1:
        raise ValueError("runner evidence requires schemaVersion 1")
    events = evidence.get("events", [])
    if not isinstance(events, list) or any(not isinstance(row, dict) for row in events):
        raise ValueError("runner evidence events must be objects")
    profiles: dict[str, dict[str, Any]] = {}
    pending: dict[str, dict[str, Any]] = {}
    calls: dict[str, dict[str, Any]] = {}
    failures: list[dict[str, Any]] = []
    symptoms: list[dict[str, Any]] = []
    terminal: dict[str, str] = {}
    usage_by_thread: dict[str, dict[str, Any]] = {}
    sampling_count = 0
    last_progress = None
    first_output = None
    first_tool = None
    active_turn = "unknown"
    event_gaps = []
    previous_event = None
    for index, row in enumerate(events):
        message = row.get("message", row.get("event", row))
        if not isinstance(message, dict):
            continue
        method = message.get("method", message.get("type", ""))
        params = message.get("params", message.get("payload", message))
        if not isinstance(params, dict):
            continue
        elapsed = row.get("elapsedMs")
        location = {"eventIndex": index, "elapsedMs": elapsed}
        if type(elapsed) in (int, float):
            if previous_event is not None and elapsed >= previous_event["elapsedMs"]:
                event_gaps.append({"fromEventIndex": previous_event["eventIndex"], "toEventIndex": index, "durationMs": elapsed - previous_event["elapsedMs"], "pendingToolIds": [call["id"] for call in pending.values()], "cause": "unclassified"})
            previous_event = location
        turn = params.get("turn", {})
        turn = turn if isinstance(turn, dict) else {}
        active_turn = str(params.get("turnId", params.get("turn_id", turn.get("id", active_turn))))
        payload_type = params.get("type", "")
        if method == "sampling_boundary":
            sampling_count += 1
        if method in ("turn/completed", "turn.completed") or payload_type in ("task_complete", "turn_aborted"):
            status = str(turn.get("status", "aborted" if payload_type == "turn_aborted" else "completed"))
            terminal[active_turn] = status
            if status not in ("completed", "complete"):
                failures.append({"kind": "turn_" + status, **location, "evidence": turn.get("error")})
        timing = params.get("timing", turn.get("timing"))
        if isinstance(timing, dict):
            profiles[active_turn] = {"timing": timing, "turn_id": active_turn, "status": terminal.get(active_turn, "unfinished"), "eventIndex": index}
        if include_tokens and method == "thread/tokenUsage/updated":
            usage = params.get("tokenUsage", {})
            if isinstance(usage, dict):
                counts = _native_usage(usage.get("total"))
                if counts:
                    usage_by_thread[str(params.get("threadId", "unknown"))] = {**counts, **location}
        if include_tokens and isinstance(params.get("usage"), dict):
            counts = _native_usage(params["usage"])
            if counts:
                usage_by_thread[active_turn] = {**counts, **location}
        item = params.get("item", params)
        if not isinstance(item, dict):
            continue
        item_type = item.get("type", "")
        is_call = item_type in ("commandExecution", "mcpToolCall", "dynamicToolCall", "fileChange", "function_call", "custom_tool_call", "webSearch")
        is_output = item_type in ("function_call_output", "custom_tool_call_output")
        call_id = str(item.get("id", item.get("call_id", params.get("itemId", ""))))
        if is_call and call_id:
            key = active_turn + ":" + call_id
            call = calls.setdefault(key, {"id": call_id, "turnId": active_turn, "tool": item.get("name", item_type), "startedMs": elapsed, "eventIndex": index})
            call.update({"status": item.get("status", "in_progress"), "lastEventIndex": index})
            if first_tool is None:
                first_tool = elapsed
            if method in ("item/completed", "item.completed"):
                call["completedMs"] = elapsed
                pending.pop(key, None)
            else:
                pending[key] = call
            exit_code = item.get("exitCode", item.get("exit_code"))
            if item.get("status") == "failed" or (type(exit_code) is int and exit_code != 0):
                failures.append({"kind": "tool_execution_failure", "toolId": call_id, "exitCode": exit_code, **location})
        if is_output and call_id:
            key = active_turn + ":" + call_id
            if key in calls:
                calls[key].update(completedMs=elapsed, status="output_observed", lastEventIndex=index)
                pending.pop(key, None)
        error = message.get("error", params.get("error"))
        if method == "error" or error:
            failures.append({"kind": "runtime_error", "evidence": error or params, **location})
        text = "\n".join(str(item[key]) for key in ("text", "output", "aggregatedOutput", "message") if key in item)
        is_model_text = item_type in ("agentMessage", "message") or method == "item/agentMessage/delta"
        if is_model_text and first_output is None:
            first_output = elapsed
        if is_model_text or is_call or is_output or method.endswith("/delta"):
            last_progress = {**location, "method": method, "itemType": item_type}
        for kind, pattern in (
            ("tool_unavailable", r"cannot access (?:the )?tool|tool (?:is )?unavailable|unknown tool|tool not found"),
            ("output_cutoff", r"output.{0,30}truncat|output.{0,30}cut off|max_output_tokens|incomplete_details"),
            ("patch_mismatch", r"patch.{0,30}(?:failed|mismatch)|failed to find expected lines"),
        ):
            match = re.search(pattern, text, re.IGNORECASE)
            if match:
                symptoms.append({"kind": kind, "source": "model_claim" if is_model_text else "tool_output", "observedText": match.group(0), "causallyEstablished": False, **location})
    failure = evidence.get("failure")
    if failure:
        failures.append({"kind": failure.get("kind", "runner_failure") if isinstance(failure, dict) else "runner_failure", "evidence": failure, "source": "runner"})
    status = evidence.get("status")
    if status in ("timeout", "attempt_timeout", "segment_budget_exhausted", "canceled", "setup_failed") and not failure:
        failures.append({"kind": status, "source": "runner_status"})
    if status in ("completed", "timeout", "attempt_timeout") and not terminal:
        failures.append({"kind": "missing_terminal_event", "source": "native_event_coverage"})
    verifier = evidence.get("verifier")
    if isinstance(verifier, dict) and verifier.get("status") not in (None, "passed", "correct"):
        failures.append({"kind": "verification_" + str(verifier["status"]), "source": "verifier", "evidence": verifier})
    valid_profiles = [record for record in profiles.values() if timing_profile_valid(record["timing"]) and record["timing"].get("classificationComplete") is True]
    runtime = _population_report(valid_profiles, include_tokens=include_tokens) if valid_profiles else None
    requests = [dict(request, _turnId=record["turn_id"], _eventIndex=record["eventIndex"]) for record in profiles.values() for request in _selected_requests(record["timing"])]
    generations = []
    for request in requests:
        classification = classify_model_request(request, classification_complete=False, prior_successful_test=None, linked_commands=[])
        generations.append({
            "turnId": request["_turnId"], "generationIndex": request.get("generationIndex"),
            "eventIndex": request["_eventIndex"], "phase": request.get("phase"),
            "effort": request.get("reasoningEffort"), "purpose": request.get("generationPurpose"),
            "reason": request.get("generationReason"), "attemptKind": request.get("attemptKind"),
            "classification": classification, "modelStreamWaitNs": request.get("modelStreamWaitNs"),
            "dispatchMs": request.get("dispatchMs"), "completedMs": request.get("completedMs"),
            "physicalAttemptIds": request.get("physicalAttemptIds", []),
            "toolCallIds": [call.get("callId") for call in profiles[request["_turnId"]]["timing"].get("toolCalls", []) if isinstance(call, dict) and call.get("generationIndex") == request.get("generationIndex")],
            "tokens": _token_report([request]) if include_tokens else disabled_tokens(),
        })
    retry_evidence = [row for row in generations if set(row["classification"]["tags"]) & {"retry", "recovery"}]
    for record in profiles.values():
        for call in record["timing"].get("toolCalls", []):
            if not isinstance(call, dict):
                continue
            projection = {key: value for key, value in call.items() if any(word in key.lower() for word in ("truncat", "cutoff", "recover", "artifact"))}
            if projection:
                symptoms.append({"kind": "native_output_projection", "toolId": call.get("callId"), "eventIndex": record["eventIndex"], "evidence": projection, "causallyEstablished": False})
    totals = runtime["tokens"] if runtime and include_tokens else disabled_tokens() if not include_tokens else None
    native_tools = {(record["turn_id"], call.get("callId")): call for record in profiles.values() for call in record["timing"].get("toolCalls", []) if isinstance(call, dict) and call.get("callId")}
    nested = sum(bool(call.get("parentCallId")) for call in native_tools.values()) if native_tools else None
    if totals is None and usage_by_thread:
        totals = {key: sum(row[key] for row in usage_by_thread.values()) if all(row.get(key) is not None for row in usage_by_thread.values()) else None
                  for key in ("inputTokens", "cachedInputTokens", "nonCachedInputTokens", "outputTokens", "visibleOutputTokens", "reasoningTokens", "totalTokens")}
        totals.update(available=True, complete=None, source="native_thread_cumulative_usage", promptCategories=None, promptCategoryCoverage=None)
    if totals is None and include_tokens:
        totals = {"available": False, "complete": False, "promptCategories": None, "providerTotals": None}
    return {
        "schemaVersion": 1, "attemptId": evidence.get("attemptId"),
        "status": evidence.get("status", "completed" if terminal else "unfinished"),
        "elapsedMs": evidence.get("elapsedMs"), "units": {"eventTime": "milliseconds", "runtimeTime": "nanoseconds", "tokens": "tokens"},
        "coverage": {"events": len(events), "nativeTimingProfiles": len(profiles), "validCompleteTimingProfiles": len(valid_profiles), "terminalTurns": len(terminal), "tokenAnalysisEnabled": include_tokens},
        "logicalGenerations": runtime.get("logicalGenerations") if runtime else sampling_count or None,
        "physicalRequests": runtime["decisionLatency"]["physicalAttempts"] if runtime else None,
        "directToolCount": len(native_tools) - nested if native_tools else len(calls), "nestedToolCount": nested,
        "toolCountCoverage": "retained_native_timing" if native_tools else "observed_native_items",
        "tools": list(calls.values()), "pendingTools": list(pending.values()),
        "nativeToolCalls": [{"turnId": turn_id, **{key: value for key, value in call.items() if include_tokens or "token" not in key.lower()}} for (turn_id, _), call in native_tools.items()],
        "longestEventGaps": sorted(event_gaps, key=lambda row: row["durationMs"], reverse=True)[:8],
        "omittedEventGaps": max(0, len(event_gaps) - 8),
        "firstOutputMs": first_output, "firstToolMs": first_tool,
        "lastProgress": last_progress, "terminalTurns": terminal,
        "failures": failures, "symptoms": symptoms, "retryEvidence": retry_evidence,
        "generations": generations, "runtime": runtime,
        "tokens": totals, "nativeProviderUsage": list(usage_by_thread.values()) if include_tokens else None,
        "measurementNote": "Missing native telemetry is unavailable. Event order and text symptoms do not prove retry causality. Thread usage updates are cumulative snapshots, not additive generations.",
    }


_RECOVERY_PURPOSES = frozenset({"failure_diagnosis", "repair", "compaction_recovery"})
_VERIFICATION_PURPOSES = frozenset({"validation_interpretation"})
_RETRY_ATTEMPT_KINDS = frozenset({"retry", "fallback"})
CONTINUATION_CLASS_PRECEDENCE = ("retry", "recovery", "verification", "non_progress")

def classify_model_request(
    request: dict[str, Any],
    *,
    classification_complete: bool,
    prior_successful_test: dict[str, Any] | None,
    linked_commands: list[dict[str, Any]],
    intervening_mutation: bool | None = None,
) -> dict[str, Any]:
    """Classify a request from observed runtime facts without claiming causality."""

    if request.get("isContinuation") is not True and request.get("attemptKind") not in _RETRY_ATTEMPT_KINDS:
        return {
            "primary": "initial",
            "tags": [],
            "confidence": "observed",
            "basis": ["isContinuation=false"],
            "interpretation": None,
            "necessityCausallyEstablished": False,
        }

    purpose = request.get("generationPurpose")
    reason = request.get("generationReason")
    attempt_kind = request.get("attemptKind")
    progress_values = request.get("progressKinds")
    progress = (
        {value for value in progress_values if isinstance(value, str)}
        if isinstance(progress_values, list)
        else set()
    )
    tags: list[str] = []
    basis: list[str] = []
    if attempt_kind in _RETRY_ATTEMPT_KINDS:
        tags.append("retry")
        basis.append(f"attemptKind={attempt_kind}")
    if (
        purpose in _RECOVERY_PURPOSES
        or reason == "compaction"
        or "failure_observation" in progress
    ):
        tags.append("recovery")
        basis.append("recovery purpose, reason, or failure observation")
    if purpose in _VERIFICATION_PURPOSES or "validation_result" in progress:
        tags.append("verification")
        basis.append("validation purpose or result")
    non_progress = (
        request.get("unchangedRelevantState") is True
        and request.get("nextStructuredActionChanged") is False
    )
    if non_progress:
        tags.append("non_progress")
        basis.append(
            "unchangedRelevantState=true and nextStructuredActionChanged=false"
        )
    repeated_test = prior_successful_test is not None and any(
        command.get("requiredTest") is True for command in linked_commands
    )
    if repeated_test:
        tags.append("post_success_verification")
        basis.append(
            "a linked required-test command followed an earlier passing required test"
        )
        # Rerunning a suite after an edit is ordinary work; rerunning it when
        # nothing was written since it last passed is what makes it redundant.
        if intervening_mutation is False:
            tags.append("no_intervening_mutation")
            basis.append(
                "no command that can write to the workspace ran between that pass "
                "and this request"
            )

    primary = next(
        (category for category in CONTINUATION_CLASS_PRECEDENCE if category in tags),
        "necessary",
    )
    if primary == "necessary":
        tags.append("necessary")
        if progress:
            basis.append("runtime recorded progress")
        elif request.get("nextStructuredActionChanged") is True:
            basis.append("runtime recorded a changed next structured action")
        else:
            basis.append(
                "no retry, recovery, verification, or non-progress predicate matched"
            )
    interpretation = (
        "redundant_verification"
        if repeated_test and intervening_mutation is False and non_progress
        else "post_success_verification_after_mutation"
        if repeated_test and intervening_mutation is True
        else None
    )
    return {
        "primary": primary,
        "tags": tags,
        "confidence": (
            "observed"
            if primary != "necessary" or progress or classification_complete
            else "heuristic"
        ),
        "basis": basis,
        "interpretation": interpretation,
        # Even the `necessary` bucket is an observational residual; this field
        # prevents downstream consumers from turning it into a causal claim.
        "necessityCausallyEstablished": False,
    }
