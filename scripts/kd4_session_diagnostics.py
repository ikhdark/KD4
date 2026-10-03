"""Coverage-aware distributions and observational comparisons of terminal turns."""

from __future__ import annotations

import collections
import hashlib
import json
import math
import re
from typing import Any

SCHEMA_VERSION = 1
# Read persisted fields, not synthesized zero defaults from aggregate reports.
_METRICS = {
    "elapsedMs": ("ms", ("inclusiveDurationNs",), 1_000_000),
    "agentActiveMs": ("ms", ("machineDurationNs",), 1_000_000),
    "modelActiveMs": ("ms", ("unions", "modelActiveUnionNs"), 1_000_000),
    "toolActiveMs": ("ms", ("unions", "toolActiveUnionNs"), 1_000_000),
    "modelOnlyMs": ("ms", ("exclusive", "modelOnlyNs"), 1_000_000),
    "toolOnlyMs": ("ms", ("exclusive", "toolOnlyNs"), 1_000_000),
    "modelPlusToolMs": ("ms", ("exclusive", "modelPlusToolNs"), 1_000_000),
    "orchestrationMs": ("ms", ("exclusive", "orchestrationNs"), 1_000_000),
    "retryOnlyMs": ("ms", ("exclusive", "retryOnlyNs"), 1_000_000),
    "humanOnlyWaitMs": ("ms", ("exclusive", "interactiveOnlyWaitNs"), 1_000_000),
    "generations": ("count", ("counters", "logicalGenerationCount"), 1),
    "toolCalls": ("count", ("counters", "toolCallCount"), 1),
    "commandTruncations": ("count", ("counters", "toolOutputTruncationCount"), 1),
    "truncationContinuationGenerations": (
        "count", ("counters", "truncationInducedContinuationCount"), 1,
    ),
    "samePurposeContinuations": (
        "count",
        ("counters", "samePurposeContinuationCount"),
        1,
    ),
    "waitOnlyGenerations": ("count", ("counters", "waitOnlyGenerationCount"), 1),
    "toolOutputTokensProjected": (
        "tokens",
        ("counters", "toolOutputModelTokenCount"),
        1,
    ),
    "artifactRecoveryCalls": ("count", ("counters", "toolOutputRecoveryCallCount"), 1),
    "artifactRereads": ("count", ("counters", "toolOutputArtifactRereadCount"), 1),
    "recoveryRetruncations": (
        "count",
        ("counters", "toolOutputRecoveryRetruncationCount"),
        1,
    ),
    "projectionTruncations": (
        "count",
        ("counters", "toolOutputProjectionTruncationCount"),
        1,
    ),
    "nonprogressGenerations": (
        "count",
        ("observationalNonprogressLatency", "logicalGenerations"),
        1,
    ),
}
_DERIVED_UNITS = {
    "rootToolCalls": "count",
    "nestedToolCalls": "count",
    "terminalGenerations": "count",
    "terminalModelMs": "ms",
    "inputEstimateAbsoluteErrorTokens": "tokens",
    "inputTokens": "tokens",
    "cachedInputTokens": "tokens",
    "uncachedInputTokens": "tokens",
    "failedCommands": "count",
    "duplicateToolRequests": "count",
    "redundantToolRequests": "count",
    "checkpointAttempts": "count",
    "usefulCheckpoints": "count",
    "discoveredEvidence": "count",
    "finalizedEvidence": "count",
    "discoveredButNotFinalizedEvidence": "count",
    "evidenceSurvival": "ratio",
    "finalAnswerRecall": "ratio",
    "finalAnswerPrecision": "ratio",
}
_THRESHOLDS = {"ms": 50, "count": 1, "tokens": 1, "ratio": 0.05}
_PHASES = (
    "modelOnlyMs",
    "toolOnlyMs",
    "modelPlusToolMs",
    "orchestrationMs",
    "retryOnlyMs",
)
_COVERAGE_BLOCKERS = (
    "parseErrorCount",
    "invalidProfiles",
    "conflictingTerminalProfiles",
    "classificationIncompleteProfiles",
    "terminalTurnsWithoutTiming",
    "terminalTurnsWithUnresolvedToolCalls",
    "terminalTurnsWithoutStart",
    "unpairedToolCalls",
)
_NOTE = (
    "Observed terminal-turn costs, not task success or avoidable waste. Cohorts separate "
    "population, terminal status/lifecycle and timing schema, not task difficulty, model "
    "or permissions. Compare matched workloads/configurations; changes are review cues, "
    "not causal regressions or statistical significance. Active turns are excluded. "
    "Percentiles use nearest rank. Phase totals sum turn time, not session elapsed time; "
    "ranked phases are not an exhaustive partition. Human wait is reported separately. "
    "Model/tool active unions overlap; orchestration is the exclusive harness phase, "
    "not elapsed minus summed requests. Projected tokens are runtime estimates, not "
    "provider billing. Duplicate requests are identical top-level inputs within "
    "a turn, not proven redundancy. Command/checkpoint observations cover paired "
    "top-level "
    "calls only, not hidden nested operations. Useful checkpoints require changed=true "
    "and checkpoint_item_persisted=true, not demonstrated task improvement. Evidence "
    "and answer-quality scores require explicit annotations, not lexical inference. "
    "Truncation sources are runtime projection operations and recovery sections; "
    "they can overlap and are not an exhaustive count of all truncation layers."
)


