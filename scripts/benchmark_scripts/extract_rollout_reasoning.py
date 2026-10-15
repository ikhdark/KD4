#!/usr/bin/env python
"""Extract reasoning / tool-call / patch structure from a Codex rollout JSONL.

Resolves `rollout_payload_artifact` stubs against <home>/rollout-payloads/<sha256>.json.
Usage:
  python -I -B extract_rollout_reasoning.py <rollout.jsonl> [--home DIR] [--mode summary|full]
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import hydrate_rollout_record, read_rollout_snapshot


try:  # keep non-ASCII rollout text from killing the run on cp1252 consoles
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except Exception:  # noqa: BLE001
    pass


def resolve(line_obj, home: Path):
    """Return (type, payload), refusing unauthenticated external evidence."""
    record = hydrate_rollout_record(line_obj, home / "sessions" / "rollout.jsonl")
    return record.get("type"), record.get("payload") or {}


def text_of(content):
    out = []
    if isinstance(content, list):
        for c in content:
            if isinstance(c, dict):
                for k in ("text", "input_text", "output_text"):
                    if k in c and isinstance(c[k], str):
                        out.append(c[k])
                        break
                else:
                    if c.get("type") == "input_image":
                        out.append("[image]")
    elif isinstance(content, str):
        out.append(content)
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("rollout")
    ap.add_argument("--home", default=r"C:\Users\kuh\Desktop\LOCAL-KD")
    ap.add_argument("--mode", default="summary", choices=["summary", "full", "raw-types"])
    ap.add_argument("--max-chars", type=int, default=4000)
    args = ap.parse_args()

    home = Path(args.home)
    path = Path(args.rollout)
    mc = args.max_chars

    type_counts: dict[str, int] = {}
    n = 0
    with read_rollout_snapshot(path) as snapshot:
        for n, obj, error, _ in snapshot.decoded_lines():
            if error:
                raise ValueError(f"{path}:{n}: {error}")
            t, payload = resolve(obj, home)
            ts = obj.get("timestamp", "")
            ptype = payload.get("type") if isinstance(payload, dict) else None
            key = f"{t}/{ptype}" if ptype else str(t)
            type_counts[key] = type_counts.get(key, 0) + 1

            if args.mode == "raw-types":
                continue

            if not isinstance(payload, dict):
                continue

            if ptype == "agent_reasoning":
                txt = payload.get("text") or ""
                if txt.strip():
                    print(f"\n~~ [{n}] {ts} THINK: {txt[:mc]}")

            elif ptype == "reasoning":
                summaries = []
                for s in payload.get("summary") or []:
                    if isinstance(s, dict) and s.get("text"):
                        summaries.append(s["text"])
                enc = payload.get("content") or []
                body = []
                for c in enc:
                    if isinstance(c, dict) and c.get("text"):
                        body.append(c["text"])
                blob = "\n".join(summaries + body)
                if blob.strip():
                    print(f"\n=== [{n}] {ts} REASONING ===")
                    print(blob[:mc])

            elif ptype == "message":
                role = payload.get("role")
                txt = text_of(payload.get("content"))
                if not txt.strip():
                    continue
                print(f"\n=== [{n}] {ts} MSG({role}) ===")
                print(txt[:mc])

            elif ptype == "function_call":
                name = payload.get("name")
                argtxt = payload.get("arguments") or ""
                print(f"\n--- [{n}] {ts} CALL {name} ---")
                print(argtxt[:mc])

            elif ptype == "function_call_output":
                out = payload.get("output")
                if isinstance(out, dict):
                    out = out.get("content") or json.dumps(out)[:mc]
                out = out if isinstance(out, str) else json.dumps(out)
                print(f"--- [{n}] OUTPUT ({len(out)} chars) ---")
                print(out[: (mc if args.mode == "full" else 1200)])

            elif ptype in ("custom_tool_call",):
                print(f"\n--- [{n}] {ts} CUSTOM_CALL {payload.get('name')} ---")
                print(str(payload.get("input"))[:mc])

            elif ptype == "custom_tool_call_output":
                out = str(payload.get("output"))
                print(f"--- [{n}] CUSTOM_OUTPUT ({len(out)} chars) ---")
                print(out[: (mc if args.mode == "full" else 1200)])

            elif ptype == "agent_message":
                txt = payload.get("message") or text_of(payload.get("content"))
                if txt.strip():
                    print(f"\n=== [{n}] {ts} AGENT_MSG ===")
                    print(txt[:mc])

            elif ptype == "user_message":
                txt = payload.get("message") or text_of(payload.get("content"))
                print(f"\n########## [{n}] {ts} USER PROMPT ##########")
                print(txt[:mc])

            elif ptype == "plan_update":
                print(f"\n[{n}] PLAN_UPDATE: {json.dumps(payload.get('plan'))[:mc]}")

            elif ptype == "sub_agent_activity":
                print(f"[{n}] SUBAGENT: {json.dumps(payload)[:800]}")

            elif ptype in ("warning", "error", "stream_error", "turn_aborted", "context_compacted"):
                print(f"\n!!! [{n}] {ts} {ptype}: {json.dumps(payload)[:1500]}")

            elif ptype in ("turn_diff", "patch_apply_begin", "patch_apply_end"):
                print(f"\n*** [{n}] {ts} {ptype} ***")
                print(json.dumps(payload)[:mc])

            elif ptype == "token_count":
                info = payload.get("info") or {}
                last = info.get("last_token_usage") or {}
                tot = info.get("total_token_usage") or {}
                print(
                    f"[{n}] TOKENS last in={last.get('input_tokens')} out={last.get('output_tokens')} "
                    f"total in={tot.get('input_tokens')} out={tot.get('output_tokens')}"
                )

    print("\n##### TYPE COUNTS #####", file=sys.stderr)
    for k, v in sorted(type_counts.items(), key=lambda kv: -kv[1]):
        print(f"{v:6d}  {k}", file=sys.stderr)
    print(f"lines={n}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
