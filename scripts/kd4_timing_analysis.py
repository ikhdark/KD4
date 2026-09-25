"""Canonical runtime timing analysis shared by KD4 measurement commands.

This library consumes runtime timing records. Capture, rollout discovery,
harness observations, experiment scoring, and report rendering stay in callers.
Nanosecond unions and exclusive ownership are distinct from summed diagnostics.
"""

from __future__ import annotations

import collections
import hashlib
import json
import math
import re
import shlex
from collections.abc import Callable, Iterable
from pathlib import Path
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


def timing_profile_error(timing: Any, *, include_tokens: bool = True) -> str | None:
    if not timing_profile_valid(timing):
        return "profile_invalid"
    if (
        type(timing.get("inclusiveDurationNs")) is not int
        or timing["inclusiveDurationNs"] < 0
    ):
        return "invalid_inclusive_duration"
    try:
        analyze_timing(timing, include_tokens=include_tokens)
    except (TypeError, ValueError, KeyError, OverflowError, AttributeError):
        return "malformed_timing_fields"
    return None


class TerminalProfiles:
    """Frozen terminal evidence; conflicting versions never select a winner."""

    def __init__(self):
        self.records = {}
        self.conflicts = set()
        self.duplicates = 0

    def add(self, key, record):
        if key in self.records:
            old = self.records[key]
            if (old["timing"], old.get("status")) == (
                record["timing"],
                record.get("status"),
            ):
                self.duplicates += 1
            else:
                self.conflicts.add(key)
            return
        self.records[key] = record


def analyze_startup_timing(records: Iterable[dict[str, Any]]) -> dict[str, Any]:
    """Analyze frozen startup trace fields, independently of terminal turn timing."""
    duration_fields = {
        "inclusiveDurationNs": "startup_timing_duration_ns",
        "sessionInitializationNs": "startup_session_initialization_ns",
        "transportPreconnectNs": "startup_transport_preconnect_ns",
        "prewarmPreparationNs": "startup_prewarm_preparation_ns",
        "prewarmRequestNs": "startup_prewarm_request_ns",
        "firstTurnPrewarmWaitNs": "startup_first_turn_wait_ns",
        "executorReadinessNs": "startup_executor_readiness_ns",
        "preconnectPreparationOverlapNs": "startup_preconnect_preparation_overlap_ns",
        "preconnectExecutorOverlapNs": "startup_preconnect_executor_overlap_ns",
    }
    counter_fields = {
        "invalidTransitionCount": "startup_timing_invalid_transition_count",
        "clockRegressionCount": "startup_timing_clock_regression_count",
        "saturationCount": "startup_timing_saturation_count",
    }

    def unsigned(value: Any, bits: int) -> int | None:
        # tracing may serialize u128 via Debug as a decimal JSON string.
        if isinstance(value, str) and re.fullmatch(r"[0-9]{1,39}", value):
            value = int(value)
        return value if type(value) is int and 0 <= value < 2**bits - 1 else None

    profiles = []
    identities = {}
    duplicates = 0
    conflicts = set()
    for fields in records:
        fields = {
            key: value for key, value in fields.items() if key.startswith("startup_")
        }
        identity = fields.get("startup_timing_correlation_id")
        started = fields.get("startup_timing_started_at_unix_ms")
        key = (
            (identity, started)
            if isinstance(identity, str) and identity and type(started) is int
            else None
        )
        if key is not None and key in identities:
            index = identities[key]
            if json.dumps(profiles[index], sort_keys=True) == json.dumps(
                fields, sort_keys=True
            ):
                duplicates += 1
            else:
                conflicts.add(index)
            continue
        if key is not None:
            identities[key] = len(profiles)
        profiles.append(fields)

    rows = []
    schemas = collections.Counter()
    statuses = collections.Counter()
    for index, fields in enumerate(profiles):
        schema = fields.get("startup_timing_schema_version")
        schemas[str(schema) if schema is not None else "missing"] += 1
        reasons = []
        if type(schema) is not int or schema != 1:
            reasons.append("unsupported_schema")
        if fields.get("startup_timing_profile_valid") is not True:
            reasons.append("profile_invalid")
        if index in conflicts:
            reasons.append("conflicting_snapshots")
        identity = fields.get("startup_timing_correlation_id")
        started = fields.get("startup_timing_started_at_unix_ms")
        completed = fields.get("startup_timing_completed_at_unix_ms")
        if not (
            isinstance(identity, str)
            and identity
            and type(started) is int
            and type(completed) is int
            and -(2**63) <= started <= completed < 2**63
        ):
            reasons.append("invalid_identity_or_timestamps")
        durations = {
            name: unsigned(fields.get(field), 128)
            for name, field in duration_fields.items()
        }
        counters = {
            name: unsigned(fields.get(field), 32)
            for name, field in counter_fields.items()
        }
        if any(value is None for value in (*durations.values(), *counters.values())):
            reasons.append("missing_invalid_or_saturated_fields")
        if any(value is not None and value > 0 for value in counters.values()):
            reasons.append("runtime_diagnostics_nonzero")
        if all(value is not None for value in durations.values()):
            if (
                any(
                    value > durations["inclusiveDurationNs"]
                    for value in durations.values()
                )
                or durations["preconnectPreparationOverlapNs"]
                > min(
                    durations["transportPreconnectNs"],
                    durations["prewarmPreparationNs"],
                )
                or durations["preconnectExecutorOverlapNs"]
                > min(
                    durations["transportPreconnectNs"], durations["executorReadinessNs"]
                )
            ):
                reasons.append("inconsistent_phase_durations")
        status = fields.get("startup_prewarm_status")
        if isinstance(status, str):
            statuses[status] += 1
        rows.append(
            {
                "correlationId": identity,
                "startedAtUnixMs": started,
                "schemaVersion": schema,
                "prewarmStatus": status,
                "valid": not reasons,
                "exclusionReasons": reasons,
                "durationsNs": durations,
                "diagnostics": counters,
            }
        )
    valid = [row for row in rows if row["valid"]]
    return {
        "schemaVersion": 1,
        "available": bool(valid),
        "profiles": len(rows),
        "validProfiles": len(valid),
        "excludedProfiles": len(rows) - len(valid),
        "duplicateSnapshots": duplicates,
        "conflictingProfiles": len(conflicts),
        "schemaVersions": dict(sorted(schemas.items())),
        "prewarmStatuses": dict(sorted(statuses.items())),
        "durationSummariesNs": {
            name: {
                "count": len(valid),
                "total": sum(row["durationsNs"][name] for row in valid),
                "min": min(row["durationsNs"][name] for row in valid),
                "max": max(row["durationsNs"][name] for row in valid),
            }
            if valid
            else None
            for name in duration_fields
        },
        "records": rows,
        "measurementNote": "Session construction through first real model send; may include user idle time. Phase unions overlap and must not be added or subtracted from turn timing. Captured profiles do not establish cold/warm startup or prewarm effectiveness.",
    }


