#!/usr/bin/env python3
"""Read-only per-request reasoning ledger from KD4 turn timing profiles.

Joins every model request recorded in a turn's terminal timing profile
(`task_complete` / `turn_aborted`) with the turn's recorded model/effort settings.
Checkpoint-only turns require --include-checkpoints and remain labeled partial.
Missing or conflicting turn settings remain unknown. Payloads are resolved from
`<home>/rollout-payloads/<sha256>.json`.

Every figure is copied from provider-reported usage inside the profile; nothing
is estimated. Requests without usage (interrupted streams) keep `null` tokens
and are counted separately, never as zero.

Usage:
  reasoning_profile_ledger.py ROOT... --out LEDGER.jsonl [--home DIR]
"""

from __future__ import annotations

import argparse
import tempfile
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import (
    discover_rollouts, existing_rollout_path, hydrate_rollout_record,
    read_rollout_snapshot,
)
from scripts.common.atomic_json import write_stream_atomic

DEFAULT_HOME = Path(r"C:\Users\kuh\Desktop\LOCAL-KD")
REQUEST_FIELDS = (
    "generationIndex", "generationReason", "generationPurpose", "disposition",
    "attemptKind", "isContinuation", "modelStreamWaitNs", "decisionLatencyNs",
    "toolCallCount", "modelEmittedToolCallCount", "outputTokens",
    "reasoningOutputTokens", "progressKinds", "nextStructuredActionChanged",
    "unchangedRelevantState", "dispatchMs", "firstModelOutputMs",
    "firstActionableOutputMs", "completedMs", "samplingRequestId",
    "requestDiagnosticsStatus",
)


def hydrate(record: dict, home: Path) -> dict:
    """Authenticate the complete reference against the explicitly selected home."""
    return hydrate_rollout_record(record, home / "sessions" / "rollout.jsonl")


def iter_records(path: Path, home: Path, hydrate_types=("event_msg", "sampling_boundary", "session_meta")):
    """Yield verified records; unavailable or malformed evidence fails the scan."""
    with read_rollout_snapshot(path) as snapshot:
        for number, record, error, _ in snapshot.decoded_lines():
            if error:
                raise ValueError(f"{path}:{number}: {error}")
            if record.get("type") == "rollout_payload_artifact":
                record = hydrate(record, home)
            yield number, record


def scan(path: Path, home: Path):
    meta: dict = {}
    settings: list[dict] = []
    terminals: dict[str, tuple[str, dict]] = {}
    checkpoints: dict[str, dict] = {}
    token_counts = 0
    reasoning_from_token_count = 0
    last_total = None
    for _, record in iter_records(path, home):
        kind = record.get("type")
        payload = record.get("payload") or {}
        if kind == "session_meta":
            body = payload.get("meta", payload)
            source = body.get("source")
            parent = None
            if isinstance(source, dict):
                sub = source.get("subagent")
                if isinstance(sub, dict):
                    parent = (sub.get("thread_spawn") or {}).get("parent_thread_id")
                source = "subagent" if sub else json.dumps(source, sort_keys=True)[:60]
            meta = {"id": body.get("id"), "source": source, "parent": parent,
                    "cwd": body.get("cwd"), "originator": body.get("originator")}
        elif kind == "turn_context":
            settings.append({
                "turn_id": payload.get("turn_id"), "model": payload.get("model"),
                "effort": payload.get("effort", payload.get("reasoning_effort")),
                "summary": payload.get("summary"),
            })
        elif kind == "sampling_boundary":
            checkpoint = payload.get("timing_checkpoint") or {}
            timing = checkpoint.get("timing")
            if isinstance(timing, dict) and payload.get("turn_id"):
                checkpoints[payload["turn_id"]] = timing
        elif kind == "event_msg":
            event = payload.get("type")
            if event in ("task_complete", "turn_complete", "turn_aborted"):
                timing = payload.get("timing")
                turn_id = payload.get("turn_id")
                if turn_id:
                    terminals[turn_id] = (event, timing)
            elif event == "token_count":
                info = payload.get("info") or {}
                total = info.get("total_token_usage")
                if total and total != last_total:
                    token_counts += 1
                    last = info.get("last_token_usage") or {}
                    reasoning = last.get("reasoning_output_tokens")
                    reasoning_from_token_count = (
                        reasoning_from_token_count + reasoning
                        if reasoning_from_token_count is not None and reasoning is not None
                        else None
                    )
                    last_total = total
    return meta, settings, terminals, checkpoints, token_counts, reasoning_from_token_count


