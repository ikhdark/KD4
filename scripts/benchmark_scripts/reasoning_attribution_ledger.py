#!/usr/bin/env python3
"""Reasoning-token attribution ledger built on RolloutDetect (read-only).

`build` runs the offline detector (`rolloutdetect.cmd parse FILE --json`) on
each rollout and keeps one compact row per retained model generation: the
detector's purpose, reason, attempt kind and request classification plus the
provider-reported token totals it surfaces. Each row is joined with what the
rollout itself records for that turn and request: model, reasoning effort,
collaboration mode, session source (root or subagent), and the runtime's
per-request progress flags from the terminal timing profile. One session row
carries detector population totals and an independent `token_count`
reconciliation (sum of `last_token_usage` over events whose cumulative total
advanced).

`report` aggregates a ledger into reasoning-token attribution tables.

Nothing is estimated. Absent usage stays null and is counted separately.
Classification tags are the detector's observational labels, not proof that a
generation was unnecessary.

Usage:
  reasoning_attribution_ledger.py build ROOT... --out LEDGER.jsonl
      [--jobs N] [--detector DIR] [--home DIR] [--repo-root DIR]
      [--timeout S] [--modified-before EPOCH]
  reasoning_attribution_ledger.py report LEDGER.jsonl [--json OUT]
"""

from __future__ import annotations

import argparse
import io
import json
import re
import subprocess
import sys
import time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

if __package__:
    from .kd4_model_attempt_analysis import spearman
else:
    from kd4_model_attempt_analysis import spearman

DEFAULT_HOME = Path(r"C:\Users\kuh\Desktop\LOCAL-KD")
DEFAULT_DETECTOR = Path(r"C:\Users\kuh\Documents\ChatGPT\rolloutdetect")
DEFAULT_REPO = Path(__file__).resolve().parents[2]
HYDRATE = {"event_msg", "session_meta", "turn_context"}
TOKEN_FIELDS = ("inputTokens", "cachedInputTokens", "outputTokens", "reasoningTokens", "visibleOutputTokens")
FLAG_FIELDS = (
    "progressKinds", "unchangedRelevantState", "nextStructuredActionChanged",
    "disposition", "isContinuation", "modelEmittedToolCallCount", "reasoningOutputTokens",
    "relevantStateFingerprint",
)
MODEL_ITEMS = {
    "reasoning", "function_call", "custom_tool_call", "local_shell_call",
    "web_search_call", "tool_search_call", "image_generation_call",
}
TOOL_OUTPUTS = {"function_call_output", "custom_tool_call_output", "tool_search_output"}
# Harness-emitted structured anchors only; free-text error words are excluded
# because file contents and test logs quoted in tool output match them. The
# same messages also appear as source format strings ({tool_name}, {detail}),
# test literals and audit quotes the model reads, so each pattern requires the
# emitted continuation.
MARKERS = {
    # Emitted records are compact serde JSON with further keys; json! source is
    # spaced and test literals end at the flag.
    "output_truncated": re.compile(r'\\*"output_truncated\\*":true,\\*"'),
    "script_failed": re.compile(r"^Script failed with cell ID", re.M),
    "preflight_failed": re.compile(r"tool `[\w.:\-]+` argument preflight failed"),
    # The runner prints its busy status with sort_keys=True, so these keys are
    # adjacent; its source spells them as Python False and a named constant.
    "admission_busy": re.compile(
        r'\\*"executed\\*":\s*false,\s*\\*"exit_code\\*":\s*75\b'),
    "cargo_launch_rejected": re.compile(r"Raw Cargo launch rejected; no build started\. [A-Z]"),
    "exec_rejected": re.compile(r"Failed to create unified exec process: Rejected\("),
    "patch_preflight_conflict": re.compile(
        r"verification failed: Patch preflight found \d+ conflict"),
    "selector_errors": re.compile(r'\\*"selector_errors\\*":\s*\[\s*\{\\*"selector_index\\*"'),
    "nonzero_exit": re.compile(r'"exit_code":\s*-?[1-9]'),
}
LEADING_TAG = re.compile(r"^\s*<([a-zA-Z_][\w\-]*)")
CELL_TOOL = re.compile(r"tools\.([A-Za-z_][\w]*)\s*\(")
EVENTS = {"stream_error", "error", "turn_aborted", "context_compacted", "warning"}


