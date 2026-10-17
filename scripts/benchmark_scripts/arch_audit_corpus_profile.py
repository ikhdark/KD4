#!/usr/bin/env python3
"""Corpus profile for the 2026-10-16 architecture-alternatives audit.

Read-only. Walks saved rollouts (resolving `rollout_payload_artifact` stubs from
the same home's `rollout-payloads/`) and reports, as one JSON document:

* per-request latency against token counts, from `task_complete.timing.modelRequests`
  (ordinary least squares; an association, not a causal decomposition);
* code-mode cell composition (patch payload, command literals, other source);
* nested tool references inside cells;
* executed commands from `item_completed/CommandExecution`, grouped by program
  family, with exit status, duration and output size;
* validation commands and compile-failure markers in their output;
* `read_file` selector kinds referenced by cells;
* compaction records;
* provider usage by session source (root, sub-agent, exec) from deduplicated
  `token_count` events, with sub-agent context sizes;
* request latency by recorded service tier.

Token counts are provider-reported where named `*Tokens`; byte counts are UTF-8
bytes of recorded text. Nothing is executed and no input is modified.
"""

from __future__ import annotations

import argparse
import collections
import json
import math
import os
import re
import sys
from pathlib import Path

PATCH_RE = re.compile(r"\*\*\* Begin Patch.*?\*\*\* End Patch", re.S)
CMD_KEY_RE = re.compile(r"""(?:\bcmd|"cmd"|'cmd'|\bscript_body|"script_body")\s*:\s*(["'`])""")
NESTED_RE = re.compile(r"\btools\.([A-Za-z_]\w*)(?:\.([A-Za-z_]\w*))?\s*\(")
HELPER_RE = re.compile(
    r"(?<![\w.])(read_files|await_command|run_graph|resolve_tool|store|load|notify|"
    r"yield_control|listKeys|deleteStored|exec_command|image)\s*\("
)
KIND_RE = re.compile(r"""kind\s*["']?\s*:\s*["'](\w+)["']""")
READ_FILE_CALL_RE = re.compile(r"\b(?:tools\.)?read_files?\s*\(")
COMPILE_FAIL_RE = re.compile(r"error\[E\d{4}\]|error: could not compile|error: aborting due to")
TEST_FAIL_RE = re.compile(r"test result: FAILED|\bFAIL \[|panicked at|AssertionError|FAILED \(")

PS_FAMILIES = {
    "get-content": "ps-read", "gc": "ps-read", "cat": "ps-read", "type": "ps-read",
    "select-string": "ps-search", "sls": "ps-search",
    "get-childitem": "ps-list", "gci": "ps-list", "ls": "ps-list", "dir": "ps-list",
    "test-path": "ps-test-path", "resolve-path": "ps-test-path",
    "set-content": "ps-write", "out-file": "ps-write", "add-content": "ps-write",
    "remove-item": "ps-fs-mutate", "move-item": "ps-fs-mutate", "copy-item": "ps-fs-mutate",
    "new-item": "ps-fs-mutate", "rename-item": "ps-fs-mutate",
    "get-process": "ps-process", "stop-process": "ps-process", "start-process": "ps-process",
    "get-item": "ps-read", "measure-object": "ps-other", "write-output": "ps-other",
    "foreach": "ps-script", "if": "ps-script", "try": "ps-script", "function": "ps-script",
    "param": "ps-script", "set-location": "ps-script", "cd": "ps-script", "push-location": "ps-script",
}


def iter_records(path: Path, payloads: Path):
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if record.get("type") == "rollout_payload_artifact":
                stub = record.get("payload") or {}
                if stub.get("item_type") not in ("event_msg", "response_item", "session_meta"):
                    continue
                try:
                    with (payloads / f"{stub['sha256']}.json").open(encoding="utf-8") as artifact:
                        resolved = json.load(artifact)
                except (OSError, KeyError, json.JSONDecodeError):
                    yield {"type": "missing_artifact", "payload": stub, "timestamp": record.get("timestamp")}
                    continue
                resolved.setdefault("timestamp", record.get("timestamp"))
                yield resolved
            else:
                yield record


