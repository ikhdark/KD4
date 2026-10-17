"""Corpus measurements for the 2026-10-16 KD4 architecture audit.

usage: python -I -B architecture_cell_composition_audit.py COMMAND [COMMAND ...]
           --home CODEX_HOME --day 2026/10/14 [--day ...]
           [--cwd-contains Desktop\\kd4] [--json OUT]

COMMAND is one or more of: cells, timing, latency, sources, navigation,
outputs, rust-phases, all. Every rollout under each `sessions/<day>` is read
once; `rollout_payload_artifact` stubs resolve from the same home's
`rollout-payloads`. Without --cwd-contains every rollout is included; with it,
only rollouts whose first `turn_context.cwd` contains that text.

Read-only. Token figures derived from text use the harness estimate
ceil(UTF-8 bytes / 4); provider usage comes from deduplicated
`token_count.last_token_usage` (cells, sources) or the terminal timing
profile's `modelRequests[].tokenUsage` (latency). Cell classes and search
pattern classes are lexical over recorded cell source, so they are estimates.

cells       code-mode cell composition: bytes, embedded patches and their line
            roles, nested tool references, selector kinds, command families,
            discovery-only runs, syntactic boilerplate and re-typed lines.
timing      sums of terminal timing-profile exclusive buckets per workspace.
latency     per-request duration fits against visible/reasoning output and
            input tokens, time to first output, purpose totals, input-size
            distribution and the share of input above fixed ceilings.
sources     provider usage split by session source (root, subagent, exec).
navigation  search cells followed by a read of a file named in the search
            output, and the shape of `rg` patterns.
outputs     model-visible tool-output bytes by cell kind.
rust-phases runner `Rust phase <name>: wall=<s>` lines in tool outputs.
"""

from __future__ import annotations

import argparse
import collections
import json
import math
import pathlib
import re
import shlex
import statistics
import sys

BS = chr(92)
NESTED_TOOL = re.compile(r"\btools\.(\w+)\s*\(")
BARE_HELPER = re.compile(r"(?<![\w.])(read_files|await_command|run_graph|exec_command|resolve_tool)\s*\(")
SELECTOR_KIND = re.compile(r"""kind\s*:\s*["'](\w+)["']""")
PATCH_BLOCK = re.compile(r"\*\*\* Begin Patch.*?\*\*\* End Patch", re.S)
RANGE_HUNK = re.compile(r"@@ codex-range \d+:\d+ sha256:")
READ_PATH = re.compile(r"""read_file\(\s*\{\s*path\s*:\s*["']([^"']+)["']""")
SELECTOR_ERROR = re.compile(r'"status"\s*:\s*"(not_found|ambiguous|invalid)"')
RUST_PHASE = re.compile(r"Rust phase ([a-z/ -]+?): wall=([0-9.]+)s")
RG_CALL = re.compile(r"(?<![\w-])rg(?:\.exe)?\s+([^;|\n]{0,400})")
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_:.]*")
DEFINITION_KEYWORD = re.compile(
    r"^(?:pub(?:\(crate\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait|impl|type|const|static|mod|def|class)\s"
)
RG_VALUE_FLAGS = {"-g", "--glob", "-t", "--type", "-m", "--max-count", "-A", "-B", "-C", "--max-columns"}

