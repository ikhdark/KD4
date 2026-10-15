#!/usr/bin/env python3
"""Read-only inventory of reasoning belonging to prior COMPLETED turns.

Later requests do not prove retention in their transmitted context. Exposure is
hypothetical (assuming retention), never measured carriage or billed tokens.

Population (deliberately narrow). A session qualifies only if it has at least
one turn that is:

  * terminated by a `task_complete`/`turn_complete` receipt with `error` absent
    and a non-empty `turn_id`;
  * never the subject of a `turn_aborted` receipt;
  * followed by at least one later model request in the same session.

Every other turn is excluded: active (no receipt), aborted, errored
(`error` present), or ambiguous (receipt without `turn_id`, or items whose
`turn_id` has no receipt).

What is measured vs estimated is kept separate:

  MEASURED   - counts of items and turns; per-request reported usage from
               `event_msg/token_count` (`input_tokens`, `cached_input_tokens`);
               encrypted payload BYTES.
  ESTIMATED  - token cost of encrypted reasoning. There is no provider token
               count for an encrypted blob, so this is the harness's own
               estimator (`estimate_reasoning_length`: decoded = len*3/4 - 650,
               then ceil(bytes/4)). It is reported as an estimate and never
               equated with the measured byte size.

This script does not modify anything and makes no network calls.

Usage:
  completed_turn_reasoning_measure.py [--latest N] [--json]
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import sys
from collections import defaultdict

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records

SESSION_ROOT = pathlib.Path(r"C:\Users\kuh\Desktop\LOCAL-KD\sessions")

COMPLETION_TYPES = ("task_complete", "turn_complete")


def est_tokens_from_text(text: str) -> int:
    return math.ceil(len(text.encode("utf-8", "replace")) / 4)


def est_tokens_encrypted(encoded_len: int) -> int:
    """Harness estimator for an encrypted reasoning payload.

    Mirrors `context_manager::history::estimate_reasoning_length` followed by
    the 4-bytes-per-token conversion. An ESTIMATE, not a measured token count.
    """
    decoded = max(0, encoded_len * 3 // 4 - 650)
    return decoded // 4


def item_turn_id(payload: dict) -> str | None:
    md = payload.get("internal_chat_message_metadata_passthrough")
    if not isinstance(md, dict):
        return None
    turn = md.get("turn_id")
    return turn if isinstance(turn, str) and turn else None


def reasoning_payload_sizes(payload: dict) -> tuple[int, int]:
    """(encrypted_bytes, summary_text_tokens) for one reasoning item."""
    encrypted = payload.get("encrypted_content")
    enc_bytes = len(encrypted) if isinstance(encrypted, str) else 0
    summary = 0
    for entry in payload.get("summary") or []:
        text = entry.get("text") if isinstance(entry, dict) else entry
        if isinstance(text, str):
            summary += est_tokens_from_text(text)
    return enc_bytes, summary


def analyze(path: pathlib.Path) -> dict | None:
    records: list[tuple[int, str, dict]] = []
    for index, (record, _) in enumerate(iter_rollout_records(path), 1):
        payload = record.get("payload")
        records.append(
            (index, record.get("type") or "", payload if isinstance(payload, dict) else {})
        )

    # Terminal receipts, by turn.
    completed_clean: dict[str, int] = {}
    excluded_turns: dict[str, str] = {}
    for index, rtype, payload in records:
        if rtype != "event_msg":
            continue
        ptype = payload.get("type")
        if ptype in COMPLETION_TYPES:
            turn = payload.get("turn_id")
            if not isinstance(turn, str) or not turn:
                continue
            if payload.get("error"):
                excluded_turns[turn] = "errored"
            else:
                # Last clean completion wins if a turn somehow reports twice.
                completed_clean[turn] = index
        elif ptype == "turn_aborted":
            turn = payload.get("turn_id")
            if isinstance(turn, str) and turn:
                excluded_turns[turn] = "aborted"
    for turn in list(completed_clean):
        if turn in excluded_turns:
            del completed_clean[turn]
    if not completed_clean:
        return None

    # Membership after compaction/rollback is unsupported here. Do not count
    # removed reasoning as present; explicitly bound coverage to the prefix.
    transition = next((index for index, kind, payload in records
                       if kind in ('compacted', 'rollback')
                       or (kind == 'event_msg' and payload.get('type') in
                           ('context_compacted', 'thread_rolled_back'))), None)

    # Model requests, in record order, with reported usage.
    requests: list[tuple[int, dict]] = []
    last_total = None
    requests_without_cumulative_usage = 0
    for index, rtype, payload in records:
        if rtype == "event_msg" and payload.get("type") == "token_count":
            info = payload.get("info") or {}
            total = info.get("total_token_usage")
            if total is None:
                requests_without_cumulative_usage += 1
                continue
            if total == last_total:
                continue
            last_total = total
            requests.append((index, info.get("last_token_usage") or {}))

    # Reasoning items, attributed by turn_id.
    reasoning: list[dict] = []
    turns_seen: set[str] = set()
    for index, rtype, payload in records:
        if rtype != "response_item":
            continue
        turn = item_turn_id(payload)
        if turn:
            turns_seen.add(turn)
        if payload.get("type") != "reasoning":
            continue
        enc_bytes, summary_tokens = reasoning_payload_sizes(payload)
        reasoning.append(
            {
                "record": index,
                "turn_id": turn,
                "encrypted_bytes": enc_bytes,
                "summary_tokens": summary_tokens,
            }
        )

    # Eligible: reasoning owned by a cleanly completed turn, where at least one
    # model request happens strictly after that turn's completion receipt.
    eligible: list[dict] = []
    ineligible_reason: dict[str, int] = defaultdict(int)
    for item in reasoning:
        turn = item["turn_id"]
        if turn is None:
            ineligible_reason["no_turn_id_on_item"] += 1
            continue
        if turn in excluded_turns:
            ineligible_reason[f"turn_{excluded_turns[turn]}"] += 1
            continue
        completion_record = completed_clean.get(turn)
        if completion_record is None:
            ineligible_reason["turn_active_or_no_receipt"] += 1
            continue
        if item["record"] > completion_record:
            # Recorded after its own turn's completion: ordering is not what
            # this measurement assumes, so do not claim it.
            ineligible_reason["item_after_own_completion"] += 1
            continue
        later = [usage for rec, usage in requests if rec > completion_record
                 and (transition is None or rec < transition)]
        if not later:
            ineligible_reason["no_subsequent_request"] += 1
            continue
        item = dict(item)
        item["subsequent_requests"] = len(later)
        item["completion_record"] = completion_record
        eligible.append(item)

    total_enc_bytes = sum(r["encrypted_bytes"] for r in reasoning)
    elig_enc_bytes = sum(r["encrypted_bytes"] for r in eligible)
    elig_est_tokens = sum(
        est_tokens_encrypted(r["encrypted_bytes"]) + r["summary_tokens"] for r in eligible
    )
    total_est_tokens = sum(
        est_tokens_encrypted(r["encrypted_bytes"]) + r["summary_tokens"] for r in reasoning
    )

    # Hypothetical exposure only: rollout order does not establish transmitted
    # history membership, even before the first explicit history transition.
    hypothetical_token_requests = sum(
        (est_tokens_encrypted(r["encrypted_bytes"]) + r["summary_tokens"])
        * r["subsequent_requests"]
        for r in eligible
    )

    peak_usage = max((u for _, u in requests if u.get("input_tokens") is not None),
                     key=lambda u: u["input_tokens"], default={})
    peak_input = peak_usage.get("input_tokens")
    peak_cached = peak_usage.get("cached_input_tokens")
    # Paired observation: cached tokens from the same request as peak input.
    cached_share = peak_cached / peak_input if peak_input and peak_cached is not None else None

    return {
        "session": path.name,
        "history_membership_complete": False,
        "exposure_assumption": "all eligible items retained in each later pre-transition request",
        "unsupported_history_transition_record": transition,
        "requests_membership_unknown": sum(1 for rec, _ in requests
                                           if transition is not None and rec >= transition),
        "turns_with_items": len(turns_seen),
        "turns_completed_clean": len(completed_clean),
        "turns_excluded": {t: r for t, r in excluded_turns.items()},
        "requests_total": len(requests),
        "token_count_events_without_cumulative_usage": requests_without_cumulative_usage,
        "requests_missing_input_usage": sum(u.get("input_tokens") is None for _, u in requests),
        "reasoning_items_total": len(reasoning),
        "reasoning_items_eligible": len(eligible),
        "ineligible_breakdown": dict(ineligible_reason),
        "measured_encrypted_bytes_total": total_enc_bytes,
        "measured_encrypted_bytes_eligible": elig_enc_bytes,
        "estimated_tokens_total_reasoning": total_est_tokens,
        "estimated_tokens_eligible": elig_est_tokens,
        "hypothetical_token_requests_if_retained": hypothetical_token_requests,
        "measured_peak_input_tokens": peak_input,
        "measured_peak_cached_input_tokens": peak_cached,
        "measured_cached_share_of_peak": cached_share,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--latest", type=int, default=60)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    if not SESSION_ROOT.exists():
        print(f"session root not found: {SESSION_ROOT}", file=sys.stderr)
        return 2
    paths = sorted(
        discover_rollouts(SESSION_ROOT), key=lambda p: p.stat().st_mtime, reverse=True
    )[: args.latest]

    reports = [r for r in (analyze(p) for p in paths) if r]
    scanned = len(paths)

    if args.json:
        print(json.dumps({"scanned_sessions": scanned, "sessions": reports}, indent=2))
        return 0

    print(f"scanned {scanned} sessions; {len(reports)} have >=1 cleanly completed turn\n")
    qualifying = [r for r in reports if r["reasoning_items_eligible"] > 0]
    print(f"sessions with eligible completed-turn reasoning: {len(qualifying)}\n")

    if qualifying:
        print(
            f"{'session':44} {'elig/total':>12} {'measBytes':>11} {'estTok':>9} {'peakIn':>9} {'cached%':>8}"
        )
        for r in sorted(qualifying, key=lambda r: -r["estimated_tokens_eligible"]):
            share = r["measured_cached_share_of_peak"]
            print(
                f"{r['session'][:44]:44} "
                f"{r['reasoning_items_eligible']:>5}/{r['reasoning_items_total']:<6} "
                f"{r['measured_encrypted_bytes_eligible']:>11,} "
                f"{r['estimated_tokens_eligible']:>9,} "
                f"{format(r['measured_peak_input_tokens'], ',') if r['measured_peak_input_tokens'] is not None else '-':>9} "
                f"{(f'{100*share:.1f}' if share is not None else '-'):>8}"
            )

    tot_elig_tok = sum(r["estimated_tokens_eligible"] for r in reports)
    tot_reas_tok = sum(r["estimated_tokens_total_reasoning"] for r in reports)
    tot_elig_bytes = sum(r["measured_encrypted_bytes_eligible"] for r in reports)
    tot_carried = sum(r["hypothetical_token_requests_if_retained"] for r in reports)
    reasons: dict[str, int] = defaultdict(int)
    for r in reports:
        for key, count in r["ineligible_breakdown"].items():
            reasons[key] += count

    print("\n== MEASURED")
    print(f"   eligible reasoning items: {sum(r['reasoning_items_eligible'] for r in reports):,}")
    print(f"   total reasoning items:    {sum(r['reasoning_items_total'] for r in reports):,}")
    print(f"   eligible encrypted bytes: {tot_elig_bytes:,}")
    print("\n== ESTIMATED (harness estimator; not provider token counts)")
    print(f"   eligible reasoning tokens:         {tot_elig_tok:,}")
    print(f"   all reasoning tokens (this pop.):  {tot_reas_tok:,}")
    if tot_reas_tok:
        print(f"   eligible share of reasoning:       {100*tot_elig_tok/tot_reas_tok:.1f}%")
    print(f"   hypothetical token*request exposure IF RETAINED (pre-transition): {tot_carried:,}")
    print("   transmitted history membership and billed carriage: unknown")
    print(f"   requests with unknown history membership: "
          f"{sum(r['requests_membership_unknown'] for r in reports):,}")
    print("\n== why reasoning was ineligible (items)")
    for key, count in sorted(reasons.items(), key=lambda kv: -kv[1]):
        print(f"   {key:30} {count:,}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
