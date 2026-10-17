#!/usr/bin/env python3
"""Patch composition and discovery-chain profile for the 2026-10-16 architecture audit.

Read-only companion to `arch_audit_corpus_profile.py`. For each saved rollout it
reports:

* patch line composition inside code-mode cells (context, removed, added,
  headers) and `codex-range` hunk usage — the share of emitted patch text that
  re-states existing source;
* the non-patch, non-command remainder of cell source, by construct;
* consecutive read-only ("discovery") cells and the model time of the
  generations that emitted them, joined through `task_complete.timing`;
* search-then-read pairs: a cell whose `read_file` path appeared in the
  previous cell's output after a search command in that previous cell;
* recovery-only cells: cells whose only nested tool is `read_tool_output`.

All byte counts are UTF-8 bytes of recorded cell source as the model emitted it.
"""

from __future__ import annotations

import argparse
import collections
import json
import re
import sys
from pathlib import Path

from arch_audit_corpus_profile import iter_records, js_string_at

PATCH_RE = re.compile(r"\*\*\* Begin Patch.*?\*\*\* End Patch", re.S)
CMD_KEY_RE = re.compile(r"""(?:\bcmd|"cmd"|'cmd'|\bscript_body|"script_body")\s*:\s*(["'`])""")
PATH_ARG_RE = re.compile(r"""\bpath\s*["']?\s*:\s*(["'`])""")
SEARCH_CMD_RE = re.compile(r"^\s*&?\s*(?:rg|git\s+grep|Select-String|findstr)\b", re.I | re.M)
EDIT_RE = re.compile(r"apply_patch|\*\*\* Begin Patch")
VALIDATION_RE = re.compile(
    r"cargo\s+(?:test|nextest|check|clippy|build)|just\s+(?:core-test|core-gate|test|fmt|clippy)|"
    r"rust_test_runner|pytest|unittest|test-python",
)
# Calls only: kd4 work often searches for these names inside command strings.
AGENT_RE = re.compile(
    r"\btools\.agents(?:__|\.)\w+|\b(?:spawn_agent|send_message|wait_agent|followup_task|list_agents|"
    r"interrupt_agent|get_agent_task)\s*\("
)
READ_RE = re.compile(r"read_files?\s*\(|read_tool_output|list_files")
NESTED_NAME_RE = re.compile(r"\btools\.([A-Za-z_]\w*)\s*\(")
SCAFFOLD_PATTERNS = {
    "text(": re.compile(r"\btext\("),
    "await": re.compile(r"\bawait\b"),
    "store/load": re.compile(r"\b(?:store|load)\("),
    "resolve_tool": re.compile(r"\bresolve_tool\("),
    "JSON.stringify": re.compile(r"JSON\.stringify\("),
    "try/catch": re.compile(r"\btry\s*\{|\bcatch\s*\("),
    "Promise.all*": re.compile(r"Promise\.all(?:Settled)?\("),
    ".map/.filter": re.compile(r"\.(?:map|filter|forEach|join|slice|split)\("),
}


def unescape_patch(body: str) -> str:
    """Decode a patch captured from a JS string literal; template literals pass through."""
    if "\n" in body or "\\n" not in body:
        return body
    try:
        return json.loads('"' + body + '"')
    except json.JSONDecodeError:
        return body.replace("\\n", "\n").replace("\\t", "\t")


