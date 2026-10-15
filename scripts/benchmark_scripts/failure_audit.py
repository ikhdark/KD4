#!/usr/bin/env python3
"""Classify unsuccessful tool operations and the model continuations they cost.

Pairs each rollout `custom_tool_call` with its `custom_tool_call_output` and
buckets genuine failures by mechanism. Classification is STRUCTURAL: it reads
the documented result fields emitted by the code-mode runtime
(`exit_code`, `success`, `status`, cell `error`) rather than grepping free text,
because command output and patch bodies routinely contain words like "timeout"
and "cancelled" on fully successful calls.

A code-mode cell emits structured result headers and framed source bodies.
Only headers are classified; JSON inside a source body is data, not another
execution result. The cell's mechanism is the first failing result.

Usage:
  python -I -B failure_audit.py scan --home <codex-home> [--json out.json]
  python -I -B failure_audit.py show <mechanism> --home <codex-home> [--limit N]
"""
from __future__ import annotations

import argparse
import collections
import json
import pathlib
import re
import sys

_DECODER = json.JSONDecoder()

# Error text emitted by the V8 runtime when the model's cell script is invalid.
_JS_ERROR = re.compile(
    r"\b(ReferenceError|TypeError|SyntaxError|RangeError|is not a function"
    r"|is not defined|Cannot read properties of)\b"
)
# apply_patch context failures.
_PATCH_CTX = re.compile(
    r"(failed to find expected lines|invalid context|context does not match"
    r"|could not find the following|no match found)", re.I
)
_PS_UNKNOWN_CMD = re.compile(
    r"is not recognized as (?:the name of )?a (?:cmdlet|name of a cmdlet)", re.I
)
_PS_BAD_PARAM = re.compile(
    r"(ParameterBindingException|ParameterCannotBeFound|A positional parameter "
    r"cannot be found|Cannot bind argument|MissingMandatoryParameter)", re.I
)
_PS_PARSE = re.compile(
    r"(ParserError|Unexpected token|Missing closing|is missing the terminator"
    r"|The term .* is not recognized)", re.I
)
_PATH_MISSING = re.compile(
    r"(ItemNotFoundException|PathNotFound|Cannot find path|No such file or "
    r"directory|does not exist|FileNotFoundException)", re.I
)
_LOCK = re.compile(
    r"(being used by another process|Access to the path .* is denied|"
    r"PermissionDenied|os error 32)", re.I
)
_CARGO_LOCK = re.compile(r"Blocking waiting for file lock", re.I)
_ADMISSION = "codex_rust_admission_v1"
_ZERO_TESTS = re.compile(
    r"(did not match any tests|zero tests|no tests to run|produced no tests"
    r"|empty selection)", re.I
)
_REQUIRES_ALL = re.compile(r"requires explicit `?--all`?", re.I)
_UNKNOWN_SUBCMD = re.compile(
    r"(unrecognized subcommand|no such command|invalid choice|unexpected argument"
    r"|unknown option|unknown flag|unrecognized option)", re.I
)
_PY_IMPORT = re.compile(r"(ModuleNotFoundError|ImportError|No module named)")
_PY_TB = re.compile(r"Traceback \(most recent call last\)")
_TEST_FAIL = re.compile(
    r"(^FAILED |^=+ FAILURES|\bFAILED\b.*::|^ERROR: test|AssertionError"
    r"|^test result: FAILED|assertion `?left)",
    re.M,
)
_RUSTC = re.compile(r"^error(\[E\d{4}\])?: ", re.M)
_RUSTC_CODE = re.compile(r"(error\[E\d{4}\]|rustc --explain|could not compile)")
# `[source {...}]` provenance trailers the projection emits around display text.
_SOURCE_TRAILER = re.compile(r"^\[source \{.*?\}\]$", re.M | re.S)
_ARG_SCHEMA = re.compile(
    r"(missing field|unknown field|invalid type: |missing required|"
    r"failed to parse function arguments|invalid value for)", re.I
)


def _docs(out: str):
    """Yield (doc, trailing_text) for each JSON document in a cell's output.

    The code-mode output projection replaces a command's inline `output` with
    `output_lines` and emits the real stdout/stderr as display text after the
    JSON document (see `code-mode-protocol/src/output_projection.rs`). So the
    diagnostic for a failed command lives in the text that FOLLOWS its result
    document, not inside it; callers must classify both together.
    """
    spans: list[tuple[object, int, int]] = []  # (doc, doc_end, next_doc_start)
    i, n = 0, len(out)
    while i < n:
        while i < n and out[i] in " \t\r\n":
            i += 1
        if i >= n:
            break
        if out.startswith("[source ", i):
            end_marker = out.find("\n", i)
            if end_marker < 0:
                break
            try:
                source = json.loads(out[i + len("[source "):end_marker].rstrip("\r")[:-1])
            except (ValueError, TypeError):
                break
            count = source.get("text_lines") if isinstance(source, dict) else None
            if type(count) is not int or count < 0:
                break  # An unbounded body is opaque through the remainder.
            i = end_marker + 1
            for _ in range(count):
                newline = out.find("\n", i)
                i = n if newline < 0 else newline + 1
                if i == n:
                    break
            continue
        if out[i] not in "{[":
            # Without framing there is no reliable next-result boundary.
            break
        try:
            doc, end = _DECODER.raw_decode(out, i)
        except json.JSONDecodeError:
            break
        spans.append((doc, end, i))
        i = end
    for k, (doc, end, _start) in enumerate(spans):
        nxt = spans[k + 1][2] if k + 1 < len(spans) else n
        yield doc, out[end:nxt]


