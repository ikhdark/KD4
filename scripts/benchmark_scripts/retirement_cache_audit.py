#!/usr/bin/env python3
"""Prompt-cache reuse around context-pressure retirement in recorded KD4 rollouts.

Read-only. Nothing here changes harness behavior or configuration.

For every recorded request this joins:
* provider usage (`inputTokens`, `cachedInputTokens`);
* the client's per-item input digest comparison with the preceding request
  (`historyItemsPrevious`, `historyFirstDivergentIndex`);
* whether the request extended the provider response chain
  (`previous_response_id` in `requestSectionSha256ByAttempt`);
* absolute dispatch times (`task_started.started_at` + `dispatchMs`).

A request's item count is the next request's `historyItemsPrevious`, so an
earlier prompt is known to still be a prefix of a later one while no request
in between diverged before that item count. For each request sent without
`previous_response_id` this records the largest such earlier prompt that was
itself sent without one (`B_stateless`) and the largest including chained
prompts (`B_any`), then compares the provider's cached tokens with both.

It also joins context-pressure checkpoint receipts with the persisted
tool-history state (`tool-history/<thread>.json`) to measure how often retired
evidence was recovered, and how often files whose read output was retired were
read again.

usage (repository root):
  python -I -B scripts/benchmark_scripts/retirement_cache_audit.py --since 2026-10-13 [--json OUT]
"""

from __future__ import annotations

import argparse
import collections
import json
import mmap
import re
import statistics
from pathlib import Path

DEFAULT_HOME = Path(r"C:\Users\kuh\Desktop\LOCAL-KD")
CALL_ID = re.compile(r"call_[0-9a-f]{32}")
ARTIFACT = re.compile(r"read_tool_output\(\s*\{[^}]*?artifact_id\s*:\s*[\"'`]([0-9a-f\-]{36})")
READ1 = re.compile(r"read_file\(\s*\{\s*path\s*:\s*[\"'`]([^\"'`]+)[\"'`]")
READN = re.compile(r"read_files\(\s*\[([^\]]*)\]")
QUOTED = re.compile(r"[\"'`]([^\"'`]+)[\"'`]")
HDR = re.compile(r"\*\*\* (?:Add|Update|Delete) File: (.+?)(?=\\n|\n)")
RECOVERY_HORIZON_CELLS = 30
CONFIG_LIMIT = 450_000           # model_auto_compact_token_limit
EFFECTIVE_WINDOW_SHARE = 0.95    # reported model_context_window / resolved window
AUTO_COMPACT_SHARE = 0.9         # limit ceiling as a share of the resolved window
# Retirement that leaves the prompt this close to the limit bought little headroom.
NEAR_LIMIT_SHARE = 0.85
# Cached tokens are reported in 128-token blocks; a boundary "hit" allows a few blocks.
BOUNDARY_SLACK = 1_024
DISTINGUISHABLE = 8_192
BINARY_MARKERS = [
    "retired consumed tool outputs before reaching the compaction limit",
    "proactive context retirement was not recorded",
    "retired consumed tool outputs instead of compacting",
]


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


def is_summary_attempt(request):
    usage = request.get("tokenUsage") or {}
    return request.get("generationReason") == "compaction" and (
        not usage or ((usage.get("reasoningTokens") or 0) == 0 and (usage.get("visibleOutputTokens") or 0) >= 1_000))


def last_attempt_sections(request):
    sections = request.get("requestSectionSha256ByAttempt") or {}
    attempts = request.get("physicalAttemptIds") or list(sections)
    for attempt in reversed(attempts):
        if attempt in sections:
            return sections[attempt]
    return next(iter(sections.values()), None)


