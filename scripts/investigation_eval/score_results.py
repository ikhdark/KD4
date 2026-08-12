#!/usr/bin/env python3
"""Score manually recorded investigation-evaluation results."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from datetime import datetime
from pathlib import Path
from typing import Any

if __package__:
    from .validate_cases import (
        DEFAULT_CASES,
        REPO_ROOT,
        ValidationError,
        case_fingerprint,
        load_cases,
        validate_cases,
    )
else:  # Direct execution from this directory.
    from validate_cases import (
        DEFAULT_CASES,
        REPO_ROOT,
        ValidationError,
        case_fingerprint,
        load_cases,
        validate_cases,
    )

SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
RFC3339_RE = re.compile(
    r"^(?P<date>[0-9]{4}-[0-9]{2}-[0-9]{2})[Tt]"
    r"(?P<hour>[01][0-9]|2[0-3]):(?P<minute>[0-5][0-9]):"
    r"(?P<second>[0-5][0-9]|60)(?:\.[0-9]+)?"
    r"(?:[Zz]|[+-](?:[01][0-9]|2[0-3]):[0-5][0-9])$"
)
FINDING_STATUSES = {"confirmed", "deferred", "uncertain"}
FROZEN_MODEL_SETTINGS = {
    "name": "gpt-5.6-sol",
    "reasoning_effort": "max",
    "codex_version": "codex-cli 0.0.0",
}
FROZEN_EXECUTION = {
    "sandbox": "read-only",
    "session_persistence": "ephemeral",
    "user_configuration": "ignored",
}


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValidationError(message)


def _resolve_results_path(value: str) -> Path:
    candidate = Path(value)
    resolved = (
        candidate.resolve()
        if candidate.is_absolute()
        else (REPO_ROOT / candidate).resolve()
    )
    _require(resolved.is_dir(), f"results directory does not exist: {value}")
    return resolved


def _binary_sha256(path: Path) -> str:
    _require(path.is_file(), f"benchmark binary does not exist: {path}")
    digest = hashlib.sha256()
    with path.open("rb") as binary:
        for chunk in iter(lambda: binary.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


_NON_TOOL_ITEM_TYPES = {"agent_message", "reasoning"}
_VOLATILE_TOOL_FIELDS = {
    "aggregated_output",
    "duration_ms",
    "error",
    "exit_code",
    "id",
    "output",
    "result",
    "status",
}


def _derive_event_metrics(raw_events: list[Any], *, path: Path) -> dict[str, Any]:
    actions: list[str] = []
    agent_messages: list[str] = []
    for index, event in enumerate(raw_events):
        prefix = f"{path}: raw_events[{index}]"
        _require(isinstance(event, dict), f"{prefix} must be an object")
        _require(isinstance(event.get("type"), str), f"{prefix}.type must be a string")
        if event["type"] != "item.completed":
            continue
        item = event.get("item")
        _require(isinstance(item, dict), f"{prefix}.item must be an object")
        item_type = item.get("type")
        _require(isinstance(item_type, str), f"{prefix}.item.type must be a string")
        if item_type == "agent_message":
            text = item.get("text")
            _require(isinstance(text, str), f"{prefix}.item.text must be a string")
            agent_messages.append(text)
            continue
        if item_type in _NON_TOOL_ITEM_TYPES:
            continue
        action = {
            key: value for key, value in item.items() if key not in _VOLATILE_TOOL_FIELDS
        }
        actions.append(json.dumps(action, sort_keys=True, separators=(",", ":")))

    _require(agent_messages, f"{path}: raw_events has no completed agent message")
    repeated = len(actions) - len(set(actions))
    return {
        "tool_calls": len(actions),
        "repeated_equivalent_actions": repeated,
        "final_message": agent_messages[-1],
    }


def _is_rfc3339_timestamp(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    match = RFC3339_RE.fullmatch(value)
    if match is None:
        return False
    normalized = value.replace("t", "T")
    if normalized.endswith(("Z", "z")):
        normalized = f"{normalized[:-1]}+00:00"
    if match.group("second") == "60":
        second_start = match.start("second")
        normalized = f"{normalized[:second_start]}59{normalized[second_start + 2 :]}"
    try:
        datetime.fromisoformat(normalized)
    except ValueError:
        return False
    return True


def _load_result(
    path: Path,
    case: dict[str, Any],
    expected_binary_sha256: str,
) -> dict[str, Any]:
    try:
        result = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise ValidationError(f"{path}: invalid JSON: {exc.msg}") from exc
    _require(isinstance(result, dict), f"{path}: result must be an object")
    required = {
        "case_id",
        "case_fingerprint",
        "completed_at",
        "model",
        "execution",
        "final_output",
        "reported_findings",
        "raw_events",
    }
    _require(
        set(result) == required,
        f"{path}: result fields must be exactly {sorted(required)}",
    )
    _require(result["case_id"] == case["id"], f"{path}: case_id mismatch")
    _require(
        result["case_fingerprint"] == case_fingerprint(case),
        f"{path}: case_fingerprint does not match the current fixture",
    )
    completed_at = result["completed_at"]
    _require(
        _is_rfc3339_timestamp(completed_at),
        f"{path}: completed_at must be an RFC 3339 timestamp",
    )
    _require(
        isinstance(result["final_output"], str),
        f"{path}: final_output must be a string",
    )
    _require(
        isinstance(result["raw_events"], list), f"{path}: raw_events must be an array"
    )
    derived_metrics = _derive_event_metrics(result["raw_events"], path=path)
    _require(
        derived_metrics["final_message"] == result["final_output"],
        f"{path}: final_output must exactly match the last completed agent message",
    )

    model = result["model"]
    _require(isinstance(model, dict), f"{path}: model must be an object")
    _require(
        set(model) == {"name", "reasoning_effort", "codex_version", "binary_sha256"},
        f"{path}: invalid model metadata fields",
    )
    _require(
        all(isinstance(model[key], str) and model[key] for key in model),
        f"{path}: invalid model metadata",
    )
    _require(
        SHA256_RE.fullmatch(model["binary_sha256"]) is not None,
        f"{path}: invalid binary_sha256",
    )
    model_settings = {key: model[key] for key in FROZEN_MODEL_SETTINGS}
    _require(
        model_settings == FROZEN_MODEL_SETTINGS,
        f"{path}: model settings do not match the frozen corpus",
    )
    _require(
        model["binary_sha256"] == expected_binary_sha256,
        f"{path}: binary_sha256 does not match the hashed benchmark binary",
    )

    execution = result["execution"]
    _require(isinstance(execution, dict), f"{path}: execution must be an object")
    _require(
        execution == FROZEN_EXECUTION,
        f"{path}: execution settings do not match the frozen baseline",
    )

    findings = result["reported_findings"]
    _require(isinstance(findings, list), f"{path}: reported_findings must be an array")
    for index, finding in enumerate(findings):
        prefix = f"{path}: reported_findings[{index}]"
        _require(isinstance(finding, dict), f"{prefix} must be an object")
        _require(
            set(finding) == {"kind", "status", "locators"}, f"{prefix}: invalid fields"
        )
        _require(
            isinstance(finding["kind"], str) and finding["kind"],
            f"{prefix}: invalid kind",
        )
        _require(finding["status"] in FINDING_STATUSES, f"{prefix}: invalid status")
        _require(
            isinstance(finding["locators"], list) and finding["locators"],
            f"{prefix}: locators must be a non-empty array",
        )
        _require(
            all(
                isinstance(locator, str) and locator for locator in finding["locators"]
            ),
            f"{prefix}: locators must contain non-empty strings",
        )
        _require(
            len(set(finding["locators"])) == len(finding["locators"]),
            f"{prefix}: locators must not contain duplicates",
        )
        _require(
            all(
                locator.replace("\\", "/").casefold()
                in result["final_output"].replace("\\", "/").casefold()
                for locator in finding["locators"]
            ),
            f"{prefix}: every structured locator must appear in final_output",
        )
    result["_derived_metrics"] = derived_metrics
    return result


def _finding_matches(reported: dict[str, Any], expected: dict[str, Any]) -> bool:
    if reported["kind"] != expected["kind"]:
        return False
    reported_locators = {
        locator.strip().replace("\\", "/").casefold()
        for locator in reported["locators"]
    }
    return all(
        locator.strip().replace("\\", "/").casefold() in reported_locators
        for locator in expected["required_locators"]
    )


def _ratio(numerator: int, denominator: int) -> float:
    return 1.0 if denominator == 0 else numerator / denominator


def score(
    cases: list[dict[str, Any]],
    results_dir: Path,
    expected_binary_sha256: str,
) -> dict[str, Any]:
    expected_total = 0
    matched_expected = 0
    confirmed_total = 0
    matched_confirmed = 0
    forbidden_confirmed = 0
    clean_control_false_positives = 0
    deferred_or_uncertain = 0
    tool_calls = 0
    repeated_actions = 0
    case_scores: list[dict[str, Any]] = []

    for case in cases:
        result_path = results_dir / f"{case['id']}.json"
        _require(result_path.is_file(), f"missing result: {result_path}")
        result = _load_result(result_path, case, expected_binary_sha256)
        confirmed = [
            finding
            for finding in result["reported_findings"]
            if finding["status"] == "confirmed"
        ]
        deferred_or_uncertain += sum(
            finding["status"] in {"deferred", "uncertain"}
            for finding in result["reported_findings"]
        )

        unused_confirmed = set(range(len(confirmed)))
        case_matches = 0
        for expected in case["expected_findings"]:
            expected_total += 1
            match_index = next(
                (
                    index
                    for index in sorted(unused_confirmed)
                    if _finding_matches(confirmed[index], expected)
                ),
                None,
            )
            if match_index is not None:
                unused_confirmed.remove(match_index)
                matched_expected += 1
                matched_confirmed += 1
                case_matches += 1

        confirmed_total += len(confirmed)
        case_forbidden = sum(
            finding["kind"] in case["forbidden_findings"] for finding in confirmed
        )
        forbidden_confirmed += case_forbidden
        case_false_positives = len(unused_confirmed)
        if case["category"] == "clean-control":
            clean_control_false_positives += len(confirmed)

        metrics = result["_derived_metrics"]
        tool_calls += metrics["tool_calls"]
        repeated_actions += metrics["repeated_equivalent_actions"]

        case_scores.append(
            {
                "id": case["id"],
                "expected": len(case["expected_findings"]),
                "matched": case_matches,
                "confirmed_reported": len(confirmed),
                "false_positives": case_false_positives,
                "forbidden_confirmed": case_forbidden,
            }
        )

    return {
        "cases": len(cases),
        "binary_sha256": expected_binary_sha256,
        "confirmed_finding_recall": _ratio(matched_expected, expected_total),
        "precision": _ratio(matched_confirmed, confirmed_total),
        "expected_findings": expected_total,
        "matched_expected_findings": matched_expected,
        "confirmed_findings_reported": confirmed_total,
        "forbidden_confirmed_findings": forbidden_confirmed,
        "clean_control_false_positives": clean_control_false_positives,
        "deferred_or_uncertain_findings": deferred_or_uncertain,
        "tool_calls": tool_calls,
        "repeated_equivalent_actions": repeated_actions,
        "case_scores": case_scores,
    }


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--results", required=True, help="directory containing <case-id>.json files"
    )
    parser.add_argument(
        "--cases", default=str(DEFAULT_CASES), help="case manifest JSONL path"
    )
    parser.add_argument(
        "--binary",
        required=True,
        help="exact Codex executable used for the run; the scorer hashes it directly",
    )
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    try:
        binary_sha256 = _binary_sha256(Path(args.binary).resolve())
        cases_path = Path(args.cases)
        if not cases_path.is_absolute():
            cases_path = (REPO_ROOT / cases_path).resolve()
        cases = load_cases(cases_path)
        validate_cases(cases)
        report = score(cases, _resolve_results_path(args.results), binary_sha256)
    except (OSError, ValidationError) as exc:
        print(f"investigation result scoring failed: {exc}", file=sys.stderr)
        return 1

    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
