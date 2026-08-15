"""Descriptive analysis for privacy-safe ``codex.model_attempt`` JSONL events."""

from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any, Sequence


COMPONENT_FIELDS = (
    "base_instructions_bytes",
    "tool_schemas_bytes",
    "conversation_history_bytes",
    "current_input_bytes",
    "repository_context_bytes",
    "memory_bytes",
    "skills_bytes",
    "other_injected_context_bytes",
    "envelope_overhead_bytes",
)
PREDICTOR_FIELDS = (
    "cached_input_token_count",
    "uncached_input_token_count",
    *COMPONENT_FIELDS,
)
RECONCILIATION_TOLERANCE_BYTES = 256.0


def percentile(values: Sequence[float], fraction: float) -> float:
    if not values:
        raise ValueError("percentile requires at least one value")
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def _number(value: Any, *, nonnegative: bool = True) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    result = float(value)
    if not math.isfinite(result) or (nonnegative and result < 0):
        return None
    return result


def _event_fields(value: Any) -> tuple[str, dict[str, Any]] | None:
    if not isinstance(value, dict):
        return None
    fields = dict(value)
    for key in ("fields", "attributes", "body"):
        nested = value.get(key)
        if isinstance(nested, dict):
            fields.update(nested)
    event_name = fields.get("event.name", fields.get("event_name"))
    if event_name == "codex.model_attempt":
        return "attempt", fields
    if event_name == "codex.model_context_component":
        return "component", fields
    return None


def load_jsonl(paths: Sequence[Path]) -> tuple[list[dict[str, Any]], dict[str, int]]:
    records: list[dict[str, Any]] = []
    exclusions: dict[str, int] = {}
    seen_attempts: dict[tuple[str, str, int], dict[str, Any]] = {}
    components: list[dict[str, Any]] = []
    seen_components: set[tuple[Any, ...]] = set()
    for path in paths:
        with path.open(encoding="utf-8") as source:
            for line in source:
                if not line.strip():
                    continue
                try:
                    recognized = _event_fields(json.loads(line))
                except json.JSONDecodeError:
                    recognized = None
                    reason = "malformed_json"
                else:
                    reason = "not_model_attempt"
                if recognized is None:
                    exclusions[reason] = exclusions.get(reason, 0) + 1
                else:
                    event_kind, fields = recognized
                    if event_kind == "component":
                        identity = (
                            fields.get("sampling_request_id"),
                            fields.get("attempt_id"),
                            fields.get("retry_index"),
                            fields.get("component_kind"),
                            fields.get("semantic_id"),
                            fields.get("content_hash"),
                        )
                        if identity in seen_components:
                            exclusions["duplicate_context_component_collapsed"] = (
                                exclusions.get("duplicate_context_component_collapsed", 0) + 1
                            )
                        else:
                            seen_components.add(identity)
                            components.append(fields)
                        continue
                    identity_values = (
                        fields.get("sampling_request_id"),
                        fields.get("attempt_id"),
                        fields.get("retry_index"),
                    )
                    if (
                        isinstance(identity_values[0], str)
                        and identity_values[0]
                        and isinstance(identity_values[1], str)
                        and identity_values[1]
                        and isinstance(identity_values[2], int)
                        and not isinstance(identity_values[2], bool)
                    ):
                        identity = identity_values
                        previous = seen_attempts.get(identity)
                        if previous == fields:
                            reason = "duplicate_physical_attempt_collapsed"
                            exclusions[reason] = exclusions.get(reason, 0) + 1
                            continue
                        if previous is not None:
                            reason = "conflicting_physical_attempt_duplicate"
                            exclusions[reason] = exclusions.get(reason, 0) + 1
                        else:
                            seen_attempts[identity] = fields
                    records.append(fields)
    attempts_by_identity = {
        (record.get("sampling_request_id"), record.get("attempt_id"), record.get("retry_index")): record
        for record in records
    }
    for component in components:
        identity = (
            component.get("sampling_request_id"),
            component.get("attempt_id"),
            component.get("retry_index"),
        )
        attempt = attempts_by_identity.get(identity)
        if attempt is None:
            exclusions["orphan_context_component"] = exclusions.get("orphan_context_component", 0) + 1
            continue
        attempt.setdefault("_stable_context_components", []).append(component)
    return records, exclusions


