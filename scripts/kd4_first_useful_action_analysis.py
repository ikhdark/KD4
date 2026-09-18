"""Internal first-useful-action metrics for the canonical turn latency audit."""

from __future__ import annotations

import json
import math
import statistics
from dataclasses import dataclass
from datetime import datetime
from typing import Any, Iterable, Sequence

try:
    from scripts.rollout_snapshot import RolloutSnapshot
except ImportError:
    from rollout_snapshot import RolloutSnapshot


CANONICAL_TIMING_SCHEMA_VERSION = 25
INFRASTRUCTURE_TOOLS = frozenset(
    {
        "exec",
        "update_plan",
        "request_user_input",
        "request_permissions",
        "wait",
        "wait_agent",
        "wait_for_environment",
        "write_stdin",
    }
)
TOOL_DISCOVERY_TOOLS = frozenset(
    {"tool_search", "list_mcp_resources", "list_mcp_resource_templates"}
)


def _tool_basename(name: str) -> str:
    return name.rsplit(".", 1)[-1]


def is_useful_tool(name: str) -> bool:
    basename = _tool_basename(name)
    return (
        bool(name)
        and basename not in INFRASTRUCTURE_TOOLS
        and basename not in TOOL_DISCOVERY_TOOLS
    )


def _timestamp_ms(value: object) -> float | None:
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1_000
    except ValueError:
        return None


def _tool_name(item: dict[str, Any]) -> str | None:
    item_type = item.get("type")
    if item_type in {"function_call", "custom_tool_call"}:
        name = item.get("name")
        return name if isinstance(name, str) else None
    builtin_names = {
        "local_shell_call": "local_shell",
        "web_search_call": "web_search",
        "image_generation_call": "image_generation",
    }
    if item_type in builtin_names:
        return builtin_names[item_type]
    if item_type == "tool_search_call" and item.get("execution") == "client":
        return "tool_search"
    return None


def _percentile(values: Sequence[float], fraction: float) -> float:
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def _summary(values: Iterable[float]) -> dict[str, float | int | None]:
    samples = list(values)
    if not samples:
        return {
            "count": 0,
            "p50": None,
            "p95": None,
            "min": None,
            "mean": None,
            "populationStdDev": None,
            "max": None,
        }
    return {
        "count": len(samples),
        "p50": round(_percentile(samples, 0.50), 3),
        "p95": round(_percentile(samples, 0.95), 3),
        "min": round(min(samples), 3),
        "mean": round(statistics.fmean(samples), 3),
        "populationStdDev": round(statistics.pstdev(samples), 3),
        "max": round(max(samples), 3),
    }


def _delta(end: object, start: object) -> float | None:
    if not isinstance(end, (int, float)) or not isinstance(start, (int, float)):
        return None
    return max(0.0, float(end) - float(start))


@dataclass
class _Turn:
    started_ms: float
    user_input_ms: float | None = None
    useful_tool_emitted_ms: float | None = None
    useful_tool_name: str | None = None


def decoded_records(snapshot: RolloutSnapshot) -> Iterable[Any]:
    with snapshot.open_lines() as handle:
        for line in handle:
            try:
                yield json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                yield None


def analyze_snapshots(snapshots: Sequence[RolloutSnapshot]) -> dict[str, Any]:
    """Analyze exact captured bytes, excluding malformed or partial records."""
    return analyze_records(
        (snapshot.metadata(), decoded_records(snapshot)) for snapshot in snapshots
    )


def canonical_milestones(timing: Any) -> dict[str, float] | None:
    """Require the domain/useful pair; other boundaries have separate coverage."""
    if not isinstance(timing, dict):
        return None
    version = timing.get("schemaVersion")
    milestones = timing.get("milestones")
    if (
        isinstance(version, int)
        and version >= CANONICAL_TIMING_SCHEMA_VERSION
        and isinstance(milestones, dict)
        and all(
            isinstance(milestones.get(key), (int, float))
            and not isinstance(milestones[key], bool)
            and math.isfinite(milestones[key])
            and milestones[key] >= 0
            for key in ("firstDomainActionMs", "firstUsefulActionMs")
        )
    ):
        return {
            key: float(value)
            for key, value in milestones.items()
            if isinstance(value, (int, float))
            and not isinstance(value, bool)
            and math.isfinite(value)
            and value >= 0
        }
    return None