def _number(value: Any) -> bool:
    return (
        type(value) is int
        and 0 <= value < 2**64 - 1
        or type(value) is float
        and math.isfinite(value)
        and 0 <= value < 2**64 - 1
    )


def tool_observation(name: str, arguments: str, output: str) -> dict[str, Any]:
    """Retain small facts, never raw commands, outputs, or private arguments."""
    try:
        arguments = json.dumps(
            json.loads(arguments), sort_keys=True, separators=(",", ":")
        )
    except ValueError:
        pass
    try:
        result = json.loads(output)
    except ValueError:
        result = None
    result = result if isinstance(result, dict) else {}
    tool = name.rsplit(".", 1)[-1]
    exit_code = result.get("exit_code")
    if type(exit_code) is not int:
        match = re.search(
            r"(?m)^(?:Process exited with code|Exit code:)\s*(-?\d+)\s*$", output
        )
        exit_code = int(match[1]) if match else None
    command = tool in ("exec_command", "shell_command", "shell", "write_stdin")
    environment_crash = (
        command and type(exit_code) is int and (exit_code & 0xFFFFFFFF) in {
            0xC0000005, 0xC000001D, 0xC0000094, 0xC00000FD,
            0xC0000135, 0xC0000142, 0xC0000374, 0xC0000409, 0x80000003,
        }
    )
    checkpoint_useful = None
    if result.get("changed") is False:
        checkpoint_useful = False
    elif (
        result.get("changed") is True
        and type(result.get("checkpoint_item_persisted")) is bool
    ):
        checkpoint_useful = result["checkpoint_item_persisted"]
    return {
        "toolKnown": tool != "unknown" and bool(tool),
        "signature": hashlib.sha256((name + "\0" + arguments).encode()).hexdigest(),
        "command": command,
        "commandFailed": exit_code != 0 if command and type(exit_code) is int else None,
        "environmentCrash": environment_crash if command and type(exit_code) is int else None,
        "mayHideCommands": tool in ("exec", "wait"),
        "checkpoint": tool == "context_checkpoint",
        "checkpointUseful": checkpoint_useful,
    }


def _distribution(values: list[int | float], unit: str, turns: int) -> dict[str, Any]:
    values = sorted(values)
    metric = {"unit": unit, "samples": len(values), "missing": turns - len(values)}
    if values:
        metric.update(
            mean=sum(values) / len(values),
            p50=values[math.ceil(len(values) * 0.50) - 1],
            p95=values[math.ceil(len(values) * 0.95) - 1],
            max=values[-1],
        )
        if unit != "ratio":
            metric["total"] = sum(values)
    return metric


