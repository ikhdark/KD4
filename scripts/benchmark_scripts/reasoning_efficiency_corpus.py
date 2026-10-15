"""Aggregate RolloutDetect `parse --summary-json` outputs with rollout settings.

Read-only. Joins each summary with the model/effort/reasoning settings recorded
in the matching rollout's `turn_context` records and prints per-session rows
plus corpus totals. Provider-reported token figures come from RolloutDetect's
`populations.all.tokens.observedTotals`; nothing here is estimated.

Usage:
    python -B scripts/benchmark_scripts/reasoning_efficiency_corpus.py \
        --summaries DIR --sessions DIR [--json OUT]
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records


def rollout_settings(path: Path) -> dict:
    models: Counter[str] = Counter()
    efforts: Counter[str] = Counter()
    summaries: Counter[str] = Counter()
    source = None
    parent = None
    compactions = 0
    for record, _ in iter_rollout_records(path):
        kind = record.get("type")
        payload = record.get("payload") or {}
        if kind == "turn_context":
            models[str(payload.get("model"))] += 1
            efforts[str(payload.get("effort", payload.get("reasoning_effort")))] += 1
            summaries[str(payload.get("summary"))] += 1
        elif kind == "session_meta":
            meta = payload.get("meta", payload)
            source = meta.get("source")
            if isinstance(source, dict):
                sub = source.get("subagent")
                if isinstance(sub, dict):
                    spawn = sub.get("thread_spawn") or {}
                    parent = spawn.get("parent_thread_id")
                source = "subagent" if sub else json.dumps(source, sort_keys=True)[:40]
        elif kind == "compacted":
            compactions += 1
    return {
        "models": dict(models),
        "efforts": dict(efforts),
        "summary_modes": dict(summaries),
        "source": source,
        "parent": parent,
        "compacted_records": compactions,
    }


def dig(obj, *keys):
    for key in keys:
        if not isinstance(obj, dict):
            return None
        obj = obj.get(key)
    return obj


def model_effort_key(row: dict) -> str:
    """Session totals cannot be attributed to the dominant setting after switches."""
    models, efforts = row.get("models") or {}, row.get("efforts") or {}
    if len(models) > 1 or len(efforts) > 1:
        return "mixed/unattributed"
    if not models or not efforts or "None" in models or "None" in efforts:
        return "unknown/unattributed"
    return f"{next(iter(models))}/{next(iter(efforts))}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--summaries", required=True, type=Path)
    parser.add_argument("--sessions", required=True, type=Path)
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()

    rollouts = {p.name.removesuffix(".zst").removesuffix(".jsonl"): p
                for p in discover_rollouts(args.sessions)}
    rows = []
    for summary_path in sorted(args.summaries.glob("*.summary.json")):
        stem = summary_path.name.removesuffix(".summary.json")
        text = summary_path.read_text(encoding="utf-8-sig", errors="replace")
        summary = json.loads(text)
        tokens = dig(summary, "populations", "all", "tokens") or {}
        totals = tokens.get("observedTotals") or {}
        purposes = dig(summary, "latencyBreakdown", "modelInference", "generationPurposes") or {}
        nonprogress = dig(summary, "populations", "all", "observationalNonprogressTokens") or {}
        categories = tokens.get("promptCategories") or {}
        row = {
            "session": stem[-36:],
            "status": dig(summary, "runnerDiagnostics", "status"),
            "physical": tokens.get("physicalAttempts"),
            "usage_attempts": tokens.get("providerUsageAttempts"),
            "retries": sum((p or {}).get("retryAttempts") or 0 for p in purposes.values()),
            "compaction_gens": sum(
                (purposes.get(k) or {}).get("logicalGenerations") or 0
                for k in ("compaction_recovery", "compaction_summary")
            ),
            "input": totals.get("inputTokens"),
            "cached": totals.get("cachedInputTokens"),
            "output": totals.get("outputTokens"),
            "reasoning": totals.get("reasoningTokens"),
            "visible_out": totals.get("visibleOutputTokens"),
            "nonprogress_gens": nonprogress.get("logicalGenerations"),
            "nonprogress_reasoning": nonprogress.get("reasoningTokens"),
            "repeated_unchanged_ctx": categories.get("repeatedUnchangedContext"),
            "over_pass_target": dig(summary, "behaviorSignals", "turnsOverSamplingPassTarget"),
            "active_s": round((dig(summary, "auditDecision", "agentActiveNs") or 0) / 1e9, 1),
            "model_share": dig(summary, "auditDecision", "dominantShare")
            if dig(summary, "auditDecision", "dominantPhase") == "model"
            else None,
            "purposes": {
                k: {
                    "gens": (v or {}).get("logicalGenerations"),
                    "retries": (v or {}).get("retryAttempts"),
                    "stream_s": round(((v or {}).get("modelStreamWaitNs") or 0) / 1e9, 1),
                }
                for k, v in purposes.items()
            },
        }
        rollout = rollouts.get(stem)
        if rollout is not None:
            row.update(rollout_settings(rollout))
        rows.append(row)

    def total(key):
        values = [r.get(key) for r in rows]
        return sum(values) if values and all(v is not None for v in values) else None

    corpus = {
        key: total(key)
        for key in (
            "physical", "usage_attempts", "retries", "compaction_gens", "input", "cached",
            "output", "reasoning", "visible_out", "nonprogress_gens", "nonprogress_reasoning",
            "repeated_unchanged_ctx", "compacted_records",
        )
    }
    corpus["sessions"] = len(rows)
    by_model_effort: dict[str, dict[str, int]] = {}
    for row in rows:
        bucket = by_model_effort.setdefault(model_effort_key(row), {"sessions": 0})
        for key in ("physical", "output", "reasoning", "retries", "compaction_gens"):
            value = row.get(key)
            bucket[key] = (bucket.get(key, 0) + value
                           if bucket.get(key, 0) is not None and value is not None else None)
        bucket["sessions"] += 1
    result = {"corpus": corpus, "by_model_effort": by_model_effort, "rows": rows}
    if args.json:
        args.json.write_text(json.dumps(result, indent=1), encoding="utf-8")
    json.dump({"corpus": corpus, "by_model_effort": by_model_effort}, sys.stdout, indent=1)
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
