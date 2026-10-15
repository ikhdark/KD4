#!/usr/bin/env python3
"""Describe usage associated with tool-surface hash oscillations.

The heuristic min(uncached_after, prev_cached) is an estimate, not a verified
recoverable floor. New history, eviction, routing and request-prefix identity
are not measured here; causal savings and post-fix hit rates remain unknown.
"""

from __future__ import annotations

import argparse
import json
import pathlib

MARKERS = ("compacted", "context_compacted")


def scan(path: pathlib.Path):
    """Walk a rollout, pairing manifest-hash transitions with the next request."""
    hashes: list[str] = []
    seen: set[str] = set()
    prev_hash = None
    # Request usage, in file order, tagged with whether an oscillation preceded it.
    pending_oscillation = False
    pending_compaction = False
    prev_usage = None
    events = []

    try:
        fh = path.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return events
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

            if rtype == "tool_manifest":
                h = payload.get("hash")
                if isinstance(h, str):
                    if prev_hash is not None and h != prev_hash and h in seen:
                        pending_oscillation = True
                    seen.add(h)
                    prev_hash = h
                    hashes.append(h)
                continue
            if rtype in MARKERS or ptype in MARKERS:
                pending_compaction = True
                continue
            if ptype != "token_count":
                continue
            info = payload.get("info") or {}
            last = info.get("last_token_usage") or {}
            li = int(last.get("input_tokens") or 0)
            if li <= 0:
                continue
            lc = int(last.get("cached_input_tokens") or 0)
            if pending_oscillation and not pending_compaction and prev_usage is not None:
                uncached_after = li - lc
                prev_cached = prev_usage[1]
                events.append({
                    "session": path.name,
                    "uncached_after": uncached_after,
                    "heuristic_overlap": max(0, min(uncached_after, prev_cached)),
                })
            prev_usage = (li, lc)
            pending_oscillation = False
            pending_compaction = False
    return events


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    args = ap.parse_args()
    files = sorted(args.root.rglob("rollout-*.jsonl"),
                   key=lambda p: p.stat().st_mtime, reverse=True)
    ev = []
    for f in files:
        ev.extend(scan(f))

    # Session-wide totals for context.
    total_input = total_cached = 0
    for f in files:
        try:
            fh = f.open("r", encoding="utf-8", errors="replace")
        except OSError:
            continue
        with fh:
            for line in fh:
                if '"token_count"' not in line:
                    continue
                try:
                    o = json.loads(line)
                except json.JSONDecodeError:
                    continue
                p = o.get("payload") or {}
                if p.get("type") != "token_count":
                    continue
                last = (p.get("info") or {}).get("last_token_usage") or {}
                li = int(last.get("input_tokens") or 0)
                if li <= 0:
                    continue
                total_input += li
                total_cached += int(last.get("cached_input_tokens") or 0)

    rec = sum(e["heuristic_overlap"] for e in ev)
    uncached = total_input - total_cached
    print(f"files                          : {len(files)}")
    print(f"oscillation-followed requests  : {len(ev)}")
    print(f"uncached input on those reqs   : {sum(e['uncached_after'] for e in ev):,}")
    print(f"heuristic overlap (unverified)     : {rec:,}")
    print()
    print(f"total input (all sessions)     : {total_input:,}")
    print(f"total uncached input           : {uncached:,}")
    if total_input:
        print(f"cache hit rate                 : {total_cached / total_input:.2%}")
    if uncached:
        print(f"heuristic share of uncached  : {rec / uncached:.2%}")
    print("recoverable savings / post-fix hit rate: unknown (no causal measurement)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
