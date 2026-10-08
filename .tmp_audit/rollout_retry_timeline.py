"""Timeline of retry episodes: stream_error / fallback warnings and the next model activity.

For every stream_error or WebSocket fallback warning, print the timestamp, the
message shape, and the delay until the next response_item / token_count record
(the first sign the provider answered again).
"""

import json
import re
import sys
from datetime import datetime
from pathlib import Path

root = Path(sys.argv[1])
NUM = re.compile(r"\d+(\.\d+)?")


def ts(rec):
    raw = rec.get("timestamp")
    if not raw:
        return None
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        return None


for path in sorted(root.rglob("*.jsonl")):
    recs = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if not line.strip():
            continue
        try:
            recs.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    hits = []
    for i, rec in enumerate(recs):
        p = rec.get("payload") or {}
        if rec.get("type") != "event_msg":
            continue
        kind = p.get("type")
        msg = p.get("message", "") or ""
        if kind == "stream_error" or (kind == "warning" and msg.startswith("Falling back")) or kind == "error":
            hits.append(i)
    if not hits:
        continue
    print(f"\n== {path.name}")
    for i in hits:
        rec = recs[i]
        p = rec["payload"]
        t0 = ts(rec)
        # previous model activity
        prev_t = None
        for j in range(i - 1, -1, -1):
            r = recs[j]
            if r.get("type") in ("response_item",) or (r.get("type") == "event_msg" and (r.get("payload") or {}).get("type") == "task_started"):
                prev_t = ts(r)
                break
        nxt = None
        for j in range(i + 1, len(recs)):
            r = recs[j]
            rp = r.get("payload") or {}
            if r.get("type") == "response_item" or (r.get("type") == "event_msg" and rp.get("type") in ("token_count", "stream_error", "error", "warning", "turn_aborted")):
                nxt = (ts(r), r.get("type"), rp.get("type"))
                break
        dt_next = (nxt[0] - t0).total_seconds() if (nxt and nxt[0] and t0) else None
        dt_prev = (t0 - prev_t).total_seconds() if (prev_t and t0) else None
        msg = p.get("message", "")
        print(f"  {t0.strftime('%H:%M:%S.%f')[:-3] if t0 else '?'} {p.get('type'):12s} since_prev_activity={dt_prev}s next={nxt[1:] if nxt else None} +{dt_next}s :: {msg[:230]}")
