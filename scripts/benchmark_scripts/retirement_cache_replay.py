#!/usr/bin/env python3
"""Offline replay of context-pressure retirement policies against prompt-cache cost.

Read-only. Nothing here changes harness behavior or configuration.

Each recorded thread is replayed request by request with the block model of
`compaction_policy_replay.py`: every request appends its recorded growth as one
block, `retirable_share` of which is consumed tool output that a recovery pin
can replace once the block is older than the newest three generations.

What a policy changes is when retirement runs, which blocks it selects, and how
far it retires. A retirement makes the next request a stateless rewrite whose
reusable prefix ends at the first retired block (`D`). Two provider models
bracket what the recorded sessions show:

* `divergence`: the provider reuses the whole unchanged prefix `D`.
* `stuck`: it reuses at most what the latest earlier stateless request in the
  same window reused (`min(D, H)`), the pattern most recorded retirements show.

Requests the recorded session sent without `previous_response_id` (turn starts,
reconnects) keep their recorded reuse; chained requests pay their new tokens
plus the recorded miss.

usage (repository root):
  python -I -B scripts/benchmark_scripts/retirement_cache_replay.py --since 2026-10-13 [--json OUT]
"""

from __future__ import annotations

import argparse
import collections
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compaction_policy_replay as replay  # noqa: E402
import retirement_cache_audit as audit  # noqa: E402

KEPT_GENERATIONS = 3          # CONTEXT_PRESSURE_KEPT_GENERATIONS
LIMIT_TARGET = 2 / 3          # retirement target as a share of the limit
PROACTIVE_MIN_SAVED = 20_000  # PROACTIVE_RETIREMENT_MIN_SAVED_TOKENS
STATIC_PREFIX = 9_344         # instructions + tools reused by every request
RECOVERED_TOKENS = 8_800      # mean original tokens of a retired output (audit)
SHARE = 0.75                  # calibrated retirable share of each request's growth

POLICIES = {
    # name: selection order, retire-to target (share of limit), proactive, piggyback threshold, near-limit summary
    "summary only (builds before 2026-10-14T17)": dict(order="oldest", target=LIMIT_TARGET, retire=False),
    "limit_only (recorded)": dict(order="oldest", target=LIMIT_TARGET),
    "proactive (deployed code)": dict(order="oldest", target=LIMIT_TARGET, proactive=True),
    "limit, retire to 1/2": dict(order="oldest", target=1 / 2),
    "limit, retire to 1/3": dict(order="oldest", target=1 / 3),
    "limit, retire all eligible": dict(order="oldest", target=0.0),
    "limit, newest eligible first": dict(order="newest", target=LIMIT_TARGET),
    "limit + piggyback on invalidated turn starts": dict(order="oldest", target=LIMIT_TARGET, piggyback=LIMIT_TARGET),
    "limit to 1/2 + piggyback": dict(order="oldest", target=1 / 2, piggyback=LIMIT_TARGET),
    "limit + summary when retirement leaves >85%": dict(order="oldest", target=LIMIT_TARGET, near_limit=0.85),
}


def load_threads(home: Path, since: str):
    payloads = home / "rollout-payloads"
    files = sorted(p for p in (home / "sessions").rglob("rollout-*.jsonl") if p.name[8:18] >= since)
    seen_requests, seen_calls, threads = set(), set(), []
    for path in files:
        _, rows, _ = audit.parse_file(path, payloads, seen_requests, seen_calls)
        grouped = collections.OrderedDict()
        for row in rows:
            grouped.setdefault(row["fp"], []).append(row)
        for thread_rows in grouped.values():
            tagged = audit.classify(thread_rows)
            if tagged:
                threads.append({"file": path.name, "rows": tagged})
    return threads


def is_long(thread):
    return any(r["I"] > LIMIT_TARGET * audit.row_limit(r) for r in thread["rows"])


def select(blocks, newest, excess, order):
    eligible = [i for i, b in enumerate(blocks) if b[1] > 0 and b[2] <= newest - KEPT_GENERATIONS]
    if order == "newest":
        eligible.reverse()
    selected, saved = [], 0.0
    for i in eligible:
        if saved >= excess:
            break
        selected.append(i)
        saved += blocks[i][1]
    return selected, saved


