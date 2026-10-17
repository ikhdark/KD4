#!/usr/bin/env python3
"""Offline replay of KD4 context-pressure policies over recorded rollouts.

Question: would the `context_management` + `token_budget` fresh-window path,
a lower `model_auto_compact_token_limit`, or both, cost less than the current
policy, once summary generation, pressure retirement, prompt-cache re-prefill,
recovery reads and extra model requests are counted?

Read-only. Nothing here changes harness behavior or configuration.

Current policy, as implemented in `core/src/session/turn.rs`:
* limit L = min(config, 90% of the resolved context window);
* proactive retirement: once the prompt exceeds 2/3 L, consumed tool outputs
  older than the newest 3 generations are replaced by recovery pins, oldest
  first, if that saves at least 20,000 tokens;
* at L, the same retirement runs without the minimum if the result fits under
  L; otherwise a remote summary compaction runs;
* a phase checkpoint after the sampling anchor forces a full re-projection
  (`project_continuation_with_workspace_cache` returns None), so the next
  request re-prefills most of the prompt, not only the retired suffix.

Unit of replay: one thread, i.e. one rollout file and one prompt-cache-key
fingerprint. Requests come, in order, from `task_complete` and `turn_aborted`
`timing.modelRequests`, deduplicated by `samplingRequestId`. Classification:
* summary attempt: reason `compaction` with no usage, or no reasoning and at
  least 1,000 visible tokens;
* summary reset: the first request after summary attempts;
* limit retirement: a later `compaction`-reason request whose prompt dropped;
* turn-start drop: a turn's first request whose prompt dropped (re-layout;
  replayed as recorded in every policy).

The replay holds the model's recorded actions fixed and prices the token,
cache, request and latency consequences of each policy. Retirable content is
a calibrated share of per-request prompt growth; the share is fitted so the
recorded policy reproduces the recorded summaries and retirements. It cannot
show whether the model would act differently with less or different context.
That needs matched live A/B runs.

usage (repository root):
  python -I -B scripts/benchmark_scripts/compaction_policy_replay.py \
      --since 2026-10-13 [--limits 200000,245000] [--json OUT.json]
"""

from __future__ import annotations

import argparse
import collections
import json
import math
import re
import statistics
from pathlib import Path

DEFAULT_HOME = Path(r"C:\Users\kuh\Desktop\LOCAL-KD")
EFFECTIVE_WINDOW_SHARE = 0.95    # ModelInfo.effective_context_window_percent (reported window)
AUTO_COMPACT_SHARE = 0.9         # ModelInfo::auto_compact_token_limit
CURRENT_LIMIT = 450_000          # model_context_window = 500000
RETIRE_TARGET = 2 / 3            # retirement target, both paths
KEPT_GENERATIONS = 3             # CONTEXT_PRESSURE_KEPT_GENERATIONS
PROACTIVE_MIN_SAVED = 20_000     # PROACTIVE_RETIREMENT_MIN_SAVED_TOKENS
TB_REMINDER_TOKENS = 6_144       # gpt-6-astra model_messages.token_budget
TB_OVERHEAD_TOKENS = 1_500       # guidance + <context_window> metadata + <=4 KB thread hint
RETIREMENT_ERA = "2026-10-14T17-00"  # first recorded limit retirement: 2026-10-14T17-42
POST_WINDOW_CELLS = 10
PRIOR_AUDIT_REASONING_SLOPE = 92 / 100_000   # REASONING_TOKEN_AUDIT_2026-10-15_CLAUDE

READ1 = re.compile(r"read_file\(\s*\{\s*path\s*:\s*[\"'`]([^\"'`]+)[\"'`]")
READN = re.compile(r"read_files\(\s*\[([^\]]*)\]")
QUOTED = re.compile(r"[\"'`]([^\"'`]+)[\"'`]")
HDR = re.compile(r"\*\*\* (?:Add|Update|Delete) File: (.+?)(?=\\n|\n)")


def records(path: Path, payloads: Path):
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if record.get("type") == "rollout_payload_artifact":
                stub = record.get("payload") or {}
                if stub.get("item_type") not in ("event_msg", "response_item"):
                    continue
                try:
                    with (payloads / f"{stub['sha256']}.json").open(encoding="utf-8") as artifact:
                        record = json.load(artifact)
                except (OSError, KeyError, ValueError):
                    continue
            yield record


def norm(path: str) -> str:
    parts = [p for p in path.replace("\\", "/").lower().split("/") if p not in ("", ".")]
    return "/".join(parts[-3:])


