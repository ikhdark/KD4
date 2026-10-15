#!/usr/bin/env python3
"""Audit prompt-cache reuse across KD4 rollout sessions.

Reads token_count events from rollout JSONL and, for each consecutive pair of
model requests in a session, compares the cached_input_tokens actually reported
by the provider against the uncached input the previous request established.

A request that re-sends a prefix the previous request already paid for, without
that prefix being reported as cached, is a cache miss with evidence.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import statistics
import sys
from dataclasses import dataclass, field


@dataclass
class Req:
    idx: int
    input_tokens: int
    cached: int
    output_tokens: int

    @property
    def uncached(self) -> int:
        return self.input_tokens - self.cached


@dataclass
class Session:
    path: pathlib.Path
    reqs: list[Req] = field(default_factory=list)


def load(path: pathlib.Path) -> Session | None:
    s = Session(path=path)
    idx = 0
    previous_total = None
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line or '"token_count"' not in line:
                    continue
                try:
                    obj = json.loads(line)
                except json.JSONDecodeError:
                    continue
                payload = obj.get("payload") or {}
                if payload.get("type") != "token_count":
                    continue
                info = payload.get("info") or {}
                total = info.get("total_token_usage")
                if total is not None:
                    if total == previous_total:
                        continue  # Repeated/stale last usage is not another request.
                    previous_total = total
                last = info.get("last_token_usage") or {}
                if not last:
                    continue
                it = int(last.get("input_tokens") or 0)
                if it <= 0:
                    continue
                idx += 1
                s.reqs.append(
                    Req(
                        idx=idx,
                        input_tokens=it,
                        cached=int(last.get("cached_input_tokens") or 0),
                        output_tokens=int(last.get("output_tokens") or 0),
                    )
                )
    except OSError:
        return None
    return s if len(s.reqs) >= 2 else None


def analyze(sessions: list[Session]) -> dict:
    """For each consecutive request pair, measure reuse of the established prefix.

    The previous request's total input_tokens is an upper bound on the prefix the
    next request could reuse. Cache granularity means the provider rounds the
    reported hit down, so a shortfall is only counted as a real break when it is
    large in both absolute and relative terms.
    """
    pairs = 0
    breaks = []
    total_uncached = 0
    total_input = 0
    total_cached = 0
    total_output = 0
    first_req_input = 0

    for s in sessions:
        for prev, cur in zip(s.reqs, s.reqs[1:]):
            pairs += 1
            reusable = prev.input_tokens
            shortfall = reusable - cur.cached
            if shortfall > 1024 and cur.cached < 0.75 * reusable:
                breaks.append(
                    {
                        "session": s.path.name,
                        "req": cur.idx,
                        "prev_input": prev.input_tokens,
                        "cached": cur.cached,
                        "shortfall": shortfall,
                        "cur_input": cur.input_tokens,
                    }
                )
        first_req_input += s.reqs[0].input_tokens
        for r in s.reqs:
            total_input += r.input_tokens
            total_cached += r.cached
            total_uncached += r.uncached
            total_output += r.output_tokens

    return {
        "sessions": len(sessions),
        "requests": sum(len(s.reqs) for s in sessions),
        "pairs": pairs,
        "breaks": breaks,
        "break_rate": (len(breaks) / pairs) if pairs else 0.0,
        "total_input": total_input,
        "total_cached": total_cached,
        "total_uncached": total_uncached,
        "total_output": total_output,
        "cache_hit_rate": (total_cached / total_input) if total_input else 0.0,
        "first_req_input": first_req_input,
        "shortfall_tokens": sum(b["shortfall"] for b in breaks),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--top", type=int, default=25)
    args = ap.parse_args()

    files = sorted(
        args.root.rglob("rollout-*.jsonl"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    if args.limit:
        files = files[: args.limit]

    sessions = [s for s in (load(f) for f in files) if s is not None]
    if not sessions:
        print("no sessions with >=2 measured requests", file=sys.stderr)
        return 1

    res = analyze(sessions)
    if args.json:
        print(json.dumps(res, indent=2))
        return 0

    print(f"sessions analyzed : {res['sessions']}")
    print(f"model requests    : {res['requests']}")
    print(f"consecutive pairs : {res['pairs']}")
    print(f"cache hit rate    : {res['cache_hit_rate']:.1%}")
    print(f"total input       : {res['total_input']:,}")
    print(f"  cached reads    : {res['total_cached']:,}")
    print(f"  uncached input  : {res['total_uncached']:,}")
    print(f"total output      : {res['total_output']:,}")
    print(f"first-request in  : {res['first_req_input']:,} (unavoidable cold)")
    print()
    print(f"prefix breaks     : {len(res['breaks'])} / {res['pairs']} ({res['break_rate']:.1%})")
    print(f"shortfall tokens  : {res['shortfall_tokens']:,}")
    if res["breaks"]:
        per = [b["shortfall"] for b in res["breaks"]]
        print(f"median shortfall  : {statistics.median(per):,.0f}")
        print()
        print("worst breaks:")
        for b in sorted(res["breaks"], key=lambda x: -x["shortfall"])[: args.top]:
            print(
                f"  {b['session'][:52]:52s} req#{b['req']:<4d} "
                f"prev_in={b['prev_input']:>8,} cached={b['cached']:>8,} "
                f"short={b['shortfall']:>8,}"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