# ---------------------------------------------------------------- raw rollout
def open_lines(path: Path):
    if path.name.endswith(".zst"):
        from compression import zstd

        return io.StringIO(zstd.decompress(path.read_bytes()).decode("utf-8", "replace"))
    return path.open("r", encoding="utf-8", errors="replace")


def hydrate(record: dict, home: Path) -> dict | None:
    ref = record.get("payload") or {}
    if ref.get("item_type") not in HYDRATE or not ref.get("sha256"):
        return None
    try:
        return json.loads((home / "rollout-payloads" / f"{ref['sha256']}.json").read_bytes())
    except (OSError, json.JSONDecodeError):
        return {"type": "_missing_artifact", "payload": {"item_type": ref.get("item_type")}}


def message_text(payload: dict) -> str:
    return "\n".join(
        part.get("text", "") for part in payload.get("content") or []
        if isinstance(part, dict) and isinstance(part.get("text"), str)
    )


def output_text(payload: dict) -> str:
    out = payload.get("output")
    if isinstance(out, dict):
        out = out.get("content")
    if isinstance(out, list):
        return "\n".join(part.get("text", "") for part in out if isinstance(part, dict))
    return out if isinstance(out, str) else json.dumps(out)


def summarize_trigger(items: list[dict], outputs: list[dict], events: list[str]) -> dict:
    """What a request answered (non-model items since the previous request's
    outputs) and which tools its own outputs called."""
    classes: Counter[str] = Counter()
    tags: Counter[str] = Counter()
    markers: Counter[str] = Counter()
    for payload in items:
        kind = payload.get("type")
        if kind in TOOL_OUTPUTS:
            classes["tool_output"] += 1
            text = output_text(payload)
            for name, pattern in MARKERS.items():
                if pattern.search(text):
                    markers[name] += 1
        elif kind == "agent_message":
            classes["agent_message"] += 1
        elif kind == "message":
            role = payload.get("role")
            text = message_text(payload)
            match = LEADING_TAG.match(text)
            tag = match.group(1) if match else None
            if role == "user" and (tag or text.startswith("# AGENTS.md instructions")):
                classes["contextual"] += 1
                tags["ctx:" + (tag or "agents_md")] += 1
            elif role == "user":
                classes["user"] += 1
            elif role == "developer":
                classes["developer"] += 1
                tags["dev:" + (tag or "untagged")] += 1
                if tag == "completed_phase_checkpoint" and '"source":"context_pressure"' in text:
                    tags["dev:pressure_retirement"] += 1
            else:
                classes[f"message:{role}"] += 1
        else:
            classes[f"other:{kind}"] += 1
    tools: Counter[str] = Counter()
    for payload in outputs:
        if payload.get("type") == "custom_tool_call":
            tools.update(CELL_TOOL.findall(str(payload.get("input") or "")))
        elif payload.get("type") == "function_call":
            tools[str(payload.get("name"))] += 1
    return {"classes": dict(classes), "tags": dict(tags), "markers": dict(markers),
            "tools": dict(tools), "events": sorted(set(events))}


