"""Scan rollout JSONL for provider retry / rate-limit / stream-error evidence.

Emits only aggregate counts and distinct, truncated message shapes.
"""

import collections
import hashlib
import json
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
files = sorted(root.rglob("*.jsonl"))

type_counts = collections.Counter()
event_counts = collections.Counter()
distinct_msgs = collections.Counter()
error_info = collections.Counter()
warn_msgs = collections.Counter()
rate_limit_samples = []
token_count_keys = collections.Counter()
files_with_retry = set()

NUM = re.compile(r"\d+(\.\d+)?")


def shape(text: str) -> str:
    text = NUM.sub("#", text)
    return text[:160]


for path in files:
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        continue
    for line in lines:
        if not line.strip():
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            type_counts["<bad json>"] += 1
            continue
        rtype = rec.get("type")
        type_counts[rtype] += 1
        payload = rec.get("payload") or {}
        if rtype == "event_msg":
            ptype = payload.get("type")
            event_counts[ptype] += 1
            if ptype in ("stream_error",):
                distinct_msgs[shape(payload.get("message", ""))] += 1
                info = payload.get("codex_error_info")
                error_info[json.dumps(info, sort_keys=True)[:120]] += 1
                files_with_retry.add(path.name)
            elif ptype == "error":
                distinct_msgs["ERROR: " + shape(payload.get("message", ""))] += 1
                info = payload.get("codex_error_info")
                error_info[json.dumps(info, sort_keys=True)[:120]] += 1
            elif ptype == "warning":
                warn_msgs[shape(payload.get("message", ""))] += 1
            elif ptype == "token_count":
                rl = payload.get("rate_limits")
                if rl and len(rate_limit_samples) < 3:
                    rate_limit_samples.append(rl)
                for k in payload.keys():
                    token_count_keys[k] += 1

print("files:", len(files))
print("record types:", dict(type_counts.most_common()))
print("event types (top 40):", dict(event_counts.most_common(40)))
print("\nstream_error / error message shapes:")
for msg, n in distinct_msgs.most_common(40):
    print(f"  {n:5d}  {msg}")
print("\ncodex_error_info:")
for k, n in error_info.most_common(20):
    print(f"  {n:5d}  {k}")
print("\nwarning shapes:")
for msg, n in warn_msgs.most_common(30):
    print(f"  {n:5d}  {msg}")
print("\nfiles with stream_error:", len(files_with_retry))
print("token_count keys:", dict(token_count_keys))
print("rate_limit samples:")
for s in rate_limit_samples:
    print("  ", json.dumps(s)[:400])