def rows_for(path: Path, home: Path, *, include_checkpoints: bool = False):
    meta, settings, terminals, checkpoints, token_counts, tc_reasoning = scan(path, home)
    by_turn = {}
    for entry in settings:
        entries = by_turn.setdefault(entry["turn_id"], [])
        if entry not in entries:
            entries.append(entry)
    turns = dict(terminals)
    if include_checkpoints:
        for turn_id, timing in checkpoints.items():
            turns.setdefault(turn_id, ("checkpoint", timing))
    missing_usage = 0
    for turn_id, (terminal, timing) in turns.items():
        if not isinstance(timing, dict):
            continue
        candidates = by_turn.get(turn_id, [])
        setting = candidates[0] if len(candidates) == 1 else {}
        settings_status = "matched" if len(candidates) == 1 else "conflicting" if candidates else "missing"
        for request in timing.get("modelRequests") or []:
            usage = request.get("tokenUsage") if isinstance(request.get("tokenUsage"), dict) else None
            if usage is None or usage.get("reasoningTokens") is None:
                missing_usage += 1
            categories = request.get("requestTokenCategories") or {}
            sections = request.get("requestSectionSha256ByAttempt") or {}
            yield {
                "file": path.name, "session": meta.get("id"), "source": meta.get("source"),
                "parent": meta.get("parent"), "turn": turn_id, "terminal": terminal,
                "profileSource": "checkpoint" if terminal == "checkpoint" else "terminal",
                "settingsStatus": settings_status,
                "model": setting.get("model"), "effort": setting.get("effort"),
                "summary": setting.get("summary"),
                "turnStartedAtUnixMs": timing.get("startedAtUnixMs"),
                "attempts": len(request.get("physicalAttemptIds") or []),
                "usage": usage,
                "previousResponseId": any("previous_response_id" in s for s in sections.values()),
                "historyItemsPrevious": categories.get("historyItemsPrevious"),
                "historyPrefixItemsReused": categories.get("historyPrefixItemsReused"),
                "currentInput": categories.get("currentInput"),
                "otherInjectedContext": categories.get("otherInjectedContext"),
                **{key: request.get(key) for key in REQUEST_FIELDS},
                "reasoningOutputTokens": usage.get("reasoningTokens") if usage else None,
                "outputTokens": (
                    usage["visibleOutputTokens"] + usage["reasoningTokens"]
                    if usage is not None and usage.get("visibleOutputTokens") is not None
                    and usage.get("reasoningTokens") is not None else None
                ),
            }
    yield {"file": path.name, "session": meta.get("id"), "summaryRow": True,
           "tokenCountEvents": token_counts, "tokenCountReasoning": tc_reasoning,
           "turnsWithTerminal": len(terminals),
           "terminalProfilesMissing": sum(not isinstance(timing, dict) for _, timing in terminals.values()),
           "requestsMissingReasoningUsage": missing_usage,
           "includesCheckpoints": include_checkpoints,
           "turnsCheckpointOnly": len(set(checkpoints) - set(terminals))}


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("roots", nargs="+", type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--home", type=Path, default=DEFAULT_HOME)
    parser.add_argument("--include-checkpoints", action="store_true",
                        help="Include explicitly labeled partial profiles for nonterminal turns")
    args = parser.parse_args(argv)
    files = []
    for root in args.roots:
        files.extend(discover_rollouts(root) if root.is_dir() else [existing_rollout_path(root)])
    output = args.out.resolve()
    for path in files:
        if output == path.resolve() or (output.exists() and output.samefile(path)):
            parser.error("--out must not overwrite an input rollout")
    count = 0
    # Stage the complete ledger before atomic publication. A failed read or
    # write leaves an existing destination untouched.
    with tempfile.TemporaryFile() as staged:
        for path in files:
            for row in rows_for(path, args.home, include_checkpoints=args.include_checkpoints):
                staged.write((json.dumps(row) + "\n").encode("utf-8"))
                count += 1
        staged.seek(0)
        write_stream_atomic(output, staged)
    print(f"{len(files)} files, {count} rows -> {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