def js_string_at(source: str, start: int) -> tuple[str, int]:
    quote = source[start]
    index = start + 1
    out = []
    while index < len(source):
        char = source[index]
        if char == "\\" and index + 1 < len(source):
            nxt = source[index + 1]
            out.append({"n": "\n", "t": "\t", "r": "\r"}.get(nxt, nxt))
            index += 2
            continue
        if char == quote:
            return "".join(out), index + 1
        out.append(char)
        index += 1
    return "".join(out), index


def quantiles(values, qs=(0.5, 0.9, 0.99)):
    if not values:
        return {}
    ordered = sorted(values)
    result = {}
    for q in qs:
        position = (len(ordered) - 1) * q
        low = math.floor(position)
        high = math.ceil(position)
        result[f"p{int(q * 100)}"] = ordered[low] + (ordered[high] - ordered[low]) * (position - low)
    return result


def ols(rows, names):
    """Least squares with intercept via normal equations (Gaussian elimination)."""
    k = len(names) + 1
    xtx = [[0.0] * k for _ in range(k)]
    xty = [0.0] * k
    ys = []
    for features, target in rows:
        vector = [1.0, *features]
        ys.append(target)
        for i in range(k):
            xty[i] += vector[i] * target
            for j in range(k):
                xtx[i][j] += vector[i] * vector[j]
    augmented = [row[:] + [xty[i]] for i, row in enumerate(xtx)]
    for col in range(k):
        pivot = max(range(col, k), key=lambda r: abs(augmented[r][col]))
        if abs(augmented[pivot][col]) < 1e-12:
            return None
        augmented[col], augmented[pivot] = augmented[pivot], augmented[col]
        for r in range(k):
            if r != col:
                factor = augmented[r][col] / augmented[col][col]
                for c in range(col, k + 1):
                    augmented[r][c] -= factor * augmented[col][c]
    beta = [augmented[i][k] / augmented[i][i] for i in range(k)]
    mean = sum(ys) / len(ys)
    ss_tot = sum((y - mean) ** 2 for y in ys)
    ss_res = 0.0
    for features, target in rows:
        predicted = beta[0] + sum(b * x for b, x in zip(beta[1:], features))
        ss_res += (target - predicted) ** 2
    return {
        "n": len(rows),
        "intercept_s": beta[0],
        "coefficients_s_per_unit": dict(zip(names, beta[1:])),
        "r2": 1 - ss_res / ss_tot if ss_tot else None,
    }


def shell_script(argv):
    if not argv:
        return None, ""
    exe = os.path.basename(str(argv[0]).replace("\\", "/")).lower()
    if exe in ("pwsh.exe", "pwsh", "powershell.exe", "powershell"):
        lowered = [str(a).lower() for a in argv]
        for flag in ("-command", "-c"):
            if flag in lowered:
                return "powershell", str(argv[lowered.index(flag) + 1]) if lowered.index(flag) + 1 < len(argv) else ""
        if "-encodedcommand" in lowered:
            return "powershell-encoded", ""
        return "powershell", " ".join(str(a) for a in argv[1:])
    if exe in ("cmd.exe", "cmd"):
        return "cmd", " ".join(str(a) for a in argv[2:])
    if exe in ("bash", "bash.exe", "sh", "sh.exe"):
        return "bash", str(argv[-1])
    return "direct", " ".join(str(a) for a in argv)