def parse_file(path: Path, payloads: Path, seen_requests: set, seen_calls: set):
    thread_id = path.stem[len("rollout-2026-10-14T17-42-33-"):]
    started, window = {}, None
    rows, stream = [], []
    for record in records(path, payloads):
        kind = record.get("type")
        payload = record.get("payload") or {}
        if not isinstance(payload, dict):
            continue
        if kind == "compacted":
            stream.append(("compacted",))
            continue
        if kind == "event_msg":
            event = payload.get("type")
            if event == "token_count":
                last = ((payload.get("info") or {}).get("last_token_usage") or {}).get("input_tokens")
                if last:
                    stream.append(("tokens", last, window))
            if event in ("task_started", "turn_started") and payload.get("turn_id"):
                started[payload["turn_id"]] = payload.get("started_at")
                window = payload.get("model_context_window") or window
            elif event in ("task_complete", "turn_aborted"):
                base = started.get(payload.get("turn_id"))
                for index, request in enumerate((payload.get("timing") or {}).get("modelRequests") or []):
                    key = request.get("samplingRequestId")
                    if key in seen_requests:
                        continue
                    seen_requests.add(key)
                    usage = request.get("tokenUsage") or {}
                    cats = request.get("requestTokenCategories") or {}
                    sections = last_attempt_sections(request)
                    rows.append({
                        "turn": payload.get("turn_id"), "index": index, "window": window,
                        "turn_start": index == 0, "reason": request.get("generationReason"),
                        "fp": request.get("promptCacheKeyFingerprint"), "summary": is_summary_attempt(request),
                        "usage": bool(usage), "I": usage.get("inputTokens") or 0, "C": usage.get("cachedInputTokens") or 0,
                        "V": usage.get("visibleOutputTokens") or 0, "R": usage.get("reasoningTokens") or 0,
                        "T": (request.get("modelStreamWaitNs") or 0) / 1e9,
                        "prev_items": cats.get("historyItemsPrevious"), "div": cats.get("historyFirstDivergentIndex"),
                        "chained": sections is not None and "previous_response_id" in sections,
                        "has_sections": sections is not None,
                        # The first request of a turn has no in-turn predecessor to compare.
                        "has_digests": "historyItemsPrevious" in cats,
                        "dispatch": base + request["dispatchMs"] / 1000 if base and request.get("dispatchMs") is not None else None,
                        "completed": base + request["completedMs"] / 1000 if base and request.get("completedMs") is not None else None,
                    })
            continue
        if kind != "response_item":
            continue
        item = payload.get("type")
        if item == "message" and payload.get("role") == "developer":
            text = json.dumps(payload.get("content"))
            if "<completed_phase_checkpoint>" in text and "context_pressure" in text:
                stream.append(("checkpoint", CALL_ID.findall(text)))
            continue
        call_id = payload.get("call_id")
        if item in ("custom_tool_call", "function_call") and call_id and call_id not in seen_calls:
            seen_calls.add(call_id)
            source = payload.get("input") if item == "custom_tool_call" else payload.get("arguments")
            source = source if isinstance(source, str) else json.dumps(source)
            reads = [norm(x) for x in READ1.findall(source)]
            for group in READN.findall(source):
                reads += [norm(x) for x in QUOTED.findall(group)]
            patches = {norm(h.strip()) for h in HDR.findall(source)}
            artifacts = ARTIFACT.findall(source)
            if payload.get("name") == "read_tool_output":
                try:
                    artifacts.append(json.loads(source)["artifact_id"])
                except (ValueError, KeyError, TypeError):
                    pass
            stream.append(("cell", call_id, reads, patches, artifacts))
    return thread_id, rows, stream