def simulate(thread, cal, policy, cache_model, share, recovery_rate, slope):
    rows = thread["rows"]
    lat = cal["latency_fit"]
    fit = cal["summary_time_fit"]
    summary_tokens = cal["summary_tokens"]["median"]
    bases = [r["I"] for r in rows if r["kind"] == "summary_post"]
    base = statistics.median(bases) if bases else cal["base_median"]
    growth = [max(0, r["I"] - r["prev_I"]) for r in rows if r["kind"] == "chained" and r["prev_I"]]
    g_med = statistics.median(growth) if growth else cal["growth_per_request_median"]
    rec = cal["recovery_after_summary"]
    reread_weight = rec["reread_cells_mean"] + rec["excess_recovery_calls"]
    reread_tokens = rec["reread_tokens_mean"] + rec["excess_recovery_tokens"]
    ledger = replay.Ledger()
    c = prefix = None
    blocks, held = [], None   # held: prefix the provider offers the next stateless request
    for k, r in enumerate(rows):
        recorded_uncached = r["I"] - r["C"]
        if c is None:
            c = prefix = r["I"]
            held = r["C"] or STATIC_PREFIX
            ledger.request(c, recorded_uncached, r["V"], r["R"], r["T"], "work")
            continue
        previous_c = c
        stateless = r["kind"] in ("turn_start", "stateless_append", "rewrite_other", "undigested")
        if r["kind"] in ("retire_post", "summary_post") or not r["prev_I"]:
            g = g_med
        else:
            g = max(0, r["I"] - r["prev_I"])
            if r["I"] < r["prev_I"]:
                # Recorded re-layout at this request: remove the same share of content.
                content = c - prefix
                if content > 0:
                    scale = max(0.0, 1 - min(r["prev_I"] - r["I"], content) / content)
                    for block in blocks:
                        block[0] *= scale
                        block[1] *= scale
                    c = prefix + sum(b[0] for b in blocks)
        blocks.append([g, share * g, k])
        c += g
        newest = k - 1
        # The limit follows the context window configured for this turn.
        limit = audit.row_limit(r)
        action, selected = None, []
        if c >= limit:
            excess = c - policy["target"] * limit if policy["target"] else c
            selected, saved = select(blocks, newest, excess, policy["order"])
            if not policy.get("retire", True):
                action = "summary"
            elif policy.get("near_limit") and c - saved > policy["near_limit"] * limit:
                action = "summary"
            elif selected and c - saved < limit:
                action = "retire"
            else:
                action = "summary"
        elif policy.get("proactive") and not r["turn_start"] and c > LIMIT_TARGET * limit:
            selected, saved = select(blocks, newest, c - LIMIT_TARGET * limit, "oldest")
            if selected and saved >= PROACTIVE_MIN_SAVED:
                action = "retire"
        elif (policy.get("piggyback") and r["kind"] == "turn_start" and r["prev_I"]
              and r["C"] < 0.5 * r["I"] and c > policy["piggyback"] * limit):
            # This turn start already lost its cached prefix in the recorded session.
            selected, saved = select(blocks, newest, c - policy["target"] * limit, "oldest")
            if selected and saved >= PROACTIVE_MIN_SAVED:
                action = "retire"
                ledger.c["piggybacked"] += 1
        if action == "summary":
            seconds = fit["intercept"] + fit["per_summary_token"] * summary_tokens + cal["failure_seconds_per_event"]
            ledger.request(c, c * (1 - cal["summary_cached_share_median"]), summary_tokens, 0, seconds, "summary")
            ledger.c["summaries"] += 1
            if reread_weight > 0:
                per = reread_tokens / reread_weight
                ledger.request(base, per + 300, 200, 100, replay.predicted_seconds(cal, 200, 100, per + 300),
                               "recovery", reread_weight)
            # Recorded growth after recorded summaries already carries their re-reads.
            blocks, prefix, c = [], base, base
            cached = STATIC_PREFIX
            held = cached
        elif action == "retire":
            first = min(selected)
            saved = 0.0
            for i in selected:
                saved += blocks[i][1]
                blocks[i][0] -= blocks[i][1]
                blocks[i][1] = 0.0
            c -= saved
            divergence = prefix + sum(b[0] for b in blocks[:first])
            cached = divergence if cache_model == "divergence" else min(divergence, held)
            if stateless:
                cached = min(cached, r["C"])
            held = cached
            ledger.c["retirements"] += 1
            ledger.c["retired_tokens"] += saved
            ledger.c["rebuild_uncached"] += c - cached
            expected = saved * recovery_rate
            if expected:
                ledger.request(c, RECOVERED_TOKENS + 300, 200, 100,
                               replay.predicted_seconds(cal, 200, 100, RECOVERED_TOKENS + 300), "recovery", expected)
        elif stateless and r["prev_I"]:
            reuse = r["C"] / r["prev_I"]
            cached = min(r["C"], previous_c) if r["C"] < 0.5 * r["I"] else min(1.0, reuse) * previous_c
            held = cached
        else:
            miss = max(0, recorded_uncached - max(0, r["I"] - r["prev_I"])) if r["kind"] == "chained" and r["prev_I"] else 178
            cached = max(0.0, c - g - miss)
        uncached = max(0.0, min(c, c - cached))
        reasoning = max(0.0, r["R"] + slope * (c - r["I"]))
        seconds = r["T"] + lat["reasoning"] * (reasoning - r["R"]) + lat["uncached_k"] * (uncached - recorded_uncached) / 1000
        ledger.request(c, uncached, r["V"], reasoning, max(0.2, seconds), "work")
    return ledger.c