def parse_file(path: Path, payloads: Path, seen_requests: set, seen_calls: set):
    cwd = window = None
    rows, stream, out_bytes = [], [], {}
    turn = 0
    for record in records(path, payloads):
        kind = record.get("type")
        payload = record.get("payload") or {}
        if not isinstance(payload, dict):
            continue
        if kind in ("session_meta", "turn_context"):
            cwd = cwd or payload.get("cwd")
            continue
        if kind == "compacted":
            stream.append(("compacted",))
            continue
        if kind == "event_msg":
            event_type = payload.get("type")
            if event_type in ("task_started", "turn_started") and window is None:
                window = payload.get("model_context_window")
            if event_type in ("task_complete", "turn_aborted"):
                turn += 1
                for index, request in enumerate((payload.get("timing") or {}).get("modelRequests") or []):
                    key = request.get("samplingRequestId")
                    if key and key in seen_requests:
                        continue
                    if key:
                        seen_requests.add(key)
                    usage = request.get("tokenUsage") or {}
                    rows.append({
                        "turn": turn, "turn_start": index == 0,
                        "reason": request.get("generationReason"),
                        "purpose": request.get("generationPurpose"),
                        "fp": request.get("promptCacheKeyFingerprint"),
                        "usage": bool(usage),
                        "I": usage.get("inputTokens") or 0, "C": usage.get("cachedInputTokens") or 0,
                        "V": usage.get("visibleOutputTokens") or 0, "R": usage.get("reasoningTokens") or 0,
                        "T": (request.get("modelStreamWaitNs") or 0) / 1e9,
                    })
            continue
        if kind != "response_item":
            continue
        item_type = payload.get("type")
        if item_type == "message" and payload.get("role") == "developer":
            text = json.dumps(payload.get("content"))
            if "<completed_phase_checkpoint>" in text and "context_pressure" in text:
                stream.append(("retire",))
            continue
        call_id = payload.get("call_id")
        if item_type in ("custom_tool_call", "function_call") and call_id and call_id not in seen_calls:
            seen_calls.add(call_id)
            source = payload.get("input") if item_type == "custom_tool_call" else payload.get("arguments")
            source = source if isinstance(source, str) else json.dumps(source)
            reads = [norm(x) for x in READ1.findall(source)]
            for group in READN.findall(source):
                reads += [norm(x) for x in QUOTED.findall(group)]
            patches = {norm(h.strip()) for h in HDR.findall(source)}
            recovers = "read_tool_output" in source or payload.get("name") == "read_tool_output"
            stream.append(("cell", call_id, reads, patches, recovers))
        elif item_type in ("custom_tool_call_output", "function_call_output") and call_id and call_id not in out_bytes:
            output = payload.get("output")
            if isinstance(output, list):
                output = "".join(x.get("text", "") for x in output if isinstance(x, dict))
            elif not isinstance(output, str):
                output = json.dumps(output)
            out_bytes[call_id] = len(output.encode("utf-8", "replace"))
    return cwd, window, rows, stream, out_bytes


def split_threads(rows):
    threads = collections.OrderedDict()
    last_fp = None
    for row in rows:
        fp = row["fp"] or last_fp or "none"
        last_fp = fp
        threads.setdefault(fp, []).append(row)
    return list(threads.values())


def is_summary_attempt(row):
    return row["reason"] == "compaction" and (not row["usage"] or (row["R"] == 0 and row["V"] >= 1_000))


def segment(rows):
    """Work requests in order (each tagged with its kind) plus summary events."""
    work, summaries, pending, prev = [], [], None, None
    for row in rows:
        if is_summary_attempt(row):
            if pending is None:
                pending = {"pre": prev["I"] if prev else 0, "attempts": 0, "failed": 0, "T": 0.0,
                           "S": 0, "S_R": 0, "S_I": 0, "S_C": 0, "base": None, "base_C": 0}
            pending["attempts"] += 1
            pending["T"] += row["T"]
            if row["usage"] and row["V"] > 0:
                pending.update(S=row["V"], S_R=row["R"], S_I=row["I"], S_C=row["C"])
            else:
                pending["failed"] += 1
            continue
        row = dict(row, kind="normal", pre=None)
        if row["usage"]:
            if pending is not None:
                pending["base"], pending["base_C"] = row["I"], row["C"]
                row["kind"] = "summary_post"
                summaries.append(pending)
                pending = None
            elif prev is not None and row["I"] < prev["I"] - 2_000:
                row["kind"] = ("retire_post" if row["reason"] == "compaction"
                               else "turn_drop" if row["turn_start"] else "other_drop")
                row["pre"] = prev["I"]
            elif prev is None:
                row["kind"] = "first"
            prev = row
        work.append(row)
    if pending is not None:
        summaries.append(pending)
    return work, summaries


def lstsq(rows, names):
    k = len(names) + 1
    xtx = [[0.0] * k for _ in range(k)]
    xty = [0.0] * k
    for features, target in rows:
        vector = [1.0, *features]
        for i in range(k):
            xty[i] += vector[i] * target
            for j in range(k):
                xtx[i][j] += vector[i] * vector[j]
    aug = [xtx[i][:] + [xty[i]] for i in range(k)]
    for col in range(k):
        pivot = max(range(col, k), key=lambda r: abs(aug[r][col]))
        if abs(aug[pivot][col]) < 1e-12:
            return None
        aug[col], aug[pivot] = aug[pivot], aug[col]
        for r in range(k):
            if r != col:
                factor = aug[r][col] / aug[col][col]
                for c in range(col, k + 1):
                    aug[r][c] -= factor * aug[col][c]
    beta = [aug[i][k] / aug[i][i] for i in range(k)]
    ys = [t for _, t in rows]
    mean = sum(ys) / len(ys)
    ss_tot = sum((y - mean) ** 2 for y in ys)
    ss_res = sum((t - beta[0] - sum(b * x for b, x in zip(beta[1:], f))) ** 2 for f, t in rows)
    return {"intercept": beta[0], **dict(zip(names, beta[1:])), "r2": 1 - ss_res / ss_tot if ss_tot else None, "n": len(rows)}