def _annotations(
    evidence: Any, records: list[dict[str, Any]], coverage: dict[str, Any]
) -> dict:
    if evidence is None:
        return {}
    if (
        not isinstance(evidence, dict)
        or type(evidence.get("schemaVersion")) is not int
        or evidence.get("schemaVersion") != 1
        or not isinstance(evidence.get("turns"), list)
    ):
        raise ValueError("diagnostic evidence requires schemaVersion 1 and turns")
    turns = {row["turn_id"]: row for row in records}
    snapshots = {row["path"]: row["sha256"] for row in coverage.get("snapshots", [])}
    annotations = {}
    fields = {
        "discoveredEvidenceIds",
        "finalEvidenceIds",
        "truthIds",
        "answerClaimIds",
        "redundantToolRequestIds",
    }
    for row in evidence["turns"]:
        if not isinstance(row, dict) or not isinstance(row.get("turnId"), str):
            raise TypeError("diagnostic evidence requires a turnId")
        turn_id = row["turnId"]
        if turn_id not in turns or turn_id in annotations:
            raise ValueError("diagnostic evidence has unknown or duplicate turnId")
        expected = snapshots.get(turns[turn_id]["file"])
        if expected is None or row.get("rolloutSha256") != expected:
            raise ValueError(
                "diagnostic evidence rolloutSha256 does not match the captured rollout"
            )
        if set(row) - fields - {"turnId", "rolloutSha256"}:
            raise ValueError("diagnostic evidence has unknown fields")
        for field in fields & row.keys():
            ids = row[field]
            if (
                not isinstance(ids, list)
                or any(not isinstance(value, str) or not value for value in ids)
                or len(set(ids)) != len(ids)
            ):
                raise ValueError(
                    f"diagnostic evidence {field} requires distinct nonempty string IDs"
                )
        annotations[turn_id] = row
    return annotations