def scan_rollout(path: Path, home: Path) -> dict:
    meta: dict = {}
    turns: dict[str, dict] = {}
    flags: dict[tuple, dict] = {}
    triggers: dict[tuple, dict | None] = {}
    missing_artifacts: Counter[str] = Counter()
    tc_events = tc_reasoning = tc_output = 0
    last_total = None
    context_window = None
    current_turn = None
    pending: list[dict] = []
    trigger: list[dict] | None = None
    outputs: list[dict] = []
    events: list[str] = []
    with open_lines(path) as lines:
        for line in lines:
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            if record.get("type") == "rollout_payload_artifact":
                record = hydrate(record, home)
                if record is None:
                    continue
                if record.get("type") == "_missing_artifact":
                    missing_artifacts[record["payload"]["item_type"]] += 1
                    continue
            kind = record.get("type")
            payload = record.get("payload") or {}
            if kind == "session_meta" and not meta:
                body = payload.get("meta", payload)
                source = body.get("source")
                parent = role = None
                if isinstance(source, dict):
                    sub = source.get("subagent")
                    if isinstance(sub, dict):
                        spawn = sub.get("thread_spawn") or {}
                        parent = spawn.get("parent_thread_id")
                        role = spawn.get("agent_role") or spawn.get("agent_nickname")
                        source = "subagent:" + (next(iter(sub)) if sub else "?")
                    elif isinstance(sub, str):
                        source = "subagent:" + sub
                    else:
                        source = json.dumps(source, sort_keys=True)[:60]
                meta = {
                    "session": body.get("id"), "source": source, "parent": parent,
                    "agentRole": role or body.get("agent_role"), "originator": body.get("originator"),
                    "cliVersion": body.get("cli_version"), "cwd": body.get("cwd"),
                }
            elif kind == "turn_context":
                current_turn = payload.get("turn_id")
                mode = payload.get("collaboration_mode") or {}
                turns.setdefault(current_turn, {
                    "model": payload.get("model"),
                    "effort": payload.get("effort", payload.get("reasoning_effort")),
                    "mode": mode.get("mode") if isinstance(mode, dict) else None,
                })
            elif kind == "response_item":
                if payload.get("type") in MODEL_ITEMS or (
                    payload.get("type") == "message" and payload.get("role") == "assistant"
                ):
                    if trigger is None:
                        trigger, pending = pending, []
                    outputs.append(payload)
                else:
                    pending.append(payload)
            elif kind == "event_msg":
                event = payload.get("type")
                if event in EVENTS:
                    events.append(event)
                if event in ("task_complete", "turn_complete", "turn_aborted"):
                    timing = payload.get("timing")
                    if isinstance(timing, dict):
                        turn_id = payload.get("turn_id")
                        for request in timing.get("modelRequests") or []:
                            key = (turn_id, request.get("generationIndex"), request.get("attemptKind"))
                            flags[key] = {field: request.get(field) for field in FLAG_FIELDS}
                elif event == "token_count":
                    info = payload.get("info") or {}
                    context_window = info.get("model_context_window") or context_window
                    total = info.get("total_token_usage")
                    if total and total != last_total:
                        last = info.get("last_token_usage") or {}
                        tc_events += 1
                        tc_reasoning += int(last.get("reasoning_output_tokens") or 0)
                        tc_output += int(last.get("output_tokens") or 0)
                        last_total = total
                        answered = trigger if trigger is not None else pending
                        if trigger is None:
                            pending = []
                        key = (current_turn, *(last.get(field) for field in (
                            "input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens")))
                        # Identical usage twice in one turn cannot be attributed; keep it unjoined.
                        triggers[key] = None if key in triggers else summarize_trigger(answered, outputs, events)
                        trigger, outputs, events = None, [], []
    return {
        "meta": meta, "turns": turns, "flags": flags, "triggers": triggers,
        "contextWindow": context_window,
        "missingArtifacts": dict(missing_artifacts),
        "tokenCount": {"advancingEvents": tc_events, "reasoningTokens": tc_reasoning, "outputTokens": tc_output},
    }


# ------------------------------------------------------------------- detector
def run_detector(path: Path, detector: Path, repo: Path, timeout: int) -> dict:
    command = [
        "cmd", "/c", str(detector / "rolloutdetect.cmd"), "parse", str(path),
        "--json", "--repo-root", str(repo), "--timeout", str(timeout),
    ]
    done = subprocess.run(command, capture_output=True, timeout=timeout + 60)
    if done.returncode != 0:
        raise RuntimeError(f"detector exit {done.returncode}: {done.stderr.decode('utf-8', 'replace')[-400:]}")
    return json.loads(done.stdout.decode("utf-8-sig"))


def dig(obj, *keys):
    for key in keys:
        if not isinstance(obj, dict):
            return None
        obj = obj.get(key)
    return obj