def string_spans(source: str, key_re: re.Pattern) -> list[tuple[int, int, str]]:
    spans = []
    for match in key_re.finditer(source):
        start = match.end() - 1
        value, end = js_string_at(source, start)
        spans.append((start, end, value))
    return spans


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--home", default=r"C:\Users\kuh\Desktop\LOCAL-KD")
    parser.add_argument("--subdir", default="sessions")
    parser.add_argument("--until", default="", help="only rollouts whose file name start time is < YYYY-MM-DDTHH-MM-SS")
    args = parser.parse_args()
    home = Path(args.home)
    payloads = home / "rollout-payloads"
    files = sorted((home / args.subdir).rglob("*.jsonl"), key=lambda p: p.name)
    if args.until:
        files = [p for p in files if p.name[8:27] < args.until]

    line_classes = collections.Counter()
    patches = 0
    file_operations = 0
    range_hunks = 0
    hunks = 0
    remainder_bytes = 0
    remainder_string_bytes = 0
    scaffold = collections.Counter()
    cell_kinds = collections.Counter()
    kind_seconds = collections.Counter()
    kind_visible = collections.Counter()
    run_lengths = collections.Counter()
    search_then_read = 0
    search_then_read_seconds = 0.0
    joined_cells = 0
    recovery_only_cells = 0
    recovery_only_seconds = 0.0

    for path in files:
        cells = []
        outputs = {}
        call_cost = {}
        for record in iter_records(path, payloads):
            kind = record.get("type")
            payload = record.get("payload") or {}
            if kind == "response_item" and payload.get("type") == "custom_tool_call" and payload.get("name") == "exec":
                cells.append((payload.get("call_id"), payload.get("input") or ""))
            elif kind == "response_item" and payload.get("type") == "custom_tool_call_output":
                output = payload.get("output")
                outputs[payload.get("call_id")] = output if isinstance(output, str) else json.dumps(output)
            elif kind == "event_msg" and payload.get("type") == "task_complete":
                timing = payload.get("timing") or {}
                generations = {}
                for request in timing.get("modelRequests") or []:
                    usage = request.get("tokenUsage") or {}
                    generations[request.get("generationIndex")] = (
                        (request.get("modelStreamWaitNs") or 0) / 1e9,
                        usage.get("visibleOutputTokens") or 0,
                    )
                for call in timing.get("toolCalls") or []:
                    cost = generations.get(call.get("generationIndex"))
                    if cost and call.get("callId"):
                        call_cost[call["callId"]] = cost

        previous_kind = None
        run = 0
        previous_search_output = None
        for call_id, source in cells:
            patch_spans = []
            for match in PATCH_RE.finditer(source):
                patches += 1
                patch_spans.append((match.start(), match.end()))
                operation = None
                for line in unescape_patch(match.group(0)).split("\n"):
                    size = len(line.encode("utf-8")) + 1
                    if line.startswith("*** "):
                        line_classes["file/envelope headers"] += size
                        if line.startswith(("*** Update File", "*** Add File", "*** Delete File")):
                            file_operations += 1
                            operation = line.split(":", 1)[0][4:]
                    elif line.startswith("@@"):
                        line_classes["hunk headers"] += size
                        hunks += 1
                        if "codex-range" in line:
                            range_hunks += 1
                    elif line.startswith("+"):
                        line_classes["added in Add File" if operation == "Add File" else "added in Update File"] += size
                    elif line.startswith("-"):
                        line_classes["removed"] += size
                    elif line.startswith(" ") or line == "":
                        line_classes["context"] += size
                    else:
                        line_classes["other"] += size
            command_spans = string_spans(source, CMD_KEY_RE)
            spans = sorted(patch_spans + [(a, b) for a, b, _ in command_spans])
            pieces = []
            position = 0
            for start, end in spans:
                if start > position:
                    pieces.append(source[position:start])
                position = max(position, end)
            pieces.append(source[position:])
            remainder = "".join(pieces)
            remainder_bytes += len(remainder.encode("utf-8"))
            index = 0
            while index < len(remainder):
                if remainder[index] in "\"'`":
                    _, end = js_string_at(remainder, index)
                    remainder_string_bytes += len(remainder[index:end].encode("utf-8"))
                    index = end
                else:
                    index += 1
            for name, pattern in SCAFFOLD_PATTERNS.items():
                scaffold[name] += len(pattern.findall(remainder))

            commands = " \n".join(value for _, _, value in command_spans)
            if EDIT_RE.search(source):
                cell_kind = "edit"
            elif VALIDATION_RE.search(commands):
                cell_kind = "validation"
            elif AGENT_RE.search(source):
                cell_kind = "agents"
            elif READ_RE.search(source) or command_spans:
                cell_kind = "discovery"
            else:
                cell_kind = "other"
            cell_kinds[cell_kind] += 1
            cost = call_cost.get(call_id)
            if cost:
                joined_cells += 1
                kind_seconds[cell_kind] += cost[0]
                kind_visible[cell_kind] += cost[1]
            nested_names = set(NESTED_NAME_RE.findall(source))
            if nested_names == {"read_tool_output"} and "exec_command" not in source and "read_file" not in source:
                recovery_only_cells += 1
                if cost:
                    recovery_only_seconds += cost[0]

            if cell_kind == "discovery" and previous_search_output is not None:
                read_paths = [value for _, _, value in string_spans(source, PATH_ARG_RE)]
                normalized_output = previous_search_output.replace("\\\\", "/").replace("\\", "/")
                if any(p and p.replace("\\", "/") in normalized_output for p in read_paths):
                    search_then_read += 1
                    if cost:
                        search_then_read_seconds += cost[0]
            previous_search_output = outputs.get(call_id) if SEARCH_CMD_RE.search(commands) else None

            if cell_kind == "discovery":
                run += 1
            else:
                if run:
                    run_lengths[min(run, 10)] += 1
                run = 0
            previous_kind = cell_kind
        if run:
            run_lengths[min(run, 10)] += 1

    total_patch = sum(line_classes.values())
    report = {
        "files": len(files),
        "patches": patches,
        "patch_file_operations": file_operations,
        "hunks": hunks,
        "codex_range_hunks": range_hunks,
        "patch_line_bytes": dict(line_classes.most_common()),
        "patch_line_share": {k: round(v / total_patch, 4) for k, v in line_classes.most_common()} if total_patch else {},
        "remainder_bytes": remainder_bytes,
        "remainder_string_literal_bytes": remainder_string_bytes,
        "scaffold_occurrences": dict(scaffold.most_common()),
        "cell_kinds": dict(cell_kinds.most_common()),
        "joined_cells": joined_cells,
        "model_seconds_by_cell_kind": {k: round(v, 1) for k, v in kind_seconds.most_common()},
        "visible_output_tokens_by_cell_kind": dict(kind_visible.most_common()),
        "discovery_run_lengths_capped_10": dict(sorted(run_lengths.items())),
        "search_then_read_pairs": search_then_read,
        "search_then_read_model_seconds": round(search_then_read_seconds, 1),
        "recovery_only_cells": recovery_only_cells,
        "recovery_only_model_seconds": round(recovery_only_seconds, 1),
    }
    json.dump(report, sys.stdout, indent=1)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
