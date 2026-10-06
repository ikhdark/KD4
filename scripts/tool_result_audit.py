#!/usr/bin/env python3
"""Deterministic payload audit of explicitly selected rollout snapshots.

No private reasoning is inferred: field mentions are lexical observations only.
Token estimates use the harness's ceil(UTF-8 bytes / 4), not provider billing.
"""

from __future__ import annotations

import argparse
import bisect
import collections
import contextlib
import difflib
import hashlib
import heapq
import itertools
import json
import re
import tempfile
from pathlib import Path

try:
    from scripts.atomic_json import write_stream_atomic
    from scripts.rollout_snapshot import existing_rollout_path, hydrate_rollout_record, read_rollout_snapshot
except ImportError:
    from atomic_json import write_stream_atomic
    from rollout_snapshot import existing_rollout_path, hydrate_rollout_record, read_rollout_snapshot


FIELDS = (
    "output",
    "exit_code",
    "error",
    "session_id",
    "execution_state",
    "process_exited",
    "output_complete",
    "output_reduced",
    "stdout",
    "stderr",
    "streams_complete",
    "artifact_id",
    "raw_output_artifact_id",
    "results",
    "complete",
    "continuation",
    "continuation_stop",
    "canonical_sha256",
    "source_sha256",
    "retained_bytes",
    "canonical_bytes",
    "subdivision_plan",
    "child_selectors",
    "structuredContent",
    "content",
    "description",
    "tools",
)
TERMINAL_EVENTS = {"task_complete", "turn_aborted"}
ARTIFACT = re.compile(
    r'"(?P<key>raw_output_artifact_id|artifact_id)"\s*:\s*"'
    r'(?P<id>[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})"'
)