def _stable_context_summary(records: Sequence[dict[str, Any]]) -> dict[str, Any]:
    grouped: dict[tuple[Any, Any, Any, Any], dict[str, Any]] = {}
    active_totals: list[float] = []
    local_constructed_bytes = 0.0
    local_reused_bytes = 0.0
    component_cache_hits = 0.0
    fail_open_attempts = 0
    successful_rebases = 0
    wire_bytes = 0.0
    cached_tokens = 0.0
    input_tokens = 0.0
    requests_over_80k = 0
    for record in records:
        active_total = 0.0
        attempt_constructed_bytes = 0.0
        attempt_reused_bytes = 0.0
        attempt_cache_hits = 0.0
        components = record.get("_stable_context_components")
        if not isinstance(components, list):
            components = []
        for component in components:
            if not isinstance(component, dict) or component.get("active") is not True:
                continue
            tokens = _number(component.get("approx_tokens")) or 0.0
            size = _number(component.get("serialized_bytes")) or 0.0
            active_total += tokens
            if component.get("local_reused") is True:
                attempt_reused_bytes += size
                attempt_cache_hits += 1
            else:
                attempt_constructed_bytes += size
            key = (
                component.get("component_kind"),
                component.get("contract_version"),
                component.get("semantic_id"),
                component.get("content_hash"),
            )
            summary = grouped.setdefault(
                key,
                {
                    "kind": key[0],
                    "contractVersion": key[1],
                    "semanticId": key[2],
                    "contentHash": key[3],
                    "requestAppearances": 0,
                    "serializedBytes": size,
                    "approxTokens": tokens,
                    "cumulativeLogicalExposureTokens": 0.0,
                    "locallyReusedAppearances": 0,
                },
            )
            summary["requestAppearances"] += 1
            summary["cumulativeLogicalExposureTokens"] += tokens
            if component.get("local_reused") is True:
                summary["locallyReusedAppearances"] += 1
        reported_active_total = _number(record.get("logical_context_tokens"))
        if reported_active_total is not None:
            active_total = reported_active_total
        active_totals.append(active_total)
        reported_constructed = _number(record.get("local_constructed_bytes"))
        local_constructed_bytes += (
            attempt_constructed_bytes
            if reported_constructed is None
            else reported_constructed
        )
        reported_reused = _number(record.get("local_reused_bytes"))
        local_reused_bytes += (
            attempt_reused_bytes if reported_reused is None else reported_reused
        )
        reported_hits = _number(record.get("component_cache_hits"))
        component_cache_hits += (
            attempt_cache_hits if reported_hits is None else reported_hits
        )
        if active_total > 80_000:
            requests_over_80k += 1
        if record.get("provider_baseline") == "fail_open_stale_retained":
            fail_open_attempts += 1
        if (
            record.get("provider_baseline") == "fresh_full_replay"
            and record.get("fresh_response_id_established") is True
        ):
            successful_rebases += 1
        wire_bytes += _number(record.get("wire_request_bytes")) or 0.0
        cached_tokens += _number(record.get("cached_input_token_count")) or 0.0
        input_tokens += _number(record.get("input_token_count")) or 0.0
    top = sorted(
        grouped.values(),
        key=lambda component: (
            -component["cumulativeLogicalExposureTokens"],
            str(component["kind"]),
            str(component["semanticId"]),
        ),
    )
    return {
        "componentVersions": top,
        "averageActiveContextTokens": round(sum(active_totals) / len(active_totals), 3)
        if active_totals
        else None,
        "peakActiveContextTokens": max(active_totals) if active_totals else None,
        "requestsOver80K": requests_over_80k,
        "cumulativeLogicalContextTokens": sum(active_totals),
        "localConstructedBytes": local_constructed_bytes,
        "localReusedBytes": local_reused_bytes,
        "componentCacheHits": component_cache_hits,
        "wireRequestBytes": wire_bytes,
        "providerCachedShare": round(cached_tokens / input_tokens, 6) if input_tokens else None,
        "failOpenAttempts": fail_open_attempts,
        "successfulRebases": successful_rebases,
    }


