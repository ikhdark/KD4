"""Split rollout output tokens into reasoning, prose and code-mode cell content.

usage: python -I -B audit_output_token_composition.py [--json] ROLLOUT_OR_RUN_DIR...

A run directory is any folder holding `rollout.jsonl` and optionally `meta.json`
(the rd-exp layout); its `arm`/`batch` label the row. Provider counts come from
the last `token_count` usage per request. Cell content is attributed by byte
share of the run's visible output tokens, so per-category token figures are
estimates; provider totals are exact.

Cell lines (split on real and escaped newlines) are classified as:
  prose     - natural-language string content written into artifacts or messages
  preflight - destination/instruction-file existence checks before a write
  verify    - re-reading or asserting on an artifact after it was written
  code      - everything else (scanner, formatting, tool calls)
"""
import glob
import json
import os
import re
import statistics as st
import sys

WORD = re.compile(r"[A-Za-z][A-Za-z'/-]+")
PREFLIGHT = re.compile(r"AGENTS\.md|\.exists\(\)|exists\b.*(?:Destination|Output|refus|reconcil)|DESTINATION_EXISTS|destination_exists|preflight", re.I)
VERIFY = re.compile(r"\bassert\b|[Vv]erif(?:y|ied)|[Vv]alidat(?:e|ed|ion)")
WRITE = re.compile(r"apply_patch\(")


def is_prose(line):
    words = WORD.findall(line)
    if len(words) < 7:
        return False
    letters = sum(len(w) for w in words) + line.count(" ")
    code_marks = sum(line.count(c) for c in "()[]{}=;<>|\\")
    return letters / max(len(line), 1) >= 0.72 and code_marks <= 4


def split_lines(cell):
    return [part for line in cell.split("\n") for part in line.split("\\n")]


def classify_cell(cell, written_before):
    """Return byte counts per category; `written_before` marks cells after an artifact write."""
    out = dict(prose=0, preflight=0, verify=0, code=0)
    seen_write = written_before
    for line in split_lines(cell):
        size = len(line.encode("utf-8")) + 1
        if WRITE.search(line):
            seen_write = True
            out["code"] += size
        elif seen_write and VERIFY.search(line):
            out["verify"] += size
        elif not seen_write and PREFLIGHT.search(line):
            out["preflight"] += size
        elif is_prose(line):
            out["prose"] += size
        else:
            out["code"] += size
    return out, seen_write


def analyze(path, label):
    usage_rows, last = [], None
    cells, messages = [], []
    with open(path, encoding="utf-8") as fh:
        for raw in fh:
            rec = json.loads(raw)
            payload = rec.get("payload") or {}
            kind = payload.get("type")
            if rec.get("type") == "event_msg" and kind == "token_count":
                usage = (payload.get("info") or {}).get("last_token_usage")
                if usage and usage != last:
                    last = usage
                    usage_rows.append(usage)
            elif rec.get("type") == "response_item" and kind in ("custom_tool_call", "function_call"):
                cells.append(payload.get("input") or payload.get("arguments") or "")
            elif rec.get("type") == "response_item" and kind == "message" and payload.get("role") == "assistant":
                messages.append("".join(c.get("text", "") for c in payload.get("content", [])))
    output = sum(u.get("output_tokens", 0) for u in usage_rows)
    reasoning = sum(u.get("reasoning_output_tokens", 0) for u in usage_rows)
    visible = output - reasoning
    totals = dict(prose=0, preflight=0, verify=0, code=0)
    written = False
    for cell in cells:
        parts, written = classify_cell(cell, written)
        for k, v in parts.items():
            totals[k] += v
    msg_bytes = sum(len(m.encode("utf-8")) for m in messages)
    cell_bytes = sum(totals.values())
    per_byte = visible / max(cell_bytes + msg_bytes, 1)
    est = {k: round(v * per_byte) for k, v in totals.items()}
    est["messages"] = round(msg_bytes * per_byte)
    return dict(label=label, path=path, requests=len(usage_rows), output=output, reasoning=reasoning,
                visible=visible, input=sum(u.get("input_tokens", 0) for u in usage_rows),
                cells=len(cells), cell_bytes=cell_bytes, max_cell=max((len(c) for c in cells), default=0),
                bytes=totals, est_tokens=est)


def targets(args):
    for a in args:
        if os.path.isdir(a) and os.path.exists(os.path.join(a, "rollout.jsonl")):
            meta_path = os.path.join(a, "meta.json")
            meta = json.load(open(meta_path)) if os.path.exists(meta_path) else {}
            yield os.path.join(a, "rollout.jsonl"), meta.get("arm", os.path.basename(a)), meta.get("batch", "")
        elif os.path.isdir(a):
            for run in sorted(glob.glob(os.path.join(a, "*"))):
                if os.path.exists(os.path.join(run, "rollout.jsonl")):
                    yield from targets([run])
        else:
            yield a, os.path.basename(a)[:60], ""


def main():
    as_json = "--json" in sys.argv
    args = [a for a in sys.argv[1:] if a != "--json"]
    rows = []
    for path, arm, batch in targets(args):
        row = analyze(path, arm)
        row["batch"] = batch
        rows.append(row)
    if as_json:
        print(json.dumps(rows, indent=1))
        return
    head = f"{'batch':6} {'arm':6} {'req':>3} {'out':>5} {'rsn':>5} {'vis':>5} | {'prose':>5} {'pre':>4} {'verif':>5} {'code':>5} {'msg':>4} | {'maxcell':>7}"
    print(head)
    for r in rows:
        e = r["est_tokens"]
        print(f"{r['batch']:6} {r['label'][:6]:6} {r['requests']:3} {r['output']:5} {r['reasoning']:5} {r['visible']:5} | "
              f"{e['prose']:5} {e['preflight']:4} {e['verify']:5} {e['code']:5} {e['messages']:4} | {r['max_cell']:7}")
    by_arm = {}
    for r in rows:
        by_arm.setdefault(r["label"], []).append(r)
    print("\nper-arm means")
    for arm, rs in sorted(by_arm.items()):
        m = lambda f: st.mean(f(r) for r in rs)
        print(f"{arm:6} n={len(rs):2} out={m(lambda r: r['output']):6.0f} rsn={m(lambda r: r['reasoning']):5.0f} "
              f"prose={m(lambda r: r['est_tokens']['prose']):5.0f} pre={m(lambda r: r['est_tokens']['preflight']):4.0f} "
              f"verify={m(lambda r: r['est_tokens']['verify']):5.0f} code={m(lambda r: r['est_tokens']['code']):5.0f} "
              f"req={m(lambda r: r['requests']):4.1f}")


if __name__ == "__main__":
    main()