def _request_retention(timings: Iterable[dict[str, Any]]) -> dict[str, Any]:
    """Compare retained request rows with the uncapped runtime request counter."""
    rows = []
    for timing in timings:
        retained = len(_selected_requests(timing))
        expected = timing.get("counters", {}).get("modelRequestCount")
        known = type(expected) is int and 0 <= expected < 2**32 - 1
        complete = (
            retained == expected if known else False if expected is not None else None
        )
        if (
            not isinstance(timing.get("modelRequests"), list)
            or len(timing["modelRequests"]) != retained
        ):
            complete = False
        rows.append(
            {
                "retained": retained,
                "expected": expected if known else None,
                "complete": complete,
            }
        )
    complete = (
        False
        if any(row["complete"] is False for row in rows)
        else True
        if rows and all(row["complete"] is True for row in rows)
        else None
    )
    return {
        "complete": complete,
        "retainedRequests": sum(row["retained"] for row in rows),
        "recordedRequests": sum(row["expected"] for row in rows)
        if rows and all(row["expected"] is not None for row in rows)
        else None,
        "unknownProfiles": sum(row["complete"] is None for row in rows),
        "incompleteProfiles": sum(row["complete"] is False for row in rows),
        "basis": "retained modelRequests versus runtime counters.modelRequestCount; missing counters leave retention unknown",
    }


def _tool_dispatch_counts(
    calls: Iterable[dict[str, Any]], overflow: int, *, available: bool
) -> dict[str, Any]:
    calls = list(calls)
    result = {
        "schemaVersion": 1,
        "available": available,
        "retainedCalls": len(calls),
        "overflowCalls": overflow,
        "scope": "retained native tool dispatch counters; missing or overflowing evidence excludes totals",
    }
    covered = [
        call
        for call in calls
        if all(
            type(call.get(key)) is int and 0 <= call[key] < 2**32 - 1
            for key in ("retryCount", "reentryCount")
        )
    ]
    complete = available and len(covered) == len(calls) and overflow == 0
    result.update(complete=complete, coveredCalls=len(covered))
    for key in ("retryCount", "reentryCount"):
        observed = sum(call[key] for call in covered)
        result[key] = observed if complete else None
        result["observed" + key[0].upper() + key[1:]] = observed
    waits = collections.Counter()
    suspects = []
    for call in calls:
        timer_waits = call.get("timerWaits")
        previous_sequence = 0
        for wait in timer_waits if isinstance(timer_waits, list) else []:
            if isinstance(wait, dict):
                # A record folds repeated wakes toward one deadline; its sequence
                # is the ordinal of the latest wake it covers.
                sequence = wait.get("sequence")
                wakes = 1
                if type(sequence) is int and sequence > previous_sequence:
                    wakes = sequence - previous_sequence
                    previous_sequence = sequence
                waits[(str(wait.get("waitKind", "unknown")), str(wait.get("wakeReason", "unknown")))] += wakes
        reentries = call.get("reentryCount")
        duration = call.get("totalDurationMs")
        if type(reentries) is int and reentries >= 1000:
            suspects.append({
                "callId": call.get("callId"),
                "turnId": call.get("_turnId"),
                "toolName": call.get("toolName"),
                "reentryCount": reentries,
                "totalDurationMs": duration,
                "reentriesPerSecond": reentries * 1000 / duration
                if type(duration) is int and duration > 0 else None,
            })
    calls_with_waits = sum(isinstance(call.get("timerWaits"), list) for call in calls)
    if not calls_with_waits and not suspects:
        return result
    result["waitDiagnostics"] = {
        "available": True,
        "observedWaits": [
            {"waitKind": kind, "wakeReason": reason, "count": count}
            for (kind, reason), count in sorted(waits.items())
        ],
        "highReentryCallCount": len(suspects),
        "highReentryCalls": sorted(suspects, key=lambda call: call["reentryCount"], reverse=True)[:20],
        "callsWithWaitEvidence": calls_with_waits,
        "note": "Observed retained waits only. High re-entry is a diagnostic signal, not proof of CPU spinning or model retries. Durations across nested calls overlap; do not sum them.",
    }
    return result


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
    if (
        not isinstance(timing.get("modelRequests"), list)
        or _request_retention([timing])["complete"] is False
    ):
        return None
    return sum(row.get("isContinuation") is True for row in _selected_requests(timing))