def stream_recovery(stream, out_bytes):
    """Recorded recovery after summaries (re-reads) and after retirements (read_tool_output)."""
    summary, retire = [], []
    cells = [i for i, e in enumerate(stream) if e[0] == "cell"]
    marks = [i for i, e in enumerate(stream) if e[0] == "compacted"]
    bounds = [-1, *marks, len(stream)]
    for m in range(1, len(bounds) - 1):
        before = [e for e in stream[bounds[m - 1] + 1: bounds[m]] if e[0] == "cell"]
        after = [e for e in stream[bounds[m] + 1: bounds[m + 1]] if e[0] == "cell"]
        size = {}
        for _, call_id, reads, _, _ in before:
            if reads:
                share = out_bytes.get(call_id, 0) / len(reads)
                for path in reads:
                    size[path] = share
        read_before = set(size)
        reread_cells = reread_bytes = 0
        for _, call_id, reads, patches, _ in after[:POST_WINDOW_CELLS]:
            hits = [p for p in reads if p in read_before]
            if hits and not patches and len(hits) == len(reads):
                reread_cells += 1
                reread_bytes += out_bytes.get(call_id, 0)
        reads_after = {p for _, _, reads, _, _ in after for p in reads}
        patches_after = {p for _, _, _, patches, _ in after for p in patches}
        relied = (read_before & patches_after) - reads_after
        summary.append({
            "reread_cells": reread_cells, "reread_tokens": reread_bytes / 4,
            "relied_files": len(relied), "relied_tokens": sum(size[p] for p in relied) / 4,
            "needed_files": len(read_before & (reads_after | patches_after)),
            "recovery_calls": sum(1 for e in after[:POST_WINDOW_CELLS] if e[4]),
            "recovery_tokens": sum(out_bytes.get(e[1], 0) for e in after[:POST_WINDOW_CELLS] if e[4]) / 4,
            "window_cells": len(after[:POST_WINDOW_CELLS]),
        })
    for i, entry in enumerate(stream):
        if entry[0] != "retire":
            continue
        after = [stream[k] for k in cells if k > i][:POST_WINDOW_CELLS]
        retire.append({"cells": len(after), "recovery_calls": sum(1 for e in after if e[4]),
                       "recovery_tokens": sum(out_bytes.get(e[1], 0) for e in after if e[4]) / 4})
    recovery_cells = sum(1 for k in cells if stream[k][4])
    return summary, retire, recovery_cells, len(cells)


def load(home: Path, since: str, until: str | None, cwd_filter: str | None):
    payloads = home / "rollout-payloads"
    files = sorted(p for p in (home / "sessions").rglob("rollout-*.jsonl")
                   if p.name[8:18] >= since and (until is None or p.name[8:18] <= until))
    seen_requests, seen_calls = set(), set()
    threads, summary_rec, retire_rec = [], [], []
    counts = collections.Counter()
    cells_with_reads = []
    for path in files:
        cwd, window, rows, stream, out_bytes = parse_file(path, payloads, seen_requests, seen_calls)
        if cwd_filter and cwd_filter.lower() not in str(cwd).lower():
            continue
        counts["files"] += 1
        counts["retirement_markers"] += sum(1 for e in stream if e[0] == "retire")
        counts["compacted_markers"] += sum(1 for e in stream if e[0] == "compacted")
        s_rec, r_rec, recovery_cells, cell_count = stream_recovery(stream, out_bytes)
        summary_rec += s_rec
        retire_rec += r_rec
        counts["recovery_cells"] += recovery_cells
        counts["cells"] += cell_count
        cells_with_reads += [len(e[2]) for e in stream if e[0] == "cell" and e[2]]
        limit = round(window / EFFECTIVE_WINDOW_SHARE * AUTO_COMPACT_SHARE) if window else CURRENT_LIMIT
        for thread_rows in split_threads(rows):
            work, summaries = segment(thread_rows)
            if work:
                threads.append({"file": path.name, "cwd": cwd, "limit": limit,
                                "retirement_era": path.name[8:24] >= RETIREMENT_ERA,
                                "work": work, "events": summaries})
    return threads, summary_rec, retire_rec, counts, cells_with_reads


def fixed_effect_slope(rows_by_group):
    """Slope of reasoning on prompt tokens after removing each group's means."""
    num = den = 0.0
    for group in rows_by_group.values():
        if len(group) < 3:
            continue
        mi = statistics.mean(r["I"] for r in group)
        mr = statistics.mean(r["R"] for r in group)
        for r in group:
            num += (r["I"] - mi) * (r["R"] - mr)
            den += (r["I"] - mi) ** 2
    return num / den if den else None


def excess_recovery(windows, base_rate):
    """read_tool_output cells after an event, above the corpus base rate."""
    if not windows:
        return {"excess_recovery_calls": 0.0, "excess_recovery_tokens": 0.0}
    calls = statistics.mean(w["recovery_calls"] for w in windows)
    tokens = statistics.mean(w["recovery_tokens"] for w in windows)
    excess = max(0.0, calls - base_rate * statistics.mean(w["window_cells"] for w in windows))
    return {"excess_recovery_calls": excess, "excess_recovery_tokens": tokens * excess / calls if calls else 0.0}