FAMILIES = {
    "rg": re.compile(r"(?<![\w-])rg(?:\.exe)?\s"),
    "select_string": re.compile(r"Select-String|findstr|git grep", re.I),
    "get_content": re.compile(r"Get-Content|\bgc\s|\bcat\s|\btype\s", re.I),
    "list_dir": re.compile(r"Get-ChildItem|\bls\s|\bdir\s|\btree\b", re.I),
    "cargo_just": re.compile(r"(?<![\w-])(cargo|just|nextest)\s"),
    "rust_runner": re.compile(r"rust_test_runner|core-test|core-gate"),
    "python": re.compile(r"(?<![\w-])(python|py)(?:\.exe)?\s", re.I),
    "git": re.compile(r"(?<![\w-])git\s"),
    "node": re.compile(r"(?<![\w-])(node|pnpm|npm|npx)\s"),
}
EMBEDDED_PY = re.compile(r"@'.*?python|python[^\n\"]{0,40}\s-c\s|\|\s*python", re.I | re.S)
EMBEDDED_HERESTRING = re.compile(r"@'\\n|@\"\\n|@'\n|@\"\n")
ESCAPED_SEP = "(?:" + BS * 4 + "|" + BS * 2 + ")"
BOILERPLATE = {
    "await_tools_prefix": re.compile(r"await\s+tools\."),
    "text_wrapper": re.compile(r"\btext\("),
    "escaped_absolute_windows_path": re.compile(r"[A-Za-z]:" + ESCAPED_SEP + r"[^\"'`\s,)]+"),
    "max_output_tokens_argument": re.compile(r"\"?max_output_tokens\"?\s*:\s*\d+"),
    "selector_object": re.compile(r"\{\s*kind\s*:\s*\"[a-z_]+\"[^{}]*\}"),
    "status_check": re.compile(r"if\s*\([^)]*(?:exit_code|status|success)[^)]*\)\s*(?:\{[^}]*\}|throw[^;]*;)"),
}
RETYPED_MIN_LINE_BYTES = 30
OUTPUT_CEILINGS = (100_000, 150_000, 200_000, 300_000)
MAX_REQUEST_SECONDS = 900


def est_tokens(nbytes: int) -> int:
    return math.ceil(nbytes / 4)


def percentile(sorted_values: list[float], fraction: float) -> float | None:
    if not sorted_values:
        return None
    return sorted_values[int(fraction * (len(sorted_values) - 1))]


def resolve(home: pathlib.Path, record: dict) -> tuple[str | None, dict]:
    if record.get("type") == "rollout_payload_artifact":
        payload = record.get("payload") or {}
        path = home / "rollout-payloads" / f"{payload.get('sha256')}.json"
        try:
            body = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return "missing", {}
        return payload.get("item_type"), body
    return record.get("type"), record


def patch_line_bytes(patches: list[str]) -> collections.Counter:
    """Bytes per patch line role; escaped newlines inside JS strings are split too."""
    roles = collections.Counter()
    for patch in patches:
        for line in re.split(r"\\n|\n", patch):
            size = len(line.encode("utf-8")) + 1
            if line.startswith("***"):
                roles["header"] += size
            elif line.startswith("@@"):
                roles["hunk_header"] += size
            elif line.startswith("+"):
                roles["added"] += size
            elif line.startswith("-"):
                roles["removed"] += size
            else:
                roles["context"] += size
    return roles


def classify_cell(src: str) -> dict:
    nested = collections.Counter(NESTED_TOOL.findall(src))
    for name in BARE_HELPER.findall(src):
        nested[name] += 1
    patches = PATCH_BLOCK.findall(src)
    families = {k: len(rx.findall(src)) for k, rx in FAMILIES.items()}
    selector_kinds = collections.Counter(SELECTOR_KIND.findall(src))
    has_edit = bool(patches) or nested.get("apply_patch", 0) > 0
    has_build = families["cargo_just"] > 0 or families["rust_runner"] > 0
    reads = nested.get("read_file", 0) + nested.get("read_files", 0) + families["get_content"]
    searches = families["rg"] + families["select_string"] + selector_kinds.get("search", 0)
    lists = nested.get("list_files", 0) + families["list_dir"]
    return {
        "bytes": len(src.encode("utf-8")),
        "patch_bytes": sum(len(p.encode("utf-8")) for p in patches),
        "patch_roles": patch_line_bytes(patches),
        "patch_count": len(patches),
        "range_hunks": len(RANGE_HUNK.findall(src)),
        "nested": nested,
        "selector_kinds": selector_kinds,
        "families": families,
        "embedded_python": bool(EMBEDDED_PY.search(src)),
        "herestring": bool(EMBEDDED_HERESTRING.search(src)),
        "has_edit": has_edit,
        "has_build": has_build,
        "discovery_only": (not has_edit) and (not has_build) and (reads + searches + lists) > 0,
        "has_search": searches > 0,
        "whole_file_read": bool(re.search(r"full\s*:\s*true", src))
        or bool(re.search(r"read_file\(\s*\{\s*path\s*:\s*\"[^\"]+\"\s*\}\s*\)", src)),
    }


def session_source(meta: dict) -> str:
    source = meta.get("source")
    if isinstance(source, dict):
        return "subagent" if "subagent" in json.dumps(source).lower() else "other"
    return source if isinstance(source, str) else "unknown"


