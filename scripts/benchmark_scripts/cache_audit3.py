#!/usr/bin/env python3
"""Attribute interleaved model requests to cache routes, then audit prefix reuse.

A single KD4 rollout can contain requests from several agents (parent plus
sub-agents), and token_count events carry no thread identity. But
total_token_usage is cumulative per agent, so a request can be attributed to the
stream whose running total its own totals continue:

    total_after == total_before + last

Requests are matched greedily to the open stream whose cumulative totals equal
this request's totals minus its own last_token_usage. A request matching no
stream starts a new one (a new agent / cache route).

Only pairs *within* one attributed stream are compared, so sub-agent interleaving
can no longer masquerade as a cache break.
"""

from __future__ import annotations

import argparse
import json
import pathlib
from collections import Counter
from dataclasses import dataclass, field

MARKERS = ("compacted", "context_compacted", "turn_context", "world_state",
           "thread_settings_applied")


@dataclass
class Stream:
    sid: int
    # Cumulative totals after the most recent attributed request.
    tot_in: int = 0
    tot_out: int = 0
    reqs: list[dict] = field(default_factory=list)


def parse(path: pathlib.Path):
    """Return (streams, markers_before_index) for one rollout."""
    streams: list[Stream] = []
    next_sid = 0
    pending: list[str] = []
    previous_total = None
    manifest_prev = None
    manifest_changed_pending = False

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
                    manifest_changed_pending = True
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
            lc = int(last.get("cached_input_tokens") or 0)
            lo = int(last.get("output_tokens") or 0)
            ti = int(tot.get("input_tokens") or 0)
            to = int(tot.get("output_tokens") or 0)

            # Attribute to the stream this request continues.
            match = None
            for s in streams:
                if s.tot_in + li == ti and s.tot_out + lo == to:
                    match = s
                    break
            if match is None:
                match = Stream(sid=next_sid)
                next_sid += 1
                streams.append(match)

            causes = sorted(set(pending))
            if manifest_changed_pending:
                causes = sorted(set(causes + ["tool_manifest_changed"]))
            match.reqs.append({
                "input": li, "cached": lc, "output": lo,
                "causes": causes, "fresh": not match.reqs,
            })
            match.tot_in = ti
            match.tot_out = to
            pending = []
            manifest_changed_pending = False
    return streams


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--abs-min", type=int, default=1024)
    ap.add_argument("--rel-max", type=float, default=0.75)
    ap.add_argument("--top", type=int, default=30)
    args = ap.parse_args()

    files = sorted(args.root.rglob("rollout-*.jsonl"),
                   key=lambda p: p.stat().st_mtime, reverse=True)
    if args.limit:
        files = files[: args.limit]

    tot_in = tot_cached = tot_out = 0
    nreq = npair = 0
    nstreams = 0
    multi = 0
    expl, unexpl = [], []

    for f in files:
        streams = parse(f)
        if not streams:
            continue
        nstreams += len(streams)
        if len(streams) > 1:
            multi += 1
        for s in streams:
            for r in s.reqs:
                nreq += 1
                tot_in += r["input"]
                tot_cached += r["cached"]
                tot_out += r["output"]
            for prev, cur in zip(s.reqs, s.reqs[1:]):
                npair += 1
                short = prev["input"] - cur["cached"]
                if short > args.abs_min and cur["cached"] < args.rel_max * prev["input"]:
                    rec = {"session": f.name, "sid": s.sid,
                           "prev": prev["input"], "cached": cur["cached"],
                           "short": short, "causes": cur["causes"]}
                    (expl if cur["causes"] else unexpl).append(rec)

    print(f"files              : {len(files)}")
    print(f"cache routes       : {nstreams}  (multi-route rollouts: {multi})")
    print(f"model requests     : {nreq}")
    print(f"in-stream pairs    : {npair}")
    print(f"total input        : {tot_in:,}")
    print(f"  cached reads     : {tot_cached:,}")
    print(f"  uncached input   : {tot_in - tot_cached:,}")
    print(f"total output       : {tot_out:,}")
    print(f"cache hit rate     : {tot_cached / tot_in:.1%}" if tot_in else "")
    print()
    print(f"breaks explained   : {len(expl):5d}  shortfall {sum(b['short'] for b in expl):>14,}")
    print(f"breaks UNEXPLAINED : {len(unexpl):5d}  shortfall {sum(b['short'] for b in unexpl):>14,}")
    cc = Counter(tuple(b["causes"]) for b in expl)
    if cc:
        print()
        print("explained causes:")
        for causes, n in cc.most_common():
            tk = sum(b["short"] for b in expl if tuple(b["causes"]) == causes)
            print(f"  {n:5d}  {tk:>14,}  {'+'.join(causes)}")
    if unexpl:
        print()
        print("UNEXPLAINED (candidate defects):")
        for b in sorted(unexpl, key=lambda x: -x["short"])[: args.top]:
            print(f"  {b['session'][:48]:48s} s{b['sid']:<3d} "
                  f"prev={b['prev']:>8,} cached={b['cached']:>8,} short={b['short']:>8,}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
