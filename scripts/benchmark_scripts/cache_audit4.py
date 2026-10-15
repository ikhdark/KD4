#!/usr/bin/env python3
"""Classify whether an observed cache shortfall recovers on the next request.

Recovery means cached[i+1] >= 90% of input[i-1]. Persistence is only an
observation: it cannot distinguish changed bytes from eviction, routing, TTL,
or another provider-side miss. Neither category proves an assembly defect.
"""

from __future__ import annotations

import argparse
import json
import pathlib
from collections import Counter

MARKERS = ("compacted", "context_compacted", "turn_context", "world_state",
           "thread_settings_applied")


def parse(path: pathlib.Path):
    streams: list[dict] = []
    pending: list[str] = []
    previous_total = None
    manifest_prev = None
    manifest_changed = False
    try:
        fh = path.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return []
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
                sig = json.dumps(payload, sort_keys=True)
                if manifest_prev is not None and sig != manifest_prev:
                    manifest_changed = True
                manifest_prev = sig
            for m in MARKERS:
                if rtype == m or ptype == m:
                    pending.append("compaction" if "compact" in m else m)

            if ptype != "token_count":
                continue
            info = payload.get("info") or {}
            total = info.get("total_token_usage")
            if total is not None:
                if total == previous_total:
                    continue  # Repeated/stale last usage is not another request.
                previous_total = total
            last = info.get("last_token_usage") or {}
            tot = info.get("total_token_usage") or {}
            li = int(last.get("input_tokens") or 0)
            if li <= 0:
                continue
            lo = int(last.get("output_tokens") or 0)
            ti = int(tot.get("input_tokens") or 0)
            to = int(tot.get("output_tokens") or 0)

            match = None
            for s in streams:
                if s["tot_in"] + li == ti and s["tot_out"] + lo == to:
                    match = s
                    break
            if match is None:
                match = {"tot_in": 0, "tot_out": 0, "reqs": []}
                streams.append(match)
            causes = sorted(set(pending + (["tool_manifest_changed"] if manifest_changed else [])))
            match["reqs"].append({
                "input": li,
                "cached": int(last.get("cached_input_tokens") or 0),
                "output": lo,
                "causes": causes,
            })
            match["tot_in"] = ti
            match["tot_out"] = to
            pending = []
            manifest_changed = False
    return streams


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    ap.add_argument("--abs-min", type=int, default=1024)
    ap.add_argument("--rel-max", type=float, default=0.75)
    ap.add_argument("--top", type=int, default=25)
    args = ap.parse_args()

    files = sorted(args.root.rglob("rollout-*.jsonl"),
                   key=lambda p: p.stat().st_mtime, reverse=True)

    recovered, persistent, terminal = [], [], []
    explained = 0
    expl_short = 0

    for f in files:
        for s in parse(f):
            reqs = s["reqs"]
            for i in range(1, len(reqs)):
                prev, cur = reqs[i - 1], reqs[i]
                short = prev["input"] - cur["cached"]
                if not (short > args.abs_min and cur["cached"] < args.rel_max * prev["input"]):
                    continue
                if cur["causes"]:
                    explained += 1
                    expl_short += short
                    continue
                rec = {"session": f.name, "req": i + 1, "prev": prev["input"],
                       "cached": cur["cached"], "short": short}
                if i + 1 < len(reqs):
                    nxt = reqs[i + 1]
                    # Did the next request reuse the prefix that existed before the break?
                    if nxt["cached"] >= 0.9 * prev["input"]:
                        rec["next_cached"] = nxt["cached"]
                        recovered.append(rec)
                    else:
                        rec["next_cached"] = nxt["cached"]
                        persistent.append(rec)
                else:
                    terminal.append(rec)

    print(f"files                       : {len(files)}")
    print(f"explained breaks            : {explained:5d}  shortfall {expl_short:>14,}")
    print()
    print("unexplained breaks, by next-request cache recovery:")
    print(f"  RECOVERED (next request) : {len(recovered):5d}  "
          f"shortfall {sum(b['short'] for b in recovered):>14,}")
    print(f"  PERSISTENT (cause unknown)    : {len(persistent):5d}  "
          f"shortfall {sum(b['short'] for b in persistent):>14,}")
    print(f"  terminal (last in stream)  : {len(terminal):5d}  "
          f"shortfall {sum(b['short'] for b in terminal):>14,}")
    print()
    print("RECOVERED = next cached usage reached 90% of previous input; byte identity unverified.")
    print("PERSISTENT = next cached usage remained low; cause and byte identity unverified.")
    if persistent:
        print()
        print("PERSISTENT detail:")
        for b in sorted(persistent, key=lambda x: -x["short"])[: args.top]:
            print(f"  {b['session'][:46]:46s} req#{b['req']:<4d} prev={b['prev']:>8,} "
                  f"cached={b['cached']:>8,} next_cached={b['next_cached']:>8,}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
