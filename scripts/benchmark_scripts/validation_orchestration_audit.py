#!/usr/bin/env python3
"""Measure the model work that surrounds validation, not the tests' runtime.

Classifies each model generation in a rollout by the validation role of the
tool calls it produced, then reports requests, tokens, reasoning and the
model-side wall clock for each role. The point of interest is orchestration:
deciding what to test, launching it, reading the result, and deciding whether
the work is done.

Counts are observations over recorded rollouts. A classification is a keyword
judgement over the recorded cell text, so per-role totals are estimates; the
request, token and timing totals they partition are exact.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Iterator, Sequence

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.benchmark_scripts.rollout_snapshot import (  # noqa: E402
    discover_rollouts,
    iter_rollout_records,
)

# A cell is validation when it launches tests/lints/schema checks, or inspects
# the state of a validation run. Ordered: the first match wins.
LAUNCH_PATTERNS = (
    r"\bjust\s+core-(?:test|gate)",
    r"\bjust\s+clippy\b",
    r"\bcargo\s+(?:test|nextest|clippy|check|build)\b",
    r"rust_test_runner\.py\s+run-(?:target|lane)",
    r"root_maintenance\.py\s+test-python",
    r"\bjust\s+\S*schema-check",
    r"config_schema_check\.py|app_server_schema_runtime_check\.py",
    r"\bpytest\b|unittest\b",
    r"\bjust\s+fmt\b|\brustfmt\b|format\.py",
)
PLAN_PATTERNS = (
    r"\bjust\s+core-test-gates-for\b",
    r"\bjust\s+core-test-plan\b",
    r"\bjust\s+core-test-list\b",
    r"kd4-rust-tests\.toml",
)
STATUS_PATTERNS = (
    r"rust_build_status\.py",
    r"\blanes\b.*\bbusy\b|codex_rust_admission_v1",
    r"validation[-_]ledger",
)

LAUNCH_RE = re.compile("|".join(LAUNCH_PATTERNS), re.IGNORECASE)
PLAN_RE = re.compile("|".join(PLAN_PATTERNS), re.IGNORECASE)
STATUS_RE = re.compile("|".join(STATUS_PATTERNS), re.IGNORECASE)

# Admission refusals and other non-results: model work that buys no evidence.
BUSY_RE = re.compile(
    r"codex_rust_admission_v1|exit (?:code )?75|Validation busy|"
    r"admission.{0,20}busy|held by another",
    re.IGNORECASE,
)
ZERO_TEST_RE = re.compile(
    r"no tests to run|0 tests run|matched no tests|zero-test", re.IGNORECASE
)

# An edit between two validation launches is what makes the second one new
# evidence. Without one, the second launch re-measures an unchanged tree.
# Code-mode cells build patches inside JS strings, so match the patch envelope
# as well as the tool name; missing those reads as a redundant relaunch.
# Code-mode cells build patches inside template literals and helper closures,
# so the tool name alone misses many edits and a relaunch after one then reads
# as redundant. Match the patch envelope and the store/change helpers too.
EDIT_RE = re.compile(
    r"apply_patch|write_file|\bedit_file\b|save_turn_diff|"
    r"\*\*\*\s*Begin Patch|Retry Patch:|Update File:|Add File:|Replace Chunk|"
    r"\bstore\(|\bchange\(",
    re.IGNORECASE,
)
# A cell that reads or inspects is doing new work, not re-running a check.
INSPECT_RE = re.compile(
    r"read_file|read_tool_output|Get-Content|Get-ChildItem|\brg\b|\bgrep\b",
    re.IGNORECASE,
)
FAILURE_RE = re.compile(
    r"\bFAILED\b|\bfailures:|test result: FAILED|error\[E\d{4}\]|panicked at",
    re.IGNORECASE,
)


@dataclass
class Generation:
    """One model request and the tool calls it produced."""

    response_id: str | None = None
    turn_id: str | None = None
    input_tokens: int = 0
    cached_input_tokens: int = 0
    output_tokens: int = 0
    reasoning_tokens: int = 0
    started: datetime | None = None
    ended: datetime | None = None
    model_seconds: float = 0.0
    calls: list[str] = field(default_factory=list)
    outputs: list[str] = field(default_factory=list)

    @property
    def role(self) -> str:
        text = "\n".join(self.calls)
        if not text.strip():
            return "other"
        if PLAN_RE.search(text):
            return "plan"
        if LAUNCH_RE.search(text):
            return "launch"
        if STATUS_RE.search(text):
            return "status"
        return "other"

    @property
    def uncached_input(self) -> int:
        return max(self.input_tokens - self.cached_input_tokens, 0)


def _parse_time(value: Any) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _call_text(payload: dict[str, Any]) -> str:
    for key in ("input", "arguments"):
        value = payload.get(key)
        if isinstance(value, str):
            return value
        if value is not None:
            return json.dumps(value)
    return ""


def _output_text(payload: dict[str, Any]) -> str:
    value = payload.get("output")
    if isinstance(value, str):
        return value
    if value is not None:
        return json.dumps(value)
    return ""


def read_generations(path: Path) -> list[Generation]:
    """Return one rollout's generations; `analyse` summarises these."""
    return _walk(path)