def analyze_timing(
    timing: dict[str, Any],
    *,
    status: str = "task_complete",
    include_tokens: bool = True,
) -> dict[str, Any]:
    """Analyze one terminal profile before a caller applies trace retention limits.

    Callers decide which profiles enter comparisons. Invalid profiles remain
    identifiable; absent milestone evidence never becomes a zero latency.
    """
    report = _population_report(
        [{"timing": timing, "turn_id": None, "status": status}],
        include_tokens=include_tokens,
    )
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
    request_list: list[dict[str, Any]] = []
    identified: dict[tuple[str, str], int] = {}
    duplicate_records = 0
    conflicting_usage: set[str] = set()
    for request in requests:
        request_id = request.get("samplingRequestId")
        scope = json.dumps(
            request.get("_turnKey", request.get("_turnId")), sort_keys=True
        )
        if not isinstance(request_id, str) or not request_id:
            # Missing identity is missing evidence, not permission to merge
            # independent requests that happen to have identical token counts.
            request_list.append(request)
            continue
        identity = (scope, request_id)
        if identity not in identified:
            identified[identity] = len(request_list)
            request_list.append(request)
            continue
        duplicate_records += 1
        index = identified[identity]
        previous = request_list[index]
        if (
            previous.get("tokenUsage") is not None
            and request.get("tokenUsage") is not None
            and previous["tokenUsage"] != request["tokenUsage"]
        ):
            conflicting_usage.add(request_id)
        # A later snapshot may supply usage that an earlier one lacked.
        if request.get("tokenUsage") is not None:
            request_list[index] = request
    physical_attempts = sum(
        _physical_attempt_count(request) for request in request_list
    )
    totals = collections.Counter()
    prompt_categories = collections.Counter()
    covered_attempts = 0
    invalid_usage_attempts = 0
    categorized_attempts = 0
    for request in request_list:
        usage = request.get("tokenUsage")
        if isinstance(usage, dict) and all(
            type(usage.get(key)) is int and usage[key] >= 0
            for key in (
                "inputTokens",
                "cachedInputTokens",
                "visibleOutputTokens",
                "reasoningTokens",
            )
        ):
            input_tokens = max(0, int(usage.get("inputTokens", 0)))
            cached_input_tokens = max(0, int(usage.get("cachedInputTokens", 0)))
            visible_output_tokens = max(0, int(usage.get("visibleOutputTokens", 0)))
            reasoning_tokens = max(0, int(usage.get("reasoningTokens", 0)))
            expected_total = input_tokens + visible_output_tokens + reasoning_tokens
            total_tokens = usage.get("totalTokens", expected_total)
            coherent = cached_input_tokens <= input_tokens and all(
                key not in usage or (type(usage[key]) is int and usage[key] == expected)
                for key, expected in (
                    ("totalTokens", expected_total),
                    ("outputTokens", visible_output_tokens + reasoning_tokens),
                    ("nonCachedInputTokens", input_tokens - cached_input_tokens),
                )
            )
            covered_attempts += int(coherent)
            invalid_usage_attempts += int(not coherent)
            if type(total_tokens) is not int or total_tokens < 0:
                total_tokens = expected_total
        else:
            invalid_usage_attempts += int(isinstance(usage, dict))
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
    usage_complete = (
        physical_attempts > 0
        and covered_attempts == physical_attempts
        and not conflicting_usage
    )
    return {
        "accountingScope": "provider usage per identified sampling request within its turn",
        "deduplicatedRequestRecords": duplicate_records,
        "conflictingUsageRequestIds": sorted(conflicting_usage),
        "physicalAttempts": physical_attempts,
        "providerUsageAttempts": covered_attempts,
        "invalidUsageAttempts": invalid_usage_attempts,
        "coverage": covered_attempts / physical_attempts if physical_attempts else None,
        "complete": usage_complete,
        "providerTotals": dict(totals) if usage_complete else None,
        **{key: value if usage_complete else None for key, value in totals.items()},
        "observedTotals": dict(totals),
        "billableTokens": observed_billable_tokens if usage_complete else None,
        "observedBillableTokens": observed_billable_tokens,
        "billableDefinition": "provider_input_including_cached_plus_output",
        "blendedTokens": observed_blended_tokens if usage_complete else None,
        "observedBlendedTokens": observed_blended_tokens,
        "blendedDefinition": "non_cached_input_plus_output",
        "promptCategoryAttempts": categorized_attempts,
        "promptCategories": dict(prompt_categories) if categorized_attempts else None,
        "promptCategoryBasis": "native_estimate",
        "promptCategoryScope": "sum of local logical-prompt estimates across requests; not provider usage or subscription cost",
        "promptCategoryCoverage": categorized_attempts / len(request_list)
        if request_list
        else None,
        "promptCategoryEvidence": {
            "accountingBases": sorted(
                {
                    str(
                        request["requestTokenCategories"].get(
                            "accountingBasis", "unknown"
                        )
                    )
                    for request in request_list
                    if isinstance(request.get("requestTokenCategories"), dict)
                }
            ),
            **{
                key: sum(
                    request["requestTokenCategories"][key]
                    for request in request_list
                    if isinstance(request.get("requestTokenCategories"), dict)
                    and type(request["requestTokenCategories"].get(key)) is int
                )
                if any(
                    isinstance(request.get("requestTokenCategories"), dict)
                    and type(request["requestTokenCategories"].get(key)) is int
                    for request in request_list
                )
                else None
                for key in (
                    "localReconciliationResidual",
                    "providerReconciliationResidual",
                    "providerInputTokens",
                )
            },
        },
        "rankedPromptConsumers": [
            {
                "category": key,
                "tokens": value,
                "share": value / prompt_categories["logicalTotal"]
                if prompt_categories["logicalTotal"]
                else None,
                "denominator": "covered_logical_prompt_estimate",
            }
            for key, value in sorted(
                prompt_categories.items(), key=lambda pair: pair[1], reverse=True
            )
            if key
            not in ("logicalTotal", "localInputEstimate", "repeatedUnchangedContext")
        ],
        "accountingNote": "Top-level totals require complete provider coverage. observedTotals retains partial provider counts and output-only fallbacks; absent input is unmeasured. Cached input and reasoning output are subsets.",
        "available": bool(
            covered_attempts
            or any("outputTokens" in request for request in request_list)
        ),
        "cacheShare": totals["cachedInputTokens"] / input_tokens
        if usage_complete and input_tokens
        else None,
        "observedCacheShare": totals["cachedInputTokens"] / input_tokens
        if covered_attempts and input_tokens
        else None,
    }


def _invalidate_token_totals(tokens: dict[str, Any]) -> None:
    """Keep observed evidence while withdrawing totals for an incomplete population."""
    tokens["complete"] = False
    for key in (
        *tokens.get("observedTotals", {}),
        "providerTotals",
        "billableTokens",
        "blendedTokens",
        "cacheShare",
    ):
        tokens[key] = None