def load_rollout(home: pathlib.Path, path: pathlib.Path) -> dict:
    """One pass over a rollout, keeping only what the commands need."""
    data = {
        "path": path,
        "cwd": None,
        "source": "unknown",
        "cells": [],  # dicts with call_id, turn_id, src, out
        "assistant_message_bytes": 0,
        "tool_output_bytes": 0,
        "usage": [],
        "timings": [],
    }
    by_call: dict[str, dict] = {}
    last_usage = None
    turn = None
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            kind, body = resolve(home, rec)
            payload = body.get("payload", body) if isinstance(body, dict) else {}
            if not isinstance(payload, dict):
                continue
            if kind == "session_meta":
                data["source"] = session_source(payload)
            elif kind == "turn_context":
                data["cwd"] = data["cwd"] or payload.get("cwd")
                turn = payload.get("turn_id") or turn
            elif kind == "event_msg":
                ptype = payload.get("type")
                if ptype == "token_count":
                    last = (payload.get("info") or {}).get("last_token_usage")
                    if last and last != last_usage:
                        data["usage"].append(last)
                        last_usage = last
                elif ptype in ("task_complete", "turn_aborted") and payload.get("timing"):
                    data["timings"].append(payload["timing"])
            elif kind == "response_item":
                ptype = payload.get("type")
                if ptype == "custom_tool_call" and payload.get("name") == "exec":
                    cell = {"call_id": payload.get("call_id"), "turn_id": turn, "src": payload.get("input") or "", "out": ""}
                    data["cells"].append(cell)
                    by_call[cell["call_id"]] = cell
                elif ptype in ("custom_tool_call_output", "function_call_output"):
                    out = payload.get("output")
                    text = out if isinstance(out, str) else json.dumps(out)
                    data["tool_output_bytes"] += len(text.encode("utf-8"))
                    if payload.get("call_id") in by_call:
                        by_call[payload["call_id"]]["out"] = text
                elif ptype == "message" and payload.get("role") == "assistant":
                    for part in payload.get("content") or []:
                        if isinstance(part, dict) and isinstance(part.get("text"), str):
                            data["assistant_message_bytes"] += len(part["text"].encode("utf-8"))
    for cell in data["cells"]:
        cell["class"] = classify_cell(cell["src"])
    return data


