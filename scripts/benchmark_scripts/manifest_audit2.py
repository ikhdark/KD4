#!/usr/bin/env python3
"""Audit tool-manifest churn using the recorded manifest hash.

tool_manifest records come in three shapes:
  {"hash": ...}                      - inline manifest identity
  {"sha256":..., "bytes":..., ...}   - payload-artifact stub (not the manifest)
  full manifest object               - expanded form

Only the manifest identity (hash) decides whether the cached tool prefix stays
valid, so churn is measured by transitions of that hash. Stubs are skipped
rather than treated as a changed manifest.

Reports how many distinct manifests a session cycles through and, critically,
how often a manifest hash *returns* to a value already seen (A->B->A), which is
oscillation: avoidable prefix invalidation rather than a one-way change.
"""

from __future__ import annotations

import argparse
import json
import pathlib
from collections import Counter


def manifest_hashes(path: pathlib.Path) -> list[str]:
    out = []
    try:
        fh = path.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return out
    with fh:
        for line in fh:
            if '"tool_manifest"' not in line:
                continue
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("type") != "tool_manifest":
                continue
            payload = obj.get("payload")
            if not isinstance(payload, dict):
                continue
            h = payload.get("hash")
            if isinstance(h, str):
                out.append(h)
                continue
            # Artifact stub: manifest bytes live elsewhere; identity unknown here.
            if "sha256" in payload:
                out.append("STUB:" + str(payload.get("sha256")))
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    args = ap.parse_args()
    files = sorted(args.root.rglob("rollout-*.jsonl"),
                   key=lambda p: p.stat().st_mtime, reverse=True)

    tot_records = 0
    tot_transitions = 0
    oscillations = 0
    per_session = []
    sessions_with_churn = 0

    for f in files:
        hs = [h for h in manifest_hashes(f) if not h.startswith("STUB:")]
        if not hs:
            continue
        tot_records += len(hs)
        trans = [(a, b) for a, b in zip(hs, hs[1:]) if a != b]
        tot_transitions += len(trans)
        if trans:
            sessions_with_churn += 1
        seen = set()
        osc = 0
        prev = None
        for h in hs:
            if h != prev:
                if h in seen:
                    osc += 1
                seen.add(h)
                prev = h
        oscillations += osc
        distinct = len(set(hs))
        if len(trans):
            per_session.append((f.name, len(hs), distinct, len(trans), osc))

    print(f"files                        : {len(files)}")
    print(f"tool_manifest records (hash) : {tot_records}")
    print(f"sessions with manifest churn : {sessions_with_churn}")
    print(f"manifest transitions         : {tot_transitions}")
    print(f"  of which OSCILLATIONS      : {oscillations}  "
          f"(returned to a previously used manifest)")
    print()
    print("An oscillation means the tool set reverted to an earlier manifest, so the")
    print("tool prefix was invalidated twice for the same end state. A one-way change")
    print("(tools genuinely added) is expected and not recoverable.")
    if per_session:
        print()
        print(f"{'session':52s} {'recs':>5s} {'distinct':>8s} {'trans':>6s} {'osc':>5s}")
        for name, n, d, t, o in sorted(per_session, key=lambda x: -x[4])[:20]:
            print(f"{name[:52]:52s} {n:>5d} {d:>8d} {t:>6d} {o:>5d}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
