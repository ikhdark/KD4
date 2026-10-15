"""Classify rollout `token_count` events against their cumulative totals.

Read-only. For each rollout, every `event_msg/token_count` with `info` is
classified by whether `total_token_usage` advanced and by how much compared to
`last_token_usage`:

* advanced_by_last   - total advanced by exactly last (normal provider usage)
* repeated           - total unchanged and last identical to the previous event's last
* synthetic          - total unchanged and last has zero input/output (local estimate reset)
* stale_last         - total unchanged, last nonzero and different from previous last
* advanced_other     - total advanced by something other than last

Summing `last_token_usage` across `repeated`/`stale_last` events double-counts
provider usage; this script reports how many such events the harness emitted.

Usage:
    python -B scripts/benchmark_scripts/token_count_event_audit.py --sessions DIR [--top N]
"""

from __future__ import annotations

import argparse
import json
from collections import Counter
from pathlib import Path

FIELDS = ("input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens")


def usage_tuple(usage) -> tuple[int, ...]:
    usage = usage or {}
    return tuple(int(usage.get(field) or 0) for field in FIELDS)


def classify(path: Path):
    counts: Counter[str] = Counter()
    duplicate_input = 0
    duplicate_reasoning = 0
    previous_total = None
    previous_last = None
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            if '"token_count"' not in line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            payload = record.get("payload") or {}
            if record.get("type") != "event_msg" or payload.get("type") != "token_count":
                continue
            info = payload.get("info")
            if not info:
                counts["no_info"] += 1
                continue
            total = usage_tuple(info.get("total_token_usage"))
            last = usage_tuple(info.get("last_token_usage"))
            if previous_total is None:
                kind = "first"
            else:
                delta = tuple(t - p for t, p in zip(total, previous_total))
                if any(delta):
                    kind = "advanced_by_last" if delta == last else "advanced_other"
                elif last[0] == 0 and last[2] == 0:
                    kind = "synthetic"
                elif last == previous_last:
                    kind = "repeated"
                    duplicate_input += last[0]
                    duplicate_reasoning += last[3]
                else:
                    kind = "stale_last"
                    duplicate_input += last[0]
                    duplicate_reasoning += last[3]
            counts[kind] += 1
            previous_total = total
            previous_last = last
    return counts, duplicate_input, duplicate_reasoning, previous_total


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sessions", required=True, type=Path)
    parser.add_argument("--top", type=int, default=8)
    args = parser.parse_args()
    corpus: Counter[str] = Counter()
    dup_input = dup_reasoning = final_input = 0
    per_session = []
    for path in sorted(args.sessions.rglob("*.jsonl")):
        counts, d_in, d_reason, final_total = classify(path)
        corpus.update(counts)
        dup_input += d_in
        dup_reasoning += d_reason
        final_input += (final_total or (0,))[0]
        per_session.append((d_in, path.name[-42:-6], dict(counts)))
    print(json.dumps({
        "event_kinds": dict(corpus),
        "double_counted_last_input_if_summed": dup_input,
        "double_counted_last_reasoning_if_summed": dup_reasoning,
        "sum_of_final_total_input": final_input,
    }, indent=1))
    for d_in, name, counts in sorted(per_session, reverse=True)[: args.top]:
        print(f"{name} dup_input={d_in} {counts}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