def calibrate(threads, summary_rec, retire_rec, counts, cells_with_reads):
    events = [e for t in threads for e in t["events"]]
    done = [e for e in events if e["S"] > 0]
    with_base = [e for e in done if e["base"]]
    rows = {k: [r for t in threads for r in t["work"] if r["usage"] and r["kind"] == k]
            for k in ("normal", "summary_post", "retire_post", "turn_drop", "other_drop")}
    growth, misses = [], []
    for t in threads:
        work = [r for r in t["work"] if r["usage"]]
        for prev, r in zip(work, work[1:]):
            if r["kind"] == "normal":
                g = max(0, r["I"] - prev["I"])
                growth.append(g)
                if r["I"] > g:
                    misses.append(max(0.0, (r["I"] - r["C"]) - g) / (r["I"] - g))
    summary_fit = lstsq([((e["S"],), e["T"]) for e in done if e["failed"] == 0 and e["attempts"] == 1],
                        ["per_summary_token"])
    fitted = sum(summary_fit["intercept"] + summary_fit["per_summary_token"] * e["S"] for e in done)
    latency_rows = [r for t in threads for r in t["work"] if r["usage"]]
    latency_fit = lstsq([((r["V"], r["R"], (r["I"] - r["C"]) / 1000, r["C"] / 1000), r["T"]) for r in latency_rows],
                        ["visible", "reasoning", "uncached_k", "cached_k"])
    latency_fit_ix = lstsq([((r["V"], r["R"], (r["I"] - r["C"]) / 1000, (r["V"] + r["R"]) * r["I"] / 100_000), r["T"])
                            for r in latency_rows], ["visible", "reasoning", "uncached_k", "output_x_prompt_100k"])
    # Reasoning associations. Retirements shrink the prompt while keeping the
    # conversation; summaries also remove it. Neither is a controlled experiment.
    around = {"retire_post": [], "summary_post": []}
    for t in threads:
        work = [r for r in t["work"] if r["usage"]]
        for j, r in enumerate(work):
            if r["kind"] not in around or j < 6 or j + 7 > len(work):
                continue
            before = [w for w in work[j - 6: j] if w["kind"] == "normal"]
            after = [w for w in work[j + 1: j + 7] if w["kind"] == "normal"]
            if len(before) >= 4 and len(after) >= 4:
                around[r["kind"]].append((statistics.mean(w["I"] for w in before), statistics.mean(w["R"] for w in before),
                                          statistics.mean(w["I"] for w in after), statistics.mean(w["R"] for w in after),
                                          statistics.mean(w["V"] for w in before), statistics.mean(w["V"] for w in after)))
    natural = {}
    for kind, windows in around.items():
        if not windows:
            continue
        i_b, r_b = statistics.mean(w[0] for w in windows), statistics.mean(w[1] for w in windows)
        i_a, r_a = statistics.mean(w[2] for w in windows), statistics.mean(w[3] for w in windows)
        natural[kind] = {"events": len(windows), "prompt_before": i_b, "prompt_after": i_a,
                         "reasoning_before": r_b, "reasoning_after": r_a,
                         "visible_before": statistics.mean(w[4] for w in windows),
                         "visible_after": statistics.mean(w[5] for w in windows),
                         "slope": (r_b - r_a) / (i_b - i_a) if i_b != i_a else None}
    groups = collections.defaultdict(list)
    for t in threads:
        for r in t["work"]:
            if r["usage"] and r["kind"] == "normal":
                groups[(t["file"], t["work"][0]["fp"], r["purpose"])].append(r)
    retire_rows = rows["retire_post"]
    turn_rows = rows["turn_drop"] + rows["other_drop"]
    base_recovery = counts["recovery_cells"] / max(1, counts["cells"])
    return {
        "summary_events": len(events), "summary_events_completed": len(done),
        "summary_failed_attempts": sum(e["failed"] for e in events),
        "summary_tokens": {"median": statistics.median(e["S"] for e in done), "mean": statistics.mean(e["S"] for e in done)},
        "summary_seconds_total": sum(e["T"] for e in events),
        "summary_time_fit": summary_fit,
        "failure_seconds_per_event": (sum(e["T"] for e in events) - fitted) / max(1, len(events)),
        "summary_cached_share_median": statistics.median(e["S_C"] / e["S_I"] for e in done if e["S_I"]),
        "pre_context_median": statistics.median(e["pre"] for e in done),
        "base_median": statistics.median(e["base"] for e in with_base),
        "base_minus_summary_median": statistics.median(e["base"] - e["S"] for e in with_base),
        "summary_reprefill_uncached_median": statistics.median(e["base"] - e["base_C"] for e in with_base),
        "static_prefix_cached": statistics.median(e["base_C"] for e in with_base),
        "limit_retirements": {
            "count": len(retire_rows),
            "post_over_pre_median": statistics.median(r["I"] / r["pre"] for r in retire_rows) if retire_rows else None,
            "retained_cache_share_mean": statistics.mean(r["C"] / r["I"] for r in retire_rows) if retire_rows else None,
            "reprefill_uncached_median": statistics.median(r["I"] - r["C"] for r in retire_rows) if retire_rows else None,
            "reprefill_uncached_total": sum(r["I"] - r["C"] for r in retire_rows),
        },
        "turn_start_drops": {
            "count": len(turn_rows),
            "drop_median": statistics.median(r["pre"] - r["I"] for r in turn_rows) if turn_rows else None,
            "uncached_share_median": statistics.median((r["I"] - r["C"]) / r["I"] for r in turn_rows) if turn_rows else None,
        },
        "growth_per_request_median": statistics.median(growth),
        "growth_per_request_mean": statistics.mean(growth),
        "normal_miss_share_median": statistics.median(misses),
        "latency_fit": latency_fit, "latency_fit_interaction": latency_fit_ix,
        "reasoning_association": {"around": natural, "within_thread_purpose_slope": fixed_effect_slope(groups)},
        "recovery_after_summary": {
            "n": len(summary_rec),
            "reread_cells_mean": statistics.mean(r["reread_cells"] for r in summary_rec) if summary_rec else 0,
            "reread_tokens_mean": statistics.mean(r["reread_tokens"] for r in summary_rec) if summary_rec else 0,
            "relied_files_mean": statistics.mean(r["relied_files"] for r in summary_rec) if summary_rec else 0,
            "relied_tokens_mean": statistics.mean(r["relied_tokens"] for r in summary_rec) if summary_rec else 0,
            "needed_files_mean": statistics.mean(r["needed_files"] for r in summary_rec) if summary_rec else 0,
            "recovery_calls_mean": statistics.mean(r["recovery_calls"] for r in summary_rec) if summary_rec else 0,
            **excess_recovery(summary_rec, base_recovery),
        },
        "recovery_after_retirement": {
            "n": len(retire_rec),
            "read_tool_output_calls_mean": statistics.mean(r["recovery_calls"] for r in retire_rec) if retire_rec else 0,
            "expected_at_base_rate": base_recovery * statistics.mean(r["cells"] for r in retire_rec) if retire_rec else 0,
            "base_rate_per_cell": base_recovery,
        },
        "files_per_read_cell": statistics.mean(cells_with_reads) if cells_with_reads else 1.0,
    }