def _turn_metrics(record: dict, turn: dict, coverage: dict, annotation: dict) -> dict:
    timing = record["timing"]
    metrics = {}
    reasons = {}
    for name, (_, path, divisor) in _METRICS.items():
        value = timing
        for key in path:
            value = value.get(key) if isinstance(value, dict) else None
        limit = (
            2**32 - 1
            if path[0] == "counters" and not path[-1].endswith("TokenCount")
            else 2**64 - 1
        )
        if (
            type(value) is int
            and _number(value)
            and value < limit
            and not timing.get("counters", {}).get("saturationCount")
        ):
            metrics[name] = value if divisor == 1 else value / divisor
        else:
            metrics[name] = None
            reasons[name] = "missing_invalid_or_saturated_measurement"
    metrics.update(dict.fromkeys(_DERIVED_UNITS))
    requests = timing.get("modelRequests", [])
    closure = timing.get("toolClosure", {})
    calls = timing.get("toolCalls", [])
    if (
        closure.get("complete") is True
        and type(metrics["toolCalls"]) is int
        and len(calls) == metrics["toolCalls"]
        and not timing.get("toolCallTimingOverflow")
        and all(row.get("source") in ("direct", "code_mode") for row in calls)
        and len({row.get("callId") for row in calls}) == len(calls)
    ):
        metrics["rootToolCalls"] = sum(row["source"] == "direct" for row in calls)
        metrics["nestedToolCalls"] = sum(row["source"] == "code_mode" for row in calls)
    if turn.get("requestRetention", {}).get("complete") is True:
        terminal = [row for row in requests if row.get("generationPurpose") == "terminal"]
        if all(type(row.get("generationIndex")) is int for row in terminal):
            metrics["terminalGenerations"] = len({row["generationIndex"] for row in terminal})
        if all(_number(row.get("modelStreamWaitNs")) for row in terminal):
            metrics["terminalModelMs"] = sum(row["modelStreamWaitNs"] for row in terminal) / 1e6
        categories = [row.get("requestTokenCategories", {}) for row in requests]
        if categories and all(
            _number(row.get("localInputEstimate"))
            and _number(row.get("providerInputTokens")) for row in categories
        ):
            metrics["inputEstimateAbsoluteErrorTokens"] = sum(
                abs(row["localInputEstimate"] - row["providerInputTokens"])
                for row in categories
            )
    if (
        metrics["nonprogressGenerations"] is None
        and "observationalNonprogressLatency" not in timing
        and turn.get("requestRetention", {}).get("complete") is True
        and all(
            type(row.get("unchangedRelevantState")) is bool
            and type(row.get("nextStructuredActionChanged")) is bool
            for row in requests
        )
        and not timing.get("counters", {}).get("saturationCount")
    ):
        metrics["nonprogressGenerations"] = turn["observationalNonprogressLatency"][
            "logicalGenerations"
        ]
        reasons.pop("nonprogressGenerations", None)
    tokens = turn.get("tokens", {})
    for name, field in (
        ("inputTokens", "inputTokens"),
        ("cachedInputTokens", "cachedInputTokens"),
        ("uncachedInputTokens", "nonCachedInputTokens"),
    ):
        if tokens.get("complete") is True and _number(tokens.get(field)):
            metrics[name] = tokens[field]
        else:
            reasons[name] = "provider_usage_unavailable_incomplete_or_disabled"
    observations = record.get("diagnosticToolObservations")
    complete_events = observations is not None and not any(
        coverage.get(key)
        for key in (
            "parseErrorCount",
            "unpairedToolCalls",
            "terminalTurnsWithoutStart",
            "terminalTurnsWithUnresolvedToolCalls",
        )
    )
    if complete_events and all(row["toolKnown"] for row in observations):
        signatures = [row["signature"] for row in observations]
        metrics["duplicateToolRequests"] = len(signatures) - len(set(signatures))
        checkpoints = [row for row in observations if row["checkpoint"]]
        commands = [row for row in observations if row["command"]]
        metrics["checkpointAttempts"] = len(checkpoints)
        for name, rows, key in (
            ("usefulCheckpoints", checkpoints, "checkpointUseful"),
            ("failedCommands", commands, "commandFailed"),
            ("environmentCrashes", commands, "environmentCrash"),
        ):
            if all(type(row.get(key)) is bool for row in rows):
                metrics[name] = sum(row[key] for row in rows)
        if any(row.get("mayHideCommands") for row in observations):
            # A successful JS wrapper is not evidence of successful children.
            # Runtime outcome labels also lack process exit codes and may count
            # several polls of one process, so do not substitute those counts.
            metrics["failedCommands"] = None
            reasons["failedCommands"] = "nested_command_outcomes_not_observed"
    for name in (
        "failedCommands",
        "duplicateToolRequests",
        "checkpointAttempts",
        "usefulCheckpoints",
    ):
        if metrics[name] is None:
            reasons.setdefault(name, "incomplete_top_level_tool_observations")
    if "redundantToolRequestIds" in annotation:
        metrics["redundantToolRequests"] = len(annotation["redundantToolRequestIds"])
    if "discoveredEvidenceIds" in annotation and "finalEvidenceIds" in annotation:
        discovered = set(annotation["discoveredEvidenceIds"])
        survived = discovered & set(annotation["finalEvidenceIds"])
        metrics.update(
            discoveredEvidence=len(discovered),
            finalizedEvidence=len(survived),
            discoveredButNotFinalizedEvidence=len(discovered - survived),
        )
        if discovered:
            metrics["evidenceSurvival"] = len(survived) / len(discovered)
        else:
            reasons["evidenceSurvival"] = "empty_discovered_evidence_set"
    if "truthIds" in annotation and "answerClaimIds" in annotation:
        truth, claims = set(annotation["truthIds"]), set(annotation["answerClaimIds"])
        matches = len(truth & claims)
        for name, denominator in (
            ("finalAnswerRecall", len(truth)),
            ("finalAnswerPrecision", len(claims)),
        ):
            if denominator:
                metrics[name] = matches / denominator
            else:
                reasons[name] = "empty_denominator"
    for name in _DERIVED_UNITS:
        if metrics[name] is None:
            reasons.setdefault(name, (
                "complete_valid_tool_call_ledger_required"
                if name in ("rootToolCalls", "nestedToolCalls")
                else "complete_valid_request_measurements_required"
                if name in ("terminalGenerations", "terminalModelMs", "inputEstimateAbsoluteErrorTokens")
                else "explicit_evidence_or_truth_set_required"
            ))
    return {
        "metrics": metrics,
        "unavailableReasons": reasons,
        "truncationCountBySource": {
            "commandOutput": metrics["commandTruncations"],
            "runtimeProjection": metrics["projectionTruncations"],
            "recoverySections": metrics["recoveryRetruncations"],
        },
        "truncationAttribution": {
            "attributedContinuationGenerations": metrics["truncationContinuationGenerations"],
            "unattributedOmissionDisposition": "unknown",
            "note": "Zero recovery/continuation counts do not prove omitted bytes were irrelevant or recovered.",
        },
    }


