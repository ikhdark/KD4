#!/usr/bin/env python3
"""Projection helpers for auditing a single Codex rollout JSONL.

Usage:
  python -I -B audit_rollout_01a13c94.py <rollout.jsonl> <mode> [args]

Modes:
  skeleton        turn/agent structure: user msgs, task boundaries, compactions
  errors          warnings, errors, stream errors
  tools           one line per tool call: line, agent, tool name, status, bytes
  toolnames       histogram of tool names
  patches         patch_apply_end records (files touched, success)
  calls <needle>  full input/output for tool calls whose text matches needle
  line <n> [m]    pretty-print records n..m
  agentmsgs       agent_message records (final answers per agent)
  reasoning       reasoning summaries
  tokens          token_count progression
  interagent      inter_agent_communication_metadata
  artifacts       rollout_payload_artifact stubs (resolve to payload files)
  grep <needle>   line numbers + context where needle appears anywhere
"""

import json
import os
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import hydrate_rollout_record, read_rollout_snapshot

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

HOME = r"C:\Users\kuh\Desktop\LOCAL-KD"
ART_DIR = os.path.join(HOME, "rollout-payloads")


def load(path):
    recs = []
    with read_rollout_snapshot(Path(path)) as snapshot:
        for n, record, error, _ in snapshot.decoded_lines():
            if error:
                raise ValueError(f"{path}:{n}: {error}")
            recs.append((n, record))
    return recs


def resolve(rec):
    """Resolve authenticated artifacts; unavailable evidence aborts the report."""
    return hydrate_rollout_record(rec, Path(HOME) / "sessions" / "rollout.jsonl")


def txt(v, limit=None):
    if v is None:
        return ""
    if isinstance(v, str):
        s = v
    else:
        s = json.dumps(v, ensure_ascii=False)
    s = s.replace("\r", "")
    if limit and len(s) > limit:
        return s[:limit] + f"...[+{len(s)-limit}b]"
    return s


def agent_of(rec):
    for key in ("agent_id", "sub_agent_id", "agent", "source_agent_id"):
        v = rec.get(key)
        if v:
            return str(v)
    p = rec.get("payload")
    if isinstance(p, dict):
        for key in ("agent_id", "sub_agent_id", "agent", "agent_name", "name"):
            v = p.get(key)
            if v:
                return str(v)
    return "-"


