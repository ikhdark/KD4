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


def _event_fields(value: Any) -> dict[str, Any] | None:
    if not isinstance(value, dict):
        return None
    fields = dict(value)
    for key in ("fields", "attributes", "body"):
        nested = value.get(key)
        if isinstance(nested, dict):
            fields.update(nested)
    event_name = fields.get("event.name", fields.get("event_name"))
    return fields if event_name == "codex.model_attempt" else None


def load_jsonl(paths: Sequence[Path]) -> tuple[list[dict[str, Any]], dict[str, int]]:
    records: list[dict[str, Any]] = []
    exclusions: dict[str, int] = {}
    for path in paths:
        with path.open(encoding="utf-8") as source:
            for line in source:
                if not line.strip():
                    continue
                try:
                    fields = _event_fields(json.loads(line))
                except json.JSONDecodeError:
                    fields = None
                    reason = "malformed_json"
                else:
                    reason = "not_model_attempt"
                if fields is None:
                    exclusions[reason] = exclusions.get(reason, 0) + 1
                else:
                    records.append(fields)
    return records, exclusions


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
    return "\n".join(lines)