def rows_for(path: Path, args) -> list[dict]:
    raw = scan_rollout(path, args.home)
    report = run_detector(path, args.detector, args.repo_root, args.timeout)
    meta = raw["meta"]
    rows = []
    runner = report.get("runnerDiagnostics") or {}
    for gen in runner.get("generations") or []:
        tokens = gen.get("tokens") or {}
        observed = tokens.get("observedTotals") or {}
        turn = raw["turns"].get(gen.get("turnId")) or {}
        flag = raw["flags"].get((gen.get("turnId"), gen.get("generationIndex"), gen.get("attemptKind"))) or {}
        cls = gen.get("classification") or {}
        trig = raw["triggers"].get((gen.get("turnId"), *(observed.get(field) for field in (
            "inputTokens", "cachedInputTokens", "outputTokens", "reasoningTokens")))) or {}
        rows.append({
            "kind": "gen", "file": path.name, "session": meta.get("session"),
            "source": meta.get("source"), "parent": meta.get("parent"), "agentRole": meta.get("agentRole"),
            "turnId": gen.get("turnId"), "generationIndex": gen.get("generationIndex"),
            "model": turn.get("model"), "effort": turn.get("effort"), "mode": turn.get("mode"),
            "purpose": gen.get("purpose"), "reason": gen.get("reason"), "attemptKind": gen.get("attemptKind"),
            "physicalAttempts": len(gen.get("physicalAttemptIds") or []),
            "toolCalls": len(gen.get("toolCallIds") or []),
            "class": cls.get("primary"), "tags": cls.get("tags") or [], "interpretation": cls.get("interpretation"),
            "reasoningStatus": dig(tokens, "componentCoverage", "reasoningTokens", "status"),
            **{field: observed.get(field) for field in TOKEN_FIELDS},
            "modelStreamWaitNs": gen.get("modelStreamWaitNs"),
            "flagsJoined": bool(flag),
            **{f"rt_{field}": flag.get(field) for field in FLAG_FIELDS},
            "trigJoined": bool(trig),
            "trigClasses": trig.get("classes"), "trigTags": trig.get("tags"),
            "trigMarkers": trig.get("markers"), "outTools": trig.get("tools"), "events": trig.get("events"),
        })
    population = dig(report, "populations", "all") or {}
    experiments = {
        item.get("hypothesis"): {k: (v or {}).get("total") for k, v in (item.get("evidence") or {}).items()}
        for item in dig(report, "sessionDiagnostics", "usageEfficiency", "experiments") or []
        if isinstance(item, dict)
    }
    rows.append({
        "kind": "session", "file": path.name, **meta,
        "efforts": dict(Counter(str(t.get("effort")) for t in raw["turns"].values())),
        "models": dict(Counter(str(t.get("model")) for t in raw["turns"].values())),
        "generations": len(runner.get("generations") or []),
        "flagsJoined": sum(1 for row in rows if row.get("flagsJoined")),
        "trigJoined": sum(1 for row in rows if row.get("trigJoined")),
        "detectorObserved": dig(population, "tokens", "observedTotals"),
        "detectorCoverage": dig(population, "tokens", "coverage"),
        "providerUsageAttempts": dig(population, "tokens", "providerUsageAttempts"),
        "physicalAttempts": dig(population, "tokens", "physicalAttempts"),
        "nonprogressTokens": population.get("observationalNonprogressTokens"),
        "waitOnlyGenerationCount": population.get("waitOnlyGenerationCount"),
        "internallyDrainedWaitCount": population.get("internallyDrainedWaitCount"),
        "statusCounts": population.get("statusCounts"),
        "coverage": {k: dig(report, "coverage", k) for k in (
            "validCompleteProfiles", "startedTurnsWithoutTerminal", "uniqueTerminalTurns", "parseErrorCount")},
        "requestClassification": dig(runner, "requestClassification", "primaryCounts"),
        "experiments": experiments,
        "tokenCount": raw["tokenCount"],
        "contextWindow": raw["contextWindow"],
        "missingArtifacts": raw["missingArtifacts"],
    })
    return rows


def build(args) -> int:
    files: list[Path] = []
    for root in args.roots:
        if root.is_dir():
            files.extend(sorted(root.rglob("*.jsonl")) + sorted(root.rglob("*.jsonl.zst")))
        else:
            files.append(root)
    skipped = []
    if args.modified_before is not None:
        kept = [f for f in files if f.stat().st_mtime < args.modified_before]
        skipped = [f.name for f in files if f.stat().st_mtime >= args.modified_before]
        files = kept
    started = time.monotonic()
    written = failures = 0
    with args.out.open("w", encoding="utf-8") as out, ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = {pool.submit(rows_for, path, args): path for path in files}
        for future in as_completed(futures):
            path = futures[future]
            try:
                rows = future.result()
            except Exception as error:  # Keep the failure visible; never drop the file silently.
                failures += 1
                rows = [{"kind": "error", "file": path.name, "error": f"{type(error).__name__}: {error}"[:600]}]
            for row in rows:
                out.write(json.dumps(row) + "\n")
                written += 1
        for name in skipped:
            out.write(json.dumps({"kind": "skipped", "file": name, "reason": "modified_after_cutoff"}) + "\n")
    print(f"files={len(files)} skipped_live={len(skipped)} failures={failures} rows={written} "
          f"elapsed={time.monotonic() - started:.0f}s -> {args.out}")
    return 1 if failures else 0


