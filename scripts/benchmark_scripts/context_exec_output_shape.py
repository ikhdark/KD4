#!/usr/bin/env python3
"""Characterize the content shape of model-visible `exec` tool outputs.

`context_tool_output_audit.py` establishes that in recorded KD4 rollouts
essentially all model-visible tool-output tokens come from one tool (`exec`)
and that none of them are exact or prefix/suffix duplicates of each other.
That rules out the obvious dedup wins and leaves the question this script
answers: what is inside those outputs, and how much of it is repeated
*content* rather than a repeated whole output.

Measured per session, over `function_call_output` / `custom_tool_call_output`
records joined to their originating call by `call_id`:

* `line_repeat_tokens` - tokens in output lines (>= MIN_LINE_CHARS, after
  stripping) whose exact text already appeared in an earlier output of the
  same session. This is the cross-output content overlap that a whole-output
  hash cannot see: a second `cargo test` run, a re-grep of the same file, a
  re-listed directory. Reported as an upper bound on what any line-level
  deduplication could address, not as a safe-to-delete figure.
* `cmd_repeat_*` - outputs with identical recorded tool name and arguments,
  including opaque code-mode programs, split by whether output was identical.
  Unrecorded environment or filesystem changes remain unknown; identical
  output does not establish that a prior receipt could replace execution.
* `by_command` - the same split keyed by exact recorded call identity, so the
  heaviest repeated commands are named rather than inferred.

Nothing is rewritten; this script only measures.

Usage:
  context_exec_output_shape.py SESSION.jsonl [...] [--json] [--top N]
  context_exec_output_shape.py --latest N [--json] [--top N]
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records
from collections import defaultdict

SESSION_ROOT = pathlib.Path(r"C:\Users\kuh\Desktop\LOCAL-KD\sessions")

OUTPUT_TYPES = {"function_call_output", "custom_tool_call_output"}
CALL_TYPES = {"function_call", "custom_tool_call", "local_shell_call"}

# Short lines (`}`, `)`, blank, single tokens) repeat for syntactic reasons in
# any corpus. Counting them as recoverable overlap would inflate the figure
# into meaninglessness, so only substantial lines are considered.
MIN_LINE_CHARS = 24


def est_tokens(text: str) -> int:
    return math.ceil(len(text.encode("utf-8", "replace")) / 4)


def output_text(payload: dict) -> str:
    """Model-visible text of a tool-output item."""
    parts: list[str] = []
    value = payload.get("output")
    if isinstance(value, str):
        parts.append(value)
    elif isinstance(value, dict):
        for field in ("content", "text"):
            if isinstance(value.get(field), str):
                parts.append(value[field])
    elif isinstance(value, list):
        for entry in value:
            if isinstance(entry, str):
                parts.append(entry)
            elif isinstance(entry, dict):
                for field in ("text", "input_text", "output_text"):
                    if isinstance(entry.get(field), str):
                        parts.append(entry[field])
    return "\n".join(parts)


def normalized_command(payload: dict) -> str | None:
    """Exact recorded call identity, not proof of equivalent execution state.

    Keep argv boundaries, whitespace, workdir and all other recorded arguments.
    Code-mode programs are opaque: extracting a cmd substring would discard
    execution context and conflate unrelated non-command calls.
    """
    if not payload:
        return None
    raw = payload.get("arguments", payload.get("input"))
    if raw is None:
        raw = payload.get("action")
    if raw is None:
        return None
    return json.dumps(
        {"name": payload.get("name"), "type": payload.get("type"), "arguments": raw},
        sort_keys=True, ensure_ascii=False,
    )


def audit(path: pathlib.Path) -> dict:
    calls: dict[str, dict] = {}
    pending: list[tuple[str, str]] = []

    # Strict verified hydration: unresolved coverage aborts rather than reporting zero.
    for record, wire_bytes in iter_rollout_records(path):
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        if not isinstance(payload, dict):
            continue
        kind = payload.get("type")
        call_id = payload.get("call_id")
        if kind in CALL_TYPES and isinstance(call_id, str):
            calls[call_id] = payload
        elif kind in OUTPUT_TYPES and isinstance(call_id, str):
            pending.append((call_id, output_text(payload)))

    seen_lines: set[str] = set()
    seen_commands: dict[str, set[str]] = defaultdict(set)
    by_command: dict[str, dict[str, int]] = defaultdict(
        lambda: {"outs": 0, "tokens": 0, "repeat_same": 0, "repeat_diff": 0}
    )

    total_tokens = 0
    line_repeat_tokens = 0
    repeat_same_tokens = 0
    repeat_diff_tokens = 0
    outputs = 0

    for call_id, text in pending:
        if not text:
            continue
        outputs += 1
        tokens = est_tokens(text)
        total_tokens += tokens

        current_lines = set()
        for raw in text.splitlines():
            stripped = raw.strip()
            if len(stripped) < MIN_LINE_CHARS:
                continue
            if stripped in seen_lines:
                line_repeat_tokens += est_tokens(raw)
            current_lines.add(stripped)
        seen_lines.update(current_lines)

        command = normalized_command(calls.get(call_id, {}))
        if command is None:
            continue
        entry = by_command[command]
        entry["outs"] += 1
        entry["tokens"] += tokens
        prior = seen_commands[command]
        if prior:
            if text in prior:
                repeat_same_tokens += tokens
                entry["repeat_same"] += tokens
            else:
                repeat_diff_tokens += tokens
                entry["repeat_diff"] += tokens
        prior.add(text)

    return {
        "session": path.name,
        "outputs": outputs,
        "tokens": total_tokens,
        "line_repeat_tokens": line_repeat_tokens,
        "cmd_repeat_identical_tokens": repeat_same_tokens,
        "cmd_repeat_changed_tokens": repeat_diff_tokens,
        "by_command": {
            key: dict(value)
            for key, value in sorted(by_command.items(), key=lambda kv: -kv[1]["tokens"])
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path)
    parser.add_argument("--latest", type=int)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--top", type=int, default=12)
    args = parser.parse_args()

    paths = list(args.paths)
    if args.latest:
        if not SESSION_ROOT.exists():
            print(f"session root not found: {SESSION_ROOT}", file=sys.stderr)
            return 2
        paths = sorted(
            discover_rollouts(SESSION_ROOT),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )[: args.latest]
    if not paths:
        parser.error("no sessions selected")

    missing = [p for p in paths if not p.is_file()]
    if missing:
        for path in missing:
            print(f"missing: {path}", file=sys.stderr)
        return 2

    reports = [audit(path) for path in paths]
    if args.json:
        print(json.dumps({"sessions": reports}, indent=2))
        return 0

    total = sum(r["tokens"] for r in reports)
    line_rep = sum(r["line_repeat_tokens"] for r in reports)
    same = sum(r["cmd_repeat_identical_tokens"] for r in reports)
    diff = sum(r["cmd_repeat_changed_tokens"] for r in reports)
    outs = sum(r["outputs"] for r in reports)

    print(f"sessions: {len(reports)}")
    print(f"tool outputs: {outs}")
    print(f"model-visible tool-output est tokens: {total:,}")
    if total:
        print(
            f"\nrepeated output LINES (>= {MIN_LINE_CHARS} chars) across outputs: "
            f"{line_rep:,} tok ({100 * line_rep / total:.1f}%)   [upper bound]"
        )
        print(
            f"repeat command, identical output: {same:,} tok "
            f"({100 * same / total:.1f}%)   [reuse safety unverified]"
        )
        print(
            f"repeat command, changed output:  {diff:,} tok "
            f"({100 * diff / total:.1f}%)   [fresh evidence]"
        )

    merged: dict[str, dict[str, int]] = defaultdict(
        lambda: {"outs": 0, "tokens": 0, "repeat_same": 0, "repeat_diff": 0}
    )
    for report in reports:
        for command, stats in report["by_command"].items():
            for key, value in stats.items():
                merged[command][key] += value

    print(f"\nheaviest commands (top {args.top}):")
    print(f"{'tokens':>10} {'outs':>5} {'rpt-same':>9} {'rpt-diff':>9}  command")
    for command, stats in sorted(merged.items(), key=lambda kv: -kv[1]["tokens"])[
        : args.top
    ]:
        label = command if len(command) <= 86 else command[:83] + "..."
        print(
            f"{stats['tokens']:>10,} {stats['outs']:>5} "
            f"{stats['repeat_same']:>9,} {stats['repeat_diff']:>9,}  {label}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