def report_cells(rollouts: list[dict]) -> dict:
    agg = collections.Counter()
    nested_total = collections.Counter()
    selector_total = collections.Counter()
    family_cells = collections.Counter()
    patch_roles = collections.Counter()
    class_bytes = collections.Counter()
    class_cells = collections.Counter()
    boilerplate = collections.Counter()
    cell_bytes: list[int] = []
    run_lengths: list[int] = []
    non_patch_bytes = 0
    retyped = collections.Counter()
    for data in rollouts:
        run = 0
        turn = None
        seen_lines: set[str] = set()
        for cell in data["cells"]:
            c = cell["class"]
            if cell["turn_id"] != turn:
                if run:
                    run_lengths.append(run)
                run = 0
                turn = cell["turn_id"]
            if c["discovery_only"]:
                run += 1
            else:
                if run:
                    run_lengths.append(run)
                run = 0
            agg["cells"] += 1
            agg["cell_bytes"] += c["bytes"]
            agg["patch_bytes"] += c["patch_bytes"]
            agg["patch_blocks"] += c["patch_count"]
            agg["range_hunks"] += c["range_hunks"]
            agg["edit_cells"] += c["has_edit"]
            agg["build_cells"] += c["has_build"]
            agg["discovery_only_cells"] += c["discovery_only"]
            agg["search_cells"] += c["has_search"]
            agg["embedded_python_cells"] += c["embedded_python"]
            agg["herestring_cells"] += c["herestring"]
            cell_bytes.append(c["bytes"])
            patch_roles.update(c["patch_roles"])
            label = (
                "edit" if c["has_edit"] else "build" if c["has_build"]
                else "embedded_python" if c["embedded_python"]
                else "discovery_only" if c["discovery_only"] else "other"
            )
            class_bytes[label] += c["bytes"]
            class_cells[label] += 1
            nested_total.update(c["nested"])
            selector_total.update(c["selector_kinds"])
            for fam, n in c["families"].items():
                if n:
                    family_cells[fam] += 1
            stripped = PATCH_BLOCK.sub("", cell["src"])
            non_patch_bytes += len(stripped.encode("utf-8"))
            for name, rx in BOILERPLATE.items():
                for m in rx.finditer(stripped):
                    boilerplate[name] += len(m.group(0).encode("utf-8"))
            for raw in re.split(r"\n|" + BS * 2 + "n", stripped):
                text = raw.strip()
                size = len(text.encode("utf-8"))
                if size >= RETYPED_MIN_LINE_BYTES:
                    if text in seen_lines:
                        retyped["bytes"] += size
                        retyped["lines"] += 1
                    seen_lines.add(text)
        if run:
            run_lengths.append(run)
        agg["assistant_message_bytes"] += data["assistant_message_bytes"]
        for u in data["usage"]:
            agg["requests"] += 1
            agg["input_tokens"] += u.get("input_tokens", 0)
            agg["cached_input_tokens"] += u.get("cached_input_tokens", 0)
            agg["output_tokens"] += u.get("output_tokens", 0)
            agg["reasoning_tokens"] += u.get("reasoning_output_tokens", 0)
    runs = collections.Counter(run_lengths)
    sorted_bytes = sorted(cell_bytes)
    return {
        "totals": dict(agg),
        "visible_output_tokens": agg["output_tokens"] - agg["reasoning_tokens"],
        "estimated_cell_tokens": est_tokens(agg["cell_bytes"]),
        "estimated_patch_tokens": est_tokens(agg["patch_bytes"]),
        "estimated_assistant_message_tokens": est_tokens(agg["assistant_message_bytes"]),
        "cell_bytes_median": statistics.median(cell_bytes) if cell_bytes else None,
        "cell_bytes_p90": percentile(sorted_bytes, 0.9),
        "patch_line_role_bytes": dict(patch_roles.most_common()),
        "cell_class_bytes": dict(class_bytes.most_common()),
        "cell_class_counts": dict(class_cells.most_common()),
        "nested_tool_references": dict(nested_total.most_common(20)),
        "selector_kinds": dict(selector_total.most_common()),
        "cells_naming_command_family": dict(family_cells.most_common()),
        "discovery_run_length_histogram": dict(sorted(runs.items())),
        "discovery_runs": sum(runs.values()),
        "discovery_cells_after_first_in_run": sum((k - 1) * v for k, v in runs.items()),
        "non_patch_cell_bytes": non_patch_bytes,
        "boilerplate_bytes": dict(boilerplate.most_common()),
        "retyped_line_bytes": retyped["bytes"],
        "retyped_lines": retyped["lines"],
    }


def report_timing(rollouts: list[dict]) -> dict:
    exclusive = collections.Counter()
    local = collections.Counter()
    turns = 0
    retry_turns = []
    for data in rollouts:
        for timing in data["timings"]:
            if not timing.get("exclusive"):
                continue
            turns += 1
            exclusive["inclusiveNs"] += timing.get("inclusiveDurationNs", 0)
            for key, value in timing["exclusive"].items():
                exclusive[key] += value
            for key, value in (timing.get("local") or {}).items():
                local[key] += value
            retry = timing["exclusive"].get("retryOnlyNs", 0)
            if retry:
                retry_turns.append({"rollout": data["path"].name, "retry_only_seconds": retry / 1e9})
    inclusive = exclusive["inclusiveNs"] or 1
    return {
        "terminal_turns": turns,
        "exclusive_seconds": {k: round(v / 1e9, 1) for k, v in exclusive.most_common()},
        "exclusive_share_percent": {k: round(100 * v / inclusive, 2) for k, v in exclusive.most_common() if k != "inclusiveNs"},
        "local_union_seconds": {k: round(v / 1e9, 1) for k, v in local.most_common(10)},
        "turns_with_retry_only_time": sorted(retry_turns, key=lambda r: -r["retry_only_seconds"])[:10],
    }


def solve(matrix: list[list[float]], vector: list[float]) -> list[float]:
    """Gaussian elimination with partial pivoting for small normal equations."""
    n = len(vector)
    rows = [row[:] + [value] for row, value in zip(matrix, vector)]
    for i in range(n):
        pivot = max(range(i, n), key=lambda r: abs(rows[r][i]))
        rows[i], rows[pivot] = rows[pivot], rows[i]
        if rows[i][i] == 0:
            raise ValueError("singular normal equations")
        for r in range(n):
            if r != i:
                factor = rows[r][i] / rows[i][i]
                rows[r] = [a - factor * b for a, b in zip(rows[r], rows[i])]
    return [rows[i][n] / rows[i][i] for i in range(n)]