SCENARIO_ASSUMPTIONS = {
    # notes size as a share of the remote summary, history reads per fresh window,
    # re-discovery multiplier on relied files, incremental notes writes per 50 work requests
    "low": {"notes_share": 0.5, "history_reads": 0, "rediscovery": 0.0, "incremental_notes": 0},
    "mid": {"notes_share": 0.75, "history_reads": 1, "rediscovery": 1.0, "incremental_notes": 0},
    "high": {"notes_share": 1.0, "history_reads": 3, "rediscovery": 2.0, "incremental_notes": 1},
}


class Ledger:
    def __init__(self):
        self.c = collections.Counter()

    def request(self, prompt, uncached, visible, reasoning, seconds, kind, weight=1.0):
        uncached = max(0.0, min(prompt, uncached))
        self.c["requests"] += weight
        self.c[f"requests_{kind}"] += weight
        self.c["input"] += weight * prompt
        self.c["uncached"] += weight * uncached
        self.c["cached"] += weight * (prompt - uncached)
        self.c["visible"] += weight * visible
        self.c["reasoning"] += weight * reasoning
        self.c["seconds"] += weight * seconds
        self.c[f"seconds_{kind}"] += weight * seconds


def predicted_seconds(cal, visible, reasoning, uncached):
    fit = cal["latency_fit"]
    return max(0.5, fit["intercept"] + fit["visible"] * visible + fit["reasoning"] * reasoning
               + fit["uncached_k"] * uncached / 1000)


def select_retirement(blocks, newest_generation, target):
    """Oldest-first eligible blocks until `target` tokens are saved, like pressure_retirement_selection."""
    selected, saved = [], 0.0
    for index, block in enumerate(blocks):
        if saved >= target:
            break
        if block[1] <= 0 or block[2] > newest_generation - KEPT_GENERATIONS:
            continue
        selected.append(index)
        saved += block[1]
    return selected, saved