def main():
    path, mode = sys.argv[1], sys.argv[2]
    args = sys.argv[3:]
    recs = load(path)
    if mode != "artifacts":
        # Authenticate the full input before emitting any report, in every mode.
        recs = [(n, resolve(record)) for n, record in recs]

    if mode == "skeleton":
        for n, r in recs:
            t = r.get("type")
            p = r.get("payload") or {}
            pt = p.get("type") if isinstance(p, dict) else None
            ts = r.get("timestamp", "")
            if t == "event_msg" and pt in (
                "user_message", "task_started", "context_compacted",
                "plan_update", "error", "stream_error", "warning",
            ):
                m = p.get("message") or p.get("msg") or ""
                print(f"{n}\t{ts}\t{pt}\t{txt(m, 400)}")
            elif t in ("compacted", "sampling_boundary", "turn_context", "world_state"):
                print(f"{n}\t{ts}\t{t}\t{txt(p, 300)}")

    elif mode == "errors":
        for n, r in recs:
            p = r.get("payload") or {}
            pt = p.get("type") if isinstance(p, dict) else None
            if r.get("type") == "event_msg" and pt in ("warning", "error", "stream_error"):
                print(f"=== {n} [{pt}] {r.get('timestamp','')}")
                print(txt(p, 4000))
            if r.get("type") == "__parse_error__":
                print(f"=== {n} [PARSE] {r.get('error')}")

    elif mode == "toolnames":
        c = Counter()
        for n, r in recs:
            r = resolve(r)
            p = r.get("payload") or {}
            if not isinstance(p, dict):
                continue
            if p.get("type") in ("custom_tool_call", "function_call"):
                c[p.get("name", "?")] += 1
        for k, v in c.most_common():
            print(f"{v}\t{k}")

    elif mode == "tools":
        pending = {}
        for n, r in recs:
            r = resolve(r)
            p = r.get("payload") or {}
            if not isinstance(p, dict):
                continue
            pt = p.get("type")
            if pt in ("custom_tool_call", "function_call"):
                cid = p.get("call_id") or p.get("id")
                pending[cid] = (n, p.get("name", "?"), agent_of(r))
                print(f"CALL\t{n}\t{agent_of(r)}\t{p.get('name','?')}\t{txt(p.get('input') or p.get('arguments'), 300)}")
            elif pt in ("custom_tool_call_output", "function_call_output"):
                cid = p.get("call_id") or p.get("id")
                src = pending.get(cid, ("?", "?", "?"))
                out = p.get("output")
                if isinstance(out, dict):
                    body = out.get("content") or out.get("output") or out
                    succ = out.get("success")
                else:
                    body = out
                    succ = None
                print(f"OUT \t{n}\tcall@{src[0]}\t{src[1]}\tsuccess={succ}\tlen={len(txt(body))}\t{txt(body,300)}")

    elif mode == "patches":
        for n, r in recs:
            r = resolve(r)
            p = r.get("payload") or {}
            if isinstance(p, dict) and p.get("type") in ("patch_apply_end", "patch_apply_begin"):
                print(f"{n}\t{p.get('type')}\tsuccess={p.get('success')}\t{txt({k: v for k, v in p.items() if k != 'type'}, 1200)}")

    elif mode == "calls":
        needle = args[0].lower()
        for n, r in recs:
            r = resolve(r)
            p = r.get("payload") or {}
            if not isinstance(p, dict):
                continue
            if p.get("type") in ("custom_tool_call", "function_call", "custom_tool_call_output", "function_call_output"):
                s = txt(p)
                if needle in s.lower():
                    print(f"=== {n} [{p.get('type')}] {p.get('name','')} agent={agent_of(r)}")
                    print(txt(p, int(args[1]) if len(args) > 1 else 6000))

    elif mode == "line":
        a = int(args[0])
        b = int(args[1]) if len(args) > 1 else a
        for n, r in recs:
            if a <= n <= b:
                print(f"=== {n} type={r.get('type')} ts={r.get('timestamp','')}")
                print(json.dumps(resolve(r), ensure_ascii=False, indent=1)[: int(args[2]) if len(args) > 2 else 8000])

    elif mode == "agentmsgs":
        for n, r in recs:
            r = resolve(r)
            p = r.get("payload") or {}
            if isinstance(p, dict) and p.get("type") == "agent_message":
                print(f"=== {n} agent={agent_of(r)}")
                print(txt(p.get("message") or p.get("content"), int(args[0]) if args else 2500))

    elif mode == "reasoning":
        for n, r in recs:
            r = resolve(r)
            p = r.get("payload") or {}
            if isinstance(p, dict) and p.get("type") == "reasoning":
                parts = []
                for s in (p.get("summary") or []):
                    parts.append(s.get("text") if isinstance(s, dict) else str(s))
                for s in (p.get("content") or []):
                    parts.append(s.get("text") if isinstance(s, dict) else str(s))
                print(f"=== {n} agent={agent_of(r)}")
                print(txt(" | ".join(x for x in parts if x), int(args[0]) if args else 1500))

    elif mode == "tokens":
        for n, r in recs:
            p = r.get("payload") or {}
            if isinstance(p, dict) and p.get("type") == "token_count":
                print(f"{n}\t{r.get('timestamp','')}\t{txt(p, 900)}")

    elif mode == "interagent":
        for n, r in recs:
            if r.get("type") == "inter_agent_communication_metadata":
                print(f"=== {n} {r.get('timestamp','')}")
                print(txt(r, int(args[0]) if args else 1500))

    elif mode == "subagent":
        for n, r in recs:
            p = r.get("payload") or {}
            if isinstance(p, dict) and p.get("type") == "sub_agent_activity":
                print(f"{n}\t{txt(p, int(args[0]) if args else 600)}")

    elif mode == "artifacts":
        miss = 0
        tot = 0
        for n, r in recs:
            if r.get("type") != "rollout_payload_artifact":
                continue
            tot += 1
            got = resolve(r)
            if "__missing__" in got or "__error__" in got:
                miss += 1
                print(f"UNRESOLVED {n} {txt(r,400)}")
        print(f"artifacts={tot} unresolved={miss}")

    elif mode == "grep":
        needle = args[0].lower()
        for n, r in recs:
            s = txt(resolve(r))
            if needle in s.lower():
                idx = s.lower().index(needle)
                print(f"{n}\t{r.get('type')}\t...{s[max(0,idx-200):idx+400]}...")

    else:
        raise SystemExit(f"unknown mode {mode}")


if __name__ == "__main__":
    main()
