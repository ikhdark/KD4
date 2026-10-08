"""Aggregate retry/backoff and model-wait timing from rollout timing profiles.

Finds every JSON object carrying a timing profile (schemaVersion + exclusive),
keeps the last profile per turn, and reports retry share, fallback counts and
model wait distributions.
"""

import collections
import json
import statistics
import sys
from pathlib import Path

root = Path(sys.argv[1])
carriers = collections.Counter()
per_turn = {}


def walk(obj, path, rec_kind, turn_hint):
    if isinstance(obj, dict):
        if "schemaVersion" in obj and "exclusive" in obj and "unions" in obj:
            carriers[f"{rec_kind}:{'/'.join(path[-3:])}"] += 1
            key = (turn_hint or obj.get("turnId") or id(obj))
            prev = per_turn.get(key)
            if prev is None or (obj.get("inclusiveDurationNs") or 0) >= (prev.get("inclusiveDurationNs") or 0):
                per_turn[key] = obj
            return
        for k, v in obj.items():
            walk(v, path + [k], rec_kind, turn_hint)
    elif isinstance(obj, list):
        for i, v in enumerate(obj[:50]):
            walk(v, path + [str(i)], rec_kind, turn_hint)


for p in sorted(root.rglob("*.jsonl")):
    for line in p.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        pl = r.get("payload")
        if not isinstance(pl, (dict, list)):
            continue
        kind = r.get("type")
        if isinstance(pl, dict):
            kind = f"{kind}.{pl.get('type')}" if pl.get("type") else kind
            turn = pl.get("turn_id") or (pl.get("item") or {}).get("turn_id") if isinstance(pl.get("item"), dict) else pl.get("turn_id")
        else:
            turn = None
        walk(pl, [], kind, (p.name, turn) if turn else None)

print("timing carriers:", dict(carriers.most_common(10)))
profiles = list(per_turn.values())
print("turn profiles:", len(profiles))

ex_keys = collections.Counter()
retry_ns = []
model_ns = []
incl_ns = []
counters_agg = collections.Counter()
for t in profiles:
    ex = t.get("exclusive") or {}
    retry_ns.append(ex.get("retryOnlyNs") or 0)
    model_ns.append(ex.get("modelOnlyNs") or 0)
    incl_ns.append(t.get("inclusiveDurationNs") or 0)
    for k, v in (t.get("counters") or {}).items():
        if isinstance(v, (int, float)):
            counters_agg[k] += v

tot_incl = sum(incl_ns)
print(f"sum inclusive: {tot_incl/1e9:.1f}s; modelOnly {sum(model_ns)/1e9:.1f}s; retryOnly {sum(retry_ns)/1e9:.1f}s")
nz = sorted(x for x in retry_ns if x)
print("turns with retryOnly>0:", len(nz), "values(s):", [round(x / 1e9, 2) for x in nz[-20:]])
print("counters (sum):", {k: v for k, v in counters_agg.most_common() if v})
if profiles:
    sample = profiles[0]
    print("profile top-level keys:", sorted(sample.keys()))
    print("model keys:", json.dumps(sample.get("model"))[:800] if sample.get("model") else None)
