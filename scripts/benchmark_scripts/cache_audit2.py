#!/usr/bin/env python3
"""Classify prompt-cache prefix breaks in KD4 rollouts by cause.

Walks each rollout in file order, tracking token_count events (model requests)
alongside the records that legitimately invalidate a cached prefix (compaction,
turn context changes, tool manifest changes). A break that lines up with such a
record is explained; a break with no explanation is a candidate defect in
request assembly.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from collections import Counter

# Records that can legitimately change the reusable prefix.
MARKERS = {
    "compacted": "compaction",
    "turn_context": "turn_context",
    "tool_manifest": "tool_manifest",
    "thread_settings_applied": "thread_settings",
    "world_state": "world_state",
}


def classify(path: pathlib.Path, abs_min: int, rel_max: float):
    """Yield (kind, detail) break records for one rollout."""
    prev = None
    idx = 0
    # Markers seen since the previous model request.
    pending: list[str] = []
    out = []
    manifest_prev = None
    previous_total = None

    try:
        fh = path.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return out
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
            payload = obj.get("payload") or {}
            ptype = payload.get("type") if isinstance(payload, dict) else None

            # Track legitimate prefix-invalidating records.
            if rtype in MARKERS:
                if rtype == "tool_manifest":
                    # Only a *changed* manifest invalidates the tool prefix.
                    sig = json.dumps(payload, sort_keys=True)
                    if manifest_prev is not None and sig != manifest_prev:
                        pending.append("tool_manifest_changed")
                    manifest_prev = sig
                else:
                    pending.append(MARKERS[rtype])
            if ptype in MARKERS:
                pending.append(MARKERS[ptype])
            if rtype == "compacted" or ptype == "compacted":
                pending.append("compaction")

            if ptype != "token_count":
                continue
            info = payload.get("info") or {}
            total = info.get("total_token_usage")
            if total is not None:
                if total == previous_total:
                    continue  # Repeated/stale last usage is not another request.
                previous_total = total
            last = info.get("last_token_usage") or {}
            it = int(last.get("input_tokens") or 0)
            if it <= 0:
                continue
            cached = int(last.get("cached_input_tokens") or 0)
            idx += 1

            if prev is not None:
                shortfall = prev - cached
                if shortfall > abs_min and cached < rel_max * prev:
                    causes = sorted(set(pending))
                    kind = "explained" if causes else "UNEXPLAINED"
                    out.append(
                        {
                            "kind": kind,
                            "session": path.name,
                            "req": idx,
                            "prev_input": prev,
                            "cached": cached,
                            "shortfall": shortfall,
                            "causes": causes,
                        }
                    )
            prev = it
            pending = []
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--abs-min", type=int, default=1024)
    ap.add_argument("--rel-max", type=float, default=0.75)
    ap.add_argument("--top", type=int, default=30)
    args = ap.parse_args()

    files = sorted(
        args.root.rglob("rollout-*.jsonl"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    if args.limit:
        files = files[: args.limit]

    all_breaks = []
    for f in files:
        all_breaks.extend(classify(f, args.abs_min, args.rel_max))

    if not all_breaks:
        print("no breaks detected")
        return 0

    expl = [b for b in all_breaks if b["kind"] == "explained"]
    unexpl = [b for b in all_breaks if b["kind"] == "UNEXPLAINED"]

    print(f"files scanned     : {len(files)}")
    print(f"breaks total      : {len(all_breaks)}")
    print(f"  explained       : {len(expl)}  shortfall {sum(b['shortfall'] for b in expl):,}")
    print(f"  UNEXPLAINED     : {len(unexpl)}  shortfall {sum(b['shortfall'] for b in unexpl):,}")
    print()

    cc = Counter()
    for b in expl:
        cc[tuple(b["causes"])] += 1
    print("explained-break causes:")
    for causes, n in cc.most_common():
        tok = sum(b["shortfall"] for b in expl if tuple(b["causes"]) == causes)
        print(f"  {n:5d}  shortfall {tok:>14,}  {'+'.join(causes)}")

    if unexpl:
        print()
        print("UNEXPLAINED breaks (candidate defects):")
        for b in sorted(unexpl, key=lambda x: -x["shortfall"])[: args.top]:
            print(
                f"  {b['session'][:50]:50s} req#{b['req']:<4d} "
                f"prev_in={b['prev_input']:>8,} cached={b['cached']:>8,} "
                f"short={b['shortfall']:>8,}"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