def build_diagnostics(
    records: list[dict[str, Any]],
    populations: dict[str, str],
    coverage: dict[str, Any],
    *,
    per_turn: list[dict[str, Any]] | None = None,
    evidence: Any = None,
) -> dict[str, Any]:
    annotations = _annotations(evidence, records, coverage)
    reports = {turn["turnId"]: turn for turn in per_turn or []}
    measured = {}
    for record in records:
        turn_id = record["turn_id"]
        measured[turn_id] = _turn_metrics(
            record, reports.get(turn_id, {}), coverage, annotations.get(turn_id, {})
        )
        if turn_id in reports:
            reports[turn_id]["diagnostics"] = measured[turn_id]
    units = {name: spec[0] for name, spec in _METRICS.items()} | _DERIVED_UNITS
    groups = collections.defaultdict(list)
    for record in records:
        key = (
            populations[record["turn_id"]],
            record["status"],
            record["lifecycle"],
            str(record["timing"].get("schemaVersion", "missing")),
        )
        groups[key].append(record)
    cohorts = []
    for (population, status, lifecycle, schema), rows in sorted(groups.items()):
        metrics = {}
        for name, unit in units.items():
            values = [measured[row["turn_id"]]["metrics"][name] for row in rows]
            metrics[name] = _distribution(
                [value for value in values if value is not None], unit, len(rows)
            )
        # Only rank phases measured for every turn; partial totals understate cost.
        ranked = sorted(
            (
                {"metric": name, "totalMs": metrics[name]["total"]}
                for name in _PHASES
                if metrics[name]["samples"]
                and not metrics[name]["missing"]
                and metrics[name]["total"] > 0
            ),
            key=lambda row: (-row["totalMs"], row["metric"]),
        )
        # Builds stay out of the cohort key: baselines compare across builds.
        builds = collections.Counter(row.get("build") or "unknown" for row in rows)
        cohorts.append(
            {
                "population": population,
                "status": status,
                "lifecycle": lifecycle,
                "timingSchemaVersion": schema,
                "builds": dict(sorted(builds.items())),
                "turns": len(rows),
                "metrics": metrics,
                "rankedTimeCosts": ranked,
            }
        )
    return {
        "schemaVersion": SCHEMA_VERSION,
        "measurementNote": _NOTE,
        "coverageBlockers": {
            key: coverage[key] for key in _COVERAGE_BLOCKERS if coverage.get(key)
        },
        "activeTurnsExcluded": coverage["startedTurnsWithoutTerminal"],
        "cohorts": cohorts,
        "metrics": {
            name: _distribution(
                [
                    row["metrics"][name]
                    for row in measured.values()
                    if row["metrics"][name] is not None
                ],
                unit,
                len(records),
            )
            for name, unit in units.items()
        },
    }


def _cohort_key(cohort: dict[str, Any]) -> tuple:
    return tuple(
        cohort[key]
        for key in ("population", "status", "lifecycle", "timingSchemaVersion")
    )


