#!/usr/bin/env python3
"""Per-request reasoning-token ledger for KD4 rollouts (read-only).

Every completed model request is reconstructed from the rollout stream:

  * usage   - provider-reported `event_msg/token_count.info.last_token_usage`
              (`reasoning_output_tokens` is MEASURED, never estimated here);
              a token_count whose `total_token_usage` did not change is a
              repeated emission, not a request, and is skipped.
  * outputs - model-generated response items recorded for that request
              (reasoning, assistant messages, tool calls).
  * trigger - non-model items recorded after the previous request's outputs
              (tool outputs, user/developer/contextual messages, agent
              messages): what the request was answering.

Aggregates associate measured reasoning tokens with trigger classes and textual
marker mentions. Mentions can be quoted source, not actual tool failures; these
associations do not establish harness causation or avoidable reasoning.

Usage:
  reasoning_token_ledger.py [--home DIR] [--latest N] [--json OUT]
                            [--marker REGEX ...] [--show-marker NAME]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:  # noqa: BLE001
    pass

DEFAULT_HOME = Path(r"C:\Users\kuh\Desktop\LOCAL-KD")

MODEL_ITEM_TYPES = {
    "reasoning",
    "function_call",
    "custom_tool_call",
    "local_shell_call",
    "web_search_call",
    "tool_search_call",
    "image_generation_call",
}
TOOL_OUTPUT_TYPES = {
    "function_call_output",
    "custom_tool_call_output",
    "tool_search_output",
}

# Textual markers only: quoted source can match these too.
DEFAULT_MARKERS = {
    "script_failed": r"^Script failed with cell ID",
    "cargo_admission_rejected": r"Raw Cargo launch rejected",
    "admission_busy": r"codex_rust_admission_v1|admission[^\n]{0,40}busy",
    "exec_rejected": r"Failed to create unified exec process: Rejected\(",
    "patch_conflict": r"PatchContextMismatch|MultiplePatchConflicts",
    "selector_error": r"\"selector_errors\"",
    "truncated_output": r"\"output_truncated\":true",
    "timeout": r"timed out|\"timed_out\":true",
    "sandbox_denied": r"sandbox[^\n]{0,40}denied|Access is denied",
    "js_error": r"(SyntaxError|TypeError|ReferenceError)",
}


def message_text(payload: dict) -> str:
    parts = []
    for item in payload.get("content") or []:
        if isinstance(item, dict):
            text = item.get("text")
            if isinstance(text, str):
                parts.append(text)
    return "\n".join(parts)


def output_text(payload: dict) -> str:
    out = payload.get("output")
    if isinstance(out, str):
        return out
    if isinstance(out, dict):
        content = out.get("content")
        if isinstance(content, str):
            return content
        if isinstance(content, list):
            return "\n".join(
                c.get("text", "") for c in content if isinstance(c, dict)
            )
    if isinstance(out, list):
        return "\n".join(c.get("text", "") for c in out if isinstance(c, dict))
    return json.dumps(out) if out is not None else ""


TAG_RE = re.compile(r"^\s*<([a-zA-Z_][\w\-]*)")


def leading_tag(text: str) -> str | None:
    match = TAG_RE.match(text)
    return match.group(1) if match else None


def classify_input(payload: dict) -> tuple[str, str]:
    """(class, detail) for a non-model response item."""
    ptype = payload.get("type")
    if ptype in TOOL_OUTPUT_TYPES:
        return "tool_output", ""
    if ptype == "agent_message":
        return "agent_message", ""
    if ptype == "message":
        role = payload.get("role")
        text = message_text(payload)
        tag = leading_tag(text)
        if role == "user":
            if tag or text.startswith("# AGENTS.md instructions"):
                return "contextual_user", tag or "agents_md"
            return "user", ""
        if role == "developer":
            return "developer", tag or "untagged"
        if role == "assistant":
            # Assistant messages are model output; handled by caller.
            return "assistant", ""
        return f"message:{role}", tag or ""
    return f"other:{ptype}", ""


def is_model_output(payload: dict) -> bool:
    ptype = payload.get("type")
    if ptype in MODEL_ITEM_TYPES:
        return True
    return ptype == "message" and payload.get("role") == "assistant"


def analyze(path: Path, markers: dict[str, re.Pattern]) -> list[dict]:
    requests: list[dict] = []
    pending: list[dict] = []
    trigger: list[dict] | None = None
    outputs: list[dict] = []
    model = effort = turn_id = None
    last_total = None
    events_since: list[str] = []
    for line_no, (record, _) in enumerate(iter_rollout_records(path), 1):
        rtype = record.get("type")
        payload = record.get("payload")
        if not isinstance(payload, dict):
            continue
        if rtype == "turn_context":
            model = payload.get("model")
            effort = payload.get("effort")
            turn_id = payload.get("turn_id")
            continue
        if rtype == "response_item":
            if is_model_output(payload):
                if trigger is None:
                    trigger = pending
                    pending = []
                outputs.append({"line": line_no, "payload": payload})
            else:
                pending.append({"line": line_no, "payload": payload})
            continue
        if rtype != "event_msg":
            continue
        etype = payload.get("type")
        if etype in (
            "stream_error",
            "error",
            "turn_aborted",
            "context_compacted",
            "task_started",
            "task_complete",
            "warning",
        ):
            events_since.append(etype)
            continue
        if etype != "token_count":
            continue
        info = payload.get("info") or {}
        last = info.get("last_token_usage")
        total = info.get("total_token_usage")
        # Missing cumulative usage cannot establish a new request. Preserve
        # deduplication and pending items until a changed total identifies one.
        if total is None or total == last_total:
            continue
        last_total = total
        req_trigger = trigger if trigger is not None else pending
        if trigger is None:
            pending = []
        requests.append(
            summarize_request(
                path.name,
                line_no,
                model,
                effort,
                turn_id,
                last or {},
                req_trigger,
                outputs,
                events_since,
                markers,
            )
        )
        trigger = None
        outputs = []
        events_since = []
    return requests


def summarize_request(
    session, line_no, model, effort, turn_id, usage, trigger, outputs, events, markers
) -> dict:
    trigger_classes: Counter[str] = Counter()
    developer_tags: Counter[str] = Counter()
    contextual_tags: Counter[str] = Counter()
    marker_hits: Counter[str] = Counter()
    for item in trigger:
        payload = item["payload"]
        cls, detail = classify_input(payload)
        trigger_classes[cls] += 1
        if cls == "developer":
            developer_tags[detail] += 1
        elif cls == "contextual_user":
            contextual_tags[detail] += 1
        elif cls == "tool_output":
            text = output_text(payload)
            for name, pattern in markers.items():
                if pattern.search(text):
                    marker_hits[name] += 1
    output_kinds: Counter[str] = Counter()
    tool_sources: list[str] = []
    phases: Counter[str] = Counter()
    for item in outputs:
        payload = item["payload"]
        ptype = payload.get("type")
        if ptype == "message":
            phases[str(payload.get("phase"))] += 1
            output_kinds["assistant_message"] += 1
        else:
            output_kinds[ptype] += 1
        if ptype == "custom_tool_call":
            tool_sources.append(str(payload.get("input") or "")[:4000])
        elif ptype == "function_call":
            tool_sources.append(
                f"{payload.get('name')}({str(payload.get('arguments') or '')[:400]})"
            )
    return {
        "session": session,
        "line": line_no,
        "model": model,
        "effort": effort,
        "turn_id": turn_id,
        "input": usage.get("input_tokens"),
        "cached": usage.get("cached_input_tokens"),
        "output": usage.get("output_tokens"),
        "reasoning": usage.get("reasoning_output_tokens"),
        "trigger": dict(trigger_classes),
        "developer_tags": dict(developer_tags),
        "contextual_tags": dict(contextual_tags),
        "marker_mentions": dict(marker_hits),
        "outputs": dict(output_kinds),
        "phases": dict(phases),
        "tool_sources": tool_sources,
        "events": events[:],
        "trigger_lines": [item["line"] for item in trigger],
    }


def trigger_class(req: dict) -> str:
    t = req["trigger"]
    if t.get("user"):
        return "user_input"
    if t.get("tool_output"):
        return "tool_output"
    if t.get("agent_message"):
        return "agent_message_only"
    if t.get("developer") or t.get("contextual_user"):
        return "harness_injection_only"
    if not t:
        return "no_new_input"
    return "other:" + ",".join(sorted(t))


def rollout_paths(home: Path, latest: int | None) -> list[Path]:
    paths = discover_rollouts(home / "sessions")
    paths += discover_rollouts(home / "archived_sessions")
    paths.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return paths[:latest] if latest else paths


def table(title: str, rows: dict, total_reasoning: int) -> None:
    print(f"\n== {title}")
    print(f"   {'key':44} {'reqs':>7} {'missing':>7} {'known rsn':>11} {'share':>7} {'rsn/known':>9}")
    for key, (count, reasoning, missing) in sorted(rows.items(), key=lambda kv: -kv[1][1]):
        share = 100 * reasoning / total_reasoning if total_reasoning else 0
        per = f"{reasoning / (count - missing):.0f}" if count > missing else "unknown"
        print(f"   {str(key)[:44]:44} {count:>7,} {missing:>7,} {reasoning:>11,} {share:>6.2f}% {per:>9}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=DEFAULT_HOME)
    parser.add_argument("--latest", type=int)
    parser.add_argument("--json", type=Path, help="write the full per-request ledger")
    parser.add_argument(
        "--marker",
        action="append",
        default=[],
        metavar="NAME=REGEX",
        help="extra tool-output textual marker (not verified failure evidence)",
    )
    parser.add_argument("--show-marker", help="print requests following this textual mention")
    args = parser.parse_args()

    markers = {name: re.compile(rx, re.M) for name, rx in DEFAULT_MARKERS.items()}
    for spec in args.marker:
        name, _, rx = spec.partition("=")
        markers[name] = re.compile(rx, re.M)

    ledger: list[dict] = []
    paths = rollout_paths(args.home, args.latest)
    for path in paths:
        ledger.extend(analyze(path, markers))

    total_reasoning = sum(r["reasoning"] for r in ledger if r["reasoning"] is not None)
    total_output = sum(r["output"] for r in ledger if r["output"] is not None)
    missing_reasoning = sum(r["reasoning"] is None for r in ledger)
    missing_output = sum(r["output"] is None for r in ledger)
    print(f"rollouts={len(paths)} requests={len(ledger):,}")
    print(
        f"known_reasoning_output_tokens={total_reasoning:,} known_output_tokens={total_output:,} "
        f"missing_reasoning_usage={missing_reasoning} missing_output_usage={missing_output}"
    )

    def bucket(keyfn) -> dict:
        rows: dict = defaultdict(lambda: [0, 0, 0])
        for req in ledger:
            for key in keyfn(req):
                rows[key][0] += 1
                rows[key][1] += req["reasoning"] if req["reasoning"] is not None else 0
                rows[key][2] += req["reasoning"] is None
        return {k: tuple(v) for k, v in rows.items()}

    table("model/effort", bucket(lambda r: [f"{r['model']}/{r['effort']}"]), total_reasoning)
    table("trigger class", bucket(lambda r: [trigger_class(r)]), total_reasoning)
    table(
        "textual marker mentions (not verified incidents; known reasoning only)",
        bucket(lambda r: list(r["marker_mentions"])),
        total_reasoning,
    )
    table(
        "developer tags in trigger",
        bucket(lambda r: list(r["developer_tags"])),
        total_reasoning,
    )
    table(
        "contextual user tags in trigger",
        bucket(lambda r: list(r["contextual_tags"])),
        total_reasoning,
    )
    table(
        "events since previous request",
        bucket(lambda r: sorted(set(r["events"]))),
        total_reasoning,
    )
    table(
        "output shape",
        bucket(
            lambda r: [
                "+".join(sorted(r["outputs"])) or "empty",
            ]
        ),
        total_reasoning,
    )

    if args.show_marker:
        print(f"\n== requests following textual mention {args.show_marker} (causation unverified)")
        for req in ledger:
            if args.show_marker in req["marker_mentions"]:
                print(
                    f"   {req['session'][-50:]}:{req['line']} rsn={req['reasoning']} "
                    f"out={req['output']} trigger_lines={req['trigger_lines'][:6]}"
                )

    if args.json:
        args.json.write_text(json.dumps(ledger), encoding="utf-8")
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