# --------------------------------------------------------------------- report
def load(path: Path):
    gens, sessions, other = [], [], []
    for line in path.open(encoding="utf-8"):
        row = json.loads(line)
        {"gen": gens, "session": sessions}.get(row["kind"], other).append(row)
    return gens, sessions, other


def table(title: str, groups: dict, total: int, limit: int = 40) -> list[dict]:
    print(f"\n== {title}")
    print(f"   {'key':52} {'gens':>7} {'rsn':>11} {'share':>7} {'rsn/gen':>8} {'out':>11} {'unmeas':>6}")
    out = []
    for key, agg in sorted(groups.items(), key=lambda kv: -kv[1]["reasoning"])[:limit]:
        share = 100 * agg["reasoning"] / total if total else 0
        per = agg["reasoning"] / agg["measured"] if agg["measured"] else 0
        print(f"   {str(key)[:52]:52} {agg['gens']:>7,} {agg['reasoning']:>11,} {share:>6.2f}% {per:>8.0f} "
              f"{agg['output']:>11,} {agg['gens'] - agg['measured']:>6}")
        out.append({"key": key, **agg, "share": share})
    return out


def group(gens, keyfn) -> dict:
    groups: dict = defaultdict(lambda: {"gens": 0, "measured": 0, "reasoning": 0, "output": 0})
    for row in gens:
        for key in keyfn(row):
            agg = groups[key]
            agg["gens"] += 1
            if row.get("reasoningTokens") is not None:
                agg["measured"] += 1
                agg["reasoning"] += row["reasoningTokens"]
                agg["output"] += row.get("outputTokens") or 0
    return dict(groups)


def is_root(row) -> bool:
    return not str(row.get("source") or "").startswith("subagent")


def runtime_nonprogress(row) -> bool:
    return bool(row.get("flagsJoined")) and not row.get("rt_progressKinds") and bool(
        row.get("rt_unchangedRelevantState"))


def is_compaction_summary(row) -> bool:
    return (row.get("purpose") == "compaction_recovery" and row.get("reason") == "compaction"
            and not row.get("rt_modelEmittedToolCallCount"))


def mechanism(row) -> str:
    """First matching harness mechanism behind a generation; ordinary work otherwise.

    Membership means the generation followed or contained the mechanism, not that
    its reasoning was avoidable."""
    tags = row.get("trigTags") or {}
    markers = row.get("trigMarkers") or {}
    tools = set(row.get("outTools") or {})
    if row.get("attemptKind") in ("retry", "fallback"):
        return "transport/provider retry or fallback"
    if tags.get("dev:forced_terminal_notice"):
        return "forced terminal synthesis (budget)"
    if tags.get("dev:pressure_retirement"):
        return "after context-pressure retirement"
    if is_compaction_summary(row):
        return "compaction summary"
    if row.get("purpose") == "compaction_recovery":
        return "compaction recovery"
    if markers.keys() & {"preflight_failed", "cargo_launch_rejected", "exec_rejected"}:
        return "harness rejection before execution"
    if markers.get("admission_busy"):
        return "after Rust admission busy"
    if markers.get("output_truncated") and "read_tool_output" in tools:
        return "recovery read after truncation"
    if markers.get("selector_errors"):
        return "after read_file selector error"
    if markers.get("patch_preflight_conflict"):
        return "after patch preflight conflict"
    if markers.get("script_failed"):
        return "after model cell script failure"
    if tools == {"update_plan"}:
        return "plan-only generation"
    if runtime_nonprogress(row):
        return "other runtime nonprogress"
    return "ordinary work"


def mean(values) -> float:
    values = list(values)
    return sum(values) / len(values) if values else 0.0


def ols(points: list[tuple[float, float, float]]) -> tuple[float, float, float] | None:
    """Least squares for y = a + b*x1 + c*x2 via the 3x3 normal equations."""
    rows = [(1.0, x1, x2) for x1, x2, _ in points]
    ys = [y for _, _, y in points]
    m = [[sum(r[i] * r[j] for r in rows) for j in range(3)] + [sum(r[i] * y for r, y in zip(rows, ys))]
         for i in range(3)]
    for col in range(3):
        pivot = max(range(col, 3), key=lambda k: abs(m[k][col]))
        if abs(m[pivot][col]) < 1e-9:
            return None
        m[col], m[pivot] = m[pivot], m[col]
        for k in range(3):
            if k != col:
                factor = m[k][col] / m[col][col]
                m[k] = [a - factor * b for a, b in zip(m[k], m[col])]
    return tuple(m[i][3] / m[i][i] for i in range(3))