def compare_diagnostics(
    current: dict[str, Any],
    baseline_report: Any,
    *,
    min_samples: int = 5,
    relative_threshold: float = 0.20,
) -> dict[str, Any]:
    if type(min_samples) is not int or min_samples < 1:
        raise ValueError("comparison min samples must be a positive integer")
    if not _number(relative_threshold):
        raise ValueError("comparison relative threshold must be finite and nonnegative")
    baseline = (
        baseline_report.get("sessionDiagnostics")
        if isinstance(baseline_report, dict)
        else None
    )
    if (
        not isinstance(baseline, dict)
        or type(baseline.get("schemaVersion")) is not int
        or baseline.get("schemaVersion") != SCHEMA_VERSION
    ):
        raise ValueError(
            "baseline requires a supported sessionDiagnostics report; regenerate it from rollouts"
        )
    if not isinstance(baseline.get("cohorts"), list) or not isinstance(
        baseline.get("coverageBlockers"), dict
    ):
        raise ValueError(
            "baseline sessionDiagnostics is incomplete; "
            "regenerate it with --json from rollouts"
        )
    try:
        previous = {_cohort_key(row): row for row in baseline["cohorts"]}
    except (KeyError, TypeError) as error:
        raise ValueError("baseline has malformed cohorts") from error
    if len(previous) != len(baseline["cohorts"]):
        raise ValueError("baseline has duplicate cohorts")
    for key, cohort in previous.items():
        if (
            not all(isinstance(value, str) for value in key)
            or not isinstance(cohort.get("metrics"), dict)
            or type(cohort.get("turns")) is not int
            or cohort["turns"] < 1
        ):
            raise ValueError("baseline has malformed cohort measurements")
    rows = []
    matched = set()
    for cohort in current["cohorts"]:
        key = _cohort_key(cohort)
        old = previous.get(key)
        if old is not None:
            matched.add(key)
        identity = dict(
            zip(
                ("population", "terminalStatus", "lifecycle", "timingSchemaVersion"),
                key,
            )
        )
        for name, metric in cohort["metrics"].items():
            before = old.get("metrics", {}).get(name) if old else None
            reason = None
            if current["coverageBlockers"] or baseline["coverageBlockers"]:
                reason = "incomplete_session_coverage"
            elif cohort["timingSchemaVersion"] == "missing":
                reason = "unknown_timing_schema"
            elif before is None:
                reason = "no_matching_baseline_metric"
            elif not isinstance(before, dict) or before.get("unit") != metric["unit"]:
                reason = "incompatible_metric"
            elif any(
                type(item.get("samples")) is not int
                or type(item.get("missing")) is not int
                or item.get("missing") != 0
                or item["samples"] != group.get("turns")
                for item, group in ((metric, cohort), (before, old))
            ):
                reason = "incomplete_metric_coverage"
            elif min(metric["samples"], before["samples"]) < min_samples:
                reason = "insufficient_samples"
            elif (
                any(not _number(before.get(stat)) for stat in ("p50", "p95"))
                or before["p50"] > before["p95"]
                or (metric["unit"] == "ratio" and before["p95"] > 1)
            ):
                reason = "invalid_baseline_distribution"
            if reason:
                rows.append(
                    {
                        **identity,
                        "metric": name,
                        "status": "unavailable",
                        "reason": reason,
                    }
                )
                continue
            for stat in ("p50", "p95"):
                delta = metric[stat] - before[stat]
                floor = _THRESHOLDS[metric["unit"]]
                changed = abs(delta) >= max(floor, before[stat] * relative_threshold)
                row = {
                    **identity,
                    "metric": name,
                    "statistic": stat,
                    "unit": metric["unit"],
                    "current": metric[stat],
                    "baseline": before[stat],
                    "delta": delta,
                    "currentSamples": metric["samples"],
                    "baselineSamples": before["samples"],
                    "status": ("increased" if delta > 0 else "decreased")
                    if changed
                    else "within_threshold",
                }
                if before[stat] > 0:
                    row["relativeChange"] = delta / before[stat]
                rows.append(row)
    return {
        "schemaVersion": SCHEMA_VERSION,
        "measurementNote": _NOTE,
        "minSamples": min_samples,
        "relativeThreshold": relative_threshold,
        "absoluteThresholds": dict(_THRESHOLDS),
        **{
            "baseline" + key[0].upper() + key[1:]: baseline_report[key]
            for key in ("source", "observedAt")
            if key in baseline_report
        },
        "unmatchedBaselineCohorts": len(previous.keys() - matched),
        "comparedStatistics": sum(row["status"] != "unavailable" for row in rows),
        "statusCounts": dict(collections.Counter(row["status"] for row in rows)),
        "metrics": rows,
    }


def gate_metric_names() -> frozenset[str]:
    return frozenset(_METRICS) | frozenset(_DERIVED_UNITS)