def _classify_doc(doc, call_input: str, trailing: str = "") -> str | None:
    """Return a failure mechanism for one structured result document, or None."""
    if not isinstance(doc, dict):
        return None

    # Harness refused to dispatch: resolve_tool / tool lookup diagnostic.
    if doc.get("status") in ("not_found", "ambiguous", "invalid_name") and (
        "candidates" in doc or "recovery" in doc
    ):
        return f"tool-resolution-{doc['status']}"

    # Rust test-runner admission (exit 75, structured record).
    if doc.get("kind") == _ADMISSION or doc.get("schema") == _ADMISSION:
        return "rust-admission-busy"

    text_fields = []
    for key in ("error", "message", "stderr", "text", "output", "reason", "detail"):
        v = doc.get(key)
        if isinstance(v, str):
            text_fields.append(v)
    # The projected display text that follows this result carries the real
    # stdout/stderr when `output` was elided into `output_lines`.
    trail = _SOURCE_TRAILER.sub("", trailing or "")
    blob = "\n".join(text_fields + [trail])

    exit_code = doc.get("exit_code")
    success = doc.get("success")
    failed = (isinstance(exit_code, int) and exit_code != 0) or success is False

    # A code-mode cell that threw: the runtime reports `error` with no exit code.
    if not failed and isinstance(doc.get("error"), str) and doc["error"]:
        if _JS_ERROR.search(doc["error"]):
            return "code-mode-script-error"
        failed = True

    if not failed:
        return None

    # --- ordered, most specific first
    if exit_code == 75 and _ADMISSION in blob:
        return "rust-admission-busy"
    if _ADMISSION in blob:
        return "rust-admission-busy"
    if _JS_ERROR.search(blob):
        return "code-mode-script-error"
    if _PATCH_CTX.search(blob) or (
        success is False and "*** Update File" in call_input
    ):
        return "patch-context-mismatch"
    if _REQUIRES_ALL.search(blob):
        return "rust-unfiltered-requires-all"
    if _ZERO_TESTS.search(blob):
        return "rust-zero-test-selection"
    if _CARGO_LOCK.search(blob):
        return "cargo-lock-contention"
    if _LOCK.search(blob):
        return "file-lock-contention"
    if _PS_UNKNOWN_CMD.search(blob):
        return "powershell-unknown-command"
    if _PS_BAD_PARAM.search(blob):
        return "powershell-bad-parameter"
    if _PS_PARSE.search(blob):
        return "powershell-parse-error"
    # rustc diagnostics must be matched before the generic argument-schema
    # patterns: `missing field`/`this function takes N arguments` are rustc
    # wording too, and misreading them as tool-schema rejections would
    # overstate the preventable-failure class.
    if _RUSTC.search(blob) or _RUSTC_CODE.search(blob):
        return "rust-compile-error"
    if _UNKNOWN_SUBCMD.search(blob):
        return "cli-argument-rejected"
    if _ARG_SCHEMA.search(blob):
        return "argument-schema-rejected"
    if _PATH_MISSING.search(blob):
        return "path-not-found"
    if _PY_IMPORT.search(blob):
        return "python-import-error"
    if _PY_TB.search(blob):
        return "python-exception"
    if _TEST_FAIL.search(blob):
        return "test-assertion-failure"
    if isinstance(exit_code, int) and exit_code != 0 and not blob.strip():
        # A nonzero exit whose result carries no stderr/output at all: the model
        # is told the command failed but given nothing to diagnose it with.
        return "nonzero-exit-no-diagnostic"
    return "nonzero-exit-unclassified"


def _classify(out: str, call_input: str) -> tuple[str | None, int]:
    """Return (first failure mechanism, number of failing documents)."""
    # This exact runtime prefix is a cell failure, not arbitrary stdout.
    if re.match(r"\AScript failed with cell ID [^\r\n]+\r?\nScript error:\r?\n", out):
        return "code-mode-script-error", 1
    first, count = None, 0
    for doc, trailing in _docs(out):
        m = _classify_doc(doc, call_input, trailing)
        if m:
            count += 1
            if first is None:
                first = m
    return first, count


def _iter_rollouts(home: pathlib.Path):
    for sub in ("sessions", "archived_sessions"):
        root = home / sub
        if root.is_dir():
            yield from sorted(root.rglob("rollout-*.jsonl"))


_repo_filter: str | None = None