def turns_of(gens) -> dict:
    turns: dict = defaultdict(dict)
    for row in gens:
        if row.get("attemptKind") == "primary" and row.get("inputTokens") is not None:
            turns[(row["file"], row["turnId"])][row["generationIndex"]] = row
    return turns


def compaction_windows(turns: dict, k: int, skip: int = 0, purposes=None) -> dict:
    """Same-turn generations before and after each compaction summary."""
    before_r, after_r, before_in, after_in = [], [], [], []
    for gens in turns.values():
        order = sorted(gens)
        for index in order:
            if not is_compaction_summary(gens[index]):
                continue
            keep = [gens[j] for j in order if gens[j].get("reason") != "compaction"
                    and (purposes is None or gens[j].get("purpose") in purposes)]
            before = [row for row in keep if row["generationIndex"] < index][-k:]
            after = [row for row in keep if row["generationIndex"] > index][skip:skip + k]
            if (len(before) == k and len(after) == k
                    and all(r.get("reasoningTokens") is not None for r in before + after)):
                before_r.append(mean(r["reasoningTokens"] for r in before))
                after_r.append(mean(r["reasoningTokens"] for r in after))
                before_in.append(mean(r["inputTokens"] for r in before))
                after_in.append(mean(r["inputTokens"] for r in after))
    return {
        "events": len(before_r),
        "meanInputBefore": round(mean(before_in)) if before_in else None,
        "meanInputAfter": round(mean(after_in)) if after_in else None,
        "meanReasoningBefore": round(mean(before_r), 1) if before_r else None,
        "meanReasoningAfter": round(mean(after_r), 1) if after_r else None,
        "eventsLowerAfter": sum(1 for b, a in zip(before_r, after_r) if a < b),
    }


