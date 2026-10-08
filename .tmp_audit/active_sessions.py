"""Summarize recently active rollouts: first user prompt and files they touched."""

import json
import re
import sys
import time
from pathlib import Path

root = Path(sys.argv[1])
window_s = float(sys.argv[2]) if len(sys.argv) > 2 else 1800
now = time.time()
PATH_RE = re.compile(r"codex-rs[\\/][\w\-./\\]+\.rs|scripts[\\/][\w\-.]+\.py")

for path in sorted(root.rglob("*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True):
    if now - path.stat().st_mtime > window_s:
        continue
    first_user = None
    cwd = None
    touched = {}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        pl = rec.get("payload") or {}
        if rec.get("type") == "turn_context" and cwd is None:
            cwd = pl.get("cwd")
        if rec.get("type") == "event_msg" and pl.get("type") == "user_message" and first_user is None:
            first_user = (pl.get("message") or "").strip().replace("\n", " ")
        if rec.get("type") == "event_msg" and pl.get("type") == "patch_apply_end":
            for k in (pl.get("changes") or {}).keys():
                touched[k] = touched.get(k, 0) + 1
    age = int(now - path.stat().st_mtime)
    print(f"\n== {path.name[8:27]} age={age}s cwd={cwd}")
    print("   prompt:", (first_user or "")[:260])
    if touched:
        names = sorted(touched, key=lambda k: -touched[k])[:12]
        print("   patched:", ", ".join(Path(n).name for n in names))
