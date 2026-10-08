"""Show one rollout's assistant messages, reasoning summaries, and patch targets (bounded)."""

import json
import sys
from pathlib import Path

root = Path(sys.argv[1])
needle = sys.argv[2]
limit = int(sys.argv[3]) if len(sys.argv) > 3 else 400
path = next(p for p in root.rglob("*.jsonl") if needle in p.name)
for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
        rec = json.loads(line)
    except json.JSONDecodeError:
        continue
    pl = rec.get("payload") or {}
    ts = (rec.get("timestamp") or "")[11:19]
    if rec.get("type") == "response_item":
        kind = pl.get("type")
        if kind == "message" and pl.get("role") == "assistant":
            text = " ".join(c.get("text", "") for c in pl.get("content") or [] if isinstance(c, dict))
            print(f"[{ts}] ASSISTANT: {text[:limit]}")
        elif kind == "reasoning":
            summ = " ".join(s.get("text", "") for s in pl.get("summary") or [] if isinstance(s, dict))
            if summ:
                print(f"[{ts}] REASONING: {summ[:limit]}")
    elif rec.get("type") == "event_msg" and pl.get("type") == "patch_apply_end":
        for k, v in (pl.get("changes") or {}).items():
            diff = ""
            if isinstance(v, dict):
                diff = v.get("unified_diff") or v.get("content") or ""
            print(f"[{ts}] PATCH {Path(k).name} ({len(diff)} chars)")
            if len(sys.argv) > 4:
                print(diff[:int(sys.argv[4])])
