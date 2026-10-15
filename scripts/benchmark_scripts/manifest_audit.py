#!/usr/bin/env python3
"""Audit tool-manifest churn in KD4 rollouts.

Tool definitions sit in the cached prefix, so a changed manifest invalidates
everything after it. This reports how often the manifest changes mid-session and
what changed, separating legitimate availability changes (a tool genuinely added
or removed) from pure reordering or re-serialization, which would be a defect:
the same tools in a different order cost a full prefix rewrite for no reason.
"""

from __future__ import annotations

import argparse
import json
import pathlib
from collections import Counter


def tool_names(payload) -> list[str]:
    """Extract the tool-name list from a tool_manifest payload, order preserved."""
    def walk(node):
        if isinstance(node, dict):
            if "name" in node and isinstance(node["name"], str):
                nested = any(isinstance(v, (list, dict)) for k, v in node.items()
                             if k not in ("name", "description"))
                if not nested or "parameters" in node or "input_schema" in node:
                    return [node["name"]]
            out = []
            for v in node.values():
                out.extend(walk(v))
            return out
        if isinstance(node, list):
            out = []
            for v in node:
                out.extend(walk(v))
            return out
        return []
    return walk(payload)


def scan(path: pathlib.Path):
    events = []
    prev_sig = None
    prev_names = None
    prev_payload = None
    try:
        fh = path.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return events
    with fh:
        for line in fh:
            line = line.strip()
            if not line or '"tool_manifest"' not in line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("type") != "tool_manifest":
                continue
            payload = obj.get("payload")
            sig = json.dumps(payload, sort_keys=True)
            names = tool_names(payload)
            if prev_sig is not None and sig != prev_sig:
                added = sorted(set(names) - set(prev_names or []))
                removed = sorted(set(prev_names or []) - set(names))
                if added or removed:
                    kind = "availability"
                elif (
                    isinstance(payload, dict) and isinstance(prev_payload, dict)
                    and isinstance(payload.get("tools"), list)
                    and isinstance(prev_payload.get("tools"), list)
                    and {k: v for k, v in payload.items() if k not in ("tools", "hash")}
                        == {k: v for k, v in prev_payload.items() if k not in ("tools", "hash")}
                    and sorted(json.dumps(t, sort_keys=True) for t in payload["tools"])
                        == sorted(json.dumps(t, sort_keys=True) for t in prev_payload["tools"])
                ):
                    kind = "REORDER_ONLY" if payload["tools"] != prev_payload["tools"] else "RESERIALIZE_ONLY"
                else:
                    # Equal names do not imply equal descriptions or schemas.
                    kind = "definition_changed"
                events.append({
                    "session": path.name, "kind": kind,
                    "added": added, "removed": removed,
                })
            prev_sig, prev_names, prev_payload = sig, names, payload
    return events


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("root", type=pathlib.Path)
    args = ap.parse_args()
    files = sorted(args.root.rglob("rollout-*.jsonl"),
                   key=lambda p: p.stat().st_mtime, reverse=True)
    ev = []
    for f in files:
        ev.extend(scan(f))
    print(f"files                 : {len(files)}")
    print(f"manifest changes      : {len(ev)}")
    for k, n in Counter(e["kind"] for e in ev).most_common():
        print(f"  {k:18s}: {n}")
    churn = [e for e in ev if e["kind"] in ("REORDER_ONLY", "RESERIALIZE_ONLY")]
    print()
    if churn:
        print(f"DEFECT CANDIDATES (same tools, changed bytes): {len(churn)}")
        for e in churn[:15]:
            print(f"  {e['session'][:52]:52s} {e['kind']}")
    else:
        print("No reorder/reserialize-only churn: changes altered availability/definitions or could not prove equivalence.")
    deltas = Counter()
    for e in ev:
        if e["kind"] == "availability":
            deltas[(tuple(e["added"]), tuple(e["removed"]))] += 1
    if deltas:
        print()
        print("availability changes (added | removed):")
        for (a, r), n in deltas.most_common(12):
            print(f"  {n:5d}  +{list(a)} -{list(r)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