def analyze_records(
    record_sets: Iterable[tuple[dict[str, str | int], Iterable[Any]]],
) -> dict[str, Any]:
    """Analyze parsed evidence shared by the primary audit without reparsing bytes."""
    exclusions = {
        "invalidJsonLines": 0,
        "invalidTimestamps": 0,
        "incompleteTurns": 0,
        "supersededTurns": 0,
        "unterminatedTurns": 0,
        "incompleteCanonicalMilestones": 0,
    }
    legacy_rows: list[dict[str, float]] = []
    canonical_rows: list[dict[str, float]] = []
    snapshot_metadata: list[dict[str, str | int]] = []
    completed_turns = 0
    record_count = 0
    started_turns = 0
    schema_versions: dict[str, int] = {}

    for metadata, records in record_sets:
        snapshot_metadata.append(metadata)
        active: _Turn | None = None
        for record in records:
            record_count += 1
            if not isinstance(record, dict):
                exclusions["invalidJsonLines"] += 1
                continue
            timestamp_ms = _timestamp_ms(record.get("timestamp"))
            if timestamp_ms is None:
                exclusions["invalidTimestamps"] += 1
                continue
            payload = record.get("payload")
            if not isinstance(payload, dict):
                continue
            record_type = record.get("type")
            payload_type = payload.get("type")

            if record_type == "event_msg" and payload_type == "task_started":
                started_turns += 1
                if active is not None:
                    exclusions["incompleteTurns"] += 1
                    exclusions["supersededTurns"] += 1
                active = _Turn(started_ms=timestamp_ms)
                continue
            if active is None:
                continue
            if record_type == "event_msg" and payload_type == "user_message":
                if active.user_input_ms is None:
                    active.user_input_ms = timestamp_ms
                continue
            if record_type == "response_item":
                tool_name = _tool_name(payload)
                if (
                    tool_name is not None
                    and active.useful_tool_emitted_ms is None
                    and is_useful_tool(tool_name)
                ):
                    active.useful_tool_emitted_ms = timestamp_ms
                    active.useful_tool_name = tool_name
                continue
            if record_type != "event_msg" or payload_type != "task_complete":
                continue

            completed_turns += 1
            timing = payload.get("timing")
            milestones = canonical_milestones(timing)
            schema_version = (
                timing.get("schemaVersion") if isinstance(timing, dict) else None
            )
            version_key = (
                str(schema_version) if type(schema_version) is int else "missing"
            )
            schema_versions[version_key] = schema_versions.get(version_key, 0) + 1
            if milestones is not None:
                canonical_rows.append(milestones)
            elif (
                isinstance(schema_version, int)
                and schema_version >= CANONICAL_TIMING_SCHEMA_VERSION
            ):
                exclusions["incompleteCanonicalMilestones"] += 1
            elif active.useful_tool_emitted_ms is not None:
                row = {
                    "startToUsefulToolEmittedMs": active.useful_tool_emitted_ms
                    - active.started_ms
                }
                if active.user_input_ms is not None:
                    row["startToUserInputEventMs"] = (
                        active.user_input_ms - active.started_ms
                    )
                    row["userInputEventToUsefulToolEmittedMs"] = (
                        active.useful_tool_emitted_ms - active.user_input_ms
                    )
                legacy_rows.append(row)
            active = None
        if active is not None:
            exclusions["incompleteTurns"] += 1
            exclusions["unterminatedTurns"] += 1

    canonical_metrics = {
        "startToUserInputRecordedMs": _summary(
            row["userInputRecordedMs"]
            for row in canonical_rows
            if "userInputRecordedMs" in row
        ),
        "userInputToUsefulAcceptedMs": _summary(
            value
            for row in canonical_rows
            if (
                value := _delta(
                    row.get("firstUsefulToolAcceptedMs"), row.get("userInputRecordedMs")
                )
            )
            is not None
        ),
        "usefulParallelGateWaitMs": _summary(
            value
            for row in canonical_rows
            if (
                value := _delta(
                    row.get("firstUsefulToolGateAdmittedMs"),
                    row.get("firstUsefulToolAcceptedMs"),
                )
            )
            is not None
        ),
        "usefulAuthorizationAndDispatchMs": _summary(
            value
            for row in canonical_rows
            if (
                value := _delta(
                    row.get("firstUsefulActionMs"),
                    row.get("firstUsefulToolGateAdmittedMs"),
                )
            )
            is not None
        ),
        "startToFirstUsefulActionMs": _summary(
            row["firstUsefulActionMs"] for row in canonical_rows
        ),
        "startToFirstInfrastructureActionMs": _summary(
            row["firstInfrastructureActionMs"]
            for row in canonical_rows
            if "firstInfrastructureActionMs" in row
        ),
        "startToFirstToolDiscoveryActionMs": _summary(
            row["firstToolDiscoveryActionMs"]
            for row in canonical_rows
            if "firstToolDiscoveryActionMs" in row
        ),
        "startToFirstDomainActionMs": _summary(
            row["firstDomainActionMs"] for row in canonical_rows
        ),
        "startToFirstSuccessfulDomainActionMs": _summary(
            row["firstSuccessfulDomainActionMs"]
            for row in canonical_rows
            if "firstSuccessfulDomainActionMs" in row
        ),
        "usefulExecutionToSuccessMs": _summary(
            value
            for row in canonical_rows
            if (
                value := _delta(
                    row.get("firstSuccessfulUsefulActionMs"),
                    row.get("firstUsefulActionMs"),
                )
            )
            is not None
        ),
    }
    legacy_metrics = {
        key: _summary(row[key] for row in legacy_rows if key in row)
        for key in (
            "startToUserInputEventMs",
            "userInputEventToUsefulToolEmittedMs",
            "startToUsefulToolEmittedMs",
        )
    }
    # These boundaries were added after schema 25. Missing events remain
    # unavailable, including completed turns that never enter a tool handler.
    for metric, key in (
        ("startToFirstToolHandlerEntryMs", "firstToolHandlerEntryMs"),
        ("startToFirstModelOutputMs", "firstModelOutputMs"),
        ("startToFirstActionableOutputMs", "firstActionableOutputMs"),
        ("startToFirstVisibleOutputMs", "firstVisibleOutputMs"),
        ("startToFirstAgentMessageMs", "firstAgentMessageMs"),
    ):
        canonical_metrics[metric] = _summary(
            row[key] for row in canonical_rows if key in row
        )
    for metrics in (canonical_metrics, legacy_metrics):
        for summary in metrics.values():
            summary["eligibleTurnCount"] = completed_turns
            summary["coverage"] = (
                summary["count"] / completed_turns if completed_turns else None
            )
    denominators = {
        "invalidJsonLines": record_count,
        "invalidTimestamps": record_count - exclusions["invalidJsonLines"],
        "incompleteTurns": started_turns,
        "supersededTurns": started_turns,
        "unterminatedTurns": started_turns,
        "incompleteCanonicalMilestones": sum(
            count
            for version, count in schema_versions.items()
            if version != "missing" and int(version) >= CANONICAL_TIMING_SCHEMA_VERSION
        ),
    }
    return {
        "schemaVersion": 1,
        "measurementContract": {
            "quantileMethod": "linear interpolation at (n-1)*q",
            "spread": "population standard deviation of observed values",
            "coverage": "Each metric reports its observed count over all completed turns. Optional or inapplicable milestones are not zero latency.",
            "incompleteTurns": "supersededTurns counts a start before the active turn completed; unterminatedTurns counts an active turn at end of input. These describe evidence shape, not its cause.",
            "canonical": (
                "timing schema 25+: separate authorized infrastructure, tool-discovery, "
                "domain, and successful-domain handler boundaries"
            ),
            "legacy": (
                "rollout event reconstruction: first domain tool emitted by the model; "
                "not handler entry and not a runtime benchmark"
            ),
        },
        "sourceFileCount": len(snapshot_metadata),
        "sourceSnapshots": snapshot_metadata,
        "completedTurnCount": completed_turns,
        "startedTurnCount": started_turns,
        "recordCount": record_count,
        "timingSchemaVersions": schema_versions,
        "canonicalTurnCount": len(canonical_rows),
        "legacyReconstructedTurnCount": len(legacy_rows),
        "canonical": canonical_metrics,
        "legacyReconstructed": legacy_metrics,
        "exclusions": exclusions,
        "exclusionRates": {
            key: {
                "count": count,
                "denominator": denominators[key],
                "rate": count / denominators[key] if denominators[key] else None,
            }
            for key, count in exclusions.items()
        },
    }
