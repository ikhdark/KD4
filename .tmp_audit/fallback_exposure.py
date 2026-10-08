"""Measure requests served after a permanent WebSocket->HTTPS fallback.

For each rollout with a fallback warning, count sampling completions (token_count
events with info) before/after the first fallback, and compare the continuation
latency: time from a tool output (function/custom_tool_call_output) to the next
model-produced response_item. Sessions without fallback give a WS-only control,
split at the same relative position so context growth is matched roughly.
"""

import json
import statistics
import sys
from datetime import datetime
from pathlib import Path

root = Path(sys.argv[1])
OUTPUT_KINDS = {"function_call_output", "custom_tool_call_output"}
MODEL_KINDS = {"reasoning", "message", "function_call", "custom_tool_call", "web_search_call"}


def ts(rec):
    raw = rec.get("timestamp")
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00")) if raw else None
    except ValueError:
        return None


def continuation_latencies(recs):
    out = []  # (index, seconds)
    pending = None
    for i, rec in enumerate(recs):
        if rec.get("type") != "response_item":
            # a user turn boundary resets the pairing
            pl = rec.get("payload") or {}
            if rec.get("type") == "event_msg" and pl.get("type") in ("user_message", "stream_error", "warning", "error"):
                pending = None
            continue
        pl = rec.get("payload") or {}
        kind = pl.get("type")
        if kind in OUTPUT_KINDS:
            pending = (i, ts(rec))
        elif kind in MODEL_KINDS and pending is not None:
            t = ts(rec)
            if t and pending[1]:
                out.append((pending[0], (t - pending[1]).total_seconds()))
            pending = None
    return out


def med(xs):
    return round(statistics.median(xs), 2) if xs else None


rows = []
ctrl_before, ctrl_after = [], []
fb_before, fb_after = [], []
total_after = 0
for path in sorted(root.rglob("*.jsonl")):
    recs = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            recs.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    fb_idx = next((i for i, r in enumerate(recs)
                   if r.get("type") == "event_msg"
                   and (r.get("payload") or {}).get("type") == "warning"
                   and ((r.get("payload") or {}).get("message") or "").startswith("Falling back")), None)
    completions = [i for i, r in enumerate(recs)
                   if r.get("type") == "event_msg"
                   and (r.get("payload") or {}).get("type") == "token_count"
                   and (r.get("payload") or {}).get("info")]
    lats = continuation_latencies(recs)
    if fb_idx is None:
        if len(lats) >= 6:
            mid = len(recs) * 0.5
            ctrl_before += [s for i, s in lats if i < mid]
            ctrl_after += [s for i, s in lats if i >= mid]
        continue
    before = [c for c in completions if c < fb_idx]
    after = [c for c in completions if c > fb_idx]
    total_after += len(after)
    b = [s for i, s in lats if i < fb_idx]
    a = [s for i, s in lats if i > fb_idx]
    fb_before += b
    fb_after += a
    msg = recs[fb_idx]["payload"]["message"]
    reason = msg.split("response: ", 1)[-1][:60]
    rows.append((path.name[8:27], len(before), len(after), med(b), med(a), len(b), len(a), reason))

print("session              req_before req_after  med_cont_before(n)  med_cont_after(n)  cause")
for r in rows:
    print(f"{r[0]}  {r[1]:9d} {r[2]:9d}  {str(r[3]):>8}({r[5]:3d})        {str(r[4]):>8}({r[6]:3d})   {r[7]}")
print(f"\nrequests served on HTTPS after fallback: {total_after} across {len(rows)} sessions")
print(f"fallback sessions continuation latency median: before={med(fb_before)}s (n={len(fb_before)}) after={med(fb_after)}s (n={len(fb_after)})")
print(f"WS-only control (first half vs second half): {med(ctrl_before)}s (n={len(ctrl_before)}) vs {med(ctrl_after)}s (n={len(ctrl_after)})")


def pct(xs, q):
    if not xs:
        return None
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(q * len(xs)))], 2)


print("p25/p75 fallback before:", pct(fb_before, .25), pct(fb_before, .75), " after:", pct(fb_after, .25), pct(fb_after, .75))
print("p25/p75 control first:", pct(ctrl_before, .25), pct(ctrl_before, .75), " second:", pct(ctrl_after, .25), pct(ctrl_after, .75))