def regression_gate(comparison: dict[str, Any], metrics: list[str]) -> dict[str, Any]:
    """Gate a baseline comparison on explicitly selected metrics.

    Ratios measure answer and evidence quality, so a drop regresses; every other
    unit is a cost, so a rise regresses. A selected metric that is unavailable in
    any cohort, or never compared, is insufficient evidence, never a pass.
    """
    selected = sorted(set(metrics))
    unknown = [name for name in selected if name not in gate_metric_names()]
    if not selected or unknown:
        raise ValueError(
            f"unknown gate metrics {unknown}; choose from {sorted(gate_metric_names())}"
            if unknown
            else "a regression gate needs at least one metric"
        )
    units = {name: spec[0] for name, spec in _METRICS.items()} | _DERIVED_UNITS
    rows = [row for row in comparison["metrics"] if row["metric"] in selected]
    regressions = [
        row
        for row in rows
        if row["status"]
        == ("decreased" if units[row["metric"]] == "ratio" else "increased")
    ]
    unavailable = [row for row in rows if row["status"] == "unavailable"]
    compared = {row["metric"] for row in rows if row["status"] != "unavailable"}
    unmeasured = [name for name in selected if name not in compared]
    if regressions:
        status = "regression"
    elif unavailable or unmeasured or comparison["unmatchedBaselineCohorts"]:
        status = "insufficient_evidence"
    else:
        status = "passed"
    policy = {
        "schemaVersion": 1,
        "metrics": selected,
        "directions": {
            name: "higher_is_better" if units[name] == "ratio" else "lower_is_better"
            for name in selected
        },
        "minSamples": comparison["minSamples"],
        "relativeThreshold": comparison["relativeThreshold"],
        "absoluteThresholds": comparison["absoluteThresholds"],
    }
    return {
        "status": status,
        "metrics": selected,
        "policy": policy,
        "policySha256": hashlib.sha256(
            json.dumps(policy, sort_keys=True, separators=(",", ":")).encode("utf-8")
        ).hexdigest(),
        "unmatchedBaselineCohorts": comparison["unmatchedBaselineCohorts"],
        "regressionCount": len(regressions),
        "unavailableCount": len(unavailable),
        "unmeasuredMetrics": unmeasured,
        "regressions": regressions,
        "unavailable": unavailable,
    }


def render_diagnostics(report: dict[str, Any]) -> list[str]:
    diagnostics = report.get("sessionDiagnostics")
    if diagnostics is None:
        return []
    lines = ["session diagnostics: " + diagnostics["measurementNote"]]
    if diagnostics["coverageBlockers"]:
        lines.append(
            f"  comparison coverage blockers: {diagnostics['coverageBlockers']}"
        )
    for cohort in diagnostics["cohorts"][:8]:
        lines.append(
            f"  {cohort['population']}/{cohort['lifecycle']}/schema-{cohort['timingSchemaVersion']}: "
            f"{cohort['turns']} turns; time costs={cohort['rankedTimeCosts']}"
        )
        if len(cohort.get("builds", {})) > 1:
            lines.append(f"    WARNING mixed builds in one cohort: {cohort['builds']}")
        for name in cohort["metrics"]:
            metric = cohort["metrics"][name]
            lines.append(
                f"    {name}: n={metric['samples']} missing={metric['missing']} "
                f"p50={metric.get('p50', 'unavailable')} p95={metric.get('p95', 'unavailable')}"
            )
    if len(diagnostics["cohorts"]) > 8:
        lines.append("  additional cohorts in JSON output")
    comparison = report.get("baselineComparison")
    if comparison is not None:
        lines.append(
            f"baseline comparison (observational): {comparison['comparedStatistics']} "
            f"compared statistics; {comparison['statusCounts']}; "
            f"unmatched baseline cohorts={comparison['unmatchedBaselineCohorts']}"
        )
        changes = sorted(
            [
                row
                for row in comparison["metrics"]
                if row["status"] != "within_threshold"
            ],
            key=lambda row: {"increased": 0, "decreased": 1, "unavailable": 2}[
                row["status"]
            ],
        )
        for row in changes[:12]:
            lines.append(
                f"  {row['population']}/{row['lifecycle']}: {row['metric']} "
                f"{row.get('statistic', '')} {row['status']}; "
                + (
                    row["reason"]
                    if "reason" in row
                    else f"delta={row['delta']:+g}{row['unit']}"
                )
            )
        if len(changes) > 12:
            lines.append(
                f"  {len(changes) - 12} additional comparison rows in JSON output"
            )
        gate = comparison.get("gate")
        if gate is not None:
            lines.append(
                f"regression gate: {gate['status']} for {', '.join(gate['metrics'])}; "
                f"regressions={gate['regressionCount']} "
                f"unavailable={gate['unavailableCount']} "
                f"unmeasured={gate['unmeasuredMetrics']}"
            )
    return lines