def classify(rows):
    """Tag the requests of one thread and attach the earlier prompts each could reuse."""
    chain = [r for r in rows if r["has_sections"]]
    for row, nxt in zip(chain, chain[1:] + [None]):
        row["n_items"] = nxt["prev_items"] if nxt else None
    out, live, prev, pending_summary = [], [], None, False
    anchor, earlier_hits = None, []
    for row in rows:
        if row["summary"]:
            pending_summary = True
        if not row["has_sections"]:
            live = []
            continue
        div = row["div"]
        live = [e for e in live if div is not None and e["n_items"] <= div]
        if row["usage"] and not row["summary"]:
            rewrite = div is not None and row["prev_items"] is not None and div < row["prev_items"]
            if pending_summary:
                kind = "summary_post"
            elif not row["has_digests"]:
                kind = "turn_start" if row["turn_start"] else "undigested"
            elif rewrite and row["reason"] == "compaction":
                kind = "retire_post"
            elif rewrite:
                kind = "rewrite_other"
            elif row["chained"]:
                kind = "chained"
            elif row["turn_start"]:
                kind = "turn_start"
            else:
                kind = "stateless_append"
            stateless = [e for e in live if not e["chained"]]
            best_s = max(stateless, key=lambda e: e["I"], default=None)
            best_a = max(live, key=lambda e: e["I"], default=None)
            tagged = dict(
                row, kind=kind, prev_I=prev["I"] if prev else None,
                gap=(row["dispatch"] - prev["completed"]) if prev and row["dispatch"] and prev["completed"] else None,
                B_stateless=best_s["I"] if best_s else 0, B_any=best_a["I"] if best_a else 0,
                B_age=(row["dispatch"] - best_s["completed"]) if best_s and row["dispatch"] and best_s["completed"] else None,
                hit_stateless_full=any(abs(row["C"] - e["I"]) <= BOUNDARY_SLACK for e in stateless),
                hit_stateless_cached=any(row["C"] == e["C"] for e in stateless if e["C"]),
                hit_chained_full=any(abs(row["C"] - e["I"]) <= BOUNDARY_SLACK for e in live if e["chained"]),
                anchor=None, repeats_earlier_hit=False,
            )
            if not row["chained"]:
                # The anchor is the latest earlier request sent without
                # previous_response_id; every request since extended it.
                if anchor is not None:
                    tagged["anchor"] = {"kind": anchor["kind"], "I": anchor["I"], "C": anchor["C"],
                                        "div": anchor["div"], "n_items": anchor["n_items"],
                                        "age": (row["dispatch"] - anchor["completed"])
                                        if row["dispatch"] and anchor["completed"] else None}
                tagged["repeats_earlier_hit"] = any(abs(row["C"] - h) <= 128 for h in earlier_hits)
                if row["C"]:
                    earlier_hits.append(row["C"])
                anchor = tagged
            elif prev is not None and not prev["chained"] and row["C"]:
                # The first chained request after a stateless one shows that prompt's own boundary.
                earlier_hits.append(row["C"])
            out.append(tagged)
            pending_summary = False
        if row["usage"]:
            prev = row
            if row["I"] and row["n_items"] is not None:
                live.append(row)
    return out


def row_limit(row):
    """Automatic-compaction limit for the context window configured for this turn."""
    window = row["window"]
    return min(CONFIG_LIMIT, round(window / EFFECTIVE_WINDOW_SHARE * AUTO_COMPACT_SHARE)) if window else CONFIG_LIMIT


def retirement_events(thread):
    """Each retirement post with what followed it in the same thread."""
    events = []
    for k, r in enumerate(thread):
        if r["kind"] != "retire_post":
            continue
        limit = row_limit(r)
        following = None
        for j in range(k + 1, len(thread)):
            if thread[j]["kind"] in ("retire_post", "summary_post"):
                following = (thread[j]["kind"], j - k, thread[j]["turn"] == r["turn"])
                break
        events.append({"I": r["I"], "C": r["C"], "prev_I": r["prev_I"], "post_share": r["I"] / limit,
                       "next": following, "remaining_requests": len(thread) - k - 1, "turn": r["turn"]})
    return events


def anchor_relation(r):
    a = r["anchor"]
    if a is None:
        return "no_anchor"
    if abs(r["C"] - a["I"]) <= BOUNDARY_SLACK or r["C"] > a["I"]:
        return "anchor_prompt_reused"
    if abs(r["C"] - a["C"]) <= 128:
        return "same_as_anchor_hit"
    return "above_anchor_hit" if r["C"] > a["C"] else "below_anchor_hit"


def boundary_outcome(r):
    if r["C"] >= r["B_any"] - BOUNDARY_SLACK:
        return "reused_chained_content" if r["C"] > r["B_stateless"] + BOUNDARY_SLACK else "full"
    if abs(r["C"] - r["B_stateless"]) <= BOUNDARY_SLACK:
        return "at_last_stateless_prompt"
    if r["C"] > r["B_stateless"]:
        return "between"
    return "below_last_stateless_prompt"