def _classification_summary(
    classifications: Iterable[dict[str, Any]],
) -> dict[str, Any]:
    rows = list(classifications)
    return {
        "requestRecords": len(rows),
        "primaryCounts": dict(
            sorted(collections.Counter(row["primary"] for row in rows).items())
        ),
        "tagCounts": dict(
            sorted(
                collections.Counter(tag for row in rows for tag in row["tags"]).items()
            )
        ),
        "confidenceCounts": dict(
            sorted(collections.Counter(row["confidence"] for row in rows).items())
        ),
        "measurementNote": "Counts describe retained request records before display truncation, not physical attempts or causal necessity. Tags can overlap.",
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


def _population_report(
    records: list[dict[str, Any]], *, include_tokens: bool = True
) -> dict[str, Any]:
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
    residual_measurements: list[int] = []
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
        all_requests.extend(
            {**request, "_turnId": record["turn_id"]} for request in requests
        )
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
        residual = counters.get("residualDeterministicGenerationCount")
        if residual is not None:
            residual_measurements.append(int(residual))
        totals["waitGenerationsWithSameRevisionCount"] += int(
            counters.get(
                "waitGenerationsWithSameRevisionCount",
                counters.get("exactRepeatedWaitCount", 0),
            )
        )
        for key in (
            "ownerDrainedContinuationCount",
            "executedValidationCount",
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
                        request.get("unchangedRelevantState") is True
                        and request.get("nextStructuredActionChanged") is False
                    ),
                ),
            )
        _sum_metric(
            deterministic,
            _request_metric(
                requests,
                lambda request: (
                    request.get("generationPurpose")
                    in ("tool_result_interpretation", "deterministic_tool_continuation")
                ),
            ),
        )

    inclusive = totals["inclusiveDurationNs"]
    machine = totals["machineDurationNs"]
    model = totals["modelOnlyNs"]
    tool = totals["toolOnlyNs"]
    retention = _request_retention(record["timing"] for record in records)
    tokens = _token_report(all_requests) if include_tokens else disabled_tokens()
    if include_tokens and retention["complete"] is False:
        _invalidate_token_totals(tokens)
    return {
        "turns": len(records),
        "statusCounts": dict(sorted(status_counts.items())),
        **dict(totals),
        "residualDeterministicGenerationCount": (
            sum(residual_measurements)
            if len(residual_measurements) == len(records) and records
            else None
        ),
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
            "physicalAttempts": request_count
            if retention["complete"] is not False
            else None,
            "retainedPhysicalAttempts": request_count,
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
        "requestRetention": retention,
        "tokens": tokens,
        "observationalNonprogressTokens": _diagnostic_token_report(
            nonprogress_token_aggregates
        )
        if include_tokens
        else {},
        "toolRelay": _tool_relay_report(all_tool_calls, tool_call_timing_overflow),
        "observationalNonprogressLatency": nonprogress,
        "toolResultInterpretationLatency": deterministic,
    }


def disabled_tokens() -> dict[str, Any]:
    """A sentinel with no counting or inspection of token-bearing evidence."""
    return {
        "enabled": False,
        "available": False,
        "complete": False,
        **dict.fromkeys(
            (
                "inputTokens",
                "cachedInputTokens",
                "nonCachedInputTokens",
                "outputTokens",
                "visibleOutputTokens",
                "reasoningTokens",
                "totalTokens",
                "billableTokens",
                "observedBillableTokens",
                "cacheShare",
                "physicalAttempts",
                "providerUsageAttempts",
            )
        ),
    }


def _native_usage(usage: Any) -> dict[str, Any] | None:
    """Normalize native usage without adding cached/reasoning subsets twice."""
    if not isinstance(usage, dict):
        return None
    fields = {
        "inputTokens": ("inputTokens", "input_tokens"),
        "cachedInputTokens": ("cachedInputTokens", "cached_input_tokens"),
        "outputTokens": ("outputTokens", "output_tokens"),
        "reasoningTokens": (
            "reasoningOutputTokens",
            "reasoning_output_tokens",
            "reasoningTokens",
        ),
    }
    counts = {
        key: next(
            (
                usage[name]
                for name in names
                if type(usage.get(name)) is int and usage[name] >= 0
            ),
            None,
        )
        for key, names in fields.items()
    }
    if counts["inputTokens"] is None or counts["outputTokens"] is None:
        return None
    counts["totalTokens"] = counts["inputTokens"] + counts["outputTokens"]
    counts["nonCachedInputTokens"] = (
        max(0, counts["inputTokens"] - counts["cachedInputTokens"])
        if counts["cachedInputTokens"] is not None
        else None
    )
    counts["visibleOutputTokens"] = (
        max(0, counts["outputTokens"] - counts["reasoningTokens"])
        if counts["reasoningTokens"] is not None
        else None
    )
    return counts


def _is_single_rg_command(command: Any) -> bool:
    # Compound shell commands cannot attribute their exit status or duration to rg.
    if not isinstance(command, str):
        return False
    if any(char in command for char in "|&;<>`$()\n\r"):
        return False
    try:
        words = shlex.split(command, posix=False)
    except ValueError:
        return False
    return bool(
        words
        and words[0].strip("\"'").replace("\\", "/").rsplit("/", 1)[-1].lower()
        in ("rg", "rg.exe")
    )


def _is_rg_no_match(command: Any, exit_code: Any, item: dict[str, Any]) -> bool:
    return (
        type(exit_code) is int
        and exit_code == 1
        and _is_single_rg_command(command)
        and not item.get("error")
        and not any(item.get(key) for key in ("output", "aggregatedOutput", "stderr"))
    )


def _observed_duration_summary(rows: list[dict[str, Any]]) -> dict[str, Any]:
    durations = [row["durationMs"] for row in rows if row.get("durationMs") is not None]
    observed_total = sum(durations) if durations else None
    return {
        "observedCount": len(rows),
        "measuredCount": len(durations),
        "missingOrInvalidCount": len(rows) - len(durations),
        "observedTotalMs": observed_total,
        "totalMs": observed_total if len(durations) == len(rows) else None,
        "minMs": min(durations) if durations else None,
        "maxMs": max(durations) if durations else None,
    }