def classify_script(script: str) -> tuple[str, str]:
    text = script.lstrip().lstrip("\ufeff")
    while text.startswith("#"):
        text = text.split("\n", 1)[1].lstrip() if "\n" in text else ""
    if text.startswith("@'") or text.startswith('@"'):
        terminator = "'@" if text.startswith("@'") else '"@'
        tail = text[text.find(terminator, 2) + 2:] if text.find(terminator, 2) >= 0 else ""
        tail_lower = tail.lower()
        if re.search(r"\|\s*(python|py|python3)(\.exe)?\b", tail_lower):
            return "python-inline", "here-string"
        if re.search(r"\|\s*node(\.exe)?\b", tail_lower):
            return "node-inline", "here-string"
        if "set-content" in tail_lower or "out-file" in tail_lower:
            return "ps-write", "here-string"
        return "ps-herestring-other", ""
    if text.startswith("&"):
        text = text[1:].lstrip()
    if text.startswith("$"):
        match = re.match(r"\$[\w:]+\s*=\s*(.*)", text, re.S)
        if match and not match.group(1).lstrip().startswith(("@(", "@{", "[", "$", "'", '"', "(")):
            inner, sub = classify_script(match.group(1))
            if inner not in ("other", "ps-script"):
                return inner, sub
        return "ps-script", ""
    token_match = re.match(r"""(["'])(.*?)\1|([^\s;|()]+)""", text)
    if not token_match:
        return "other", ""
    token = token_match.group(2) or token_match.group(3) or ""
    base = os.path.basename(token.replace("\\", "/")).lower()
    if base.endswith(".exe"):
        base = base[:-4]
    rest = text[token_match.end():].lstrip()
    second = re.match(r"[^\s;|]+", rest)
    sub = second.group(0).lower() if second else ""
    if base == "rg":
        return "rg", ""
    if base == "git":
        return "git", sub
    if base == "cargo":
        return "cargo", sub
    if base == "just":
        return "just", sub
    if base in ("python", "python3", "py"):
        if sub == "-c" or " -c " in rest[:12]:
            return "python-c", ""
        if sub in ("-", "-b", "-i") and "-" in rest.split()[:3]:
            return "python-inline", "stdin"
        script_name = next((part for part in rest.split() if part.endswith(".py")), "")
        return "python-script", os.path.basename(script_name.replace("\\", "/"))
    if base in ("node", "npm", "pnpm", "npx"):
        return "node", base
    if base in PS_FAMILIES:
        return PS_FAMILIES[base], base
    if "-" in base and base[0].isalpha():
        return "ps-cmdlet-other", base
    return "other", base


VALIDATION_PATTERNS = [
    ("cargo-nextest", re.compile(r"\bcargo\s+(?:\+\S+\s+)?nextest\b")),
    ("cargo-test", re.compile(r"\bcargo\s+(?:\+\S+\s+)?test\b")),
    ("cargo-check", re.compile(r"\bcargo\s+(?:\+\S+\s+)?(?:check|clippy)\b")),
    ("cargo-build", re.compile(r"\bcargo\s+(?:\+\S+\s+)?build\b")),
    ("just-core", re.compile(r"\bjust\s+(?:core-test|core-gate|test|core-test-fast|fmt|fix|clippy)")),
    ("rust-test-runner", re.compile(r"rust_test_runner\.py")),
    ("python-tests", re.compile(r"\b(?:pytest|unittest|test-python|root_maintenance\.py\s+test)")),
    ("schema-check", re.compile(r"\bjust\s+\S*schema")),
]