def tool_history_state(home: Path, thread_id: str):
    path = home / "tool-history" / f"{thread_id}.json"
    try:
        state = json.loads(path.read_text(encoding="utf-8"))["state"]
    except (OSError, ValueError, KeyError):
        return None
    parent = {nested: call for call, results in state.get("code_mode_nested_evidence", {}).items() for nested in results}
    owner = {}
    # Same precedence as ToolHistoryState::rebuild_artifact_index, with nested
    # code-mode results attributed to the cell that produced them.
    for artifact, origin in state.get("internal_artifact_origins", {}).items():
        if isinstance(origin, list) and origin:
            owner[artifact] = parent.get(origin[0], origin[0])
    for call_id, candidate in state.get("candidates", {}).items():
        if candidate.get("artifact_id"):
            owner.setdefault(candidate["artifact_id"], call_id)
    return {"candidates": state.get("candidates", {}), "owner": owner,
            "recovered": {parent.get(c, c) for c in state.get("recovered_call_ids", [])}}


def retirement_recovery(stream, state):
    """Per retired call: age at retirement, recovery after retirement, later re-read of its files."""
    out = []
    cells = [(i, e) for i, e in enumerate(stream) if e[0] == "cell"]
    rank = {e[1]: n for n, (_, e) in enumerate(cells)}
    resets = [i for i, e in enumerate(stream) if e[0] == "compacted"] + [len(stream)]
    retired = set()
    for position, entry in enumerate(stream):
        if entry[0] != "checkpoint":
            continue
        end = next(r for r in resets if r > position)
        before = sum(1 for i, _ in cells if i < position)
        later_all = [e for i, e in cells if position < i < end]
        later = later_all[:RECOVERY_HORIZON_CELLS]
        for call_id in entry[1]:
            if call_id in retired or call_id not in rank:
                continue
            retired.add(call_id)
            candidate = (state or {}).get("candidates", {}).get(call_id)
            owner = (state or {}).get("owner", {})
            paths = set(cells[rank[call_id]][1][2])
            patched = set()
            reread = False
            for e in later:
                if paths & (set(e[2]) - patched):
                    reread = True
                    break
                patched |= paths & e[3]
            out.append({
                "age_cells": before - rank[call_id],
                "mapped": candidate is not None,
                "tokens": (candidate or {}).get("original_tokens"),
                "recovered_in_horizon": any(owner.get(a) == call_id for e in later for a in e[4]),
                "recovered_before_reset": any(owner.get(a) == call_id for e in later_all for a in e[4]),
                "recovered_ever": call_id in (state or {}).get("recovered", set()),
                "read_paths": len(paths),
                "reread": reread,
            })
    return out


def retirement_depth(stream, state):
    """Per checkpoint: tokens it removed, and eligible consumed output it left inline.

    Removed tokens come from the token count the runtime emits after
    discounting retired context. Eligible output is every complete, projection
    eligible candidate consumed before the newest three generations and not yet
    retired, in the candidates' original-token units.
    """
    if not state:
        return []
    candidates = state["candidates"]
    order = {e[1]: i for i, e in enumerate(stream) if e[0] == "cell"}
    retired, out = set(), []
    for position, entry in enumerate(stream):
        if entry[0] != "checkpoint":
            continue
        before = next((e for e in reversed(stream[:position]) if e[0] == "tokens"), None)
        after = next((e for e in stream[position + 1:] if e[0] == "tokens"), None)
        preceding = sorted((c for call, c in candidates.items() if order.get(call, len(stream)) < position),
                           key=lambda c: order[c["call_id"]])
        # Like pressure_retirement_selection: the generations that consumed the
        # newest outputs stay inline.
        kept = []
        for c in reversed(preceding):
            if len(kept) >= 3:
                break
            generation = c.get("consumed_by_generation")
            if generation and generation not in kept:
                kept.append(generation)
        now = [c for c in entry[1] if c in candidates and c not in retired]
        retired.update(entry[1])
        eligible = [c for c in preceding
                    if c["call_id"] not in retired and c.get("complete") and c.get("projection_eligible")
                    and c.get("successful") and c.get("consumed_by_generation")
                    and c["consumed_by_generation"] not in kept]
        out.append({
            "trigger_tokens": before[1] if before else None,
            "limit": row_limit({"window": before[2]}) if before else None,
            "measured_saved": before[1] - after[1] if before and after else None,
            "retired_original_tokens": sum(candidates[c].get("original_tokens") or 0 for c in now),
            "eligible_left_original_tokens": sum(c.get("original_tokens") or 0 for c in eligible),
        })
    return out


