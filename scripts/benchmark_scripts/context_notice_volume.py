#!/usr/bin/env python3
"""Measure model-visible developer-notice volume in recorded KD4 rollouts.

Scope: `response_item` records only, which is what actually enters a request
body. `event_msg/*` and `tool_manifest` records are rollout storage and are not
counted. External references are authenticated before classifying their records.

For each recognized notice family this reports:

* `items`   - how many model-visible items carry the notice.
* `tokens`  - harness estimate ceil(UTF-8 bytes / 4) over the whole item text.
* `preamble_tokens` - tokens in the fixed instructional prose that is byte-wise
  identical across every occurrence of that family.
* `payload_tokens`  - the remainder, i.e. the per-occurrence varying records.

`preamble_tokens` beyond the first occurrence is the measured upper bound on
what deduplicating the prose could remove. It is an upper bound, not a saving:
whether those bytes are billed depends on prompt-cache reuse, which this script
does not claim. Cached-vs-uncached reporting comes from `event_msg/token_count`
and is printed separately so the two are not conflated.

Usage:
  context_notice_volume.py SESSION.jsonl [...] [--json]
  context_notice_volume.py --latest N [--json]
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

# Notice families keyed by the exact model-visible opening marker. The preamble
# is the fixed prose between the marker and the first varying record; it is
# located structurally (first line break after the marker) rather than by
# hard-coding the wording, so prompt edits do not silently break the measure.
NOTICE_MARKERS = (
    "<workspace_evidence_invalidation>",
    "<tool_search_sources>",
    "<task_state>",
    "<user_instructions>",
    "<environment_context>",
    "<phase_checkpoint>",
)


def est_tokens(text: str) -> int:
    return math.ceil(len(text.encode("utf-8", "replace")) / 4)


def item_texts(payload: dict) -> list[str]:
    """Model-visible text blocks of a response_item."""
    out: list[str] = []
    for key in ("content", "input", "output"):
        value = payload.get(key)
        if isinstance(value, str):
            out.append(value)
        elif isinstance(value, list):
            for entry in value:
                if isinstance(entry, str):
                    out.append(entry)
                elif isinstance(entry, dict):
                    for field in ("text", "input_text", "output_text"):
                        if isinstance(entry.get(field), str):
                            out.append(entry[field])
        elif isinstance(value, dict):
            for field in ("content", "text"):
                if isinstance(value.get(field), str):
                    out.append(value[field])
    return out


def split_notice(text: str) -> tuple[str, str, str] | None:
    """Return (marker, preamble, payload) when `text` opens a known notice.

    The preamble is the fixed prose line that follows the opening marker. The
    payload is everything after it, including the closing marker, because that
    is the part that varies per occurrence.
    """
    stripped = text.lstrip()
    for marker in NOTICE_MARKERS:
        if not stripped.startswith(marker):
            continue
        rest = stripped[len(marker) :].lstrip("\n")
        head, sep, tail = rest.partition("\n")
        if not sep:
            # Single-line notice: all of it is payload, no separable prose.
            return marker, "", rest
        return marker, head, tail
    return None


def audit(path: pathlib.Path) -> dict:
    families: dict[str, dict] = defaultdict(
        lambda: {
            "items": 0,
            "tokens": 0,
            "preamble_tokens": 0,
            "payload_tokens": 0,
            "distinct_preambles": set(),
            "distinct_payloads": set(),
            "repeated_payload_tokens": 0,
        }
    )
    visible_tokens = 0
    reported = []
    malformed = 0

    # Strict verified hydration: unresolved coverage aborts rather than reporting zero.
    for record, wire_bytes in iter_rollout_records(path):
        payload = record.get("payload")
        if not isinstance(payload, dict):
            payload = {}
        rtype = record.get("type")

        if rtype == "event_msg" and payload.get("type") == "token_count":
            last = (payload.get("info") or {}).get("last_token_usage") or {}
            if last:
                reported.append(
                    {
                        "input": last.get("input_tokens") or 0,
                        "cached": last.get("cached_input_tokens") or 0,
                    }
                )
            continue

        if rtype != "response_item":
            continue

        for text in item_texts(payload):
            visible_tokens += est_tokens(text)
            split = split_notice(text)
            if split is None:
                continue
            marker, preamble, body = split
            family = families[marker]
            family["items"] += 1
            family["tokens"] += est_tokens(text)
            family["preamble_tokens"] += est_tokens(preamble)
            family["payload_tokens"] += est_tokens(body)
            family["distinct_preambles"].add(preamble)
            if body in family["distinct_payloads"]:
                family["repeated_payload_tokens"] += est_tokens(body)
            else:
                family["distinct_payloads"].add(body)

    result_families = {}
    for marker, stats in sorted(families.items(), key=lambda kv: -kv[1]["tokens"]):
        distinct_preambles = len(stats["distinct_preambles"])
        # Only the occurrences beyond the first copy of each distinct preamble
        # are candidates for removal.
        per_preamble = (
            sum(est_tokens(p) for p in stats["distinct_preambles"])
            if distinct_preambles
            else 0
        )
        result_families[marker] = {
            "items": stats["items"],
            "tokens": stats["tokens"],
            "preamble_tokens": stats["preamble_tokens"],
            "payload_tokens": stats["payload_tokens"],
            "distinct_preambles": distinct_preambles,
            "redundant_preamble_tokens": max(
                0, stats["preamble_tokens"] - per_preamble
            ),
            "distinct_payloads": len(stats["distinct_payloads"]),
            "repeated_payload_tokens": stats["repeated_payload_tokens"],
        }

    peak_input = max((r["input"] for r in reported), default=None)
    peak_cached = max((r["cached"] for r in reported), default=None)
    uncached = sum(max(0, r["input"] - r["cached"]) for r in reported) or None

    return {
        "session": path.name,
        "malformed_records": malformed,
        "model_visible_tokens": visible_tokens,
        "families": result_families,
        "reported_requests": len(reported),
        "reported_peak_input_tokens": peak_input,
        "reported_peak_cached_tokens": peak_cached,
        "reported_uncached_input_tokens": uncached,
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

    totals: dict[str, dict[str, int]] = defaultdict(
        lambda: {
            "items": 0,
            "tokens": 0,
            "preamble_tokens": 0,
            "redundant_preamble_tokens": 0,
            "repeated_payload_tokens": 0,
            "payload_tokens": 0,
        }
    )
    visible = 0
    sessions_with_notices = 0
    for report in reports:
        visible += report["model_visible_tokens"]
        if report["families"]:
            sessions_with_notices += 1
        for marker, stats in report["families"].items():
            bucket = totals[marker]
            for key in bucket:
                bucket[key] += stats[key]

    print(f"sessions: {len(reports)} ({sessions_with_notices} carry notices)")
    print(f"model-visible est tokens: {visible:,}")
    print(
        f"\n{'notice family':<36}{'items':>8}{'tokens':>11}"
        f"{'prose':>10}{'prose-dup':>11}{'payload-dup':>12}"
    )
    grand_dup = 0
    for marker, stats in sorted(totals.items(), key=lambda kv: -kv[1]["tokens"]):
        grand_dup += stats["redundant_preamble_tokens"] + stats["repeated_payload_tokens"]
        print(
            f"{marker:<36}{stats['items']:>8,}{stats['tokens']:>11,}"
            f"{stats['preamble_tokens']:>10,}"
            f"{stats['redundant_preamble_tokens']:>11,}"
            f"{stats['repeated_payload_tokens']:>12,}"
        )
    share = (100 * grand_dup / visible) if visible else 0.0
    print(
        f"\nredundant notice tokens (upper bound): {grand_dup:,}"
        f"  = {share:.2f}% of model-visible"
    )
    print(
        "Upper bound only: these bytes sit in the stable prefix and are mostly\n"
        "cached. Cached-vs-uncached is reported per session, not assumed here."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