def run(threads, cal, policy, **kw):
    total = collections.Counter()
    for t in threads:
        total.update(simulate(t, cal, policy, **kw))
    total = dict(total)
    total["weighted_api_like"] = replay.weighted(total, 0.1, 8.0)
    total["weighted_flat_cache"] = replay.weighted(total, 0.25, 4.0)
    return total


def recorded(threads):
    total = collections.Counter()
    for t in threads:
        for r in t["rows"]:
            total["requests"] += 1
            total["input"] += r["I"]
            total["uncached"] += r["I"] - r["C"]
            total["cached"] += r["C"]
            total["seconds"] += r["T"]
            if r["kind"] == "retire_post":
                total["retirements"] += 1
                total["rebuild_uncached"] += r["I"] - r["C"]
            total["summary_posts"] += r["kind"] == "summary_post"
    return dict(total)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--home", type=Path, default=audit.DEFAULT_HOME)
    parser.add_argument("--since", default="2026-10-13")
    parser.add_argument("--json", type=Path)
    args = parser.parse_args(argv)
    calibration_threads, summary_rec, retire_rec, counts, cells_with_reads = replay.load(args.home, args.since, None, None)
    cal = replay.calibrate(calibration_threads, summary_rec, retire_rec, counts, cells_with_reads)
    threads = load_threads(args.home, args.since)
    long_threads = [t for t in threads if is_long(t)]
    eras = {"retirement_era": [t for t in long_threads if t["file"][8:24] >= replay.RETIREMENT_ERA],
            "summary_only_era": [t for t in long_threads if t["file"][8:24] < replay.RETIREMENT_ERA]}
    validation = {}
    for era, subset, name in (("retirement_era", eras["retirement_era"], "limit_only (recorded)"),
                              ("summary_only_era", eras["summary_only_era"], "summary only (builds before 2026-10-14T17)")):
        validation[era] = {"threads": len(subset), "recorded": recorded(subset)}
        for share in (0.65, SHARE, 0.9):
            for model in ("divergence", "stuck"):
                total = run(subset, cal, POLICIES[name], cache_model=model, share=share, recovery_rate=0.0, slope=0.0)
                validation[era][f"simulated[share={share},{model}]"] = {
                    k: total.get(k, 0) for k in ("requests", "input", "uncached", "cached", "seconds",
                                                 "retirements", "summaries", "rebuild_uncached", "retired_tokens")}
    # Measured recovery after retirement: 6 read_tool_output recoveries of 9.92M retired tokens.
    rates = {"measured": 6 / 9_920_647, "measured_x10": 60 / 9_920_647}
    keys = ("requests", "input", "uncached", "cached", "reasoning", "seconds", "summaries", "retirements",
            "piggybacked", "retired_tokens", "rebuild_uncached", "requests_recovery",
            "weighted_api_like", "weighted_flat_cache")
    # 0.75 jointly reproduces recorded retirement-era retirements, summaries, and
    # the measured all-eligible retirement floor; 0.65 and 0.9 bound it.
    configs = [(SHARE, model, rate, "none") for model in ("divergence", "stuck") for rate in rates]
    configs += [(share, model, "measured", "none") for share in (0.65, 0.9) for model in ("divergence", "stuck")]
    configs += [(SHARE, model, "measured", "prior_audit") for model in ("divergence", "stuck")]
    slopes = {"none": 0.0, "prior_audit": replay.PRIOR_AUDIT_REASONING_SLOPE}
    results = {}
    for share, model, rate_name, slope_name in configs:
        for scope, subset in (("all_long_threads", long_threads), ("retirement_era", eras["retirement_era"])):
            if scope == "retirement_era" and slope_name != "none":
                continue
            tag = f"{scope}/share={share}/{model}/recovery={rate_name}/slope={slope_name}"
            results[tag] = {}
            for name, policy in POLICIES.items():
                total = run(subset, cal, policy, cache_model=model, share=share,
                            recovery_rate=rates[rate_name], slope=slopes[slope_name])
                results[tag][name] = {k: total.get(k, 0) for k in keys}
    report = {
        "threads": len(threads), "threads_above_two_thirds_limit": len(long_threads),
        "recorded_long_threads": recorded(long_threads),
        "validation": validation,
        "results": results,
    }
    text = json.dumps(report, indent=1, default=float)
    if args.json:
        args.json.write_text(text, encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
