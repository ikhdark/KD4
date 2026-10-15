#!/usr/bin/env python3
"""Attribute model-visible tool-output tokens in recorded KD4 rollouts.

`context_window_composition.py` establishes that tool outputs dominate the
model-visible context. This script asks where inside those outputs the tokens
go, so a reduction can be aimed at a measured source instead of a guess.

Scope: `response_item` records of type `function_call_output` /
`custom_tool_call_output` only. Rollout-only records are excluded.

Reported per tool (the tool name is recovered by joining each output to its
originating call via `call_id`):

* `outputs` / `tokens`  - occurrences and harness estimate ceil(bytes / 4).
* `dup_tokens`          - tokens in outputs whose exact text was already
  present in an earlier output of the same session. Exact repeats only.
* `near_dup_tokens`     - tokens in outputs whose text is an exact prefix or
  suffix superset/subset of an earlier output's text, which is what a re-read
  of a growing file or a re-run of a paged command produces. Reported
  separately from exact repeats because collapsing them is lossy.
* `header_tokens`       - tokens in the fixed per-output framing lines the
  harness adds (metadata lines matched structurally, not by wording).
* `truncation_markers`  - outputs that already carry a truncation notice, i.e.
  outputs the harness bounded. Counted so that bounded and unbounded outputs
  are not conflated.

No output is rewritten or summarized by this script; it only measures.

Usage:
  context_tool_output_audit.py SESSION.jsonl [...] [--json] [--top N]
  context_tool_output_audit.py --latest N [--json] [--top N]
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records
from collections import defaultdict

SESSION_ROOT = pathlib.Path(r"C:\Users\kuh\Desktop\LOCAL-KD\sessions")

OUTPUT_TYPES = {"function_call_output", "custom_tool_call_output"}
CALL_TYPES = {"function_call", "custom_tool_call", "local_shell_call"}

# Framing the harness prepends to a tool result. Matched as whole lines at the
# start of an output so ordinary content that merely mentions these words is
# not counted as framing.
HEADER_LINE = re.compile(
    r"^(?:\[?(?:Tool|Command|Exit code|Wall time|Duration|Output|stdout|stderr|"
    r"Total (?:lines|bytes)|Truncated|Showing|Omitted|Note)\b[^\n]*)$",
    re.IGNORECASE,
)
TRUNCATION_HINT = re.compile(
    r"truncat|omitted \d|output limit|read_tool_output|\[\.\.\.\]", re.IGNORECASE
)


def est_tokens(text: str) -> int:
    return math.ceil(len(text.encode("utf-8", "replace")) / 4)


def output_text(payload: dict) -> str:
    value = payload.get("output")
    parts: list[str] = []
    if isinstance(value, str):
        parts.append(value)
    elif isinstance(value, dict):
        for field in ("content", "text"):
            inner = value.get(field)
            if isinstance(inner, str):
                parts.append(inner)
            elif isinstance(inner, list):
                for entry in inner:
                    if isinstance(entry, dict):
                        for sub in ("text", "input_text", "output_text"):
                            if isinstance(entry.get(sub), str):
                                parts.append(entry[sub])
                    elif isinstance(entry, str):
                        parts.append(entry)
    elif isinstance(value, list):
        for entry in value:
            if isinstance(entry, dict):
                for sub in ("text", "input_text", "output_text"):
                    if isinstance(entry.get(sub), str):
                        parts.append(entry[sub])
            elif isinstance(entry, str):
                parts.append(entry)
    return "\n".join(parts)


def header_tokens(text: str) -> int:
    total = 0
    for line in text.split("\n")[:6]:
        if HEADER_LINE.match(line.strip()):
            total += est_tokens(line)
        elif line.strip():
            break
    return total


def audit(path: pathlib.Path, near_dup_min_tokens: int) -> dict:
    call_names: dict[str, str] = {}
    per_tool: dict[str, dict[str, int]] = defaultdict(
        lambda: {
            "outputs": 0,
            "tokens": 0,
            "dup_tokens": 0,
            "near_dup_tokens": 0,
            "header_tokens": 0,
            "truncation_markers": 0,
            "max_tokens": 0,
        }
    )
    # Two passes: calls can appear after outputs only in malformed logs, but a
    # first pass over call records keeps the join robust either way.
    records: list[tuple[str, dict]] = []
    malformed = 0
    # Strict verified hydration: unresolved coverage aborts rather than reporting zero.
    for record, wire_bytes in iter_rollout_records(path):
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        if not isinstance(payload, dict):
            continue
        kind = payload.get("type") or ""
        if kind in CALL_TYPES:
            call_id = payload.get("call_id")
            name = payload.get("name")
            if isinstance(call_id, str) and isinstance(name, str):
                call_names[call_id] = name
        elif kind in OUTPUT_TYPES:
            records.append((kind, payload))

    seen_exact: dict[str, int] = {}
    seen_texts: list[str] = []
    unattributed = 0
    for _kind, payload in records:
        text = output_text(payload)
        if not text:
            continue
        call_id = payload.get("call_id")
        name = call_names.get(call_id) if isinstance(call_id, str) else None
        if name is None:
            name = payload.get("name") if isinstance(payload.get("name"), str) else None
        if name is None:
            name = "<unattributed>"
            unattributed += 1
        tokens = est_tokens(text)
        stats = per_tool[name]
        stats["outputs"] += 1
        stats["tokens"] += tokens
        stats["max_tokens"] = max(stats["max_tokens"], tokens)
        stats["header_tokens"] += header_tokens(text)
        if TRUNCATION_HINT.search(text[-400:]) or TRUNCATION_HINT.search(text[:400]):
            stats["truncation_markers"] += 1

        if text in seen_exact:
            stats["dup_tokens"] += tokens
        else:
            seen_exact[text] = 1
            if tokens >= near_dup_min_tokens:
                # Containment against previously seen large bodies only; this is
                # O(n*m) on purpose and bounded by the token floor.
                for earlier in seen_texts:
                    if text in earlier or earlier in text:
                        stats["near_dup_tokens"] += min(tokens, est_tokens(earlier))
                        break
                seen_texts.append(text)

    return {
        "session": path.name,
        "malformed_records": malformed,
        "unattributed_outputs": unattributed,
        "tools": {
            name: dict(stats)
            for name, stats in sorted(per_tool.items(), key=lambda kv: -kv[1]["tokens"])
        },
        "total_output_tokens": sum(s["tokens"] for s in per_tool.values()),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path)
    parser.add_argument("--latest", type=int)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--top", type=int, default=14)
    parser.add_argument(
        "--near-dup-min-tokens",
        type=int,
        default=200,
        help="ignore containment matches below this size (default 200)",
    )
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

    reports = [audit(path, args.near_dup_min_tokens) for path in paths]
    if args.json:
        print(json.dumps({"sessions": reports}, indent=2))
        return 0

    totals: dict[str, dict[str, int]] = defaultdict(
        lambda: {
            "outputs": 0,
            "tokens": 0,
            "dup_tokens": 0,
            "near_dup_tokens": 0,
            "header_tokens": 0,
            "truncation_markers": 0,
            "max_tokens": 0,
        }
    )
    for report in reports:
        for name, stats in report["tools"].items():
            bucket = totals[name]
            for key in bucket:
                if key == "max_tokens":
                    bucket[key] = max(bucket[key], stats[key])
                else:
                    bucket[key] += stats[key]

    grand = sum(s["tokens"] for s in totals.values()) or 1
    print(f"sessions: {len(reports)}")
    print(f"model-visible tool-output est tokens: {grand:,}\n")
    print(
        f"{'tool':<30}{'outs':>7}{'tokens':>12}{'share':>8}"
        f"{'exact-dup':>11}{'contained':>11}{'framing':>9}{'trunc':>7}{'max':>9}"
    )
    for name, stats in sorted(totals.items(), key=lambda kv: -kv[1]["tokens"])[
        : args.top
    ]:
        print(
            f"{name[:29]:<30}{stats['outputs']:>7,}{stats['tokens']:>12,}"
            f"{100 * stats['tokens'] / grand:>7.1f}%"
            f"{stats['dup_tokens']:>11,}{stats['near_dup_tokens']:>11,}"
            f"{stats['header_tokens']:>9,}{stats['truncation_markers']:>7,}"
            f"{stats['max_tokens']:>9,}"
        )
    tot_dup = sum(s["dup_tokens"] for s in totals.values())
    tot_near = sum(s["near_dup_tokens"] for s in totals.values())
    tot_head = sum(s["header_tokens"] for s in totals.values())
    print(
        f"\nexact-duplicate output tokens: {tot_dup:,} ({100*tot_dup/grand:.2f}%)"
        f"\ncontained (prefix/suffix) tokens: {tot_near:,} ({100*tot_near/grand:.2f}%)"
        f"  [collapsing these is lossy]"
        f"\nfixed framing tokens: {tot_head:,} ({100*tot_head/grand:.2f}%)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