def ols(features: list[list[float]], target: list[float]) -> list[float]:
    width = len(features[0])
    xtx = [[sum(x[i] * x[j] for x in features) for j in range(width)] for i in range(width)]
    xty = [sum(x[i] * y for x, y in zip(features, target)) for i in range(width)]
    return solve(xtx, xty)


def report_latency(rollouts: list[dict]) -> dict:
    rows = []
    for data in rollouts:
        for timing in data["timings"]:
            for req in timing.get("modelRequests") or []:
                usage = req.get("tokenUsage") or {}
                dispatch, first, done = req.get("dispatchMs"), req.get("firstModelOutputMs"), req.get("completedMs")
                if dispatch is None or done is None or not usage:
                    continue
                duration = (done - dispatch) / 1000
                if not 0 < duration < MAX_REQUEST_SECONDS:
                    continue
                visible = usage.get("visibleOutputTokens", 0)
                reasoning = usage.get("reasoningTokens", 0)
                rows.append({
                    "duration": duration,
                    "ttft": (first - dispatch) / 1000 if first is not None and first >= dispatch else None,
                    "visible": visible,
                    "reasoning": reasoning,
                    "output": req.get("outputTokens") or visible + reasoning,
                    "input": usage.get("inputTokens", 0),
                    "cached": usage.get("cachedInputTokens", 0),
                    "purpose": req.get("generationPurpose"),
                })
    if len(rows) < 10:
        return {"requests": len(rows)}
    durations = [r["duration"] for r in rows]
    split = ols([[1, r["visible"], r["reasoning"], r["input"] / 1e5] for r in rows], durations)
    single = ols([[1, r["output"], r["input"] / 1e5] for r in rows], durations)
    ttft_rows = [r for r in rows if r["ttft"] is not None]
    ttft = sorted(r["ttft"] for r in ttft_rows)
    ttft_fit = ols([[1, r["reasoning"]] for r in ttft_rows], [r["ttft"] for r in ttft_rows])
    decode = sorted(
        (r["duration"] - r["ttft"]) / r["output"] * 1000
        for r in ttft_rows
        if r["output"] > 200 and r["duration"] > r["ttft"]
    )
    by_purpose = collections.defaultdict(list)
    for r in rows:
        by_purpose[r["purpose"]].append(r)
    inputs = sorted(r["input"] for r in rows)
    total_input = sum(inputs)
    return {
        "requests": len(rows),
        "fit_split_seconds": {
            "intercept": round(split[0], 2),
            "per_visible_token_ms": round(split[1] * 1000, 1),
            "per_reasoning_token_ms": round(split[2] * 1000, 1),
            "per_100k_input": round(split[3], 2),
        },
        "fit_single_slope_seconds": {
            "intercept": round(single[0], 2),
            "per_output_token_ms": round(single[1] * 1000, 1),
            "per_100k_input": round(single[2], 2),
        },
        "ttft_median_seconds": round(statistics.median(ttft), 2),
        "ttft_p90_seconds": round(percentile(ttft, 0.9), 2),
        "ttft_fit": {"intercept": round(ttft_fit[0], 2), "per_reasoning_token_ms": round(ttft_fit[1] * 1000, 1)},
        "post_first_output_ms_per_token_median": round(statistics.median(decode), 1) if decode else None,
        "duration_median_seconds": round(statistics.median(durations), 2),
        "duration_mean_seconds": round(statistics.mean(durations), 2),
        "duration_sum_seconds": round(sum(durations)),
        "output_tokens_median": statistics.median(r["output"] for r in rows),
        "reasoning_share_of_output_percent": round(100 * sum(r["reasoning"] for r in rows) / max(1, sum(r["output"] for r in rows)), 1),
        "totals": {
            "input_tokens": total_input,
            "uncached_input_tokens": sum(r["input"] - r["cached"] for r in rows),
            "visible_output_tokens": sum(r["visible"] for r in rows),
            "reasoning_tokens": sum(r["reasoning"] for r in rows),
        },
        "input_tokens_p10_p50_p90_mean": [
            percentile(inputs, 0.1), percentile(inputs, 0.5), percentile(inputs, 0.9), round(total_input / len(inputs)),
        ],
        "input_above_ceiling": {
            str(cap): {
                "requests_above_percent": round(100 * sum(1 for x in inputs if x > cap) / len(inputs), 1),
                "tokens_above_percent_of_input": round(100 * sum(x - cap for x in inputs if x > cap) / total_input, 1),
            }
            for cap in OUTPUT_CEILINGS
        },
        "by_purpose": {
            purpose: {
                "requests": len(items),
                "sum_seconds": round(sum(i["duration"] for i in items)),
                "median_seconds": round(statistics.median(i["duration"] for i in items), 1),
                "median_visible_tokens": statistics.median(i["visible"] for i in items),
                "median_reasoning_tokens": statistics.median(i["reasoning"] for i in items),
            }
            for purpose, items in sorted(by_purpose.items(), key=lambda kv: -sum(i["duration"] for i in kv[1]))
        },
    }