def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def text_body(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(
            text_body(item.get("text", item))
            if isinstance(item, dict)
            else text_body(item)
            for item in value
        )
    return encoded(value).decode("utf-8")


def json_packets(text):
    """Only complete JSON lines/whole packets; never parse clipped fragments."""
    try:
        return [json.loads(text)]
    except ValueError:
        packets = []
        for line in text.splitlines():
            if line.startswith(("{", "[")):
                try:
                    packets.append(json.loads(line))
                except ValueError:
                    pass
        return packets


def structures(value, pointer=""):
    yield pointer, value
    if isinstance(value, dict):
        for key, item in value.items():
            yield from structures(
                item, pointer + "/" + key.replace("~", "~0").replace("/", "~1")
            )
    elif isinstance(value, list):
        for index, item in enumerate(value):
            yield from structures(item, f"{pointer}/{index}")


def source_read_paths(source):
    """Lexical shell-read candidates, not a shell parser or freshness proof."""
    candidates = re.findall(
        r"Get-Content\s+(?:(?:-Raw|-LiteralPath|-Path)\s+)*"
        r"(?:'([^']+)'|\"([^\"]+)\"|([^\s;|()'\"{}]+))", source,
        flags=re.IGNORECASE,
    )
    return sorted({
        path for parts in candidates if (path := next(p for p in parts if p))
        and not path.startswith(("-", "$"))
    })


def execution_timings(records):
    """Merge checkpoint deltas by identity; terminal arrays remain authoritative."""
    turns = {}
    current = None
    for line, row, _ in records:
        payload = row.get("payload", {})
        kind = payload.get("type")
        if kind == "task_started":
            current = payload.get("turn_id")
        checkpoint = payload.get("timing_checkpoint") or {}
        if row["type"] == "sampling_boundary":
            turn_id = checkpoint.get("turn_id", current)
            timing = checkpoint.get("timing", checkpoint)
        elif kind in TERMINAL_EVENTS:
            turn_id = payload.get("turn_id", current)
            timing = payload.get("timing") or {}
        else:
            continue
        turn = turns.setdefault(turn_id, {
            "turn_id": turn_id, "completed": False, "completion_record": None,
            "terminal_status": None, "terminal_record": None,
            "checkpoint_records": [], "modelRequests": {}, "toolCalls": {},
            "terminal_fields": [],
        })
        terminal = kind in TERMINAL_EVENTS
        if terminal:
            turn["completed"] = kind == "task_complete"
            turn["completion_record"] = line if turn["completed"] else None
            turn["terminal_status"] = kind
            turn["terminal_record"] = line
        else:
            turn["checkpoint_records"].append(line)
        for field, identity in (("modelRequests", "samplingRequestId"), ("toolCalls", "callId")):
            if field not in timing or (not terminal and field in turn["terminal_fields"]):
                continue
            if terminal:
                turn[field] = {}
                if field not in turn["terminal_fields"]:
                    turn["terminal_fields"].append(field)
            for item in timing[field]:
                # Never silently merge unrelated observations without identity.
                key = item.get(identity)
                if key is None:
                    raise ValueError(f"missing {identity} in timing record {line}")
                previous = turn[field].setdefault(key, {})
                previous.update({k: v for k, v in item.items() if v is not None})
    return [{**turn, **{field: list(turn[field].values())
                       for field in ("modelRequests", "toolCalls")}}
            for turn in turns.values()]


def tool_call_trace(records):
    """Inventory actual dispatches, not lexical mentions or checkpoint mirrors.

    Keep source/receipt references so reviewers can annotate purpose and material
    contribution without pretending telemetry proves semantic consumption.
    Nested inputs/outputs are not separately recorded in ordinary rollouts.
    """
    calls, outputs, timings = {}, collections.defaultdict(list), {}
    turn_id = None
    completed_turns = set()
    aborted_turns = set()
    for line, row, _ in records:
        payload = row.get("payload", {})
        kind = payload.get("type")
        if kind == "task_started":
            turn_id = payload.get("turn_id")
        if row["type"] == "response_item":
            call_id = payload.get("call_id")
            key = (turn_id, call_id)
            if kind in {"function_call", "custom_tool_call"}:
                calls[key] = {
                    "turn_id": turn_id, "call_id": call_id, "record": line,
                    "tool": payload.get("name"), "source": "direct",
                    "input": payload.get("input", payload.get("arguments")),
                }
            elif kind in {"function_call_output", "custom_tool_call_output"}:
                body = text_body(payload.get("output", "")).encode("utf-8")
                outputs[key].append({
                    "record": line, "bytes": len(body),
                    "sha256": hashlib.sha256(body).hexdigest(),
                })
        if kind == "task_complete":
            completed = payload.get("turn_id", turn_id)
            completed_turns.add(completed)
        elif kind == "turn_aborted":
            aborted_turns.add(payload.get("turn_id", turn_id))
    for turn in execution_timings(records):
        for timing in turn["toolCalls"]:
            timings[(turn["turn_id"], timing["callId"])] = timing
    for key, timing in timings.items():
        calls.setdefault(key, {
            "turn_id": key[0], "call_id": key[1], "record": None,
            "tool": timing.get("toolName"), "source": timing.get("source"),
            "input": None,
        })
    for key, call in calls.items():
        timing = timings.get(key, {})
        parent = calls.get((key[0], timing.get("parentCallId")))
        call.update(
            parent_call_id=timing.get("parentCallId"),
            input_source_record=parent["record"] if parent else call["record"],
            outputs=outputs.get(key, []),
            output_container_records=[o["record"] for o in outputs.get(
                (key[0], timing.get("parentCallId")), []
            )] if parent else [],
            outcome=timing.get("outcome"),
            duration_ms=timing.get("totalDurationMs"),
            timing_available=bool(timing),
            material_contribution=None,
            semantic_consumption="requires_source_review",
        )
    ordered = sorted(calls.values(), key=lambda call: (
        call["input_source_record"] if call["input_source_record"] is not None else float("inf"),
        call["source"] != "direct", call["call_id"],
    ))
    return {
        "calls": ordered,
        "coverage": {
            "records_scanned": len(records), "calls": len(ordered),
            "outer_calls": sum(c["record"] is not None for c in ordered),
            "nested_calls": sum(c["source"] == "code_mode" for c in ordered),
            "completed_turns": len(completed_turns),
            "aborted_turns": len(aborted_turns),
            "terminal_turns": len(completed_turns | aborted_turns),
            "calls_without_timing": [c["call_id"] for c in ordered if not c["timing_available"]],
            "outer_calls_without_output": [c["call_id"] for c in ordered if c["record"] is not None and not c["outputs"]],
            "orphan_output_records": [o["record"] for key, values in outputs.items()
                                      if key not in calls for o in values],
        },
        "limitations": [
            "Nested arguments and return values are unavailable independently; inspect their parent source/output references.",
            "Unfinished turns use checkpoint deltas merged by callId; their uncheckpointed tail remains unknown. Source mentions are not dispatch counts.",
            "Purpose and material contribution require source review; output delivery is not proof of consumption.",
            "Parent and child durations overlap and must not be added as wall-clock time.",
        ],
    }


def repeated_input_overlap(previous, current):
    """Conditional bounds, not a tokenizer-aligned attribution of cache hits.

    An unchanged non-history manifest and a completely reused history prefix
    support using the previous provider input length as an append-only proxy.
    Missing telemetry, compaction, or changed instructions must fail closed.
    """
    if previous is None:
        return None
    before = previous.get("local_prompt_categories", {})
    after = current.get("local_prompt_categories", {})
    old_hashes = before.get("promptSectionSha256", {})
    new_hashes = after.get("promptSectionSha256", {})
    if not old_hashes or old_hashes.keys() != new_hashes.keys():
        return None
    if any(old_hashes[k] != new_hashes[k] for k in old_hashes if k != "history"):
        return None
    prefix = after.get("historyItemsPrevious")
    if (type(prefix) is not int or prefix <= 0
            or after.get("historyPrefixItemsReused") != prefix
            or after.get("historyFirstDivergentIndex") != prefix):
        return None
    repeated = previous.get("provider_usage", {}).get("inputTokens")
    usage = current.get("provider_usage", {})
    total, cached = usage.get("inputTokens"), usage.get("cachedInputTokens")
    if (not all(type(n) is int for n in (repeated, total, cached))
            or not 0 <= repeated <= total or not 0 <= cached <= total):
        return None
    return {
        "basis": "conditional_append_only_previous_provider_input",
        "repeated_input_proxy": repeated,
        "cached_overlap_min": max(0, repeated + cached - total),
        "cached_overlap_max": min(repeated, cached),
        "uncached_overlap_min": max(0, repeated - cached),
        "uncached_overlap_max": min(repeated, total - cached),
    }


def execution_context_audit(records):
    """Keep provider usage separate from local prompt estimates and disk records."""
    turns = []
    categories = collections.Counter()
    usage = collections.Counter()
    bulk = collections.defaultdict(list)
    boundaries = {}
    outputs = []
    previous_outputs = []
    identical_outputs = {}
    block_index = collections.defaultdict(set)
    # Bound the optional fuzzy text analysis, not record/usage coverage. Exact
    # identities bypass SequenceMatcher; omissions remain explicit lower bounds.
    pair_budget = 10000
    match_work_budget = 2000000
    current_turn = None
    started = set()
    last_reported_usage = None
    for line, row, wire_bytes in records:
        payload = row.get("payload", {})
        kind = payload.get("type")
        if kind == "token_count":
            reported = (payload.get("info") or {}).get("total_token_usage")
            if reported:
                last_reported_usage = {"record": line, "usage": reported}
        if kind == "task_started":
            current_turn = payload.get("turn_id")
            started.add(current_turn)
        if row["type"] == "sampling_boundary":
            boundaries[payload.get("sampling_request_id")] = line
        if row["type"] in {"tool_manifest", "sampling_boundary"}:
            bulk[row["type"]].append(encoded(payload))
        if row["type"] == "response_item" and kind in {
            "function_call_output",
            "custom_tool_call_output",
        }:
            body = text_body(payload.get("output", ""))
            lines = tuple(body.splitlines(keepends=True))
            byte_prefix = [0]
            for output_line in lines:
                byte_prefix.append(byte_prefix[-1] + len(output_line.encode("utf-8")))
            blocks = {tuple(lines[i:i + 4]) for i in range(max(0, len(lines) - 3))}
            candidates = set()
            for block in blocks:
                candidates.update(block_index.get(block, ()))
            covered = set()
            matches = []
            candidate_pairs = sum(len(previous_outputs[i][1]) for i in candidates)
            selected_pairs = min(pair_budget, candidate_pairs)
            skipped = candidate_pairs - selected_pairs
            # Index distinct bodies, not every replay. A thousand identical
            # outputs have one posting, while bounded evidence keeps record IDs.
            prior_records = heapq.merge(*(previous_outputs[i][1] for i in candidates))
            for prior_line, prior_index in itertools.islice(prior_records, selected_pairs):
                prior_lines = previous_outputs[prior_index][0]
                pair_budget -= 1
                if prior_lines == lines:
                    matching = [difflib.Match(0, 0, len(lines))]
                else:
                    # The nested occurrence visits dominate repetitive inputs;
                    # n*m is a conservative input-size work estimate.
                    work = len(prior_lines) * len(lines)
                    if work > match_work_budget:
                        skipped += 1
                        continue
                    match_work_budget -= work
                    matching = difflib.SequenceMatcher(
                        None, prior_lines, lines, autojunk=False
                    ).get_matching_blocks()
                for match in matching:
                    size = byte_prefix[match.b + match.size] - byte_prefix[match.b]
                    if match.size >= 4 and size >= 160:
                        covered.update(range(match.b, match.b + match.size))
                        matches.append(
                            {
                                "prior_record": prior_line,
                                "start_line": match.b + 1,
                                "lines": match.size,
                                "bytes": size,
                            }
                        )
            outputs.append(
                {
                    "record": line,
                    "turn_id": current_turn,
                    "call_id": payload.get("call_id"),
                    "bytes": len(body.encode("utf-8")),
                    "repeated_block_bytes": sum(
                        len(lines[i].encode("utf-8")) for i in covered
                    ),
                    "matching_blocks": matches,
                    "matching_blocks_coverage": {
                        "candidate_pairs": candidate_pairs,
                        "omitted_pairs": skipped,
                        "complete": skipped == 0,
                    },
                }
            )
            prior_index = identical_outputs.get(lines)
            if prior_index is None:
                prior_index = len(previous_outputs)
                identical_outputs[lines] = prior_index
                previous_outputs.append((lines, []))
                for block in blocks:
                    block_index[block].add(prior_index)
            previous_outputs[prior_index][1].append((line, prior_index))
    # audit() supplies outputs in record order. Index the owning turn once,
    # rather than scanning every output for every retained request.
    outputs_by_turn = collections.defaultdict(list)
    for output in outputs:
        outputs_by_turn[output["turn_id"]].append(output)
    output_records_by_turn = {
        turn_id: [output["record"] for output in turn_outputs]
        for turn_id, turn_outputs in outputs_by_turn.items()
    }
    request_boundaries_by_turn = collections.defaultdict(list)
    for timing in execution_timings(records):
        requests = timing["modelRequests"]
        for field in ("modelRequests", "toolCalls"):
            bulk[field].append(encoded(timing.get(field, [])))
        rounds = []
        previous = None
        previous_boundary = 0
        for request in requests:
            tokens = request.get("tokenUsage") or {}
            context = request.get("requestTokenCategories") or {}
            boundary = boundaries.get(request.get("samplingRequestId"))
            turn_id = timing["turn_id"]
            output_records = output_records_by_turn.get(turn_id, [])
            added_outputs = [] if boundary is None else outputs_by_turn[turn_id][
                bisect.bisect_right(output_records, previous_boundary):
                bisect.bisect_left(output_records, boundary)
            ]
            if boundary is not None:
                request_boundaries_by_turn[turn_id].append(boundary)
            input_tokens = tokens.get("inputTokens")
            cached = tokens.get("cachedInputTokens")
            rounds.append(
                {
                    "generation": request.get("generationIndex"),
                    "sampling_request_id": request.get("samplingRequestId"),
                    "boundary_record": boundary,
                    "provider_usage": tokens,
                    "provider_uncached_input_tokens": input_tokens - cached
                    if input_tokens is not None and cached is not None
                    else None,
                    "input_growth_since_previous_request": input_tokens - previous
                    if previous is not None and input_tokens is not None
                    else None,
                    "local_prompt_categories": context,
                    "added_tool_result_records": [o["record"] for o in added_outputs],
                    "added_tool_result_bytes": sum(o["bytes"] for o in added_outputs),
                }
            )
            rounds[-1]["repeated_input_overlap"] = repeated_input_overlap(
                rounds[-2] if len(rounds) > 1 else None, rounds[-1]
            )
            previous = input_tokens
            if boundary is not None:
                previous_boundary = boundary
            usage.update({k: v for k, v in tokens.items() if isinstance(v, int)})
            categories.update(
                {
                    k: context[k]
                    for k in (
                        "baseInstructions",
                        "toolSchemas",
                        "conversationHistory",
                        "currentInput",
                        "repositoryContext",
                        "skills",
                        "otherInjectedContext",
                        "logicalTotal",
                        "repeatedUnchangedContext",
                        "providerReconciliationResidual",
                    )
                    if isinstance(context.get(k), int)
                }
            )
        turns.append(
            {
                "turn_id": timing["turn_id"],
                "completed": timing["completed"],
                "completion_record": timing["completion_record"],
                "terminal_status": timing["terminal_status"],
                "terminal_record": timing["terminal_record"],
                "timing_source": "terminal" if "modelRequests" in timing["terminal_fields"] else "checkpoint",
                "checkpoint_records": timing["checkpoint_records"],
                "request_count": len(requests),
                "rounds": rounds,
            }
        )
    completed = {t["turn_id"] for t in turns if t["completed"]}
    terminal = {t["turn_id"] for t in turns if t["terminal_status"] is not None}
    aborted = {t["turn_id"] for t in turns if t["terminal_status"] == "turn_aborted"}
    # Weighted only within the owning turn, not across compaction/turn boundaries.
    for turn_boundaries in request_boundaries_by_turn.values():
        turn_boundaries.sort()
    for output in outputs:
        turn_boundaries = request_boundaries_by_turn[output["turn_id"]]
        exposure = len(turn_boundaries) - bisect.bisect_right(
            turn_boundaries, output["record"]
        )
        output["subsequent_requests_in_turn"] = exposure
        output["raw_replay_estimated_tokens"] = ((output["bytes"] + 3) // 4) * exposure
    reconciliation = {"status": "unavailable", "last_token_count": last_reported_usage,
                      "request_minus_cumulative": {}}
    if last_reported_usage is not None:
        fields = {
            "input_tokens": ("inputTokens",),
            "cached_input_tokens": ("cachedInputTokens",),
            "output_tokens": ("visibleOutputTokens", "reasoningTokens"),
            "reasoning_output_tokens": ("reasoningTokens",),
            "total_tokens": ("totalTokens",),
        }
        for name, keys in fields.items():
            reported = last_reported_usage["usage"].get(name)
            if type(reported) is int and all(key in usage for key in keys):
                reconciliation["request_minus_cumulative"][name] = sum(usage[key] for key in keys) - reported
        differences = reconciliation["request_minus_cumulative"]
        if differences:
            reconciliation["status"] = (
                "different" if any(differences.values())
                else "matched" if len(differences) == len(fields)
                else "partial_match"
            )
    return {
        "turns": turns,
        "coverage": {
            "records_scanned": len(records),
            "started_turns": len(started),
            "completed_turns": len(completed),
            "aborted_turns": len(aborted),
            "terminal_turns": len(terminal),
            "unfinished_turn_ids": sorted(started - terminal, key=str),
            "requests_with_usage": sum(
                bool(r["provider_usage"]) for t in turns for r in t["rounds"]
            ),
            "requests_without_usage": sum(
                not r["provider_usage"] for t in turns for r in t["rounds"]
            ),
        },
        "provider_usage_totals": dict(usage),
        "provider_usage_reconciliation": reconciliation,
        "provider_uncached_input_tokens": usage["inputTokens"]
        - usage["cachedInputTokens"]
        if usage
        else None,
        "repeated_uncached_input_tokens": None,
        "local_prompt_category_totals": dict(categories),
        "tool_outputs": outputs,
        "bulk_records": {
            k: {
                "count": len(v),
                "bytes": sum(map(len, v)),
                "ordered_sha256": hashlib.sha256(b"\n".join(v)).hexdigest(),
                "distinct_sha256": sorted({hashlib.sha256(x).hexdigest() for x in v}),
            }
            for k, v in sorted(bulk.items())
        },
        "limitations": [
            "Provider input includes cached input; visible output and reasoning are disjoint here.",
            "Repeated uncached context cannot be identified from aggregate cache usage or prompt section hashes.",
            "Per-round repeated_input_overlap is conditional on append-only tokenization of the reused history and unchanged non-history sections; bounds are not exact repeated-cache attribution. Missing evidence is null, not zero.",
            "Local prompt categories are tokenizer estimates, not additive provider billing attribution; history includes tool results.",
            "Input growth is net change, not an exact decomposition of newly added context.",
            "Raw replay estimates assume output survives subsequent requests in its turn; they are exposure proxies, not measured savings.",
            "Matching text blocks are byte-equal evidence, not proof they are semantically unnecessary or current.",
            "Repeated-block matching is budgeted; matching_blocks_coverage reports skipped candidate pairs. Repeated bytes are a lower bound when coverage is incomplete. Four-line indexing excludes only pairs that cannot meet the minimum block length.",
            "Terminal arrays take precedence; otherwise incremental checkpoints are merged by request/call identity, never summed as independent requests.",
            "Aborted turns are terminal, not successful completions; terminal status does not establish complete usage or resolved tools.",
            "Unfinished executions report observed usage only; requests without usage and uncheckpointed tail work are not assumed free or complete.",
            "Usage reconciliation compares request totals with the last cumulative token event; differences can reflect inherited usage or different capture horizons, not necessarily double counting.",
        ],
    }


def audit(path):
    records = []
    incomplete_tail = False
    with read_rollout_snapshot(path) as snapshot, snapshot.open_lines() as lines:
        for number, line in enumerate(lines, 1):
            try:
                item = json.loads(line)
            except ValueError:
                if line.endswith(b"\n"):
                    raise ValueError(
                        f"invalid complete record {path}:{number}"
                    ) from None
                incomplete_tail = True
                break
            if not isinstance(item, dict):
                raise ValueError(f"non-object record {path}:{number}")
            records.append(
                (number, hydrate_rollout_record(item, snapshot.path), len(line))
            )
    session = next(
        (r["payload"] for _, r, _ in records if r["type"] == "session_meta"), {}
    )
    home = next(
        (p.parent for p in path.parents if p.name in {"sessions", "archived_sessions"}),
        None,
    )
    artifact_root = (
        home / "tool-output" / session.get("id", "missing") if home else None
    )
    calls = {}
    outputs = []
    manifests = collections.Counter()
    manifest_bytes = 0
    timings = {}
    metadata = collections.Counter()
    repeated = {}
    artifact_cache = {}
    for index, (line, row, wire_bytes) in enumerate(records):
        payload = row.get("payload", {})
        kind = payload.get("type")
        if row["type"] == "tool_manifest":
            manifests[hashlib.sha256(encoded(payload)).hexdigest()] += 1
            manifest_bytes += wire_bytes
        if kind in TERMINAL_EVENTS and payload.get("timing"):
            timings[payload.get("turn_id", str(line))] = payload["timing"]
        if row["type"] != "response_item":
            continue
        if kind in {"function_call", "custom_tool_call"}:
            calls[payload["call_id"]] = payload
            continue
        if kind not in {"function_call_output", "custom_tool_call_output"}:
            continue
        body = text_body(payload.get("output", ""))
        data = body.encode("utf-8")
        call_id = payload["call_id"]
        call = calls.get(call_id, {})
        source = text_body(call.get("input", call.get("arguments", "")))
        digest = hashlib.sha256(data).hexdigest()
        following = []
        next_call = None
        for _, later, _ in records[index + 1 :]:
            p = later.get("payload", {})
            if later["type"] == "response_item" and p.get("type") in {
                "function_call",
                "custom_tool_call",
            }:
                next_source = text_body(p.get("input", p.get("arguments", "")))
                next_call = {
                    "call_id": p["call_id"],
                    "tool": p.get("name"),
                    "shared_shell_read_paths": sorted(
                        set(source_read_paths(source))
                        & set(source_read_paths(next_source))
                    ),
                    "recovery_call_mentions": len(
                        re.findall(r"(?:tools\.)?read_tool_output\s*\(", next_source)
                    ),
                }
                break
            if later["type"] == "response_item" and (
                p.get("type") == "reasoning"
                or (p.get("type") == "message" and p.get("role") == "assistant")
            ):
                following.append(text_body(p.get("summary", p.get("content", ""))))
        subsequent = "\n".join(following)
        artifact_refs = []
        for match in ARTIFACT.finditer(body):
            key = match["id"]
            if key not in artifact_cache:
                evidence = {"id": key, "available": False}
                if artifact_root is not None:
                    raw_path = artifact_root / f"{key}.log"
                    meta_path = artifact_root / f"{key}.meta.json"
                    if raw_path.is_file():
                        raw = raw_path.read_bytes()
                        evidence.update(
                            available=True,
                            retained_file_bytes=len(raw),
                            sha256=hashlib.sha256(raw).hexdigest(),
                            path=str(raw_path),
                        )
                        if meta_path.is_file():
                            meta = json.loads(meta_path.read_bytes())
                            evidence["canonical_bytes"] = meta.get("canonical_bytes")
                            evidence["canonical_sha256"] = meta.get("canonical_sha256")
                            evidence["canonical_verified"] = evidence[
                                "sha256"
                            ] == meta.get("canonical_sha256") and len(raw) == meta.get(
                                "canonical_bytes"
                            )
                artifact_cache[key] = evidence
            ref = {"field": match["key"], "id": key}
            if ref not in artifact_refs:
                artifact_refs.append(ref)
        lists = []
        descriptions = []
        exact_mirrors = 0
        for packet_index, packet in enumerate(json_packets(body)):
            for pointer, value in structures(packet, f"/{packet_index}"):
                if isinstance(value, list) and len(value) > 50:
                    lists.append(
                        {
                            "pointer": pointer,
                            "items": len(value),
                            "bytes": len(encoded(value)),
                        }
                    )
                if not isinstance(value, dict):
                    continue
                for field in FIELDS:
                    if field in value:
                        metadata[field] += 1
                if isinstance(value.get("description"), str):
                    desc = value["description"]
                    descriptions.append(
                        {
                            "pointer": pointer,
                            "bytes": len(desc.encode()),
                            "json_escape_bytes": len(encoded(desc))
                            - len(desc.encode())
                            - 2,
                        }
                    )
                if "structuredContent" in value and isinstance(
                    value.get("content"), list
                ):
                    for content in value["content"]:
                        if (
                            isinstance(content, dict)
                            and set(content) == {"type", "text"}
                            and content["type"] == "text"
                        ):
                            try:
                                if (
                                    json.loads(content["text"])
                                    == value["structuredContent"]
                                ):
                                    exact_mirrors += len(encoded(content))
                            except (ValueError, TypeError):
                                pass
        outputs.append(
            {
                "line": line,
                "call_id": call_id,
                "tool": call.get("name"),
                "raw_tool_result_bytes": None,
                "visible_bytes": len(data),
                "visible_estimated_tokens": (len(data) + 3) // 4,
                "sha256": digest,
                "exact_duplicate_of": repeated.get(digest),
                "truncation_marker": any(
                    marker in body
                    for marker in (
                        "truncated output",
                        '"output_truncated":true',
                        "[... ",
                        "[omitted ",
                    )
                ),
                "recovery_call_mentions": len(
                    re.findall(r"(?:tools\.)?read_tool_output\s*\(", source)
                ),
                "producer_field_mentions": [
                    field
                    for field in FIELDS
                    if re.search(
                        r"\." + field + r"\b|\[['\"]" + field + r"['\"]\]", source
                    )
                ],
                "later_reasoning_field_mentions": [
                    field
                    for field in FIELDS
                    if re.search(r"\b" + field + r"\b", subsequent)
                ],
                "artifact_refs": artifact_refs,
                "oversized_lists": lists,
                "descriptions": descriptions,
                "exact_mirror_bytes": exact_mirrors,
                "nested_tool_mentions": dict(
                    sorted(
                        collections.Counter(
                            re.findall(r"tools\.([\w]+)\s*\(", source)
                        ).items()
                    )
                ),
                "shell_read_paths": source_read_paths(source),
                "next_call": next_call,
            }
        )
        repeated.setdefault(digest, call_id)
    counters = collections.Counter()
    for timing in timings.values():
        for key, value in timing.get("counters", {}).items():
            if key.startswith(("toolOutput", "truncationInduced")) and isinstance(
                value, int
            ):
                counters[key] += value
    return {
        "snapshot": snapshot.metadata(),
        "records": len(records),
        "incomplete_tail": incomplete_tail,
        "session_id": session.get("id"),
        "results": outputs,
        "artifacts": list(artifact_cache.values()),
        "manifest_records": sum(manifests.values()),
        "distinct_manifest_hashes": len(manifests),
        "manifest_wire_bytes": manifest_bytes,
        "metadata_occurrences": dict(metadata),
        "timing_counter_totals": dict(counters),
        "execution_context": execution_context_audit(records),
        "tool_call_trace": tool_call_trace(records),
        "summary": {
            "results": len(outputs),
            "visible_bytes": sum(o["visible_bytes"] for o in outputs),
            "visible_estimated_tokens": sum(
                o["visible_estimated_tokens"] for o in outputs
            ),
            "truncated_results": sum(o["truncation_marker"] for o in outputs),
            # Keep the old name for saved-report consumers, but do not mistake
            # quoted source/diagnostics for measured runtime truncation.
            "truncation_marker_results": sum(o["truncation_marker"] for o in outputs),
            "recovery_call_mentions": sum(o["recovery_call_mentions"] for o in outputs),
            "exact_duplicate_results": sum(
                o["exact_duplicate_of"] is not None for o in outputs
            ),
            "oversized_lists": sum(len(o["oversized_lists"]) for o in outputs),
        },
    }


def summary_limit(value):
    limit = int(value)
    if not 1 <= limit <= 10:
        raise argparse.ArgumentTypeError("summary limit must be between 1 and 10")
    return limit


def check_report_sources(report):
    """Compare raw source snapshots without parsing or hydrating their records.

    A matching prefix proves append-only growth at this observation, not that
    the report covers appended records. Referenced artifacts are not revalidated.
    """
    checks = []
    cached = {}
    for session in report["sessions"]:
        previous = session["snapshot"]
        key = (previous["path"], previous["byteLength"], previous["sha256"])
        if key in cached:
            checks.append(dict(cached[key]))
            continue
        check = {"status": "unavailable", "snapshot_bytes": previous["byteLength"],
                 "snapshot_sha256": previous["sha256"]}
        try:
            current = read_rollout_snapshot(Path(previous["path"]))
            with contextlib.closing(current.stream):
                check.update(observed_bytes=current.byte_length,
                             observed_sha256=current.sha256)
                if current.byte_length == previous["byteLength"] and current.sha256 == previous["sha256"]:
                    check["status"] = "unchanged"
                elif current.path.name.endswith(".zst") != Path(previous["path"]).name.endswith(".zst"):
                    # The shared snapshot reader can follow .jsonl -> .jsonl.zst.
                    # Compressed bytes cannot establish a JSONL prefix identity.
                    check["status"] = "representation_changed"
                elif current.byte_length < previous["byteLength"]:
                    check["status"] = "shortened"
                else:
                    digest = hashlib.sha256()
                    remaining = previous["byteLength"]
                    current.stream.seek(0)
                    while remaining:
                        chunk = current.stream.read(min(remaining, 1024 * 1024))
                        if not chunk:
                            raise OSError("source snapshot ended before recorded prefix")
                        digest.update(chunk)
                        remaining -= len(chunk)
                    check["status"] = (
                        "appended" if digest.hexdigest() == previous["sha256"]
                        else "changed"
                    )
                    if check["status"] == "appended":
                        check["unaudited_bytes"] = current.byte_length - previous["byteLength"]
        except OSError as error:
            # Do not print an unbounded error/path; the ledger retains the path.
            check.update(status="unavailable", error_type=type(error).__name__,
                         errno=error.errno)
        cached[key] = check
        checks.append(dict(check))
    return checks


def compact_report(report, path, digest, byte_length, limit=5, source_checks=None):
    """Project counts and ranked references, never source text or arbitrary metadata.

    JSON pointers address the immutable, hashed report, not current live logs.
    Each list is globally bounded (not multiplied by the session count).
    """
    if not 1 <= limit <= 10:
        raise ValueError("summary limit must be between 1 and 10")
    sessions = report["sessions"]
    if source_checks is not None and len(source_checks) != len(sessions):
        raise ValueError("source checks must cover every session")
    totals = collections.Counter()
    results = []
    followups = []
    unavailable = []
    coverage = []
    provider = collections.Counter()
    terminal_truncation = collections.Counter()
    terminal_truncation_sessions = 0
    reconciliation_statuses = collections.Counter()
    for index, session in enumerate(sessions):
        prefix = f"/sessions/{index}"
        totals.update(session["summary"])
        if "truncation_marker_results" not in session["summary"]:
            totals["truncation_marker_results"] += session["summary"].get("truncated_results", 0)
        counters = session.get("timing_counter_totals", {})
        if type(counters.get("toolOutputTruncationCount")) is int:
            terminal_truncation_sessions += 1
        for key in ("toolOutputTruncationCount", "truncationInducedContinuationCount"):
            if type(counters.get(key)) is int:
                terminal_truncation[key] += counters[key]
        totals["records"] += session["records"]
        totals["incomplete_tails"] += bool(session["incomplete_tail"])
        context = session.get("execution_context", {})
        covered = context.get("coverage", {})
        unfinished = covered.get("unfinished_turn_ids")
        totals["sessions_without_turn_coverage"] += not bool(covered)
        totals["unfinished_turns"] += len(unfinished or [])
        for field in ("completed_turns", "aborted_turns", "terminal_turns",
                      "requests_with_usage", "requests_without_usage"):
            if field in covered:
                totals[field] += covered[field]
        usage = context.get("provider_usage_totals", {})
        totals["sessions_without_provider_usage"] += not bool(usage)
        provider.update({key: usage[key] for key in (
            "inputTokens", "cachedInputTokens", "visibleOutputTokens", "reasoningTokens", "totalTokens"
        ) if type(usage.get(key)) is int})
        reconciliation = context.get("provider_usage_reconciliation", {}).get("status", "unavailable")
        reconciliation_statuses[reconciliation] += 1
        totals["repeated_block_bytes"] += sum(
            row["repeated_block_bytes"] for row in context.get("tool_outputs", [])
        )
        totals["repeated_block_omitted_pairs"] += sum(
            row.get("matching_blocks_coverage", {}).get("omitted_pairs", 0)
            for row in context.get("tool_outputs", [])
        )
        coverage.append({
            "pointer": prefix,
            "records": session["records"],
            "results": session["summary"]["results"],
            "visible_bytes": session["summary"]["visible_bytes"],
            "incomplete_tail": session["incomplete_tail"],
            "completed_turns": covered.get("completed_turns"),
            "aborted_turns": covered.get("aborted_turns"),
            "terminal_turns": covered.get("terminal_turns"),
            "unfinished_turns": len(unfinished) if unfinished is not None else None,
            "requests_with_usage": covered.get("requests_with_usage"),
            "requests_without_usage": covered.get("requests_without_usage"),
            "provider_input_tokens": usage.get("inputTokens"),
            "usage_reconciliation": reconciliation,
        })
        if source_checks is not None:
            coverage[-1]["source_check"] = source_checks[index]
        for result_index, result in enumerate(session["results"]):
            row = {
                "pointer": f"{prefix}/results/{result_index}",
                "rollout_line": result["line"],
                "visible_bytes": result["visible_bytes"],
                "visible_estimated_tokens": result["visible_estimated_tokens"],
                "truncation_marker": result["truncation_marker"],
            }
            results.append(row)
            next_call = result.get("next_call") or {}
            shared = len(next_call.get("shared_shell_read_paths", []))
            recovery = next_call.get("recovery_call_mentions", 0)
            if result["truncation_marker"] and (shared or recovery):
                followups.append(dict(row, shared_read_paths=shared,
                                      next_recovery_mentions=recovery))
        for artifact_index, artifact in enumerate(session["artifacts"]):
            if not artifact["available"]:
                unavailable.append({"pointer": f"{prefix}/artifacts/{artifact_index}"})
    # Stable input order resolves equal-size ties. Never rank by completion order.
    results.sort(key=lambda row: -row["visible_bytes"])
    followups.sort(key=lambda row: -row["visible_bytes"])

    def bounded(rows):
        return {"total": len(rows), "omitted": max(0, len(rows) - limit),
                "items": rows[:limit]}

    return {
        "report": str(path.resolve()),
        "report_sha256": digest,
        "report_bytes": byte_length,
        "source_check": {
            "status": "checked" if source_checks is not None else "not_checked",
            "counts": dict(sorted(collections.Counter(
                check["status"] for check in source_checks or []
            ).items())),
            "scope": "Raw snapshot bytes only; not later appends or referenced artifacts. Audit totals remain those of the saved report.",
        },
        "totals": dict(sorted(totals.items())),
        "terminal_truncation": {
            "sessions_with_counts": terminal_truncation_sessions,
            "tool_output_truncations": terminal_truncation.get("toolOutputTruncationCount"),
            "induced_continuations": terminal_truncation.get("truncationInducedContinuationCount"),
            "scope": "Recorded terminal counters only; open turns and missing counters are not zero.",
        },
        "provider_usage_totals": dict(provider),
        "provider_usage_reconciliation": dict(sorted(reconciliation_statuses.items())),
        "sessions": bounded(coverage),
        "largest_results": bounded(results),
        "follow_up_candidates": bounded(followups),
        "unavailable_artifact_references": bounded(unavailable),
        "limitations": [
            "Provider usage totals count observed requests, including cached input; unfinished/pending requests remain incomplete. Other token estimates use ceil(bytes/4), not provider attribution.",
            "Reconciliation status unavailable includes older ledgers. Different totals can reflect inherited usage or differing capture horizons; inspect each session's execution_context.",
            "truncated_results is a legacy alias of truncation_marker_results: lexical matches including quoted source, not runtime truncations. Follow-ups are candidates, not causal or dispatch measurements.",
            "Unavailable references may be quoted or cross-session IDs, not lost artifacts.",
            "repeated_block_bytes is a lower bound when repeated_block_omitted_pairs is nonzero; record and token coverage are unaffected by the text-matching budget.",
            "Pointers address the hashed report snapshot; counts cover all records, displayed lists are bounded. Full limitations: /limitations.",
        ],
    }


def describe_contract():
    """Small, side-effect-free contract for report consumers; not sample data."""
    return {
        "report_version": 2,
        "commands": {
            "create": "tool_result_audit.py ROLLOUT [ROLLOUT ...] --output NEW_REPORT",
            "reuse": "tool_result_audit.py --from-report REPORT [--summary-limit 1..10] [--check-sources]",
        },
        "output": "stdout is a bounded summary; the report is the complete ledger. Never scrape Markdown tables.",
        "paths": "Rollout paths are explicit and all checked before analysis; relative paths use the caller cwd. Plain-to-compressed handoff is supported. No checkout discovery or ancestor scans.",
        "publication": "New reports only; existing destinations fail before analysis. Atomic no-overwrite publication.",
        "metric_semantics": {
            "/sessions/*/summary/truncated_results": "Legacy alias of truncation_marker_results; lexical output-marker matches, including quoted source, not measured truncations.",
            "/sessions/*/timing_counter_totals": "Recorded terminal-turn counters only; missing values and open turns are not zero.",
            "/sessions/*/execution_context/coverage": "Completed and aborted turns are terminal; unfinished_turn_ids excludes both. Terminal status does not prove usage or tool completeness. Older ledgers can lack terminal/aborted counts.",
        },
        "record_fields": {
            "/sessions/*/results/*": {"line": "1-based source record", "call_id": "outer call ID"},
            "/sessions/*/tool_call_trace/calls/*": {
                "record": "1-based outer call record; null for nested calls",
                "input_source_record": "parent source record for nested calls",
                "output_container_records": "parent result records for nested calls",
            },
            "/sessions/*/execution_context/tool_outputs/*": {"record": "1-based output record"},
        },
        "recovery": "Summary pointers address the hashed report, not a live rollout. Reuse --from-report; do not rerun the producer for omitted summary rows.",
        "freshness": "Replay is not_checked by default. --check-sources hashes current raw snapshots, distinguishes appended/changed/shortened/unavailable/representation_changed sources, and leaves saved audit totals unchanged. Referenced artifacts are not revalidated.",
        "python_snapshot_api": {
            "decoded": "read_rollout_records(Path) returns list[(hydrated_record, wire_bytes)] and closes its snapshot",
            "owned": "with read_rollout_snapshot(Path) as snapshot: ...",
            "lines": "with snapshot.open_lines() as lines: ...; hydrate_rollout_record(json.loads(line), snapshot.path)",
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--describe", action="store_true", help="Print the bounded CLI/report/Python API contract without reading logs")
    parser.add_argument("paths", type=Path, nargs="*")
    parser.add_argument(
        "--from-report", type=Path,
        help="Project an existing version 2 ledger without rescanning logs or writing files",
    )
    parser.add_argument(
        "--summary-limit", type=summary_limit, default=5,
        help="Maximum entries per summary list, globally across sessions (1-10; default 5)",
    )
    parser.add_argument(
        "--check-sources", action="store_true",
        help="With --from-report, hash current raw snapshots and report growth/change without re-auditing records",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="New complete JSON evidence ledger; never overwrite",
    )
    args = parser.parse_args()
    if args.describe:
        if args.paths or args.output or args.from_report or args.check_sources:
            parser.error("--describe cannot be combined with paths, --output, --from-report or --check-sources")
        print(json.dumps(describe_contract()))
        return
    if args.from_report:
        if args.paths or args.output:
            parser.error("--from-report cannot be combined with paths or --output")
        data = args.from_report.read_bytes()
        report = json.loads(data)
        if report.get("version") != 2:
            parser.error("--from-report requires a version 2 audit ledger")
        print(json.dumps(compact_report(
            report, args.from_report, hashlib.sha256(data).hexdigest(), len(data),
            args.summary_limit,
            check_report_sources(report) if args.check_sources else None,
        )))
        return
    if args.check_sources:
        parser.error("--check-sources requires --from-report")
    if not args.paths or args.output is None:
        parser.error("provide rollout paths and --output, or use --from-report")
    # Detect known destination conflicts before parsing/hydrating every rollout.
    # lstat also rejects a dangling link. Publication checks again atomically.
    try:
        args.output.lstat()
    except FileNotFoundError:
        pass
    else:
        raise FileExistsError(f"audit report already exists: {args.output}; reuse --from-report or choose a new output")
    for path in args.paths:
        candidate = existing_rollout_path(path)
        if not candidate.is_file():
            raise FileNotFoundError(
                f"rollout is not an existing file: {candidate.absolute()}; "
                f"relative paths use cwd {Path.cwd()}; no alternate checkout was searched"
            )
    report = {
        "version": 2,
        "limitations": [
            "UTF-8 bytes are exact; tokens are ceil(bytes/4) estimates, not provider attribution.",
            "Raw nested return objects are not generally logged. Artifact bytes describe referenced evidence, not necessarily the entire call envelope.",
            "Counter totals may include both nested and outer projections; do not equate them with model-visible response-item totals.",
            "Field mentions and recovery-call source mentions are lexical evidence, not measured mental consumption or proven causality.",
            "Lists/descriptions/metadata are classified only inside complete JSON packets. Textual lists are not classified.",
            "raw_tool_result_bytes is unknown unless independently instrumented; raw artifact evidence and aggregate canonical counters are not a substitute.",
            "The next call and shared Get-Content paths are follow-up candidates only, not proof that truncation caused a reread.",
        ],
        "sessions": [audit(path) for path in args.paths],
    }
    digest = hashlib.sha256()
    byte_length = 0
    with tempfile.SpooledTemporaryFile(max_size=4 * 1024 * 1024) as handle:
        for chunk in json.JSONEncoder(ensure_ascii=False, indent=2).iterencode(report):
            data = chunk.encode("utf-8")
            handle.write(data)
            digest.update(data)
            byte_length += len(data)
        handle.write(b"\n")
        digest.update(b"\n")
        byte_length += 1
        handle.seek(0)
        write_stream_atomic(args.output, handle, exclusive=True)
    print(json.dumps(compact_report(
        report, args.output, digest.hexdigest(), byte_length, args.summary_limit,
    )))


if __name__ == "__main__":
    main()
