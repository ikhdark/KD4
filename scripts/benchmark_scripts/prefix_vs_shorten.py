#!/usr/bin/env python3
"""Compare preserving a cached prefix against shortening it.

Codex compacts when the context fills, which discards history and forces the
next request to re-read a cold prefix. This measures, from real usage metadata,
the weighted recorded usage after compaction. A no-compaction counterfactual
cannot be recovered from aggregate usage: new-token volume and cache reuse
without compaction are unknown.

Effective usage weights cached reads at 0.1x uncached input, matching the
standard prompt-cache discount, so "shorter prompt" and "warm prompt" can be
compared on one axis. The discount is a parameter: pass --cache-discount to test
sensitivity rather than trusting a single assumed rate.
"""

from __future__ import annotations

import argparse
import json
import pathlib


def sessions(root: pathlib.Path):
    for path in sorted(root.rglob("rollout-*.jsonl")):
        reqs = []
        compaction_before = set()
        pending = False
        try:
            fh = path.open("r", encoding="utf-8", errors="replace")
        except OSError:
            continue
        with fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError:
                    continue
                rtype = obj.get("type")
                payload = obj.get("payload") if isinstance(obj.get("payload"), dict) else {}
                ptype = payload.get("type")
                if rtype == "compacted" or ptype in ("compacted", "context_compacted"):
                    pending = True
                    continue
                if ptype != "token_count":
                    continue
                last = (payload.get("info") or {}).get("last_token_usage") or {}
                li = int(last.get("input_tokens") or 0)
                if li <= 0:
                    continue
                if pending:
                    compaction_before.add(len(reqs))
                    pending = False
                reqs.append((li, int(last.get("cached_input_tokens") or 0)))
        if reqs:
            yield path, reqs, compaction_before


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    ap.add_argument("--cache-discount", type=float, default=0.1,
                    help="cost of a cached token relative to an uncached one")
    args = ap.parse_args()
    d = args.cache_discount

    n_comp = 0
    # Cost actually paid on the first request after compaction.
    paid_uncached = 0
    paid_cached = 0

    for _path, reqs, comp in sessions(args.root):
        for i in sorted(comp):
            if i == 0 or i >= len(reqs):
                continue
            prev_in, _prev_cached = reqs[i - 1]
            cur_in, cur_cached = reqs[i]
            n_comp += 1
            paid_uncached += cur_in - cur_cached
            paid_cached += cur_cached

    paid_eff = paid_uncached + d * paid_cached

    print(f"compaction boundaries measured : {n_comp}")
    print(f"cache discount assumed         : {d:g}x")
    print()
    print("cost actually paid (compacted / shortened prefix):")
    print(f"  uncached input               : {paid_uncached:,}")
    print(f"  cached reads                 : {paid_cached:,}")
    print(f"  effective                    : {paid_eff:,.0f}")
    print()
    print("counterfactual preserved-prefix usage: unknown")
    print("New uncached tokens cannot be separated from cache misses after compaction.")
    print("No lower-cost strategy is established by these observations.")
    print()
    print("Note: compaction also buys context-window headroom. Where a thread would")
    print("otherwise exceed the window, compaction is required for correctness and")
    print("this comparison does not apply.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