def report_sources(rollouts: list[dict]) -> dict:
    agg = collections.defaultdict(collections.Counter)
    for data in rollouts:
        bucket = agg[data["source"]]
        bucket["rollouts"] += 1
        for u in data["usage"]:
            bucket["requests"] += 1
            bucket["input_tokens"] += u.get("input_tokens", 0)
            bucket["cached_input_tokens"] += u.get("cached_input_tokens", 0)
            bucket["output_tokens"] += u.get("output_tokens", 0)
            bucket["reasoning_tokens"] += u.get("reasoning_output_tokens", 0)
    total_input = sum(v["input_tokens"] for v in agg.values()) or 1
    total_output = sum(v["output_tokens"] for v in agg.values()) or 1
    return {
        source: dict(
            counts,
            input_share_percent=round(100 * counts["input_tokens"] / total_input, 1),
            output_share_percent=round(100 * counts["output_tokens"] / total_output, 1),
            mean_input_per_request=round(counts["input_tokens"] / counts["requests"]) if counts["requests"] else None,
        )
        for source, counts in sorted(agg.items(), key=lambda kv: -kv[1]["input_tokens"])
    }


def normalize_path(text: str) -> str:
    return text.replace(BS * 2, "/").replace(BS, "/").lower()


def rg_pattern_class(argument: str) -> str:
    argument = argument.replace(BS + '"', '"').replace(BS * 2, BS)
    if re.search(r"(?:^|\s)(-l|--files|--files-with-matches|-c|--count)(\s|$)", argument):
        return "files_or_count_mode"
    try:
        tokens = shlex.split(argument, posix=True)
    except ValueError:
        tokens = argument.split()
    pattern = None
    i = 0
    while i < len(tokens):
        token = tokens[i]
        if token in ("-e", "--regexp") and i + 1 < len(tokens):
            pattern = tokens[i + 1]
            break
        if token in RG_VALUE_FLAGS:
            i += 2
            continue
        if token.startswith("-"):
            i += 1
            continue
        pattern = token
        break
    if pattern is None:
        return "unparsed"
    pattern = pattern.strip(chr(39) + chr(34))
    alternatives = [x.strip() for x in re.sub(r"\\b|\^|\$|\\s\+|\\s\*", "", pattern).split("|") if x.strip()]
    if any(DEFINITION_KEYWORD.match(x) for x in alternatives):
        return "definition_keyword"
    if alternatives and all(IDENTIFIER.fullmatch(x.rstrip("(").rstrip(BS)) for x in alternatives):
        return "identifiers_only"
    if any(" " in x for x in alternatives):
        return "phrase"
    return "other_regex"