def analyse(path: Path) -> dict[str, Any]:
    """Walk one rollout, pairing each token_usage_record with its tool calls.

    Rollouts record a generation as `call -> token_usage_record -> output`: the
    model's tool call is written before the usage record that accounts for the
    request that produced it, and the tool's output lands after. So calls seen
    since the previous usage record belong to the generation being closed, and
    outputs attach to the generation already closed.
    """
    return summarise(path, _walk(path))


def _walk(path: Path) -> list[Generation]:
    """Decode one rollout into its generations."""
    generations: list[Generation] = []
    pending_calls: list[str] = []
    last_event: datetime | None = None
    previous: Generation | None = None
    usage_shape: str | None = None
    # `sampling_boundary.modelOnlyNs` is cumulative within a turn and resets at
    # each new turn, so the per-request cost is the rise since the last
    # boundary of the same turn.
    turn_model_ns: dict[str, int] = {}
    pending_model_seconds = 0.0

    def reject_gap(gap):
        raise ValueError(f"incomplete rollout coverage: {gap['error']}")

    for record, _ in iter_rollout_records(path, recover=True, on_gap=reject_gap):
        kind = record.get("type")
        stamp = _parse_time(record.get("timestamp"))
        payload = record.get("payload") or {}

        if kind == "sampling_boundary":
            timing = (payload.get("timing_checkpoint") or {}).get("timing") or {}
            total = (timing.get("exclusive") or {}).get("modelOnlyNs")
            turn = payload.get("turn_id") or ""
            if isinstance(total, (int, float)):
                rise = total - turn_model_ns.get(turn, 0)
                turn_model_ns[turn] = int(total)
                if rise > 0:
                    pending_model_seconds += rise / 1e9
            continue

        # Two recorded shapes carry per-request usage. Newer rollouts write a
        # `token_usage_record`; older ones a `token_count` event whose
        # `last_token_usage` is the same per-request delta. Accept either, and
        # never both for one request: a rollout uses one shape throughout.
        usage = None
        shape = None
        if kind == "token_usage_record":
            usage, shape = payload.get("usage") or {}, "record"
        elif kind == "event_msg" and payload.get("type") == "token_count":
            info = payload.get("info") or {}
            if info.get("last_token_usage"):
                usage, shape = info["last_token_usage"], "event"

        # Keep whichever shape this rollout used first, so a rollout carrying
        # both never counts a request twice.
        if usage is not None and usage_shape is None:
            usage_shape = shape
        if usage is not None and shape != usage_shape:
            usage = None

        if usage is not None:
            generation = Generation(
                response_id=payload.get("response_id"),
                turn_id=payload.get("turn_id"),
                input_tokens=usage.get("input_tokens") or 0,
                cached_input_tokens=usage.get("cached_input_tokens") or 0,
                output_tokens=usage.get("output_tokens") or 0,
                reasoning_tokens=usage.get("reasoning_output_tokens") or 0,
                started=last_event,
                ended=stamp,
                model_seconds=pending_model_seconds,
                calls=pending_calls,
            )
            generations.append(generation)
            previous = generation
            pending_calls = []
            pending_model_seconds = 0.0
            last_event = stamp
            continue

        if kind == "response_item":
            item_type = payload.get("type")
            if item_type in ("custom_tool_call", "function_call"):
                pending_calls.append(_call_text(payload))
            elif item_type in ("custom_tool_call_output", "function_call_output"):
                if previous is not None:
                    previous.outputs.append(_output_text(payload))
        if stamp is not None:
            last_event = stamp

    return generations