def _ranks(values: Sequence[float]) -> list[float]:
    ordered = sorted(enumerate(values), key=lambda pair: pair[1])
    result = [0.0] * len(values)
    index = 0
    while index < len(ordered):
        end = index + 1
        while end < len(ordered) and ordered[end][1] == ordered[index][1]:
            end += 1
        average_rank = (index + 1 + end) / 2.0
        for original_index, _ in ordered[index:end]:
            result[original_index] = average_rank
        index = end
    return result


def spearman(values_x: Sequence[float], values_y: Sequence[float]) -> float | None:
    if len(values_x) != len(values_y) or len(values_x) < 2:
        return None
    ranks_x, ranks_y = _ranks(values_x), _ranks(values_y)
    mean_x = sum(ranks_x) / len(ranks_x)
    mean_y = sum(ranks_y) / len(ranks_y)
    numerator = sum((x - mean_x) * (y - mean_y) for x, y in zip(ranks_x, ranks_y))
    variance_x = sum((x - mean_x) ** 2 for x in ranks_x)
    variance_y = sum((y - mean_y) ** 2 for y in ranks_y)
    if variance_x == 0 or variance_y == 0:
        return None
    return round(numerator / math.sqrt(variance_x * variance_y), 6)


def _distribution(values: Sequence[float]) -> dict[str, float | int | None]:
    return {
        "count": len(values),
        "p50": round(percentile(values, 0.50), 3) if values else None,
        "p95": round(percentile(values, 0.95), 3) if values else None,
    }