def report_navigation(rollouts: list[dict]) -> dict:
    counts = collections.Counter()
    rg_classes = collections.Counter()
    for data in rollouts:
        cells = data["cells"]
        for i, cell in enumerate(cells):
            c = cell["class"]
            counts["cells"] += 1
            if c["selector_kinds"].get("symbol"):
                counts["cells_with_symbol_selector"] += 1
                counts["selector_error_statuses_in_symbol_cells"] += len(SELECTOR_ERROR.findall(cell["out"]))
            for m in RG_CALL.finditer(cell["src"]):
                rg_classes[rg_pattern_class(m.group(1))] += 1
            if not c["has_search"]:
                continue
            counts["search_cells"] += 1
            if i + 1 >= len(cells):
                continue
            nxt = cells[i + 1]
            if nxt["class"]["has_search"]:
                counts["search_then_search"] += 1
            output = normalize_path(cell["out"])
            names = [normalize_path(p).split("/")[-1] for p in READ_PATH.findall(nxt["src"])]
            if any(len(name) > 3 and name in output for name in names):
                counts["search_then_read_of_file_named_in_output"] += 1
            if c["families"]["rg"] and nxt["class"]["discovery_only"] and (
                nxt["class"]["nested"].get("read_file") or nxt["class"]["nested"].get("read_files")
            ):
                counts["rg_cell_then_discovery_read_cell"] += 1
    return {"counts": dict(counts), "rg_pattern_classes": dict(rg_classes.most_common()), "rg_invocations": sum(rg_classes.values())}


def output_kind(c: dict) -> str:
    nested = c["nested"]
    reads = nested.get("read_file") or nested.get("read_files")
    if c["has_build"]:
        return "build/test"
    if c["embedded_python"]:
        return "embedded python"
    if nested.get("read_tool_output"):
        return "artifact recovery"
    if reads and not nested.get("exec_command"):
        return "whole-file read" if c["whole_file_read"] else "selector read"
    if nested.get("exec_command") and c["has_search"] and not reads:
        return "shell search"
    if nested.get("exec_command"):
        return "shell other/mixed"
    return "other"


def report_outputs(rollouts: list[dict]) -> dict:
    size = collections.Counter()
    cells = collections.Counter()
    for data in rollouts:
        for cell in data["cells"]:
            kind = output_kind(cell["class"])
            size[kind] += len(cell["out"].encode("utf-8"))
            cells[kind] += 1
    total = sum(size.values()) or 1
    return {
        kind: {
            "cells": cells[kind],
            "output_bytes": nbytes,
            "share_percent": round(100 * nbytes / total, 1),
            "mean_bytes_per_cell": round(nbytes / cells[kind]),
        }
        for kind, nbytes in size.most_common()
    }


def report_rust_phases(rollouts: list[dict]) -> dict:
    phases = collections.defaultdict(list)
    for data in rollouts:
        for cell in data["cells"]:
            seen = set()
            for m in RUST_PHASE.finditer(cell["out"]):
                key = (m.group(1).strip(), m.group(2))
                if key in seen:
                    continue
                seen.add(key)
                phases[key[0]].append(float(key[1]))
    result = {}
    for phase, values in sorted(phases.items(), key=lambda kv: -sum(kv[1])):
        values.sort()
        result[phase] = {
            "runs": len(values),
            "sum_seconds": round(sum(values)),
            "median_seconds": round(statistics.median(values), 1),
            "p90_seconds": round(percentile(values, 0.9), 1),
            "max_seconds": round(values[-1], 1),
        }
    return result


REPORTS = {
    "cells": report_cells,
    "timing": report_timing,
    "latency": report_latency,
    "sources": report_sources,
    "navigation": report_navigation,
    "outputs": report_outputs,
    "rust-phases": report_rust_phases,
}


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("commands", nargs="+", choices=[*REPORTS, "all"])
    parser.add_argument("--home", required=True, type=pathlib.Path)
    parser.add_argument("--day", action="append", required=True)
    parser.add_argument("--cwd-contains")
    parser.add_argument("--json", type=pathlib.Path)
    args = parser.parse_args(argv)
    commands = list(REPORTS) if "all" in args.commands else args.commands
    files = [p for day in args.day for p in sorted((args.home / "sessions" / day).glob("rollout-*.jsonl"))]
    rollouts = []
    for path in files:
        try:
            data = load_rollout(args.home, path)
        except OSError:
            continue
        if args.cwd_contains and (not data["cwd"] or args.cwd_contains.lower() not in data["cwd"].lower()):
            continue
        rollouts.append(data)
    report = {"files_scanned": len(files), "rollouts_matched": len(rollouts), "cwd_contains": args.cwd_contains}
    for command in commands:
        report[command] = REPORTS[command](rollouts)
    text = json.dumps(report, indent=2, default=str)
    if args.json:
        args.json.write_text(text, encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