def summarise(path: Path, generations: list[Generation]) -> dict[str, Any]:
    roles: dict[str, dict[str, float]] = defaultdict(
        lambda: {
            "requests": 0,
            "input": 0,
            "uncached_input": 0,
            "output": 0,
            "reasoning": 0,
            "seconds": 0.0,
        }
    )
    waste = Counter()
    waste_seconds: Counter[str] = Counter()
    launch_signatures: Counter[str] = Counter()

    for generation in generations:
        role = generation.role
        bucket = roles[role]
        bucket["requests"] += 1
        bucket["input"] += generation.input_tokens
        bucket["uncached_input"] += generation.uncached_input
        bucket["output"] += generation.output_tokens
        bucket["reasoning"] += generation.reasoning_tokens
        # Prefer the harness's own `modelOnlyNs`; fall back to the record gap
        # only where no boundary was recorded, bounding it so an idle user wait
        # between turns cannot be billed to a request.
        seconds = generation.model_seconds
        if seconds <= 0.0 and generation.started and generation.ended:
            seconds = min(
                max((generation.ended - generation.started).total_seconds(), 0.0),
                600.0,
            )
        bucket["seconds"] += seconds

        joined_output = "\n".join(generation.outputs)
        if role in ("launch", "status"):
            if BUSY_RE.search(joined_output):
                waste["admission_busy"] += 1
                waste_seconds["admission_busy"] += seconds
            if ZERO_TEST_RE.search(joined_output):
                waste["zero_tests"] += 1
                waste_seconds["zero_tests"] += seconds
        if role == "launch":
            for match in re.findall(
                r"(?:just\s+core-(?:test|gate)\S*|cargo\s+\w+|"
                r"root_maintenance\.py\s+test-python)[^\"'\n;]{0,120}",
                "\n".join(generation.calls),
            ):
                launch_signatures[" ".join(match.split())] += 1

    # Chronological candidates only: selection, prior success, external edits,
    # and input identity are not proven by tool-call text.
    unverified_relaunches = 0
    unchanged_seconds = 0.0
    repeat_failure_reads = 0
    repeat_failure_seconds = 0.0
    edited_since_launch = True
    saw_launch = False
    last_failure: str | None = None
    previous_launch = ""
    samples: list[dict[str, Any]] = []

    for generation in generations:
        role = generation.role
        text = "\n".join(generation.calls)
        seconds = generation.model_seconds
        if role == "launch":
            # Same-cell edits must exclude a candidate before classification.
            if (
                saw_launch
                and not edited_since_launch
                and not INSPECT_RE.search(text)
                and not EDIT_RE.search(text)
            ):
                unverified_relaunches += 1
                unchanged_seconds += seconds
                samples.append(
                    {
                        "rollout": path.name,
                        "seconds": round(seconds, 1),
                        "previous": previous_launch[:300],
                        "relaunch": text[:300],
                        "redundancy": "unverified",
                    }
                )
            saw_launch = True
            edited_since_launch = False
            previous_launch = text
            joined = "\n".join(generation.outputs)
            match = FAILURE_RE.search(joined)
            if match:
                # Compare the failing test names, so an identical failure set
                # re-analysed after no edit is visible as repeated analysis.
                names = tuple(sorted(set(re.findall(r"[\w:]+::\w+", joined)))[:20])
                signature = "|".join(names)
                if signature and signature == last_failure:
                    repeat_failure_reads += 1
                    repeat_failure_seconds += seconds
                last_failure = signature
            else:
                last_failure = None
        if EDIT_RE.search(text):
            edited_since_launch = True

    repeats = {sig: n for sig, n in launch_signatures.items() if n > 1}
    return {
        "rollout": path.name,
        "generations": len(generations),
        "roles": {role: dict(values) for role, values in roles.items()},
        "waste": dict(waste),
        "waste_seconds": {k: round(v, 1) for k, v in waste_seconds.items()},
        "repeated_launches": repeats,
        "repeated_launch_extra": sum(n - 1 for n in repeats.values()),
        "unverified_relaunches": unverified_relaunches,
        "unverified_relaunch_seconds": round(unchanged_seconds, 1),
        "repeat_failure_reads": repeat_failure_reads,
        "repeat_failure_seconds": round(repeat_failure_seconds, 1),
        "relaunch_samples": samples,
    }


