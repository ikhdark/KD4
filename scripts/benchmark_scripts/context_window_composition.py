#!/usr/bin/env python3
"""Measure model-visible context composition in recorded KD4 rollouts.

Separates model-visible `response_item` records (what actually enters the
request body) from rollout-only bookkeeping (`tool_manifest`,
`event_msg/*`), which is stored but never sent. External payload references
are authenticated and classified by the hydrated record type.

Token counts are the harness's own estimate, ceil(UTF-8 bytes / 4), applied to
the model-visible projection of each item. They are not provider billing. Real
reported usage is read from `event_msg/token_count` where present and shown
alongside, so the estimate can be checked against it rather than trusted.

Usage:
  context_window_composition.py SESSION.jsonl [...] [--json]
  context_window_composition.py --latest N [--json]
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records
from collections import defaultdict

SESSION_ROOT = pathlib.Path(r"C:\Users\kuh\Desktop\LOCAL-KD\sessions")

# Records stored in the rollout that never reach the model context.
ROLLOUT_ONLY_TYPES = {
    "tool_manifest",
    "rollout_payload_artifact",
    "event_msg",
    "inter_agent_communication_metadata",
    "sampling_boundary",
    "turn_context",
    "world_state",
    "compacted",
}


def est_tokens(text: str) -> int:
    return math.ceil(len(text.encode("utf-8", "replace")) / 4)


def reasoning_tokens(payload: dict) -> int:
    """Harness estimate for a reasoning item.

    Encrypted reasoning is opaque provider state, not prose. The harness models
    its decoded cost as `len*3/4 - 650` (see
    `context_manager::history::estimate_reasoning_length`), so mirror that here
    instead of charging the base64 length.
    """
    total = 0
    encrypted = payload.get("encrypted_content")
    if isinstance(encrypted, str):
        total += max(0, len(encrypted) * 3 // 4 - 650) // 4
    for entry in payload.get("summary") or []:
        text = entry.get("text") if isinstance(entry, dict) else entry
        if isinstance(text, str):
            total += est_tokens(text)
    return total


def item_text(payload: dict) -> str:
    """Model-visible text of a response_item, mirroring harness projection."""
    parts: list[str] = []
    for key in ("content", "input", "output"):
        value = payload.get(key)
        if isinstance(value, str):
            parts.append(value)
        elif isinstance(value, list):
            for entry in value:
                if isinstance(entry, str):
                    parts.append(entry)
                elif isinstance(entry, dict):
                    for field in ("text", "input_text", "output_text", "image_url"):
                        if isinstance(entry.get(field), str):
                            parts.append(entry[field])
        elif isinstance(value, dict):
            for field in ("content", "text"):
                if isinstance(value.get(field), str):
                    parts.append(value[field])
    for key in ("arguments", "name", "summary_text"):
        value = payload.get(key)
        if isinstance(value, str):
            parts.append(value)
    return "\n".join(parts)


def classify(payload: dict) -> str:
    kind = payload.get("type") or "unknown"
    if kind in ("custom_tool_call_output", "function_call_output"):
        return "tool_output"
    if kind in ("custom_tool_call", "function_call", "local_shell_call"):
        return "tool_call"
    if kind == "reasoning":
        return "reasoning"
    if kind in ("message", "agent_message"):
        role = payload.get("role") or "assistant"
        return f"message:{role}"
    return kind


def audit(path: pathlib.Path) -> dict:
    visible: dict[str, dict[str, int]] = defaultdict(lambda: {"count": 0, "tokens": 0})
    rollout_only: dict[str, dict[str, int]] = defaultdict(
        lambda: {"count": 0, "bytes": 0}
    )
    # Duplicate detection over exact model-visible tool-output text.
    output_text_seen: dict[str, int] = defaultdict(int)
    output_text_tokens: dict[str, int] = {}
    token_counts: list[dict] = []
    malformed = 0

    # Strict verified hydration: unresolved coverage aborts rather than reporting zero.
    for record, wire_bytes in iter_rollout_records(path):
        rtype = record.get("type")
        payload = record.get("payload")
        if not isinstance(payload, dict):
            payload = {}

        if rtype == "response_item":
            bucket = classify(payload)
            if bucket == "reasoning":
                visible[bucket]["count"] += 1
                visible[bucket]["tokens"] += reasoning_tokens(payload)
                continue
            text = item_text(payload)
            tokens = est_tokens(text)
            visible[bucket]["count"] += 1
            visible[bucket]["tokens"] += tokens
            if bucket == "tool_output" and text:
                output_text_seen[text] += 1
                output_text_tokens[text] = tokens
            continue

        if rtype == "event_msg" and payload.get("type") == "token_count":
            info = payload.get("info") or {}
            last = info.get("last_token_usage") or {}
            if last:
                token_counts.append(
                    {
                        "input": last.get("input_tokens"),
                        "cached": last.get("cached_input_tokens"),
                        "output": last.get("output_tokens"),
                    }
                )

        key = rtype if rtype in ROLLOUT_ONLY_TYPES else f"other:{rtype}"
        rollout_only[key]["count"] += 1
        rollout_only[key]["bytes"] += wire_bytes

    dup_tokens = sum(
        output_text_tokens[text] * (seen - 1)
        for text, seen in output_text_seen.items()
        if seen > 1
    )
    dup_groups = sum(1 for seen in output_text_seen.values() if seen > 1)

    visible_total = sum(v["tokens"] for v in visible.values())
    peak_input = max((t["input"] or 0) for t in token_counts) if token_counts else None
    peak_cached = (
        max((t["cached"] or 0) for t in token_counts) if token_counts else None
    )

    return {
        "session": path.name,
        "malformed_records": malformed,
        "model_visible": {
            k: dict(v)
            for k, v in sorted(
                visible.items(), key=lambda kv: -kv[1]["tokens"]
            )
        },
        "model_visible_total_tokens": visible_total,
        "rollout_only": {
            k: dict(v)
            for k, v in sorted(
                rollout_only.items(), key=lambda kv: -kv[1]["bytes"]
            )
        },
        "rollout_only_total_bytes": sum(v["bytes"] for v in rollout_only.values()),
        "duplicate_tool_output_groups": dup_groups,
        "duplicate_tool_output_tokens": dup_tokens,
        "reported_requests": len(token_counts),
        "reported_peak_input_tokens": peak_input,
        "reported_peak_cached_tokens": peak_cached,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path)
    parser.add_argument("--latest", type=int)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    paths = list(args.paths)
    if args.latest:
        if not SESSION_ROOT.exists():
            print(f"session root not found: {SESSION_ROOT}", file=sys.stderr)
            return 2
        paths = sorted(
            discover_rollouts(SESSION_ROOT),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )[: args.latest]
    if not paths:
        parser.error("no sessions selected")

    missing = [p for p in paths if not p.is_file()]
    if missing:
        for path in missing:
            print(f"missing: {path}", file=sys.stderr)
        return 2

    reports = [audit(path) for path in paths]
    if args.json:
        print(json.dumps({"sessions": reports}, indent=2))
        return 0

    tot_visible: dict[str, int] = defaultdict(int)
    tot_dup = 0
    tot_rollout = 0
    for report in reports:
        for bucket, stats in report["model_visible"].items():
            tot_visible[bucket] += stats["tokens"]
        tot_dup += report["duplicate_tool_output_tokens"]
        tot_rollout += report["rollout_only_total_bytes"]
        print(f"\n== {report['session']}")
        print(
            f"   model-visible est {report['model_visible_total_tokens']:,} tok"
            f" | reported peak input {report['reported_peak_input_tokens']:,}"
            if report["reported_peak_input_tokens"]
            else f"   model-visible est {report['model_visible_total_tokens']:,} tok"
        )
        for bucket, stats in list(report["model_visible"].items())[:6]:
            print(
                f"     {bucket:<22} {stats['tokens']:>9,} tok  "
                f"({stats['count']} items)"
            )
        if report["duplicate_tool_output_groups"]:
            print(
                f"     duplicate tool outputs: {report['duplicate_tool_output_groups']}"
                f" groups, {report['duplicate_tool_output_tokens']:,} redundant tok"
            )

    total = sum(tot_visible.values()) or 1
    print("\n== TOTAL model-visible composition")
    for bucket, tokens in sorted(tot_visible.items(), key=lambda kv: -kv[1]):
        print(f"   {bucket:<22} {tokens:>10,} tok  {100 * tokens / total:5.1f}%")
    print(f"   {'TOTAL':<22} {total:>10,} tok")
    print(f"\n   exact-duplicate tool-output tokens: {tot_dup:,} ({100*tot_dup/total:.1f}%)")
    print(f"   rollout-only stored bytes (never sent): {tot_rollout:,}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