def simulate(thread, cal, policy):
    """Replay one thread under `policy`.

    policy keys: kind ('remote' | 'token_budget'), limit (None = recorded), proactive,
    limit_retire (None = recorded era), retirable_share, anchored, slope, assumptions,
    interaction, modeled_recovery.
    """
    work = thread["work"]
    limit = policy["limit"] or thread["limit"]
    limit_retire = thread["retirement_era"] if policy["limit_retire"] is None else policy["limit_retire"]
    share = policy["retirable_share"]
    events = [e for e in thread["events"] if e["S"] > 0]
    s_thread = statistics.median(e["S"] for e in events) if events else cal["summary_tokens"]["median"]
    bases = [e["base"] for e in events if e["base"]]
    base = statistics.median(bases) if bases else cal["base_median"]
    base_wo_s = max(cal["static_prefix_cached"], base - s_thread)
    usage_rows = [r for r in work if r["usage"]]
    normal_growth = [max(0, b["I"] - a["I"]) for a, b in zip(usage_rows, usage_rows[1:]) if b["kind"] == "normal"]
    g_med = statistics.median(normal_growth) if normal_growth else cal["growth_per_request_median"]
    fit, lat, ix = cal["summary_time_fit"], cal["latency_fit"], cal["latency_fit_interaction"]
    rec = cal["recovery_after_summary"]
    static = cal["static_prefix_cached"]
    retained = cal["limit_retirements"]["retained_cache_share_mean"] or 0.0
    miss_med = cal["normal_miss_share_median"]
    assumptions = policy["assumptions"]
    ledger = Ledger()
    blocks, prefix, c = [], None, None
    break_pos, break_is_reprefill = None, False
    prev_i, generation, since_notes = None, 0, 0

    def add_content(tokens):
        nonlocal c
        blocks.append([tokens, share * tokens, generation])
        c += tokens

    def retire(selected):
        nonlocal c, break_pos, break_is_reprefill
        position = prefix + sum(b[0] for b in blocks[: selected[0]])
        saved = 0.0
        for index in selected:
            saved += blocks[index][1]
            blocks[index][0] -= blocks[index][1]
            blocks[index][1] = 0.0
        c -= saved
        # Recorded behavior: the checkpoint forces a full re-projection, so the
        # next request keeps only the recorded share of its prompt cached.
        cut = position if policy["anchored"] else max(static, retained * c)
        break_pos = cut if break_pos is None else min(break_pos, cut)
        break_is_reprefill = True
        ledger.c["retirements"] += 1
        ledger.c["retired_tokens"] += saved

    def reset_window(new_prefix):
        nonlocal blocks, prefix, c, break_pos, break_is_reprefill
        blocks, prefix, c = [], new_prefix, new_prefix
        break_pos, break_is_reprefill = static, True

    def full_request(new_tokens, visible, reasoning, kind):
        """A policy-added request whose prompt is the whole current window."""
        nonlocal break_pos, break_is_reprefill
        uncached = new_tokens
        if break_pos is not None:
            uncached = c - break_pos
            if break_is_reprefill:
                ledger.c["reprefill_uncached"] += uncached
            break_pos, break_is_reprefill = None, False
        ledger.request(c, uncached, visible, reasoning, predicted_seconds(cal, visible, reasoning, uncached), kind)

    recovery_weight = rec["reread_cells_mean"] + rec["excess_recovery_calls"]
    recovery_tokens = rec["reread_tokens_mean"] + rec["excess_recovery_tokens"]

    def expected_rereads(sign=1.0):
        """Recorded post-summary file re-reads and excess read_tool_output calls, in expectation."""
        weight = recovery_weight
        if weight <= 0:
            return
        per_cell = recovery_tokens / weight
        ledger.request(c, per_cell + 300, 200, 100, predicted_seconds(cal, 200, 100, per_cell + 300),
                       "recovery", sign * weight)

    for r in work:
        if not r["usage"]:
            ledger.c["seconds"] += r["T"]
            ledger.c["seconds_unattributed"] += r["T"]
            continue
        generation += 1
        if c is None:
            prefix = c = r["I"]
            prev_i = r["I"]
            ledger.request(c, r["I"] - r["C"], r["V"], r["R"], r["T"], "work")
            continue
        kind = r["kind"]
        g = max(0, r["I"] - prev_i) if kind == "normal" else g_med
        prev_i = r["I"]
        if kind in ("turn_drop", "other_drop"):
            # Exogenous re-layout at a turn boundary: replay the recorded share removed.
            removed = max(0.0, r["pre"] + g - r["I"])
            content = c - prefix
            if content > 0:
                scale = max(0.0, 1 - min(removed, content) / content)
                for block in blocks:
                    block[0] *= scale
                    block[1] *= scale
                c = prefix + sum(b[0] for b in blocks)
        if policy["modeled_recovery"] and kind == "summary_post":
            # The recorded summary's re-reads are in the growth that follows it.
            g = max(0.0, g - recovery_tokens)
            expected_rereads(-1.0)
        add_content(g)
        newest = generation - 1
        if policy["kind"] == "remote":
            if c >= limit:
                selected, saved = (select_retirement(blocks, newest, c - RETIRE_TARGET * limit)
                                   if limit_retire else ([], 0.0))
                if selected and c - saved < limit:
                    retire(selected)
                else:
                    seconds = fit["intercept"] + fit["per_summary_token"] * s_thread + cal["failure_seconds_per_event"]
                    ledger.request(c, c * (1 - cal["summary_cached_share_median"]), s_thread, 0, seconds, "summary")
                    ledger.c["summaries"] += 1
                    reset_window(base)
                    if policy["modeled_recovery"]:
                        expected_rereads()
                        add_content(recovery_tokens)
            elif policy["proactive"] and c > RETIRE_TARGET * limit:
                selected, saved = select_retirement(blocks, newest, c - RETIRE_TARGET * limit)
                if selected and saved >= PROACTIVE_MIN_SAVED:
                    retire(selected)
        else:
            if c >= limit - TB_REMINDER_TOKENS:
                notes = assumptions["notes_share"] * s_thread
                full_request(g + 500, notes, 100, "notes_write")
                ledger.request(c + notes, notes + 200, 30, 0, predicted_seconds(cal, 30, 0, notes + 200), "new_context")
                ledger.c["fresh_windows"] += 1
                reset_window(base_wo_s + TB_OVERHEAD_TOKENS)
                reads = [notes] + [2_000] * assumptions["history_reads"]
                relied = rec["relied_tokens_mean"] * assumptions["rediscovery"]
                if relied:
                    cells = max(1, math.ceil(rec["relied_files_mean"] * assumptions["rediscovery"]
                                             / max(1.0, cal["files_per_read_cell"])))
                    reads += [relied / cells] * cells
                last = 0.0
                for tokens in reads:
                    full_request(last + 300, 200, 100, "recovery")
                    add_content(tokens)
                    last = tokens
                expected_rereads()
                add_content(recovery_tokens)
                break_pos, break_is_reprefill = c - last - recovery_tokens, False
            elif policy["proactive"] and c > RETIRE_TARGET * limit:
                selected, saved = select_retirement(blocks, newest, c - RETIRE_TARGET * limit)
                if selected and saved >= PROACTIVE_MIN_SAVED:
                    retire(selected)
        # The recorded work request, at the counterfactual prompt size.
        recorded_uncached = r["I"] - r["C"]
        if break_pos is not None:
            uncached = c - break_pos
            if break_is_reprefill:
                ledger.c["reprefill_uncached"] += uncached
            break_pos, break_is_reprefill = None, False
        elif kind in ("turn_drop", "other_drop"):
            uncached = recorded_uncached / max(1, r["I"]) * c
            ledger.c["turn_drop_uncached"] += uncached
        elif kind in ("summary_post", "retire_post"):
            uncached = g + miss_med * max(0.0, c - g)
        else:
            miss = max(0.0, recorded_uncached - g) / max(1.0, r["I"] - g)
            uncached = g + miss * max(0.0, c - g)
        uncached = min(c, uncached)
        reasoning = max(0.0, r["R"] + policy["slope"] * (c - r["I"]))
        seconds = r["T"] + lat["reasoning"] * (reasoning - r["R"]) + lat["uncached_k"] * (uncached - recorded_uncached) / 1000
        if policy["interaction"] and ix:
            seconds += ix["output_x_prompt_100k"] * (r["V"] + reasoning) * (c - r["I"]) / 100_000
        ledger.request(c, uncached, r["V"], reasoning, max(0.2, seconds), "work")
        if policy["kind"] == "token_budget" and assumptions["incremental_notes"]:
            since_notes += 1
            if since_notes >= 50:
                since_notes = 0
                ledger.request(c, 500, 300, 50, predicted_seconds(cal, 300, 50, 500), "notes_write")
                add_content(350)
    return ledger.c