def scan(home: pathlib.Path) -> dict:
    mech = collections.Counter()
    mech_docs = collections.Counter()
    mech_sessions = collections.defaultdict(set)
    examples = collections.defaultdict(list)
    chains = collections.Counter()
    chain_lens = collections.defaultdict(list)
    totals = collections.Counter()
    tok = collections.Counter()

    for path in _iter_rollouts(home):
        calls: dict[str, dict] = {}
        order: list[str] = []
        cwd = ""
        keep = True
        try:
            fh = path.open(encoding="utf-8", errors="replace")
        except OSError:
            continue
        with fh:
            for lineno, line in enumerate(fh, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                p = rec.get("payload")
                if not isinstance(p, dict):
                    continue
                ptype = p.get("type")
                if not cwd:
                    c = p.get("cwd") or (p.get("turn_context") or {}).get("cwd")
                    if isinstance(c, str) and c:
                        cwd = c
                        keep = _repo_filter is None or _repo_filter.lower() in c.lower()
                if ptype == "token_count":
                    info = p.get("info") or {}
                    last = info.get("last_token_usage") or {}
                    tok["input"] += last.get("input_tokens") or 0
                    tok["cached"] += last.get("cached_input_tokens") or 0
                    tok["output"] += last.get("output_tokens") or 0
                    totals["requests"] += 1
                elif ptype in ("custom_tool_call", "function_call"):
                    cid = p.get("call_id")
                    if cid:
                        calls[cid] = {
                            "input": p.get("input") or p.get("arguments") or "",
                            "name": p.get("name") or "",
                            "line": lineno,
                        }
                        totals["calls"] += 1
                elif ptype in ("custom_tool_call_output", "function_call_output"):
                    cid = p.get("call_id")
                    out = p.get("output")
                    if not isinstance(out, str):
                        out = json.dumps(out) if out is not None else ""
                    totals["outputs"] += 1
                    call = calls.get(cid, {})
                    m, ndocs = _classify(out, call.get("input", ""))
                    if m is None:
                        continue
                    totals["failing_calls"] += 1
                    mech[m] += 1
                    mech_docs[m] += ndocs
                    mech_sessions[m].add(path.name)
                    order.append(m)
                    if len(examples[m]) < 8:
                        examples[m].append(
                            {
                                "rollout": path.name,
                                "cwd": cwd,
                                "call_line": call.get("line"),
                                "out_line": lineno,
                                "tool": call.get("name"),
                                "input": (call.get("input") or "")[:1800],
                                "output": out[:1800],
                                "failing_docs": ndocs,
                            }
                        )
        run_m, run_n = None, 0
        for m in order:
            if m == run_m:
                run_n += 1
            else:
                if run_m is not None and run_n >= 2:
                    chains[run_m] += 1
                    chain_lens[run_m].append(run_n)
                run_m, run_n = m, 1
        if run_m is not None and run_n >= 2:
            chains[run_m] += 1
            chain_lens[run_m].append(run_n)

    return {
        "totals": dict(totals),
        "tokens": dict(tok),
        "mechanisms": [
            {
                "mechanism": m,
                "failing_cells": c,
                "failing_docs": mech_docs[m],
                "sessions": len(mech_sessions[m]),
                "repeat_chains": chains[m],
                "cells_in_chains": sum(chain_lens[m]),
                "max_chain": max(chain_lens[m], default=0),
            }
            for m, c in mech.most_common()
        ],
        "examples": dict(examples),
    }


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("scan")
    s.add_argument("--home", required=True)
    s.add_argument("--json")
    s2 = sub.add_parser("show")
    s2.add_argument("mechanism")
    s2.add_argument("--home", required=True)
    s2.add_argument("--limit", type=int, default=5)
    s2.add_argument("--from-json")
    a = ap.parse_args(argv)

    if a.cmd == "show" and getattr(a, "from_json", None):
        res = json.loads(pathlib.Path(a.from_json).read_text(encoding="utf-8"))
    else:
        res = scan(pathlib.Path(a.home))

    if a.cmd == "show":
        for ex in res["examples"].get(a.mechanism, [])[: a.limit]:
            print(json.dumps(ex, indent=2))
        return 0

    if a.json:
        pathlib.Path(a.json).write_text(json.dumps(res, indent=2), encoding="utf-8")
    t = res["totals"]
    print(
        f"rollouts={t.get('rollouts',0)} requests={t.get('requests',0)} "
        f"calls={t.get('calls',0)} outputs={t.get('outputs',0)} "
        f"failing_cells={t.get('failing_calls',0)}"
    )
    k = res["tokens"]
    print(
        f"tokens input={k.get('input',0):,} cached={k.get('cached',0):,} "
        f"output={k.get('output',0):,}"
    )
    hdr = f"{'mechanism':32} {'cells':>6} {'docs':>6} {'sess':>5} {'chains':>7} {'inChain':>8} {'max':>4}"
    print(hdr)
    for m in res["mechanisms"]:
        print(
            f"{m['mechanism']:32} {m['failing_cells']:6d} {m['failing_docs']:6d} "
            f"{m['sessions']:5d} {m['repeat_chains']:7d} {m['cells_in_chains']:8d} "
            f"{m['max_chain']:4d}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