def baseline_reread(stream):
    """Unpatched re-reads of never-retired file reads, over the same horizon starting 4 cells later."""
    retired = {c for e in stream if e[0] == "checkpoint" for c in e[1]}
    cells = [e for e in stream if e[0] == "cell"]
    hits = total = 0
    for n, entry in enumerate(cells):
        if not entry[2] or entry[1] in retired:
            continue
        paths, patched = set(entry[2]), set()
        for e in cells[n + 1: n + 4]:
            patched |= paths & e[3]
        total += 1
        for e in cells[n + 4: n + 4 + RECOVERY_HORIZON_CELLS]:
            if paths & (set(e[2]) - patched):
                hits += 1
                break
            patched |= paths & e[3]
    return hits, total


def summarize_depth(depth):
    measured = [d for d in depth if d["measured_saved"] and d["retired_original_tokens"] and d["limit"]]
    ratio = (sum(d["measured_saved"] for d in measured) / sum(d["retired_original_tokens"] for d in measured)
             if measured else None)
    # In-context tokens an all-eligible retirement would have removed, and the prompt it would leave.
    floor = [(d["trigger_tokens"] - d["measured_saved"] - ratio * d["eligible_left_original_tokens"]) / d["limit"]
             for d in measured] if ratio else []
    return {
        "checkpoints": len(depth), "measured": len(measured),
        "in_context_saved_per_original_token": ratio,
        "measured_saved_median": q([d["measured_saved"] for d in measured], .5),
        "trigger_share_of_limit_median": q([d["trigger_tokens"] / d["limit"] for d in measured], .5),
        "after_share_of_limit": {f: q([(d["trigger_tokens"] - d["measured_saved"]) / d["limit"] for d in measured], f)
                                 for f in (.25, .5, .75)},
        "eligible_left_in_context_median": q([ratio * d["eligible_left_original_tokens"] for d in measured], .5) if ratio else None,
        "all_eligible_after_share_of_limit": {f: q(floor, f) for f in (.1, .25, .5, .75, .9)},
    }


def deployment(home: Path):
    manifest_path = home / "runtime" / "desktop-app-server-runtime.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    result = {k: manifest.get(k) for k in ("executablePath", "buildCommit", "buildDirty", "buildProfile", "publishedBuildMatch")}
    try:
        with open(manifest["executablePath"], "rb") as handle, mmap.mmap(handle.fileno(), 0, access=mmap.ACCESS_READ) as data:
            result["markers"] = {m: data.find(m.encode()) != -1 for m in BINARY_MARKERS}
    except (OSError, KeyError, ValueError):
        result["markers"] = None
    return result


def q(values, fraction):
    values = sorted(values)
    return values[int(fraction * (len(values) - 1))] if values else None