def recorded_totals(threads):
    total = collections.Counter()
    for t in threads:
        for r in t["work"]:
            total["requests"] += 1 if r["usage"] else 0
            total["input"] += r["I"]
            total["cached"] += r["C"]
            total["uncached"] += r["I"] - r["C"]
            total["visible"] += r["V"]
            total["reasoning"] += r["R"]
            total["seconds"] += r["T"]
            if r["kind"] == "retire_post":
                total["retirements"] += 1
                total["reprefill_uncached"] += r["I"] - r["C"]
            elif r["kind"] == "summary_post":
                total["reprefill_uncached"] += r["I"] - r["C"]
        for e in t["events"]:
            total["summaries"] += 1
            total["requests"] += 1 if e["S"] else 0
            total["requests_summary"] += e["attempts"]
            total["input"] += e["S_I"]
            total["cached"] += e["S_C"]
            total["uncached"] += e["S_I"] - e["S_C"]
            total["visible"] += e["S"]
            total["reasoning"] += e["S_R"]
            total["seconds"] += e["T"]
            total["seconds_summary"] += e["T"]
    return total


def weighted(c, cached_weight, output_weight):
    return (c.get("uncached", 0) + cached_weight * c.get("cached", 0)
            + output_weight * (c.get("visible", 0) + c.get("reasoning", 0)))