def _captured_request_metrics(evidence: dict[str, Any]) -> dict[str, Any]:
    result = {
        "available": False,
        "requestCount": None,
        "serializedRequestBytes": None,
        "error": None,
        "measurementNote": "Compact, sorted-key UTF-8 JSON request bodies, including tools and input; a request-volume proxy, not tokens, wire bytes, or provider cache hits.",
    }
    path = evidence.get("providerRequestsPath")
    if not path:
        return result
    count = size = 0
    try:
        with Path(path).open(encoding="utf-8") as stream:
            for line in stream:
                if not line.strip():
                    continue
                row = json.loads(line)
                if not isinstance(row, dict) or not isinstance(
                    row.get("request"), dict
                ):
                    raise ValueError("captured provider record has no request object")
                body = json.dumps(
                    row["request"],
                    sort_keys=True,
                    separators=(",", ":"),
                    ensure_ascii=False,
                    allow_nan=False,
                )
                size += len(body.encode("utf-8"))
                count += 1
    except (OSError, ValueError, TypeError) as error:
        result["error"] = (
            f"captured request metrics unavailable: {type(error).__name__}"
        )
        return result
    result.update(available=True, requestCount=count, serializedRequestBytes=size)
    return result


_NATIVE_TOOL_ITEM_TYPES = (
    "commandExecution",
    "mcpToolCall",
    "dynamicToolCall",
    "fileChange",
    "webSearch",
    "toolCall",
    "collabAgentToolCall",
)


def _observed_tool_activity(
    calls: Iterable[dict[str, Any]], *, terminal_observed: bool
) -> dict[str, Any]:
    """Count native item lifecycles, not inferred model decisions or nested work."""

    def counts(rows: list[dict[str, Any]]) -> dict[str, Any]:
        return {
            "observedCount": len(rows),
            "startedCount": sum("startEventIndex" in row for row in rows),
            "completedCount": sum("completionEventIndex" in row for row in rows),
            "pendingCount": sum("completionEventIndex" not in row for row in rows),
            "byKind": {
                kind: sum(row["itemType"] == kind for row in rows)
                for kind in _NATIVE_TOOL_ITEM_TYPES
            },
        }

    rows = [row for row in calls if row["itemType"] in _NATIVE_TOOL_ITEM_TYPES]
    turns: dict[tuple[str | None, str], list[dict[str, Any]]] = collections.defaultdict(
        list
    )
    for row in rows:
        turns[row["threadId"], row["turnId"]].append(row)
    return {
        "schemaVersion": 1,
        "available": bool(rows) or terminal_observed,
        "scope": "observed_native_items",
        **counts(rows),
        "durations": {
            "byKind": {
                kind: _observed_duration_summary(items)
                for kind in _NATIVE_TOOL_ITEM_TYPES
                if (items := [row for row in rows if row["itemType"] == kind])
            },
            "rgSearch": _observed_duration_summary(
                [
                    row
                    for row in rows
                    if row["itemType"] == "commandExecution"
                    and _is_single_rg_command(row.get("command"))
                ]
            ),
        },
        "turns": [
            {"threadId": thread_id, "turnId": turn_id, **counts(items)}
            for (thread_id, turn_id), items in turns.items()
        ],
        "measurementNote": (
            "Unique (thread, turn, item) lifecycles in captured native notifications. "
            "Completed means a terminal item notification, including failed or interrupted work. "
            "Counts include observed child threads but do not establish complete child coverage, "
            "model decisions, nested dispatch counts, or edit correctness. Durations require both "
            "observed start and completion and measure notification latency, not exclusive execution. "
            "Duration totals sum intervals, including overlaps and failed work; partial totals stay "
            "unavailable while observedTotalMs retains measured intervals. rgSearch recognizes only "
            "single rg commands, including no-match outcomes; compound commands remain unattributed."
        ),
    }