def summarize(rows):
    by_kind = collections.defaultdict(list)
    for r in rows:
        by_kind[r["kind"]].append(r)
    kinds = {}
    for kind, xs in sorted(by_kind.items()):
        kinds[kind] = {
            "requests": len(xs),
            "cached_share_median": statistics.median(r["C"] / r["I"] for r in xs if r["I"]),
            "uncached_tokens": sum(r["I"] - r["C"] for r in xs),
            "below_half_cached": sum(1 for r in xs if r["I"] and r["C"] < 0.5 * r["I"]),
        }
    stateless = [r for r in rows if not r["chained"]]
    outcome = collections.defaultdict(collections.Counter)
    by_age = collections.defaultdict(collections.Counter)
    for r in stateless:
        if r["B_any"] - r["B_stateless"] < DISTINGUISHABLE:
            outcome[r["kind"]]["indistinguishable"] += 1
            continue
        result = boundary_outcome(r)
        outcome[r["kind"]][result] += 1
        if r["B_age"] is not None:
            bucket = "<5 min" if r["B_age"] < 300 else "5-60 min" if r["B_age"] < 3600 else ">=60 min"
            by_age[bucket][result] += 1
    matches = collections.defaultdict(collections.Counter)
    for r in stateless:
        matches[r["kind"]]["n"] += 1
        matches[r["kind"]]["C_matches_earlier_stateless_prompt"] += r["hit_stateless_full"]
        matches[r["kind"]]["C_equals_earlier_stateless_cached"] += r["hit_stateless_cached"] and not r["hit_stateless_full"]
        matches[r["kind"]]["C_matches_earlier_chained_prompt"] += r["hit_chained_full"]
    chained = by_kind["chained"]
    chained_miss = [max(0, (r["I"] - r["C"]) - max(0, r["I"] - (r["prev_I"] or 0))) for r in chained if r["prev_I"]]
    retire = by_kind["retire_post"]
    relation = collections.defaultdict(collections.Counter)
    for r in stateless:
        relation[r["kind"]][anchor_relation(r)] += 1
        relation[r["kind"]]["repeats_earlier_hit"] += r["repeats_earlier_hit"]
    # A retirement that diverged well past its anchor's own divergence shares
    # strictly more of the anchor's prompt than the anchor's cache hit covered.
    past_anchor = [r for r in retire if r["anchor"] and r["anchor"]["div"] is not None
                   and r["anchor"]["kind"] == "retire_post" and r["div"] > r["anchor"]["div"] + 20]
    turn_gap = collections.defaultdict(list)
    for r in by_kind["turn_start"]:
        if r["gap"] is None or not r["I"]:
            continue
        bucket = "<1 min" if r["gap"] < 60 else "1-5 min" if r["gap"] < 300 else "5-60 min" if r["gap"] < 3600 else ">=60 min"
        turn_gap[bucket].append(r)
    return {
        "kinds": kinds,
        "stateless_relation_to_anchor": {k: dict(v) for k, v in relation.items()},
        "retire_post_past_anchor_divergence": {
            "n": len(past_anchor),
            "same_as_anchor_hit": sum(1 for r in past_anchor if anchor_relation(r) == "same_as_anchor_hit"),
            "anchor_prompt_reused": sum(1 for r in past_anchor if anchor_relation(r) == "anchor_prompt_reused"),
            "items_past_anchor_divergence_median": q([r["div"] - r["anchor"]["div"] for r in past_anchor], .5),
            "anchor_age_seconds_median": q([r["anchor"]["age"] for r in past_anchor if r["anchor"]["age"] is not None], .5),
        },
        "turn_start_by_idle_gap": {
            b: {"n": len(v), "cached_share_median": q([r["C"] / r["I"] for r in v], .5),
                "below_half": sum(1 for r in v if r["C"] < 0.5 * r["I"]),
                "prompt_smaller_than_previous": sum(1 for r in v if r["prev_I"] and r["I"] < r["prev_I"]),
                "uncached_tokens": sum(r["I"] - r["C"] for r in v)}
            for b, v in sorted(turn_gap.items())},
        "chained_uncached_beyond_new_tokens": {"median": q(chained_miss, .5), "p90": q(chained_miss, .9),
                                               "over_8k": sum(1 for x in chained_miss if x > 8_192), "n": len(chained_miss)},
        "stateless_boundary_outcome_by_kind": {k: dict(v) for k, v in outcome.items()},
        "stateless_boundary_outcome_by_boundary_age": {k: dict(v) for k, v in by_age.items()},
        "stateless_exact_matches_by_kind": {k: dict(v) for k, v in matches.items()},
        "retire_post": {
            "count": len(retire),
            "chained": sum(r["chained"] for r in retire),
            "divergence_share_of_items": {"p25": q([r["div"] / r["prev_items"] for r in retire], .25),
                                          "median": q([r["div"] / r["prev_items"] for r in retire], .5),
                                          "p75": q([r["div"] / r["prev_items"] for r in retire], .75)},
            "gap_seconds_median": q([r["gap"] for r in retire if r["gap"] is not None], .5),
            "uncached_median": q([r["I"] - r["C"] for r in retire], .5),
            "uncached_beyond_last_stateless_prompt_median": q([r["I"] - max(r["C"], r["B_stateless"]) for r in retire], .5),
        },
    }


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--home", type=Path, default=DEFAULT_HOME)
    parser.add_argument("--since", default="2026-10-13")
    parser.add_argument("--json", type=Path)
    parser.add_argument("--rows", type=Path, help="write the per-request dataset as JSON lines")
    args = parser.parse_args(argv)
    payloads = args.home / "rollout-payloads"
    files = sorted(p for p in (args.home / "sessions").rglob("rollout-*.jsonl") if p.name[8:18] >= args.since)
    seen_requests, seen_calls, all_rows, recovery, events, depth = set(), set(), [], [], [], []
    reread_hits = reread_total = checkpoints = 0
    for path in files:
        thread_id, rows, stream = parse_file(path, payloads, seen_requests, seen_calls)
        threads = collections.OrderedDict()
        for row in rows:
            threads.setdefault(row["fp"], []).append(row)
        for thread_rows in threads.values():
            tagged = classify(thread_rows)
            all_rows += [dict(r, file=path.name) for r in tagged]
            events += retirement_events(tagged)
        checkpoints += sum(1 for e in stream if e[0] == "checkpoint")
        if any(e[0] == "checkpoint" for e in stream):
            state = tool_history_state(args.home, thread_id)
            recovery += retirement_recovery(stream, state)
            depth += retirement_depth(stream, state)
        hits, total = baseline_reread(stream)
        reread_hits += hits
        reread_total += total
    age_buckets = collections.defaultdict(list)
    for r in recovery:
        bucket = "04-20 cells" if r["age_cells"] <= 20 else "21-60 cells" if r["age_cells"] <= 60 else "61+ cells"
        age_buckets[bucket].append(r)
    mapped = [r for r in recovery if r["mapped"]]
    near_limit = [e for e in events if e["post_share"] >= NEAR_LIMIT_SHARE]
    report = {
        "deployment": deployment(args.home),
        "files": len(files), "requests": len(all_rows), "pressure_checkpoints": checkpoints,
        "cache": summarize(all_rows),
        "retirement_spacing": {
            "events": len(events),
            "post_share_of_limit": {"p25": q([e["post_share"] for e in events], .25),
                                    "median": q([e["post_share"] for e in events], .5),
                                    "p75": q([e["post_share"] for e in events], .75)},
            "next_event": dict(collections.Counter(
                "none" if e["next"] is None else f"{e['next'][0]} {'<=5' if e['next'][1] <= 5 else '6-20' if e['next'][1] <= 20 else '>20'} requests"
                for e in events)),
            "near_limit": {
                "events": len(near_limit), "uncached_tokens": sum(e["I"] - e["C"] for e in near_limit),
                "next_event": dict(collections.Counter("none" if e["next"] is None else e["next"][0] for e in near_limit)),
                "saved_tokens_median": q([e["prev_I"] - e["I"] for e in near_limit if e["prev_I"]], .5),
            },
            "other": {"events": len(events) - len(near_limit),
                      "uncached_tokens": sum(e["I"] - e["C"] for e in events if e["post_share"] < NEAR_LIMIT_SHARE)},
        },
        "retirement_depth": summarize_depth(depth),
        "retired_evidence": {
            "retired_outputs": len(recovery),
            "mapped_to_artifact": len(mapped),
            "mapped_tokens": sum(r["tokens"] or 0 for r in mapped),
            "recovered_within_horizon": sum(r["recovered_in_horizon"] for r in mapped),
            "recovered_before_next_compaction": sum(r["recovered_before_reset"] for r in mapped),
            "recovered_ever_by_state": sum(r["recovered_ever"] for r in mapped),
            "with_file_reads": sum(1 for r in recovery if r["read_paths"]),
            "unpatched_file_reread_within_horizon": sum(r["reread"] for r in recovery),
            "by_age": {b: {"n": len(v), "mapped": sum(r["mapped"] for r in v),
                           "recovered_within_horizon": sum(r["recovered_in_horizon"] for r in v),
                           "with_reads": sum(1 for r in v if r["read_paths"]), "reread": sum(r["reread"] for r in v)}
                       for b, v in sorted(age_buckets.items())},
            "baseline_never_retired_reads": {"reread": reread_hits, "cells": reread_total},
        },
    }
    if args.rows:
        with args.rows.open("w", encoding="utf-8") as handle:
            for r in all_rows:
                handle.write(json.dumps({k: v for k, v in r.items() if k != "fp"}) + "\n")
    text = json.dumps(report, indent=1, default=str)
    if args.json:
        args.json.write_text(text, encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
