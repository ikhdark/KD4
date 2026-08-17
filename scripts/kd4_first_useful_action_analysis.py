#!/usr/bin/env python3
"""Analyze first-useful-action latency from Codex rollout JSONL files."""

from __future__ import annotations

import argparse
import json
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable, Sequence


CANONICAL_TIMING_SCHEMA_VERSION = 20
CONTROL_ONLY_TOOLS = frozenset(
    {
        "update_plan",
        "request_user_input",
        "request_permissions",
        "wait",
        "wait_agent",
        "wait_for_environment",
        "write_stdin",
    }
)


def _tool_basename(name: str) -> str:
    return name.rsplit(".", 1)[-1]


def is_useful_tool(name: str) -> bool:
    return bool(name) and _tool_basename(name) not in CONTROL_ONLY_TOOLS


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
        return {"count": 0, "p50": None, "p95": None, "max": None}
    return {
        "count": len(samples),
        "p50": round(_percentile(samples, 0.50), 3),
        "p95": round(_percentile(samples, 0.95), 3),
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


def _rollout_files(inputs: Sequence[Path]) -> list[Path]:
    files: dict[str, Path] = {}
    for input_path in inputs:
        candidates = input_path.rglob("*.jsonl") if input_path.is_dir() else (input_path,)
        for candidate in candidates:
            if candidate.is_file():
                resolved = candidate.resolve()
                files[str(resolved).casefold()] = resolved
    return sorted(files.values())


def analyze(inputs: Sequence[Path]) -> dict[str, Any]:
    files = _rollout_files(inputs)
    exclusions = {"invalidJsonLines": 0, "invalidTimestamps": 0, "incompleteTurns": 0}
    legacy_rows: list[dict[str, float]] = []
    canonical_rows: list[dict[str, float]] = []
    completed_turns = 0

    for path in files:
        active: _Turn | None = None
        with path.open("r", encoding="utf-8") as handle:
            for raw_line in handle:
                try:
                    record = json.loads(raw_line)
                except json.JSONDecodeError:
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
                    if active is not None:
                        exclusions["incompleteTurns"] += 1
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
                milestones = timing.get("milestones") if isinstance(timing, dict) else None
                schema_version = timing.get("schemaVersion") if isinstance(timing, dict) else None
                if (
                    isinstance(schema_version, int)
                    and schema_version >= CANONICAL_TIMING_SCHEMA_VERSION
                    and isinstance(milestones, dict)
                    and isinstance(milestones.get("firstUsefulActionMs"), (int, float))
                ):
                    canonical_rows.append(
                        {
                            key: float(value)
                            for key, value in milestones.items()
                            if isinstance(value, (int, float))
                        }
                    )
                elif active.useful_tool_emitted_ms is not None:
                    row = {
                        "startToUsefulToolEmittedMs": active.useful_tool_emitted_ms
                        - active.started_ms
                    }
                    if active.user_input_ms is not None:
                        row["startToUserInputEventMs"] = active.user_input_ms - active.started_ms
                        row["userInputEventToUsefulToolEmittedMs"] = (
                            active.useful_tool_emitted_ms - active.user_input_ms
                        )
                    legacy_rows.append(row)
                active = None
        if active is not None:
            exclusions["incompleteTurns"] += 1

    canonical_metrics = {
        "startToUserInputRecordedMs": _summary(
            row["userInputRecordedMs"]
            for row in canonical_rows
            if "userInputRecordedMs" in row
        ),
        "userInputToUsefulAcceptedMs": _summary(
            value
            for row in canonical_rows
            if (value := _delta(row.get("firstUsefulToolAcceptedMs"), row.get("userInputRecordedMs")))
            is not None
        ),
        "usefulParallelGateWaitMs": _summary(
            value
            for row in canonical_rows
            if (value := _delta(row.get("firstUsefulToolGateAdmittedMs"), row.get("firstUsefulToolAcceptedMs")))
            is not None
        ),
        "usefulAuthorizationAndDispatchMs": _summary(
            value
            for row in canonical_rows
            if (value := _delta(row.get("firstUsefulActionMs"), row.get("firstUsefulToolGateAdmittedMs")))
            is not None
        ),
        "startToFirstUsefulActionMs": _summary(
            row["firstUsefulActionMs"] for row in canonical_rows
        ),
        "usefulExecutionToSuccessMs": _summary(
            value
            for row in canonical_rows
            if (value := _delta(row.get("firstSuccessfulUsefulActionMs"), row.get("firstUsefulActionMs")))
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
    return {
        "schemaVersion": 1,
        "measurementContract": {
            "canonical": (
                "timing schema 20+: authorized non-control handler entry, with accepted, "
                "gate, input, and successful-completion phase boundaries"
            ),
            "legacy": (
                "rollout event reconstruction: first non-control tool emitted by the model; "
                "not handler entry and not a runtime benchmark"
            ),
        },
        "sourceFileCount": len(files),
        "completedTurnCount": completed_turns,
        "canonicalTurnCount": len(canonical_rows),
        "legacyReconstructedTurnCount": len(legacy_rows),
        "canonical": canonical_metrics,
        "legacyReconstructed": legacy_metrics,
        "exclusions": exclusions,
    }


def render(analysis: dict[str, Any]) -> str:
    def milliseconds(value: Any) -> str:
        return "None" if value is None else f"{value}ms"

    def metric(label: str, value: dict[str, Any]) -> str:
        return (
            f"{label}: n={value['count']} p50={milliseconds(value['p50'])} "
            f"p95={milliseconds(value['p95'])} max={milliseconds(value['max'])}"
        )

    lines = [
        "First-useful-action latency",
        f"files={analysis['sourceFileCount']} completed_turns={analysis['completedTurnCount']} ",
        f"canonical_turns={analysis['canonicalTurnCount']} legacy_turns={analysis['legacyReconstructedTurnCount']}",
        "Legacy values are tool-emission approximations, not handler-entry runtime measurements.",
    ]
    lines.extend(metric(name, value) for name, value in analysis["canonical"].items())
    lines.extend(
        metric(f"legacy.{name}", value)
        for name, value in analysis["legacyReconstructed"].items()
    )
    return "\n".join(lines)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inputs", nargs="+", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    result = analyze(args.inputs)
    print(json.dumps(result, sort_keys=True) if args.json else render(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