def aggregate(reports: Sequence[dict[str, Any]]) -> dict[str, Any]:
    roles: dict[str, dict[str, float]] = defaultdict(
        lambda: {
            "requests": 0,
            "input": 0,
            "uncached_input": 0,
            "output": 0,
            "reasoning": 0,
            "seconds": 0.0,
        }
    )
    waste: Counter[str] = Counter()
    waste_seconds: Counter[str] = Counter()
    repeated_extra = 0
    repeated_top: Counter[str] = Counter()
    totals: Counter[str] = Counter()

    for report in reports:
        for role, values in report["roles"].items():
            for key, value in values.items():
                roles[role][key] += value
        waste.update(report["waste"])
        for key, value in report["waste_seconds"].items():
            waste_seconds[key] += value
        repeated_extra += report["repeated_launch_extra"]
        repeated_top.update(report["repeated_launches"])
        for key in (
            "unverified_relaunches",
            "unverified_relaunch_seconds",
            "repeat_failure_reads",
            "repeat_failure_seconds",
        ):
            totals[key] += report[key]

    return {
        "rollouts": len(reports),
        "generations": sum(r["generations"] for r in reports),
        "roles": {
            role: {
                k: (round(v, 1) if k == "seconds" else int(v))
                for k, v in values.items()
            }
            for role, values in roles.items()
        },
        "waste": dict(waste),
        "waste_seconds": {k: round(v, 1) for k, v in waste_seconds.items()},
        "repeated_launch_extra": repeated_extra,
        "repeated_launch_top": repeated_top.most_common(15),
        "unverified_relaunches": int(totals["unverified_relaunches"]),
        "unverified_relaunch_seconds": round(totals["unverified_relaunch_seconds"], 1),
        "repeat_failure_reads": int(totals["repeat_failure_reads"]),
        "repeat_failure_seconds": round(totals["repeat_failure_seconds"], 1),
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="Directory of rollout JSONL files")
    parser.add_argument("--json", type=Path, help="Write the full report here")
    parser.add_argument("--limit", type=int, default=0, help="Cap rollouts examined")
    args = parser.parse_args(argv)

    paths = discover_rollouts(args.root)
    if args.limit:
        paths = paths[: args.limit]
    if not paths:
        print(f"no rollouts under {args.root}", file=sys.stderr)
        return 1

    reports = []
    failures = []
    for path in paths:
        try:
            reports.append(analyse(path))
        except Exception as error:  # noqa: BLE001 - one bad rollout must not stop the audit
            failures.append({"path": str(path), "error": str(error)})
            print(f"warning: {path.name}: {error}", file=sys.stderr)

    summary = aggregate(reports)
    summary['coverage'] = {'selected': len(paths), 'succeeded': len(reports),
                           'failed': len(failures), 'failures': failures,
                           'complete': not failures}
    validation_roles = ("plan", "launch", "status")
    total_requests = sum(v["requests"] for v in summary["roles"].values())
    validation_requests = sum(
        summary["roles"].get(role, {}).get("requests", 0) for role in validation_roles
    )
    validation_seconds = sum(
        summary["roles"].get(role, {}).get("seconds", 0.0) for role in validation_roles
    )
    total_seconds = sum(v["seconds"] for v in summary["roles"].values())
    summary["validation_share"] = {
        "requests": validation_requests,
        "request_pct": round(100 * validation_requests / max(total_requests, 1), 1),
        "seconds": round(validation_seconds, 1),
        "second_pct": round(100 * validation_seconds / max(total_seconds, 1), 1),
        "reasoning": sum(
            summary["roles"].get(role, {}).get("reasoning", 0)
            for role in validation_roles
        ),
    }

    print(json.dumps(summary, indent=2))
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(summary, indent=2), encoding="utf-8")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