def _quantile_bins(rows: Sequence[dict[str, Any]], predictor: str, count: int = 4) -> list[dict[str, Any]]:
    eligible = [row for row in rows if _number(row.get(predictor)) is not None]
    eligible.sort(key=lambda row: (float(row[predictor]), str(row["attempt_id"])))
    count = min(count, len(eligible))
    bins: list[dict[str, Any]] = []
    for index in range(count):
        chunk = eligible[index * len(eligible) // count : (index + 1) * len(eligible) // count]
        values = [float(row[predictor]) for row in chunk]
        waits = [float(row["first_output_wait_us"]) for row in chunk]
        bins.append(
            {
                "index": index,
                "count": len(chunk),
                "predictorMin": min(values),
                "predictorMax": max(values),
                "firstOutputWaitUs": _distribution(waits),
            }
        )
    return bins


def analyze(records: Sequence[dict[str, Any]], exclusions: dict[str, int] | None = None) -> dict[str, Any]:
    exclusion_counts = dict(exclusions or {})
    outcome_counts = {"success": 0, "failed": 0, "cancelled": 0}
    logical: dict[str, list[dict[str, Any]]] = {}
    for record in records:
        outcome = record.get("outcome")
        if outcome in outcome_counts:
            outcome_counts[outcome] += 1
        request_id = record.get("sampling_request_id")
        if not isinstance(request_id, str) or not request_id:
            exclusion_counts["missing_sampling_request_id"] = exclusion_counts.get("missing_sampling_request_id", 0) + 1
            continue
        logical.setdefault(request_id, []).append(record)

    rows: list[dict[str, Any]] = []
    for attempts in logical.values():
        reason: str | None = None
        record = attempts[0]
        if len(attempts) != 1:
            reason = "multiple_physical_attempts"
        elif record.get("retry_index") != 0:
            reason = "nonzero_retry_index"
        elif record.get("outcome") != "success":
            reason = f"outcome_{record.get('outcome', 'missing')}"
        elif _number(record.get("first_model_output_us")) is None:
            reason = "missing_first_model_output"
        elif _number(record.get("dispatch_ready_us")) is None:
            reason = "missing_dispatch_ready"
        elif float(record["first_model_output_us"]) < float(record["dispatch_ready_us"]):
            reason = "invalid_timing_order"
        else:
            input_tokens = _number(record.get("input_token_count"))
            cached_tokens = _number(record.get("cached_input_token_count"))
            uncached_tokens = _number(record.get("uncached_input_token_count"))
            if input_tokens is None or cached_tokens is None or uncached_tokens is None:
                reason = "invalid_token_accounting"
            elif cached_tokens + uncached_tokens != input_tokens:
                reason = "inconsistent_token_accounting"
        if reason is not None:
            exclusion_counts[reason] = exclusion_counts.get(reason, 0) + 1
            continue

        row = {
            "sampling_request_id": record["sampling_request_id"],
            "attempt_id": record.get("attempt_id"),
            "model": record.get("model"),
            "transport": record.get("transport"),
            "request_kind": record.get("request_kind"),
            "input_token_count": record["input_token_count"],
            "cached_input_token_count": record["cached_input_token_count"],
            "uncached_input_token_count": record["uncached_input_token_count"],
            "first_output_wait_us": record["first_model_output_us"] - record["dispatch_ready_us"],
            "reconciliation_residual_bytes": record.get("reconciliation_residual_bytes"),
            "logical_request_bytes": record.get("logical_request_bytes"),
        }
        row.update({field: record.get(field) for field in COMPONENT_FIELDS})
        rows.append(row)

    members_by_group: dict[tuple[Any, Any, Any], list[dict[str, Any]]] = {}
    for row in rows:
        key = (row["model"], row["transport"], row["request_kind"])
        members_by_group.setdefault(key, []).append(row)
    groups: list[dict[str, Any]] = []
    for key, members in sorted(members_by_group.items(), key=lambda item: tuple(str(value) for value in item[0])):
        predictors: dict[str, Any] = {}
        for predictor in PREDICTOR_FIELDS:
            pairs = [
                (float(row[predictor]), float(row["first_output_wait_us"]))
                for row in members
                if _number(row.get(predictor)) is not None
            ]
            predictors[predictor] = {
                "distribution": _distribution([x for x, _ in pairs]),
                "spearmanRho": spearman([x for x, _ in pairs], [y for _, y in pairs]),
                "quantileBins": _quantile_bins(members, predictor),
            }
        groups.append(
            {
                "model": key[0],
                "transport": key[1],
                "requestKind": key[2],
                "sampleCount": len(members),
                "firstOutputWaitUs": _distribution([float(row["first_output_wait_us"]) for row in members]),
                "inputTokens": _distribution([float(row["input_token_count"]) for row in members]),
                "cachedInputTokens": _distribution([float(row["cached_input_token_count"]) for row in members]),
                "uncachedInputTokens": _distribution([float(row["uncached_input_token_count"]) for row in members]),
                "predictors": predictors,
            }
        )
    residuals: list[float] = []
    supplied_residual_mismatches = 0
    missing_reconciliation = 0
    for row in rows:
        logical_bytes = _number(row.get("logical_request_bytes"))
        components = [_number(row.get(field)) for field in COMPONENT_FIELDS]
        if logical_bytes is None or any(value is None for value in components):
            missing_reconciliation += 1
            continue
        computed_residual = float(logical_bytes) - sum(
            float(value) for value in components if value is not None
        )
        residuals.append(computed_residual)
        supplied = _number(row.get("reconciliation_residual_bytes"), nonnegative=False)
        if supplied is None or not math.isclose(float(supplied), computed_residual, abs_tol=0.5):
            supplied_residual_mismatches += 1
        row["computed_reconciliation_residual_bytes"] = computed_residual
    return {
        "interpretation": "observational and non-causal",
        "totalPhysicalAttempts": len(records),
        "totalLogicalRequests": len(logical),
        "includedLogicalRequests": len(rows),
        "outcomeCounts": outcome_counts,
        "exclusionCounts": dict(sorted(exclusion_counts.items())),
        "groups": groups,
        "componentReconciliation": {
            "coveredCount": len(residuals),
            "missingCount": missing_reconciliation,
            "coverageFraction": round(len(residuals) / len(rows), 6) if rows else None,
            "toleranceBytes": RECONCILIATION_TOLERANCE_BYTES,
            "withinToleranceCount": sum(
                abs(residual) <= RECONCILIATION_TOLERANCE_BYTES for residual in residuals
            ),
            "outsideToleranceCount": sum(
                abs(residual) > RECONCILIATION_TOLERANCE_BYTES for residual in residuals
            ),
            "suppliedResidualMismatchCount": supplied_residual_mismatches,
            "residualBytes": _distribution(residuals),
        },
        "stableContext": _stable_context_summary(records),
        "rows": rows,
    }


def render(analysis: dict[str, Any]) -> str:
    lines = [
        "Model attempt latency analysis (observational and non-causal)",
        f"physical attempts: {analysis['totalPhysicalAttempts']}",
        f"logical requests: {analysis['totalLogicalRequests']}",
        f"clean included requests: {analysis['includedLogicalRequests']}",
        f"outcomes: {json.dumps(analysis['outcomeCounts'], sort_keys=True)}",
        f"exclusions: {json.dumps(analysis['exclusionCounts'], sort_keys=True)}",
    ]
    for group in analysis["groups"]:
        wait = group["firstOutputWaitUs"]
        lines.append(
            f"{group['model']} / {group['transport']} / {group['requestKind']}: "
            f"n={group['sampleCount']} first-output-wait-us p50={wait['p50']} p95={wait['p95']}"
        )
        lines.append(
            "  tokens p50/p95: "
            f"input={group['inputTokens']['p50']}/{group['inputTokens']['p95']} "
            f"cached={group['cachedInputTokens']['p50']}/{group['cachedInputTokens']['p95']} "
            f"uncached={group['uncachedInputTokens']['p50']}/{group['uncachedInputTokens']['p95']}"
        )
        for predictor, summary in sorted(group["predictors"].items()):
            distribution = summary["distribution"]
            lines.append(
                f"  {predictor}: p50={distribution['p50']} p95={distribution['p95']} "
                f"spearman={summary['spearmanRho']} "
                f"bins={json.dumps(summary['quantileBins'], sort_keys=True)}"
            )
    reconciliation = analysis["componentReconciliation"]
    lines.append(
        "reconciliation: "
        f"covered={reconciliation['coveredCount']} missing={reconciliation['missingCount']} "
        f"within_tolerance={reconciliation['withinToleranceCount']} "
        f"outside_tolerance={reconciliation['outsideToleranceCount']} "
        f"tolerance_bytes={reconciliation['toleranceBytes']} "
        f"residual={json.dumps(reconciliation['residualBytes'], sort_keys=True)}"
    )
    lines.append(
        "Provider queueing, cache lookup, prefill execution, and generation startup are not separately observable."
    )
    stable = analysis["stableContext"]
    lines.append(
        "stable context: "
        f"average_active_tokens={stable['averageActiveContextTokens']} "
        f"peak_active_tokens={stable['peakActiveContextTokens']} "
        f"requests_over_80k={stable['requestsOver80K']} "
        f"local_constructed_bytes={stable['localConstructedBytes']} "
        f"local_reused_bytes={stable['localReusedBytes']} "
        f"component_cache_hits={stable['componentCacheHits']} "
        f"wire_bytes={stable['wireRequestBytes']} "
        f"provider_cached_share={stable['providerCachedShare']} "
        f"successful_rebases={stable['successfulRebases']} "
        f"fail_open_attempts={stable['failOpenAttempts']}"
    )
    for component in stable["componentVersions"][:15]:
        lines.append(
            "  component "
            f"{component['kind']} {component['semanticId']}: "
            f"requests={component['requestAppearances']} "
            f"tokens={component['approxTokens']} "
            f"cumulative={component['cumulativeLogicalExposureTokens']}"
        )
    return "\n".join(lines)