def analyze_runner_evidence(
    evidence: dict[str, Any], *, include_tokens: bool = True
) -> dict[str, Any]:
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
    profiles: dict[tuple[str | None, str], dict[str, Any]] = {}
    terminal_profiles = TerminalProfiles()
    pending: dict[tuple[str | None, str, str], dict[str, Any]] = {}
    calls: dict[tuple[str | None, str, str], dict[str, Any]] = {}
    failures: list[dict[str, Any]] = []
    symptoms: list[dict[str, Any]] = []
    terminal: dict[tuple[str | None, str], str] = {}
    usage_by_thread: dict[str, dict[str, Any]] = {}
    sampling_count = 0
    last_progress = None
    first_output = None
    first_tool = None
    active_turn = "unknown"
    active_turn_by_thread: dict[str | None, str] = {}
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
                event_gaps.append(
                    {
                        "fromEventIndex": previous_event["eventIndex"],
                        "toEventIndex": index,
                        "durationMs": elapsed - previous_event["elapsedMs"],
                        "pendingToolIds": [call["id"] for call in pending.values()],
                        "cause": "unclassified",
                    }
                )
            previous_event = location
        turn = params.get("turn", {})
        turn = turn if isinstance(turn, dict) else {}
        thread_id = params.get("threadId", params.get("thread_id", row.get("file")))
        thread_id = thread_id if isinstance(thread_id, str) else None
        active_turn = str(
            params.get(
                "turnId",
                params.get(
                    "turn_id",
                    turn.get("id", active_turn_by_thread.get(thread_id, "unknown")),
                ),
            )
        )
        active_turn_by_thread[thread_id] = active_turn
        turn_key = (thread_id, active_turn)
        payload_type = params.get("type", "")
        if method == "sampling_boundary":
            sampling_count += 1
        if method in ("turn/completed", "turn.completed") or payload_type in (
            "task_complete",
            "turn_aborted",
        ):
            default_status = (
                "aborted"
                if payload_type == "turn_aborted"
                else "failed"
                if turn.get("error") is not None
                else "completed"
            )
            status = str(turn.get("status") or default_status)
            terminal[turn_key] = status
            if status not in ("completed", "complete"):
                failures.append(
                    {
                        "kind": "turn_" + status,
                        **location,
                        "evidence": turn.get("error"),
                    }
                )
        timing = params.get("timing", turn.get("timing"))
        if isinstance(timing, dict):
            record = {
                "timing": timing,
                "turn_id": active_turn,
                "thread_id": thread_id,
                "status": terminal.get(turn_key, "unfinished"),
                "eventIndex": index,
            }
            if turn_key in terminal:
                terminal_profiles.add(turn_key, record)
                profiles[turn_key] = terminal_profiles.records[turn_key]
            elif turn_key not in terminal_profiles.records:
                profiles[turn_key] = record
        if include_tokens and method == "thread/tokenUsage/updated":
            usage = params.get("tokenUsage", {})
            if isinstance(usage, dict):
                counts = _native_usage(usage.get("total"))
                if counts:
                    usage_by_thread[str(params.get("threadId", "unknown"))] = {
                        **counts,
                        **location,
                    }
        if include_tokens and isinstance(params.get("usage"), dict):
            counts = _native_usage(params["usage"])
            if counts:
                usage_by_thread[active_turn] = {**counts, **location}
        item = params.get("item", params)
        if not isinstance(item, dict):
            continue
        item_type = item.get("type", "")
        is_call = item_type in (
            *_NATIVE_TOOL_ITEM_TYPES,
            "function_call",
            "custom_tool_call",
        )
        is_output = item_type in ("function_call_output", "custom_tool_call_output")
        # Response item IDs identify separate call/output records; call_id links
        # those records. Native item notifications instead share their item ID.
        if is_output or item_type in ("function_call", "custom_tool_call"):
            call_id = str(item.get("call_id", item.get("id", "")))
        else:
            call_id = str(item.get("id", params.get("itemId", "")))
        if is_call and call_id:
            key = (thread_id, active_turn, call_id)
            call = calls.setdefault(
                key,
                {
                    "id": call_id,
                    "threadId": thread_id,
                    "turnId": active_turn,
                    "itemType": item_type,
                    "tool": item.get("name", item_type),
                    "eventIndex": index,
                },
            )
            if "completionEventIndex" not in call:
                call.update(
                    {
                        "status": item.get("status", "in_progress"),
                        "lastEventIndex": index,
                    }
                )
            for field in ("command", "exitCode"):
                if field in item:
                    call[field] = item[field]
            if first_tool is None:
                first_tool = elapsed
            if method in ("item/completed", "item.completed"):
                call.setdefault("completionEventIndex", index)
                call.setdefault("completedMs", elapsed)
                pending.pop(key, None)
            elif "completionEventIndex" not in call:
                if method in ("item/started", "item.started"):
                    call.setdefault("startEventIndex", index)
                    call.setdefault("startedMs", elapsed)
                pending[key] = call
            exit_code = item.get("exitCode", item.get("exit_code"))
            no_match = _is_rg_no_match(call.get("command"), exit_code, item)
            if no_match:
                call["outcome"] = "no_match"
            if not no_match and (
                item.get("status") == "failed"
                or (type(exit_code) is int and exit_code != 0)
            ):
                failures.append(
                    {
                        "kind": "tool_execution_failure",
                        "toolId": call_id,
                        "exitCode": exit_code,
                        **location,
                    }
                )
        if is_output and call_id:
            key = (thread_id, active_turn, call_id)
            if key in calls:
                calls[key].setdefault("completionEventIndex", index)
                calls[key].setdefault("completedMs", elapsed)
                calls[key].update(status="output_observed", lastEventIndex=index)
                pending.pop(key, None)
        error = message.get("error", params.get("error"))
        if method == "error" or error:
            failures.append(
                {"kind": "runtime_error", "evidence": error or params, **location}
            )
        text = "\n".join(
            str(item[key])
            for key in ("text", "output", "aggregatedOutput", "message")
            if key in item
        )
        is_model_text = (
            item_type in ("agentMessage", "message")
            or method == "item/agentMessage/delta"
        )
        if is_model_text and first_output is None:
            first_output = elapsed
        if is_model_text or is_call or is_output or method.endswith("/delta"):
            last_progress = {**location, "method": method, "itemType": item_type}
        for kind, pattern in (
            (
                "tool_unavailable",
                r"cannot access (?:the )?tool|tool (?:is )?unavailable|unknown tool|tool not found",
            ),
            (
                "output_cutoff",
                r"output.{0,30}truncat|output.{0,30}cut off|max_output_tokens|incomplete_details",
            ),
            (
                "patch_mismatch",
                r"patch.{0,30}(?:failed|mismatch)|failed to find expected lines",
            ),
        ):
            match = re.search(pattern, text, re.IGNORECASE)
            if match:
                symptoms.append(
                    {
                        "kind": kind,
                        "source": "model_claim" if is_model_text else "tool_output",
                        "observedText": match.group(0),
                        "causallyEstablished": False,
                        **location,
                    }
                )
    failure = evidence.get("failure")
    if failure:
        failures.append(
            {
                "kind": failure.get("kind", "runner_failure")
                if isinstance(failure, dict)
                else "runner_failure",
                "evidence": failure,
                "source": "runner",
            }
        )
    status = evidence.get("status")
    if (
        status
        in (
            "timeout",
            "attempt_timeout",
            "segment_budget_exhausted",
            "canceled",
            "setup_failed",
        )
        and not failure
    ):
        failures.append({"kind": status, "source": "runner_status"})
    if status in ("completed", "timeout", "attempt_timeout") and not terminal:
        failures.append(
            {"kind": "missing_terminal_event", "source": "native_event_coverage"}
        )
    verifier = evidence.get("verifier")
    if isinstance(verifier, dict) and verifier.get("status") not in (
        None,
        "passed",
        "correct",
    ):
        failures.append(
            {
                "kind": "verification_" + str(verifier["status"]),
                "source": "verifier",
                "evidence": verifier,
            }
        )
    native_profile_count = len(profiles)
    for key, record in list(profiles.items()):
        reason = (
            "conflicting_terminal_profiles"
            if key in terminal_profiles.conflicts
            else timing_profile_error(record["timing"], include_tokens=include_tokens)
        )
        record["exclusionReason"] = reason
        if reason:
            failures.append({"kind": reason, "turnId": key[1], "threadId": key[0]})
            if reason not in {"profile_invalid", "invalid_inclusive_duration"}:
                del profiles[key]
    # Preserve legacy labels for unique turn IDs; qualify all labels when threads
    # reuse an ID so compatibility fields cannot silently overwrite a thread.
    turn_keys = set(profiles) | set(terminal)
    qualify_turns = len({turn_id for _, turn_id in turn_keys}) != len(turn_keys)

    def turn_label(key: tuple[str | None, str]) -> str:
        return json.dumps(key, separators=(",", ":")) if qualify_turns else key[1]

    valid_profiles = [
        record
        for record in profiles.values()
        if record["exclusionReason"] is None
        and record["timing"].get("classificationComplete") is True
    ]
    runtime_profiles = [
        dict(record, turn_id=turn_label((record["thread_id"], record["turn_id"])))
        for record in valid_profiles
    ]
    runtime = (
        _population_report(runtime_profiles, include_tokens=include_tokens)
        if runtime_profiles
        else None
    )
    if runtime is not None:
        runtime["population"] = {
            "scope": "valid complete timing profiles only; summed across threads, not task elapsed time",
            "turnIds": [
                turn_label((record["thread_id"], record["turn_id"]))
                for record in valid_profiles
            ],
        }
    requests = [
        dict(
            request,
            _turnId=record["turn_id"],
            _threadId=record["thread_id"],
            _turnKey=key,
            _eventIndex=record["eventIndex"],
        )
        for key, record in profiles.items()
        for request in _selected_requests(record["timing"])
    ]
    generations = []
    for request in requests:
        classification = classify_model_request(
            request,
            classification_complete=False,
            prior_successful_test=None,
            linked_commands=[],
        )
        generations.append(
            {
                "turnId": request["_turnId"],
                "threadId": request["_threadId"],
                "generationIndex": request.get("generationIndex"),
                "eventIndex": request["_eventIndex"],
                "phase": request.get("phase"),
                "effort": request.get("reasoningEffort"),
                "purpose": request.get("generationPurpose"),
                "reason": request.get("generationReason"),
                "attemptKind": request.get("attemptKind"),
                "classification": classification,
                "modelStreamWaitNs": request.get("modelStreamWaitNs"),
                "dispatchMs": request.get("dispatchMs"),
                "completedMs": request.get("completedMs"),
                "physicalAttemptIds": request.get("physicalAttemptIds", []),
                "toolCallIds": [
                    call.get("callId")
                    for call in profiles[request["_turnKey"]]["timing"].get(
                        "toolCalls", []
                    )
                    if isinstance(call, dict)
                    and call.get("generationIndex") == request.get("generationIndex")
                ],
                "tokens": _token_report([request])
                if include_tokens
                else disabled_tokens(),
            }
        )
    retry_evidence = [
        row
        for row in generations
        if set(row["classification"]["tags"]) & {"retry", "recovery"}
    ]
    for record in profiles.values():
        for call in record["timing"].get("toolCalls", []):
            if not isinstance(call, dict):
                continue
            projection = {
                key: value
                for key, value in call.items()
                if any(
                    word in key.lower()
                    for word in ("truncat", "cutoff", "recover", "artifact")
                )
            }
            if projection:
                symptoms.append(
                    {
                        "kind": "native_output_projection",
                        "toolId": call.get("callId"),
                        "eventIndex": record["eventIndex"],
                        "evidence": projection,
                        "causallyEstablished": False,
                    }
                )
    # Timing classification validity does not establish or invalidate provider
    # usage. Account every captured request, including profiles rejected above.
    totals = (
        _token_report(requests)
        if include_tokens and requests
        else disabled_tokens()
        if not include_tokens
        else None
    )
    retention = _request_retention(record["timing"] for record in profiles.values())
    usage_turns = (
        {request["_turnKey"] for request in requests} if include_tokens else set()
    )
    missing_usage_turns = (
        sorted(turn_label(key) for key in set(terminal) - usage_turns)
        if include_tokens
        else []
    )
    usage_coverage = (
        {
            "requestRetention": retention,
            "requestProfileTurns": len(usage_turns),
            "terminalTurns": len(terminal),
            "missingTerminalTurnIds": missing_usage_turns,
            "unfinishedProfileTurnIds": sorted(
                turn_label(key) for key in set(profiles) - set(terminal)
            ),
            "timingValidityRequired": False,
        }
        if include_tokens
        else None
    )
    if include_tokens and totals is not None:
        totals["source"] = "captured_request_usage"
        totals["scope"] = (
            "all captured native request profiles, independent of timing validity"
        )
        totals["turnCoverage"] = usage_coverage
        totals["complete"] = bool(
            totals["complete"]
            and terminal
            and not missing_usage_turns
            and not usage_coverage["unfinishedProfileTurnIds"]
            and retention["complete"] is not False
        )
        if not totals["complete"]:
            _invalidate_token_totals(totals)
    native_tools = {
        (record["thread_id"], record["turn_id"], call.get("callId")): call
        for record in profiles.values()
        for call in record["timing"].get("toolCalls", [])
        if isinstance(call, dict) and call.get("callId")
    }
    nested = (
        sum(bool(call.get("parentCallId")) for call in native_tools.values())
        if native_tools
        else None
    )
    dispatch = _tool_dispatch_counts(
        native_tools.values(),
        sum(
            max(0, int(record["timing"].get("toolCallTimingOverflow", 0)))
            for record in profiles.values()
        ),
        available=bool(profiles)
        and set(profiles) == set(terminal)
        and all(
            isinstance(record["timing"].get("toolCalls"), list)
            for record in profiles.values()
        ),
    )
    cumulative = None
    reconciliation = None
    if include_tokens and usage_by_thread:
        cumulative = {
            key: sum(row[key] for row in usage_by_thread.values())
            if all(row.get(key) is not None for row in usage_by_thread.values())
            else None
            for key in (
                "inputTokens",
                "cachedInputTokens",
                "nonCachedInputTokens",
                "outputTokens",
                "visibleOutputTokens",
                "reasoningTokens",
                "totalTokens",
            )
        }
        cumulative.update(
            available=True,
            complete=None,
            source="native_thread_cumulative_usage",
            promptCategories=None,
            promptCategoryCoverage=None,
        )
        if totals is not None:
            reconciliation = {
                "basis": "latest native cumulative snapshots minus captured request usage; populations may differ (including resumed-session history)",
                "residuals": {
                    key: cumulative[key] - totals["observedTotals"][key]
                    if type(cumulative.get(key)) is int
                    and type(totals["observedTotals"].get(key)) is int
                    else None
                    for key in (
                        "inputTokens",
                        "cachedInputTokens",
                        "outputTokens",
                        "reasoningTokens",
                        "totalTokens",
                    )
                },
                "addedToRequestTotals": False,
            }
            if any(
                value not in (None, 0) for value in reconciliation["residuals"].values()
            ):
                _invalidate_token_totals(totals)
        else:
            totals = dict(
                cumulative,
                turnCoverage=usage_coverage,
                observedTotals={
                    key: cumulative[key]
                    for key in (
                        "inputTokens",
                        "cachedInputTokens",
                        "nonCachedInputTokens",
                        "outputTokens",
                        "visibleOutputTokens",
                        "reasoningTokens",
                        "totalTokens",
                    )
                },
            )
            _invalidate_token_totals(totals)
    if totals is None and include_tokens:
        totals = {
            "available": False,
            "complete": False,
            "promptCategories": None,
            "providerTotals": None,
        }
    for call in calls.values():
        start, end = call.get("startedMs"), call.get("completedMs")
        call["durationMs"] = (
            end - start
            if type(start) in (int, float)
            and type(end) in (int, float)
            and math.isfinite(start)
            and math.isfinite(end)
            and 0 <= start <= end
            else None
        )
    cache_hit_rate = None
    if (
        include_tokens
        and totals
        and totals.get("complete") is True
        and all(
            request["tokenUsage"]["cachedInputTokens"]
            <= request["tokenUsage"]["inputTokens"]
            for request in requests
        )
    ):
        input_tokens, cached_tokens = (
            totals.get("inputTokens"),
            totals.get("cachedInputTokens"),
        )
        if (
            type(input_tokens) is int
            and type(cached_tokens) is int
            and 0 <= cached_tokens <= input_tokens
            and input_tokens > 0
        ):
            cache_hit_rate = cached_tokens / input_tokens
    config_response = evidence.get("effectiveConfig")
    config = (
        config_response.get("config") if isinstance(config_response, dict) else None
    )
    configuration = {
        "sha256": hashlib.sha256(
            json.dumps(
                config, sort_keys=True, separators=(",", ":"), ensure_ascii=True
            ).encode()
        ).hexdigest()
        if isinstance(config, dict)
        else None,
        "scope": "captured config/read effective config; excludes layer provenance and later thread overrides",
    }
    return {
        "schemaVersion": 1,
        "attemptId": evidence.get("attemptId"),
        "configuration": configuration,
        "status": evidence.get(
            "status",
            next(
                (
                    status
                    for status in terminal.values()
                    if status not in ("completed", "complete")
                ),
                "completed" if terminal else "unfinished",
            ),
        ),
        "elapsedMs": evidence.get("elapsedMs"),
        "units": {
            "eventTime": "milliseconds",
            "runtimeTime": "nanoseconds",
            "tokens": "tokens",
        },
        "coverage": {
            "events": len(events),
            "nativeTimingProfiles": native_profile_count,
            "validCompleteTimingProfiles": len(valid_profiles),
            "terminalTurns": len(terminal),
            "tokenAnalysisEnabled": include_tokens,
        },
        "logicalGenerations": runtime.get("logicalGenerations")
        if runtime
        else sampling_count or None,
        "physicalRequests": runtime["decisionLatency"]["physicalAttempts"]
        if runtime and retention["complete"] is not False
        else None,
        "capturedRequests": _captured_request_metrics(evidence),
        "toolActivity": _observed_tool_activity(
            calls.values(), terminal_observed=bool(terminal)
        ),
        "toolDispatch": dispatch,
        "requestRetention": retention,
        "cacheHitRate": cache_hit_rate,
        "directToolCount": len(native_tools) - nested if native_tools else len(calls),
        "nestedToolCount": nested,
        "toolCountCoverage": "retained_native_timing"
        if native_tools
        else "observed_native_items",
        "tools": list(calls.values()),
        "pendingTools": list(pending.values()),
        "nativeToolCalls": [
            {
                "threadId": thread_id,
                "turnId": turn_id,
                **{
                    key: value
                    for key, value in call.items()
                    if include_tokens or "token" not in key.lower()
                },
            }
            for (thread_id, turn_id, _), call in native_tools.items()
        ],
        "longestEventGaps": sorted(
            event_gaps, key=lambda row: row["durationMs"], reverse=True
        )[:8],
        "omittedEventGaps": max(0, len(event_gaps) - 8),
        "firstOutputMs": first_output,
        "firstToolMs": first_tool,
        "lastProgress": last_progress,
        "terminalTurns": {turn_label(key): value for key, value in terminal.items()},
        "failures": failures,
        "symptoms": symptoms,
        "retryEvidence": retry_evidence,
        "generations": generations,
        "runtime": runtime,
        "requestClassification": _classification_summary(
            row["classification"] for row in generations
        ),
        "tokens": totals,
        "nativeProviderUsage": list(usage_by_thread.values())
        if include_tokens
        else None,
        "nativeCumulativeTokens": cumulative,
        "tokenReconciliation": reconciliation,
        "tokenCoverage": usage_coverage,
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

    if (
        request.get("isContinuation") is False
        and request.get("attemptKind") not in _RETRY_ATTEMPT_KINDS
    ):
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
    if primary == "necessary" and request.get("isContinuation") is not True:
        return {
            "primary": "unknown",
            "tags": tags,
            "confidence": "unknown",
            "basis": [*basis, "isContinuation is unavailable or not a boolean"],
            "interpretation": None,
            "necessityCausallyEstablished": False,
        }
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
