"""Attribute each model request's output tokens to the purpose of the code-mode cell it emitted.

usage: python -I -B audit_output_round_purposes.py [--arms ARM,...] EXP_ROOT...

Reads every EXP_ROOT/runs/<run>/rollout.jsonl with its meta.json (the rd-exp layout) and
classifies each request, in order, as:
  explore     cells before the main generation cell
  recover     short cells that mainly call read_tool_output (truncated-output recovery)
  generate    the first large scanner cell (> 2500 chars walking the tree)
  review      cells after generation and before the write
  write       the cell that writes the artifact (apply_patch / write_text / Out-File)
  post_write  later cells (read-back, fixes, delivery)
  final_msg   a request that emitted only a message
Requests whose tool output reports a failure are counted in `failed`. Provider token counts
are exact; the purpose labels are heuristics for the inventory-style tasks run by rd-exp.
"""
import glob
import json
import os
import re
import statistics as st
import sys

PAYLOAD_DIRS = [r"C:\Users\kuh\Desktop\LOCAL-KD\rollout-payloads", r"C:\Users\kuh\.codex\rollout-payloads"]
SCAN = re.compile(r"os\.walk|rglob\(|rg','--files|rg --files")
FAILED = re.compile(r"Script failed|\"exit_code\":[1-9]|\"success\":false")
WRITE = re.compile(r"apply_patch\(|write_text\(|Out-File|Set-Content")
KINDS = ["explore", "recover", "generate", "review", "write", "post_write", "final_msg"]


def resolve(record):
    if record.get("type") == "rollout_payload_artifact":
        for directory in PAYLOAD_DIRS:
            path = os.path.join(directory, record["payload"]["sha256"] + ".json")
            if os.path.exists(path):
                return json.load(open(path, encoding="utf-8"))
    return record


def requests(rollout):
    out, cur, last = [], {"cells": [], "outputs": []}, None
    for line in open(rollout, "rb"):
        record = resolve(json.loads(line))
        payload = record.get("payload") or {}
        kind = payload.get("type") if isinstance(payload, dict) else None
        if record.get("type") == "response_item" and kind == "custom_tool_call":
            cur["cells"].append(payload["input"])
        if record.get("type") == "response_item" and kind == "custom_tool_call_output":
            value = payload.get("output")
            cur["outputs"].append(value if isinstance(value, str) else json.dumps(value))
        if record.get("type") == "event_msg" and kind == "token_count":
            usage = (payload.get("info") or {}).get("last_token_usage")
            if usage and usage != last:
                last = usage
                cur["usage"] = usage
                out.append(cur)
                cur = {"cells": [], "outputs": []}
    state, wrote = "explore", False
    for q in out:
        code, output = "\n".join(q["cells"]), "\n".join(q["outputs"])
        if not code:
            q["kind"] = "final_msg"
        elif WRITE.search(code):
            q["kind"] = "post_write" if wrote else "write"
            wrote = True
        elif wrote:
            q["kind"] = "post_write"
        elif "read_tool_output(" in code and len(code) < 1500:
            q["kind"] = "recover"
        elif len(code) > 2500 and SCAN.search(code):
            q["kind"], state = "generate", "review"
        else:
            q["kind"] = state
        q["failed"] = bool(FAILED.search(output))
        q["out"] = q["usage"]["output_tokens"]
        q["rs"] = q["usage"]["reasoning_output_tokens"]
    return out


def main():
    args = sys.argv[1:]
    arms = None
    if args[:1] == ["--arms"]:
        arms, args = set(args[1].split(",")), args[2:]
    rows = []
    for root in args:
        for run in sorted(glob.glob(os.path.join(root, "runs", "*"))):
            rollout, meta = os.path.join(run, "rollout.jsonl"), os.path.join(run, "meta.json")
            if not (os.path.exists(rollout) and os.path.exists(meta)):
                continue
            arm = json.load(open(meta))["arm"]
            if arms and arm not in arms:
                continue
            reqs = requests(rollout)
            rows.append(dict(run=os.path.basename(run), arm=arm, reqs=reqs, out=sum(q["out"] for q in reqs)))
    for arm in sorted({r["arm"] for r in rows}):
        rs = [r for r in rows if r["arm"] == arm]
        print(f"{arm:6s} n={len(rs):2d} out={st.mean(r['out'] for r in rs):5.0f} "
              f"req={st.mean(len(r['reqs']) for r in rs):4.1f} "
              f"failed_rounds={sum(q['failed'] for r in rs for q in r['reqs'])}")
        for kind in KINDS:
            qs = [q for r in rs for q in r["reqs"] if q["kind"] == kind]
            print(f"   {kind:10s} rounds/run={len(qs) / len(rs):4.2f} out/run={sum(q['out'] for q in qs) / len(rs):5.0f} "
                  f"reasoning/run={sum(q['rs'] for q in qs) / len(rs):4.0f}")
    if len(rows) > 2:
        n = [len(r["reqs"]) for r in rows]
        o = [r["out"] for r in rows]
        mn, mo = st.mean(n), st.mean(o)
        cov = sum((a - mn) * (b - mo) for a, b in zip(n, o))
        vn = sum((a - mn) ** 2 for a in n)
        vo = sum((b - mo) ** 2 for b in o)
        if vn and vo:
            print(f"runs={len(rows)} corr(requests, output)={cov / (vn * vo) ** 0.5:.2f} "
                  f"slope={cov / vn:.0f} output tokens per extra request")


if __name__ == "__main__":
    main()
