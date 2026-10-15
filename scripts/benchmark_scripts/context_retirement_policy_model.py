#!/usr/bin/env python3
"""Project the token savings of alternative context-pressure retirement
thresholds against recorded KD4 rollouts.

Background measured by `context_window_composition.py` and
`context_exec_output_shape.py`: recorded long sessions carry no exact-duplicate
tool outputs, so the remaining context growth is not duplication. What the
rollouts do show is a sawtooth: `retire_consumed_tool_outputs_for_context_pressure`
is reachable only from `run_auto_compact`, which is gated on
`token_limit_reached` (90% of the window). Retirement then targets two thirds
of that limit, so a long session oscillates between ~2/3 and ~90% and re-sends
retirement-eligible evidence on every request in between.

This model asks what a *proactive* trigger would have saved, using the real
retirement events in each rollout as the evidence of what was eligible.

Method, per session:

* Read the per-request reported `input_tokens` series and the request index of
  each recorded `context_pressure` retirement checkpoint.
* Each observed retirement gives an observed reclaim: the drop in reported
  input tokens across that event. That drop is evidence that this much
  retirement-eligible evidence had accumulated by that point, measured rather
  than assumed.
* Attribute that reclaim linearly back over the requests since the previous
  retirement. This is the model's central approximation: eligibility actually
  accrues in discrete steps as individual outputs are consumed, and the true
  accrual curve is not recorded. Linear accrual is used because it is neutral
  between front-loaded and back-loaded accrual. Front-loaded accrual would
  save more than modeled, back-loaded less, so a single session's figure is
  indicative and only the aggregate direction is relied on.
* Replay the series under a candidate trigger: whenever modeled eligible
  savings would bring the prompt to or below `target_ratio` of the window and
  the saving is at least `min_saving`, retire at that point instead of waiting
  for the 90% threshold. Carry the retired tokens forward as a reduction on
  every subsequent request until the next real retirement resets the pool.

Reported per policy: total input tokens, the delta against the recorded
baseline, and the retirement count. Retirement costs no model request, so a
higher retirement count is not itself a cost; it does move evidence to
recovery pins sooner, which is the tradeoff the thresholds express.

This script only models recorded data. It does not modify history or change
harness behavior.

Usage:
  context_retirement_policy_model.py SESSION.jsonl [...] [--json]
  context_retirement_policy_model.py --latest N [--json]
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records

SESSION_ROOT = pathlib.Path(r"C:\Users\kuh\Desktop\LOCAL-KD\sessions")

# Mirrors ModelInfo::auto_compact_token_limit (90% of the resolved window) and
# retire_consumed_tool_outputs_for_context_pressure's `limit * 2 / 3` target.
AUTO_COMPACT_RATIO = 0.9
CURRENT_TARGET_OF_LIMIT = 2.0 / 3.0

# Candidate proactive triggers, expressed as a fraction of the full context
# window, plus the minimum modeled saving that justifies retiring early.
POLICIES: list[tuple[str, float, int]] = [
    ("baseline (90% limit only)", 0.0, 0),
    ("proactive 60% window", 0.60, 20_000),
    ("proactive 2/3 limit (60% window)", AUTO_COMPACT_RATIO * CURRENT_TARGET_OF_LIMIT, 20_000),
    ("proactive 70% window", 0.70, 20_000),
    ("proactive 75% window", 0.75, 20_000),
    ("proactive 80% window", 0.80, 20_000),
    ("proactive 70% window, min 50k", 0.70, 50_000),
]


def load_session(path: pathlib.Path) -> dict | None:
    """Reported input-token series plus the request index of each retirement."""
    series: list[int] = []
    retirements: list[int] = []
    window: int | None = None

    # Strict verified hydration: unresolved coverage aborts rather than reporting zero.
    for record, wire_bytes in iter_rollout_records(path):
        rtype = record.get("type")
        payload = record.get("payload")
        if not isinstance(payload, dict):
            payload = {}

        if rtype == "event_msg" and payload.get("type") == "token_count":
            info = payload.get("info") or {}
            if info.get("model_context_window"):
                window = info["model_context_window"]
            last = info.get("last_token_usage") or {}
            value = last.get("input_tokens") or 0
            if value:
                series.append(value)
        elif rtype == "response_item":
            # The retirement checkpoint is a recorded history item; match it
            # structurally rather than by prose.
            if "context_pressure" in json.dumps(payload):
                retirements.append(len(series))

    if not series or not window:
        return None
    return {
        "session": path.name,
        "window": window,
        "series": series,
        "retirements": retirements,
    }


def observed_reclaims(series: list[int], retirements: list[int]) -> list[tuple[int, int]]:
    """(request index, observed reclaimed tokens) for each real retirement."""
    reclaims = []
    for index in retirements:
        if index <= 0 or index >= len(series):
            continue
        before = series[index - 1]
        after = series[index]
        if after < before:
            reclaims.append((index, before - after))
    return reclaims


def simulate(session: dict, target_ratio: float, min_saving: int) -> dict:
    """Replay the series under a proactive trigger; baseline when ratio is 0."""
    series = session["series"]
    window = session["window"]
    reclaims = observed_reclaims(series, session["retirements"])

    if target_ratio <= 0:
        return {
            "total": sum(series),
            "retirements": len(reclaims),
            "peak": max(series),
        }

    target = int(window * target_ratio)

    # Model eligible savings accruing linearly between real retirements, with
    # each real retirement's observed reclaim as that interval's total.
    accrual = [0.0] * len(series)
    previous = 0
    for index, reclaimed in reclaims:
        span = max(1, index - previous)
        for step in range(previous, index):
            accrual[step] = reclaimed / span
        previous = index

    total = 0
    retirements = 0
    pool = 0.0          # modeled eligible-but-unretired tokens
    retired = 0         # tokens already retired, still discounted
    peak = 0
    reclaim_at = {index for index, _ in reclaims}

    for step, reported in enumerate(series):
        # The recorded prompt already includes the real retirement's saving.
        if step in reclaim_at:
            pool = 0.0
            retired = 0
        pool += accrual[step]
        effective = reported - retired

        # Proactive trigger: retire when it brings the prompt to target and the
        # saving is material. Retirement costs no model request.
        if effective > target and pool >= min_saving:
            take = min(pool, effective - target)
            if take >= min_saving:
                retired += int(take)
                pool -= take
                retirements += 1
                effective = reported - retired

        effective = max(effective, 0)
        total += effective
        peak = max(peak, effective)

    return {"total": total, "retirements": retirements, "peak": peak}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path)
    parser.add_argument("--latest", type=int)
    parser.add_argument("--json", action="store_true")
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

    sessions = [s for s in (load_session(p) for p in paths if p.is_file()) if s]
    if not sessions:
        print("no sessions with reported token usage", file=sys.stderr)
        return 2

    # Only sessions that actually retired exercise the policy; the rest never
    # approach the threshold and are reported separately so they cannot dilute
    # the measured effect.
    active = [s for s in sessions if observed_reclaims(s["series"], s["retirements"])]

    results: dict[str, dict] = {}
    for name, ratio, min_saving in POLICIES:
        totals = [simulate(s, ratio, min_saving) for s in active]
        results[name] = {
            "total_input_tokens": sum(t["total"] for t in totals),
            "retirements": sum(t["retirements"] for t in totals),
            "peak": max((t["peak"] for t in totals), default=0),
        }

    baseline = results["baseline (90% limit only)"]["total_input_tokens"]

    if args.json:
        print(
            json.dumps(
                {
                    "sessions_total": len(sessions),
                    "sessions_exercising_retirement": len(active),
                    "baseline_total_input_tokens": baseline,
                    "policies": results,
                },
                indent=2,
            )
        )
        return 0

    print(f"sessions scanned: {len(sessions)}")
    print(f"sessions that actually retired (policy-exercising): {len(active)}")
    for session in active:
        reclaims = observed_reclaims(session["series"], session["retirements"])
        observed = sum(r for _, r in reclaims)
        print(
            f"   {session['session'][:44]}  reqs={len(session['series']):>4}"
            f"  retirements={len(reclaims)}  observed reclaim={observed:,}"
        )

    print(f"\n{'policy':<34} {'input tokens':>14} {'vs baseline':>14} {'retires':>8} {'peak':>9}")
    for name, _, _ in POLICIES:
        row = results[name]
        delta = row["total_input_tokens"] - baseline
        pct = (100 * delta / baseline) if baseline else 0.0
        delta_text = "—" if delta == 0 else f"{delta:+,} ({pct:+.1f}%)"
        print(
            f"{name:<34} {row['total_input_tokens']:>14,} {delta_text:>14} "
            f"{row['retirements']:>8} {row['peak']:>9,}"
        )

    print(
        "\nLinear accrual between observed retirements is an approximation; see"
        "\nthe module docstring. Treat the aggregate direction as the result,"
        "\nnot any single session's figure."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
