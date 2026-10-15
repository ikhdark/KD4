#!/usr/bin/env python3
"""Find consecutive model requests with identical usage in KD4 rollouts.

Two adjacent token_count events with the same input, cached and output token
counts indicate the same prompt was dispatched twice. That is either a retry
(the first attempt failed) or a duplicated dispatch. Retries are expected and
are identified by an accompanying error/warning/stream_error record; a duplicate
with no such record is a candidate for wasted uncached input.

Reports the uncached cost of each duplicate, which is what a fix would recover.
"""

from __future__ import annotations

import argparse
import json
import pathlib
from collections import Counter

ERROR_MARKERS = ("stream_error", "error", "turn_aborted", "warning",
                 "retry", "rate_limit")


def scan(path: pathlib.Path):
    """Return list of duplicate-pair records for one rollout."""
    out = []
    prev = None
    pending_err: list[str] = []
    idx = 0
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
            payload = obj.get("payload") if isinstance(obj.get("payload"), dict) else {}
            ptype = payload.get("type")
            if ptype in ERROR_MARKERS:
                pending_err.append(ptype)
            if ptype != "token_count":
                continue
            info = payload.get("info") or {}
            last = info.get("last_token_usage") or {}
            tot = info.get("total_token_usage") or {}
            li = int(last.get("input_tokens") or 0)
            if li <= 0:
                continue
            cur = (
                li,
                int(last.get("cached_input_tokens") or 0),
                int(last.get("output_tokens") or 0),
                int(tot.get("input_tokens") or 0),
            )
            idx += 1
            if prev is not None:
                # Identical last-usage triple AND identical cumulative total means
                # the second event did not advance the agent's own accounting.
                same_usage = cur[:3] == prev[:3]
                if same_usage:
                    out.append({
                        "session": path.name,
                        "req": idx,
                        "input": cur[0],
                        "cached": cur[1],
                        "uncached": cur[0] - cur[1],
                        "same_total": cur[3] == prev[3],
                        "errors": sorted(set(pending_err)),
                    })
            prev = cur
            pending_err = []
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    ap.add_argument("--top", type=int, default=25)
    args = ap.parse_args()

    files = sorted(args.root.rglob("rollout-*.jsonl"),
                   key=lambda p: p.stat().st_mtime, reverse=True)
    dups = []
    for f in files:
        dups.extend(scan(f))

    # A repeated event with an unchanged cumulative total is a duplicate *log*
    # record, not a second billed request. Only an advanced total means the
    # provider actually charged for a second dispatch.
    billed = [d for d in dups if not d["same_total"]]
    logged = [d for d in dups if d["same_total"]]
    with_err = [d for d in billed if d["errors"]]
    no_err = [d for d in billed if not d["errors"]]

    print(f"files                      : {len(files)}")
    print(f"identical adjacent usage   : {len(dups)}")
    print(f"  duplicate log records    : {len(logged)}  (total unchanged; not billed)")
    print(f"  second dispatch (billed) : {len(billed)}")
    print(f"    with error/retry marker: {len(with_err)}  "
          f"uncached {sum(d['uncached'] for d in with_err):,}")
    print(f"    NO marker (candidate)  : {len(no_err)}  "
          f"uncached {sum(d['uncached'] for d in no_err):,}")
    if no_err:
        print()
        print("unexplained duplicate dispatches:")
        for d in sorted(no_err, key=lambda x: -x["uncached"])[: args.top]:
            print(f"  {d['session'][:48]:48s} req#{d['req']:<4d} "
                  f"in={d['input']:>8,} cached={d['cached']:>8,} "
                  f"uncached={d['uncached']:>8,}")
    if with_err:
        print()
        print("retry reasons:")
        for k, n in Counter(tuple(d["errors"]) for d in with_err).most_common():
            print(f"  {n:5d}  {'+'.join(k)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