def validation_kind(script: str):
    for name, pattern in VALIDATION_PATTERNS:
        if pattern.search(script):
            return name
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--home", default=r"C:\Users\kuh\Desktop\LOCAL-KD")
    parser.add_argument("--subdir", default="sessions")
    parser.add_argument("--since", default="", help="only rollouts whose file name date is >= YYYY-MM-DD")
    parser.add_argument("--until", default="", help="only rollouts whose file name start time is < YYYY-MM-DDTHH-MM-SS")
    args = parser.parse_args()
    home = Path(args.home)
    payloads = home / "rollout-payloads"
    files = sorted(
        [*(home / args.subdir).rglob("*.jsonl")],
        key=lambda p: p.name,
    )
    if args.since:
        files = [p for p in files if p.name[8:18] >= args.since]
    if args.until:
        files = [p for p in files if p.name[8:27] < args.until]

    requests = []
    purpose = collections.defaultdict(lambda: collections.Counter())
    cells = []
    nested = collections.Counter()
    helpers = collections.Counter()
    selector_kinds = collections.Counter()
    read_calls_without_selectors = 0
    read_calls = 0
    commands = collections.defaultdict(lambda: collections.Counter())
    command_durations = collections.defaultdict(list)
    command_sub = collections.Counter()
    validation = collections.defaultdict(lambda: collections.Counter())
    validation_durations = collections.defaultdict(list)
    shells = collections.Counter()
    inline_python_bytes = 0
    compactions = 0
    missing_artifacts = 0
    tier_requests = collections.Counter()
    tier_seconds = collections.defaultdict(list)
    by_source = collections.defaultdict(collections.Counter)
    child_request_input = []
    child_first_input = []
    child_requests_per_file = []

    for path in files:
        tier = None
        session_source = "unknown"
        last_total = None
        file_requests = 0
        for record in iter_records(path, payloads):
            kind = record.get("type")
            payload = record.get("payload") or {}
            if kind == "missing_artifact":
                missing_artifacts += 1
                continue
            if kind == "compacted":
                compactions += 1
                continue
            if kind == "session_meta" and session_source == "unknown":
                raw = payload.get("source")
                session_source = "subagent" if isinstance(raw, dict) else str(raw or "unknown")
                continue
            if kind == "event_msg":
                event = payload.get("type")
                if event == "token_count":
                    info = payload.get("info") or {}
                    usage = info.get("last_token_usage") or {}
                    total = (info.get("total_token_usage") or {}).get("total_tokens")
                    # Adjacent records with an unchanged cumulative total are duplicate logs.
                    if not usage or (total is not None and total == last_total):
                        continue
                    last_total = total
                    bucket = by_source[session_source]
                    bucket["requests"] += 1
                    bucket["input"] += usage.get("input_tokens") or 0
                    bucket["cached"] += usage.get("cached_input_tokens") or 0
                    bucket["output"] += usage.get("output_tokens") or 0
                    bucket["reasoning"] += usage.get("reasoning_output_tokens") or 0
                    if session_source == "subagent":
                        if file_requests == 0:
                            child_first_input.append(usage.get("input_tokens") or 0)
                        child_request_input.append(usage.get("input_tokens") or 0)
                    file_requests += 1
                elif event == "thread_settings_applied":
                    tier = (payload.get("thread_settings") or {}).get("service_tier") or tier
                elif event == "task_complete":
                    timing = payload.get("timing") or {}
                    for request in timing.get("modelRequests") or []:
                        usage = request.get("tokenUsage") or {}
                        wait_ns = request.get("modelStreamWaitNs")
                        if wait_ns is None or not usage:
                            continue
                        row = {
                            "purpose": request.get("generationPurpose") or "unknown",
                            "seconds": wait_ns / 1e9,
                            "input": usage.get("inputTokens") or 0,
                            "cached": usage.get("cachedInputTokens") or 0,
                            "visible": usage.get("visibleOutputTokens") or 0,
                            "reasoning": usage.get("reasoningTokens") or 0,
                            "tier": tier or "unknown",
                            "progress": tuple(request.get("progressKinds") or ()),
                        }
                        requests.append(row)
                        tier_requests[row["tier"]] += 1
                        tier_seconds[row["tier"]].append(row["seconds"])
                        bucket = purpose[row["purpose"]]
                        bucket["requests"] += 1
                        bucket["seconds"] += row["seconds"]
                        bucket["visible"] += row["visible"]
                        bucket["reasoning"] += row["reasoning"]
                        bucket["input"] += row["input"]
                        bucket["uncached"] += row["input"] - row["cached"]
                elif event == "item_completed":
                    item = payload.get("item") or {}
                    if item.get("type") != "CommandExecution":
                        continue
                    argv = item.get("command") or []
                    if isinstance(argv, str):
                        argv = [argv]
                    shell, script = shell_script(argv)
                    shells[shell] += 1
                    family, sub = classify_script(script) if shell != "direct" else classify_script(script)
                    duration = item.get("duration") or {}
                    seconds = (duration.get("secs") or 0) + (duration.get("nanos") or 0) / 1e9
                    output = item.get("aggregated_output") or ""
                    exit_code = item.get("exit_code")
                    bucket = commands[family]
                    bucket["count"] += 1
                    bucket["seconds"] += seconds
                    bucket["output_bytes"] += len(output.encode("utf-8", "replace"))
                    bucket["script_bytes"] += len(script.encode("utf-8", "replace"))
                    if exit_code not in (0, None):
                        no_match = (item.get("output_metadata") or {}).get("search_no_match")
                        bucket["nonzero_no_match" if no_match else "nonzero"] += 1
                    if exit_code is None:
                        bucket["no_exit_code"] += 1
                    command_durations[family].append(seconds)
                    if family in ("git", "cargo", "just"):
                        command_sub[f"{family} {sub}"] += 1
                    if family == "python-inline":
                        inline_python_bytes += len(script.encode("utf-8", "replace"))
                    vkind = validation_kind(script)
                    if vkind:
                        vb = validation[vkind]
                        vb["count"] += 1
                        vb["seconds"] += seconds
                        if COMPILE_FAIL_RE.search(output):
                            vb["compile_failure"] += 1
                        elif exit_code not in (0, None):
                            vb["nonzero_other"] += 1
                            if TEST_FAIL_RE.search(output):
                                vb["test_failure_marker"] += 1
                        else:
                            vb["exit_zero"] += 1
                        validation_durations[vkind].append(seconds)
            elif kind == "response_item" and payload.get("type") == "custom_tool_call":
                if payload.get("name") != "exec":
                    nested[f"direct:{payload.get('name')}"] += 1
                    continue
                source = payload.get("input") or ""
                total = len(source.encode("utf-8"))
                patch_bytes = sum(len(m.group(0).encode("utf-8")) for m in PATCH_RE.finditer(source))
                cmd_bytes = 0
                for match in CMD_KEY_RE.finditer(source):
                    _, end = js_string_at(source, match.end() - 1)
                    cmd_bytes += len(source[match.end() - 1:end].encode("utf-8"))
                cells.append((total, patch_bytes, cmd_bytes))
                for match in NESTED_RE.finditer(source):
                    name = match.group(1)
                    if match.group(2) and name not in ("exec_command",):
                        name = f"{name}.{match.group(2)}"
                    nested[name] += 1
                for match in HELPER_RE.finditer(source):
                    helpers[match.group(1)] += 1
                for match in READ_FILE_CALL_RE.finditer(source):
                    read_calls += 1
                    window = source[match.end(): match.end() + 1500]
                    closing = window.find("})")
                    segment = window[: closing if closing >= 0 else 600]
                    kinds = KIND_RE.findall(segment)
                    if not kinds:
                        read_calls_without_selectors += 1
                    for found in kinds:
                        selector_kinds[found] += 1
            elif kind == "response_item" and payload.get("type") == "function_call":
                nested[f"direct:{payload.get('name')}"] += 1
        by_source[session_source]["files"] += 1
        if session_source == "subagent":
            child_requests_per_file.append(file_requests)

    model_rows = [r for r in requests if r["purpose"] != "compaction_summary"]
    fit_all = ols(
        [((r["visible"], r["reasoning"], r["cached"] / 1000, (r["input"] - r["cached"]) / 1000), r["seconds"]) for r in model_rows],
        ["visible_output_token", "reasoning_token", "cached_input_ktok", "uncached_input_ktok"],
    )
    fits_by_tier = {}
    for tier_name in sorted({r["tier"] for r in model_rows}):
        rows = [r for r in model_rows if r["tier"] == tier_name]
        if len(rows) >= 200:
            fits_by_tier[tier_name] = ols(
                [((r["visible"], r["reasoning"], r["cached"] / 1000, (r["input"] - r["cached"]) / 1000), r["seconds"]) for r in rows],
                ["visible_output_token", "reasoning_token", "cached_input_ktok", "uncached_input_ktok"],
            )
    compaction_rows = [r for r in requests if r["purpose"] == "compaction_summary"]
    fit_compaction = ols(
        [((r["visible"] + r["reasoning"], r["input"] / 1000), r["seconds"]) for r in compaction_rows],
        ["output_token", "input_ktok"],
    ) if len(compaction_rows) > 10 else None

    total_seconds = sum(r["seconds"] for r in requests)
    progress_only_source = [r for r in requests if r["progress"] == ("new_source_evidence",)]
    cell_totals = [c[0] for c in cells]
    report = {
        "files": len(files),
        "missing_artifacts": missing_artifacts,
        "requests": {
            "count": len(requests),
            "model_stream_wait_s": round(total_seconds, 1),
            "by_tier": dict(tier_requests),
            "visible_output_tokens": sum(r["visible"] for r in requests),
            "reasoning_tokens": sum(r["reasoning"] for r in requests),
            "input_tokens": sum(r["input"] for r in requests),
            "uncached_input_tokens": sum(r["input"] - r["cached"] for r in requests),
            "seconds_quantiles": quantiles([r["seconds"] for r in model_rows]),
            "visible_output_quantiles": quantiles([r["visible"] for r in model_rows]),
            "input_quantiles": quantiles([r["input"] for r in model_rows]),
            "source_evidence_only": {
                "count": len(progress_only_source),
                "seconds": round(sum(r["seconds"] for r in progress_only_source), 1),
            },
            "by_purpose": {k: {kk: round(vv, 1) if isinstance(vv, float) else vv for kk, vv in v.items()} for k, v in sorted(purpose.items(), key=lambda kv: -kv[1]["seconds"])},
            "latency_fit_non_compaction": fit_all,
            "latency_fit_by_tier": fits_by_tier,
            "latency_fit_compaction_summary": fit_compaction,
        },
        "cells": {
            "count": len(cells),
            "source_bytes": sum(cell_totals),
            "source_bytes_quantiles": quantiles(cell_totals),
            "patch_payload_bytes": sum(c[1] for c in cells),
            "command_literal_bytes": sum(c[2] for c in cells),
            "cells_with_patch": sum(1 for c in cells if c[1]),
        },
        "nested_tool_references": dict(nested.most_common()),
        "helper_references": dict(helpers.most_common()),
        "read_file": {
            "calls": read_calls,
            "calls_without_selector_kinds": read_calls_without_selectors,
            "selector_kinds": dict(selector_kinds.most_common()),
        },
        "commands": {
            "shell": dict(shells.most_common()),
            "families": {k: dict(v) | {"seconds_quantiles": quantiles(command_durations[k])} for k, v in sorted(commands.items(), key=lambda kv: -kv[1]["count"])},
            "subcommands": dict(command_sub.most_common(40)),
            "inline_python_script_bytes": inline_python_bytes,
        },
        "validation": {k: dict(v) | {"seconds_quantiles": quantiles(validation_durations[k])} for k, v in validation.items()},
        "compacted_records": compactions,
        "usage_by_session_source": {
            "basis": "deduplicated token_count last_token_usage",
            "sources": {k: dict(v) for k, v in by_source.items()},
            "subagent_first_request_input_quantiles": quantiles(child_first_input),
            "subagent_request_input_quantiles": quantiles(child_request_input),
            "subagent_requests_per_file_quantiles": quantiles(child_requests_per_file),
        },
        "latency_by_tier": {
            k: {"n": len(v), "mean_s": round(sum(v) / len(v), 2), **{q: round(x, 2) for q, x in quantiles(v).items()}}
            for k, v in tier_seconds.items()
        },
    }
    json.dump(report, sys.stdout, indent=1, default=str)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