def context_analysis(gens, sessions) -> dict:
    work = {"implementation", "tool_result_interpretation"}
    primary = [r for r in gens if r.get("attemptKind") == "primary"
               and r.get("inputTokens") is not None and r.get("reasoningTokens") is not None]
    bands = (25_000, 50_000, 100_000, 150_000, 200_000, 300_000, 400_000, 10**12)
    dose: dict = defaultdict(list)
    for row in primary:
        if row.get("effort") == "high" and row.get("purpose") in work:
            band = next(b for b in bands if row["inputTokens"] < b)
            dose[(row["purpose"], band)].append(row["reasoningTokens"])
    print("\n== reasoning per generation by prompt size (effort=high)")
    dose_rows = []
    for (purpose, band), values in sorted(dose.items()):
        label = f"<{band // 1000}K" if band < 10**12 else ">=400K"
        print(f"   {purpose[:26]:26} {label:>7} gens={len(values):>5} mean={mean(values):6.0f} "
              f"median={sorted(values)[len(values) // 2]:>5}")
        dose_rows.append({"purpose": purpose, "band": label, "gens": len(values), "mean": mean(values)})

    by_session: dict = defaultdict(list)
    for row in primary:
        if row.get("purpose") in work:
            by_session[(row["file"], row.get("effort"))].append(row)
    correlations = [spearman([r["inputTokens"] for r in v], [r["reasoningTokens"] for r in v])
                    for v in by_session.values() if len(v) >= 30]
    rhos = sorted(rho for rho in correlations if rho is not None)
    within = {"sessions": len(rhos), "undefinedSessions": len(correlations) - len(rhos),
              "medianRho": round(rhos[len(rhos) // 2], 3) if rhos else None,
              "positive": sum(1 for r in rhos if r > 0), "below_minus_0_2": sum(1 for r in rhos if r < -0.2)}
    print(f"\n== within-session Spearman(input, reasoning), >=30 work generations: {within}")

    compacted = {r["file"] for r in gens if is_compaction_summary(r)}
    coefficients = []
    for (file, _effort), rows in by_session.items():
        if file not in compacted or len(rows) < 30:
            continue
        turn_order: dict = {}
        for row in rows:
            turn_order.setdefault(row["turnId"], len(turn_order))
        rows = sorted(rows, key=lambda r: (turn_order[r["turnId"]], r["generationIndex"]))
        fit = ols([(r["inputTokens"] / 1e5, i / 100, r["reasoningTokens"]) for i, r in enumerate(rows)])
        if fit:
            coefficients.append(fit)
    input_coef = sorted(c[1] for c in coefficients)
    position_coef = sorted(c[2] for c in coefficients)
    partial = {
        "sessions": len(coefficients),
        "medianReasoningPer100kInput": round(input_coef[len(input_coef) // 2], 1) if input_coef else None,
        "inputPositive": sum(1 for c in input_coef if c > 0),
        "medianReasoningPer100Generations": round(position_coef[len(position_coef) // 2], 1)
        if position_coef else None,
        "positionPositive": sum(1 for c in position_coef if c > 0),
    }
    print(f"== OLS reasoning ~ input + position (sessions with a compaction): {partial}")

    turns = turns_of(gens)
    natural = {
        "allPurposes_k6": compaction_windows(turns, 6),
        "allPurposes_k6_skip6": compaction_windows(turns, 6, skip=6),
        "implementation_k3": compaction_windows(turns, 3, purposes={"implementation"}),
        "interpretation_k3": compaction_windows(turns, 3, purposes={"tool_result_interpretation"}),
    }
    print("== compaction natural experiment (same turn, before vs after the summary)")
    for name, value in natural.items():
        print(f"   {name:24} {value}")

    # Proactive retirement (in tree, unpublished) targets 2/3 of the automatic
    # compaction limit, which is 90% of the raw window; token_count reports the
    # 95% effective window.
    windows = {s["file"]: s.get("contextWindow") for s in sessions}
    slope = partial["medianReasoningPer100kInput"] or 0.0
    projection: dict = defaultdict(lambda: {"gens": 0, "reasoning": 0, "regression": 0.0, "proportional": 0.0})
    for row in primary:
        effective = windows.get(row["file"])
        key = str(effective) if effective else "unknown_window"
        agg = projection[key]
        agg["gens"] += 1
        agg["reasoning"] += row["reasoningTokens"]
        if not effective:
            continue
        target = effective / 0.95 * 0.9 * 2 / 3
        if row["inputTokens"] > target:
            agg["regression"] += slope * (row["inputTokens"] - target) / 1e5
            agg["proportional"] += row["reasoningTokens"] * (1 - target / row["inputTokens"])
    print("== modeled reasoning reduction if prompts were held at the proactive-retirement target")
    for key, agg in sorted(projection.items()):
        print(f"   window={key:>14} gens={agg['gens']:>6} reasoning={agg['reasoning']:>10,} "
              f"regression={agg['regression']:>9,.0f} proportional={agg['proportional']:>9,.0f}")
    # Window-independent steady-state ceilings, for comparison with the
    # window-derived target above (285-300K once a 500K window is configured).
    fixed = {}
    print("== same model with a fixed steady-state ceiling for every session")
    for ceiling in (300_000, 228_000, 163_200):
        above = [r for r in primary if r["inputTokens"] > ceiling]
        fixed[ceiling] = {
            "gensAbove": len(above),
            "regression": round(sum(slope * (r["inputTokens"] - ceiling) / 1e5 for r in above)),
            "proportional": round(sum(r["reasoningTokens"] * (1 - ceiling / r["inputTokens"]) for r in above)),
        }
        print(f"   ceiling={ceiling:>7,} {fixed[ceiling]}")
    return {"doseResponse": dose_rows, "withinSession": within, "partialRegression": partial,
            "compactionNaturalExperiment": natural, "proactiveRetirementProjection": dict(projection),
            "fixedCeilingProjection": fixed}


def report(args) -> int:
    gens, sessions, other = load(args.ledger)
    total = sum(r.get("reasoningTokens") or 0 for r in gens)
    output = sum(r.get("outputTokens") or 0 for r in gens)
    tc_reasoning = sum(s["tokenCount"]["reasoningTokens"] for s in sessions)
    det_reasoning = sum((s.get("detectorObserved") or {}).get("reasoningTokens") or 0 for s in sessions)
    unmeasured = sum(1 for r in gens if r.get("reasoningTokens") is None)
    errors = [r for r in other if r["kind"] == "error"]
    skipped = [r for r in other if r["kind"] == "skipped"]
    print(f"sessions={len(sessions)} errors={len(errors)} skipped={len(skipped)} generations={len(gens):,} "
          f"unmeasured_reasoning={unmeasured}")
    print(f"generation reasoning={total:,} output={output:,} share={100 * total / max(output, 1):.1f}%")
    print(f"detector population reasoning={det_reasoning:,}  token_count reasoning={tc_reasoning:,}")
    result = {"totals": {"reasoning": total, "output": output, "tokenCountReasoning": tc_reasoning,
                         "detectorReasoning": det_reasoning, "generations": len(gens),
                         "unmeasured": unmeasured, "sessions": len(sessions),
                         "errors": errors, "skipped": len(skipped)}}
    result["byRole"] = table("root vs subagent", group(gens, lambda r: ["root" if is_root(r) else "subagent"]), total)
    result["byEffort"] = table("effort (root/subagent)", group(
        gens, lambda r: [f"{r.get('effort')}/{'root' if is_root(r) else 'sub'}"]), total)
    result["byPurpose"] = table("purpose", group(gens, lambda r: [r.get("purpose")]), total)
    result["byReason"] = table("reason", group(gens, lambda r: [r.get("reason")]), total)
    result["byClass"] = table("detector primary class", group(gens, lambda r: [r.get("class")]), total)
    result["byTag"] = table("detector tags (overlap)", group(gens, lambda r: r.get("tags") or ["<none>"]), total)
    result["byAttempt"] = table("attempt kind", group(gens, lambda r: [r.get("attemptKind")]), total)
    result["byRuntimeNonprogress"] = table("runtime observational nonprogress (no progress kinds and unchanged state)", group(
        gens, lambda r: ["unjoined" if not r.get("flagsJoined") else
                         "nonprogress" if (not r.get("rt_progressKinds") and r.get("rt_unchangedRelevantState"))
                         else "progress"]), total)
    result["byPurposeEffort"] = table("purpose x effort", group(
        gens, lambda r: [f"{r.get('purpose')}|{r.get('effort')}"]), total, limit=60)
    result["byRoleAgent"] = table("subagent role", group(
        [r for r in gens if not is_root(r)], lambda r: [str(r.get("agentRole"))]), total)
    result["byMechanism"] = table("harness mechanism (first match; membership is not avoidability)",
                                  group(gens, lambda r: [mechanism(r)]), total)
    result["byTriggerMarker"] = table("structured markers in the answered tool output (overlap)", group(
        gens, lambda r: list((r.get("trigMarkers") or {}).keys()) or ["<none>"]
        if r.get("trigJoined") else ["<unjoined>"]), total)
    result["byTriggerTag"] = table("injected context in the answered input (overlap)", group(
        gens, lambda r: list((r.get("trigTags") or {}).keys()) or ["<none>"]
        if r.get("trigJoined") else ["<unjoined>"]), total)
    values = sorted(r["reasoningTokens"] for r in gens if r.get("reasoningTokens") is not None)
    quantum = Counter(values)
    result["distribution"] = {
        "p50": values[len(values) // 2] if values else None,
        "p90": values[int(len(values) * 0.9)] if values else None,
        "p99": values[int(len(values) * 0.99)] if values else None,
        "max": values[-1] if values else None, "zero": quantum[0],
        "top1pctShare": sum(values[-max(1, len(values) // 100):]) / max(total, 1) if values else None,
        "exactValues": {v: quantum[v] for v in (516, 1034, 1552, 2588)},
    }
    print(f"\n== distribution {result['distribution']}")
    result["context"] = context_analysis(gens, sessions)
    if args.json:
        args.json.write_text(json.dumps(result, indent=1, default=str), encoding="utf-8")
        print(f"\nwrote {args.json}")
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    b = sub.add_parser("build")
    b.add_argument("roots", nargs="+", type=Path)
    b.add_argument("--out", required=True, type=Path)
    b.add_argument("--jobs", type=int, default=4)
    b.add_argument("--detector", type=Path, default=DEFAULT_DETECTOR)
    b.add_argument("--home", type=Path, default=DEFAULT_HOME)
    b.add_argument("--repo-root", type=Path, default=DEFAULT_REPO)
    b.add_argument("--timeout", type=int, default=300)
    b.add_argument("--modified-before", type=float, help="skip files modified at/after this Unix time")
    r = sub.add_parser("report")
    r.add_argument("ledger", type=Path)
    r.add_argument("--json", type=Path)
    args = parser.parse_args(argv)
    return build(args) if args.command == "build" else report(args)


if __name__ == "__main__":
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except AttributeError:
        pass
    raise SystemExit(main())