def run_policy(threads, cal, **policy):
    total = collections.Counter()
    for t in threads:
        total.update(simulate(t, cal, policy))
    total = dict(total)
    total["weighted_api_like"] = weighted(total, 0.1, 8.0)
    total["weighted_flat_cache"] = weighted(total, 0.25, 4.0)
    return total


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--home", type=Path, default=DEFAULT_HOME)
    parser.add_argument("--since", default="2026-10-13")
    parser.add_argument("--until", default=None)
    parser.add_argument("--cwd-contains", default=None)
    parser.add_argument("--limits", default="200000,245000")
    parser.add_argument("--retirable-share", type=float, default=None, help="skip calibration and use this share")
    parser.add_argument("--json", type=Path)
    args = parser.parse_args(argv)
    # Calibrate on the whole corpus; --cwd-contains selects only the replayed workload.
    threads, summary_rec, retire_rec, counts, cells_with_reads = load(args.home, args.since, args.until, None)
    cal = calibrate(threads, summary_rec, retire_rec, counts, cells_with_reads)
    recorded_all = recorded_totals(threads)
    base_policy = {"kind": "remote", "limit": None, "proactive": False, "limit_retire": None,
                   "anchored": False, "slope": 0.0, "assumptions": SCENARIO_ASSUMPTIONS["mid"],
                   "interaction": False, "modeled_recovery": False}
    # Calibrate the retirable share: the recorded policy must reproduce recorded summaries and retirements.
    grid = {}
    for share in [x / 20 for x in range(4, 20)]:
        sim = run_policy(threads, cal, **dict(base_policy, retirable_share=share))
        grid[share] = {k: sim.get(k, 0) for k in ("summaries", "retirements", "uncached", "input", "seconds", "reprefill_uncached")}
    if args.retirable_share is None:
        def error(share):
            sim = grid[share]
            return (abs(sim["summaries"] - recorded_all["summaries"]) / max(1, recorded_all["summaries"])
                    + abs(sim["retirements"] - recorded_all["retirements"]) / max(1, recorded_all["retirements"]))
        share = min(grid, key=error)
    else:
        share = args.retirable_share
    if args.cwd_contains:
        threads = [t for t in threads if args.cwd_contains.lower() in str(t["cwd"]).lower()]
    recorded = recorded_totals(threads)
    recorded["weighted_api_like"] = weighted(recorded, 0.1, 8.0)
    recorded["weighted_flat_cache"] = weighted(recorded, 0.25, 4.0)
    association = cal["reasoning_association"]
    slopes = {"none": 0.0, "prior_audit": PRIOR_AUDIT_REASONING_SLOPE}
    within = association["within_thread_purpose_slope"]
    if within is not None:
        slopes["within_thread"] = max(0.0, within)
    retire_assoc = (association["around"].get("retire_post") or {}).get("slope")
    if retire_assoc is not None:
        slopes["around_retirement"] = retire_assoc
    limits = [CURRENT_LIMIT, *[int(x) for x in args.limits.split(",") if x]]
    scenarios = {}
    common = dict(base_policy, retirable_share=share, limit_retire=True, modeled_recovery=True)
    for slope_name, slope in slopes.items():
        for limit in limits:
            for proactive in (True, False):
                tag = "proactive" if proactive else "limit_only"
                scenarios[f"remote@{limit // 1000}k/{tag}[{slope_name}]"] = run_policy(
                    threads, cal, **dict(common, limit=limit, proactive=proactive, slope=slope))
                if slope_name == "none":
                    scenarios[f"remote@{limit // 1000}k/{tag}/anchored[{slope_name}]"] = run_policy(
                        threads, cal, **dict(common, limit=limit, proactive=proactive, slope=slope, anchored=True))
            for level, assumptions in SCENARIO_ASSUMPTIONS.items():
                scenarios[f"token_budget@{limit // 1000}k/{level}[{slope_name}]"] = run_policy(
                    threads, cal, **dict(common, kind="token_budget", limit=limit, proactive=True, slope=slope,
                                         assumptions=assumptions))
                if slope_name == "none":
                    # A build without proactive retirement: token budget then retires nothing.
                    scenarios[f"token_budget@{limit // 1000}k/{level}/no_retirement[{slope_name}]"] = run_policy(
                        threads, cal, **dict(common, kind="token_budget", limit=limit, proactive=False, slope=slope,
                                             assumptions=assumptions))
    for limit in limits:
        scenarios[f"remote@{limit // 1000}k/proactive[none,decode_interaction]"] = run_policy(
            threads, cal, **dict(common, limit=limit, proactive=True, interaction=True))
    sensitivity = {}
    for s in sorted({0.4, 0.65} - {share}):
        for limit in limits:
            for proactive in (True, False):
                tag = "proactive" if proactive else "limit_only"
                sensitivity[f"share={s:.2f}/remote@{limit // 1000}k/{tag}"] = run_policy(
                    threads, cal, **dict(common, retirable_share=s, limit=limit, proactive=proactive))
            sensitivity[f"share={s:.2f}/token_budget@{limit // 1000}k/mid"] = run_policy(
                threads, cal, **dict(common, retirable_share=s, kind="token_budget", limit=limit, proactive=True))
    calibration_run = run_policy(threads, cal, **dict(base_policy, retirable_share=share))
    by_era = {}
    for era in (False, True):
        subset = [t for t in threads if t["retirement_era"] == era]
        rec_era = recorded_totals(subset)
        sim_era = run_policy(subset, cal, **dict(base_policy, retirable_share=share))
        by_era["retirement_era" if era else "summary_only_era"] = {
            k: [rec_era.get(k, 0), sim_era.get(k, 0)] for k in ("summaries", "retirements", "input", "uncached", "seconds")}
    report = {
        "counts": dict(counts), "threads": len(threads),
        "threads_with_summary": sum(1 for t in threads if t["events"]),
        "limits_recorded": dict(collections.Counter(t["limit"] for t in threads)),
        "recorded": dict(recorded),
        "calibration": cal,
        "retirable_share": share, "retirable_share_grid": grid,
        "calibration_recorded_policy": calibration_run, "calibration_by_era": by_era,
        "reasoning_slopes": slopes,
        "scenarios": scenarios, "sensitivity": sensitivity,
        "weights": {"api_like": [0.1, 8.0], "flat_cache": [0.25, 4.0]},
    }
    text = json.dumps(report, indent=1, default=float)
    if args.json:
        args.json.write_text(text, encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
