#!/usr/bin/env python3
"""Attribute measured reasoning tokens in saved KD4 rollouts to request classes.

Read-only. Three recorded sources are joined per model request; nothing is
estimated:

* terminal turn timing profiles (`task_complete`/`turn_aborted`
  `timing.modelRequests`, else the turn's last `sampling_boundary` checkpoint):
  the harness's generation purpose/reason, attempt kind, progress kinds,
  unchanged-relevant-state flag and provider usage per request;
* `event_msg/token_count.info.last_token_usage`: provider usage per completed
  request (an event whose cumulative total did not advance is a repeat);
* the non-model response items recorded after the previous request's first
  output and before this request's first output (its trigger). RolloutDetect's
  anchored failure incidents (`scripts/audit_scripts/preventable_failures.py`)
  are joined to triggers by tool call ID when `--rolloutdetect` is given.

A profile request joins a token_count request of the same turn only when the
(input, cached, output, reasoning) usage is identical; the rest stay unjoined.
Files modified within `--stable-seconds` are live inputs and are excluded.

Usage:
  reasoning_waste_audit.py extract --out LEDGER.jsonl [--home DIR] [--rolloutdetect DIR]
  reasoning_waste_audit.py report LEDGER.jsonl [--json OUT] [--top N]
"""

from __future__ import annotations

import argparse
import collections
import hashlib
import importlib.util
import json
import re
import shutil
import sys
import tempfile
import time
from pathlib import Path

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:  # noqa: BLE001
    pass

DEFAULT_HOME = Path.home() / "Desktop" / "LOCAL-KD"
HYDRATED_TYPES = {"session_meta", "event_msg", "sampling_boundary", "response_item", "turn_context"}
MODEL_ITEM_TYPES = {
    "reasoning", "function_call", "custom_tool_call", "local_shell_call", "web_search_call",
    "tool_search_call", "image_generation_call", "compaction", "context_compaction",
}
TOOL_OUTPUT_TYPES = {"function_call_output", "custom_tool_call_output", "tool_search_output"}
TERMINAL_EVENTS = {"task_complete", "turn_complete", "turn_aborted"}
LIFECYCLE_EVENTS = {
    "stream_error", "error", "turn_aborted", "context_compacted", "task_started",
    "task_complete", "warning", "thread_rolled_back", "entered_review_mode",
}
# Harness-emitted markers in a triggering tool output. Lexical observations,
# not failure verdicts; RolloutDetect incidents carry the origin attribution.
MARKERS = {
    "script_failed": r"Script failed with cell ID",
    "argument_preflight": r"argument preflight failed",
    "validation_busy": r"Validation busy|codex_rust_admission_v1",
    "raw_cargo_rejected": r"Raw Cargo launch rejected",
    "exec_rejected": r"Failed to create unified exec process",
    "patch_conflict": r"PatchContextMismatch|MultiplePatchConflicts",
    "output_truncated": r"\"output_truncated\":\s*true|middle omitted",
    "timed_out": r"\"timed_out\":\s*true",
}
# Markers in the tool-call source a request emitted (what the generation chose to do).
OUTPUT_MARKERS = {
    "patch_artifact": r"\.codex[\\/]+gitdiff|save_turn_diff",
    "patch_file": r"\.patch\b",
    "save_turn_diff": r"save_turn_diff",
    "git_probe": r"\bgit\s+(?:diff|status|log|show)\b",
}
OUTPUT_MARKER_PATTERNS = {name: re.compile(rx) for name, rx in OUTPUT_MARKERS.items()}
TAG_RE = re.compile(r"^\s*<([A-Za-z_][\w\-]*)")
NESTED_TOOL_RE = re.compile(r"\btools\.([A-Za-z_][\w]*)\s*\(")
# Developer/contextual injections recurring on most requests; heads of the rest are kept.
ROUTINE_TAGS = {"codex_plan_state_v1", "codex_task_state", "permissions", "multi_agent_mode",
                "multi_agent_usage_hint", "tool_search_sources", "exhaustive_scope_feasibility",
                "environment_context", "agents_md", "skills_instructions", "multi_agent_role"}
SUMMARY_HEAD_CHARS = 600
INJECTION_HEAD_CHARS = 240


def load_preventable(rolloutdetect: Path | None):
    if rolloutdetect is None:
        return None
    path = rolloutdetect / "scripts" / "audit_scripts" / "preventable_failures.py"
    spec = importlib.util.spec_from_file_location("rd_preventable_failures", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def open_rollout(path: Path):
    if path.name.endswith(".zst"):
        from compression import zstd

        return zstd.open(path, "rt", encoding="utf-8", errors="replace")
    return path.open("r", encoding="utf-8", errors="replace")


def iter_records(path: Path, home: Path, stats: collections.Counter):
    with open_rollout(path) as handle:
        for number, line in enumerate(handle, 1):
            if '"type":"tool_manifest"' in line[:200] or '"type":"world_state"' in line[:200]:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                stats["parse_errors"] += 1
                continue
            if record.get("type") == "rollout_payload_artifact":
                ref = record.get("payload") or {}
                if ref.get("item_type") not in HYDRATED_TYPES:
                    continue
                try:
                    record = json.loads((home / "rollout-payloads" / f"{ref.get('sha256')}.json").read_bytes())
                except (OSError, json.JSONDecodeError):
                    stats[f"missing_artifacts:{ref.get('item_type')}"] += 1
                    continue
                stats["hydrated_artifacts"] += 1
            yield number, record


def text_of(payload: dict) -> str:
    parts = []
    for key in ("output", "content"):
        value = payload.get(key)
        if isinstance(value, str):
            parts.append(value)
        elif isinstance(value, list):
            parts.extend(item.get("text", "") for item in value if isinstance(item, dict))
        elif isinstance(value, dict):
            parts.append(json.dumps(value, ensure_ascii=False))
    return "\n".join(part for part in parts if isinstance(part, str))


def classify_input(payload: dict) -> tuple[str, str]:
    kind = payload.get("type")
    if kind in TOOL_OUTPUT_TYPES:
        return "tool_output", ""
    if kind == "agent_message":
        return "agent_message", ""
    if kind == "message":
        role = payload.get("role")
        text = text_of(payload)
        tag = (TAG_RE.match(text) or [None, None])[1]
        if role == "user":
            if tag or text.startswith("# AGENTS.md instructions"):
                return "contextual_user", tag or "agents_md"
            return "user", ""
        if role == "developer":
            ident = str(payload.get("id") or "")
            return "developer", tag or (ident.rsplit("_", 1)[0] if ident.startswith("msg_") else "untagged")
        return f"message:{role}", tag or ""
    return f"other:{kind}", ""


def is_model_output(payload: dict) -> bool:
    kind = payload.get("type")
    return kind in MODEL_ITEM_TYPES or (kind == "message" and payload.get("role") == "assistant")


def usage_key(usage: dict | None, profile: bool) -> tuple | None:
    if not isinstance(usage, dict):
        return None
    if profile:
        fields = (usage.get("inputTokens"), usage.get("cachedInputTokens"),
                  usage.get("visibleOutputTokens"), usage.get("reasoningTokens"))
        if not all(type(value) is int for value in fields):
            return None
        return (fields[0], fields[1], fields[2] + fields[3], fields[3])
    fields = (usage.get("input_tokens"), usage.get("cached_input_tokens"),
              usage.get("output_tokens"), usage.get("reasoning_output_tokens"))
    return tuple(int(value or 0) for value in fields)


def session_meta(payload: dict) -> dict:
    body = payload.get("meta", payload)
    source = body.get("source")
    parent = role = None
    if isinstance(source, dict):
        sub = source.get("subagent")
        if isinstance(sub, dict):
            spawn = sub.get("thread_spawn") or {}
            parent = spawn.get("parent_thread_id")
            role = spawn.get("agent_role") or spawn.get("agent_nickname")
            source = "subagent"
        elif sub is not None:
            source = f"subagent:{sub}"
        else:
            source = json.dumps(source, sort_keys=True)[:60]
    base = body.get("base_instructions")
    base = base.get("text") if isinstance(base, dict) else base
    return {"session": body.get("id"), "source": source, "parent": parent, "role": role,
            "originator": body.get("originator"), "cwd": body.get("cwd"),
            "cli_version": body.get("cli_version"),
            "base_sha": hashlib.sha256(base.encode()).hexdigest()[:10] if isinstance(base, str) else None}


class Extractor:
    def __init__(self, path: Path, home: Path, markers: dict[str, re.Pattern], incidents: dict):
        self.path, self.home, self.markers, self.incidents = path, home, markers, incidents
        self.stats: collections.Counter = collections.Counter()
        self.meta: dict = {}
        self.settings: dict[str, dict] = {}
        self.turn: str | None = None
        self.token_requests: list[dict] = []
        self.terminals: dict[str, tuple[str, dict]] = {}
        self.checkpoints: dict[str, dict[tuple, dict]] = {}
        self.pending: list[dict] = []
        self.trigger: list[dict] | None = None
        self.outputs: collections.Counter = collections.Counter()
        self.output_tools: collections.Counter = collections.Counter()
        self.output_markers: collections.Counter = collections.Counter()
        self.call_tools: dict[str, list[str]] = {}
        self.call_markers: dict[str, list[str]] = {}
        self.summary: list[str] = []
        self.events: list[str] = []
        self.warnings: list[str] = []
        self.aborts: dict[str, str] = {}
        self.last_total = None

    def run(self) -> list[dict]:
        for number, record in iter_records(self.path, self.home, self.stats):
            self.feed(number, record)
        return self.rows()

    def feed(self, number: int, record: dict) -> None:
        kind = record.get("type")
        payload = record.get("payload")
        if not isinstance(payload, dict):
            return
        if kind == "session_meta" and not self.meta:
            self.meta = session_meta(payload)
        elif kind == "turn_context":
            self.turn = payload.get("turn_id") or self.turn
            mode = payload.get("collaboration_mode") or {}
            self.settings.setdefault(self.turn, {
                "model": payload.get("model"),
                "effort": payload.get("effort", payload.get("reasoning_effort")),
                "summary_mode": payload.get("summary"),
                "mode": mode.get("mode") if isinstance(mode, dict) else None,
            })
        elif kind == "sampling_boundary":
            # Incremental checkpoints carry a sliding window of recent requests;
            # merge them per turn, a later copy of a request replacing an earlier one.
            timing = (payload.get("timing_checkpoint") or {}).get("timing")
            if isinstance(timing, dict) and payload.get("turn_id"):
                merged = self.checkpoints.setdefault(payload["turn_id"], {})
                for request in timing.get("modelRequests") or []:
                    identity = (request.get("generationIndex"), request.get("attemptKind"),
                                tuple(request.get("physicalAttemptIds") or ()) or request.get("samplingRequestId"))
                    merged[identity] = request
        elif kind == "response_item":
            item = {"line": number, "payload": payload}
            if is_model_output(payload):
                if self.trigger is None:
                    self.trigger, self.pending = self.pending, []
                ptype = payload.get("type")
                self.outputs[f"message:{payload.get('phase')}" if ptype == "message" else ptype] += 1
                if ptype in ("custom_tool_call", "function_call", "local_shell_call"):
                    source = str(payload.get("input") or payload.get("arguments") or "")
                    names = [str(payload.get("name") or ptype)]
                    names += sorted({f"{names[0]}>{n}" for n in NESTED_TOOL_RE.findall(source)})
                    self.output_tools.update(names)
                    found = [name for name, pattern in OUTPUT_MARKER_PATTERNS.items() if pattern.search(source)]
                    self.output_markers.update(found)
                    if payload.get("call_id"):
                        self.call_tools[payload["call_id"]] = names
                        self.call_markers[payload["call_id"]] = found
            else:
                self.pending.append(item)
        elif kind == "event_msg":
            self.event(number, payload)

    def event(self, number: int, payload: dict) -> None:
        event = payload.get("type")
        if event == "task_started" and payload.get("turn_id"):
            self.turn = payload["turn_id"]
        if event in TERMINAL_EVENTS and isinstance(payload.get("timing"), dict) and payload.get("turn_id"):
            self.terminals[payload["turn_id"]] = (event, payload["timing"])
        if event in LIFECYCLE_EVENTS:
            self.events.append(event)
        if event == "warning":
            self.warnings.append(" ".join(str(payload.get("message") or "").split())[:INJECTION_HEAD_CHARS])
        if event == "turn_aborted" and payload.get("turn_id"):
            self.aborts[payload["turn_id"]] = str(payload.get("reason"))
        if event == "agent_reasoning" and isinstance(payload.get("text"), str):
            self.summary.append(payload["text"])
            return
        if event != "token_count":
            return
        info = payload.get("info") or {}
        last, total = info.get("last_token_usage"), info.get("total_token_usage")
        if not last or total == self.last_total:
            self.stats["repeated_token_counts"] += 1
            return
        self.last_total = total
        trigger = self.trigger if self.trigger is not None else self.pending
        if self.trigger is None:
            self.pending = []
        self.token_requests.append(self.summarize(number, last, trigger))
        self.trigger, self.outputs, self.summary, self.events = None, collections.Counter(), [], []
        self.output_tools, self.output_markers, self.warnings = collections.Counter(), collections.Counter(), []

    def summarize(self, number: int, usage: dict, trigger: list[dict]) -> dict:
        classes: collections.Counter = collections.Counter()
        developer: collections.Counter = collections.Counter()
        contextual: collections.Counter = collections.Counter()
        markers: collections.Counter = collections.Counter()
        trigger_tools: collections.Counter = collections.Counter()
        trigger_cell_markers: collections.Counter = collections.Counter()
        injections = []
        incidents = []
        for item in trigger:
            payload = item["payload"]
            cls, detail = classify_input(payload)
            classes[cls] += 1
            if cls in ("developer", "contextual_user"):
                (developer if cls == "developer" else contextual)[detail] += 1
                if detail not in ROUTINE_TAGS and len(injections) < 3:
                    injections.append(f"{detail}: " + " ".join(text_of(payload).split())[:INJECTION_HEAD_CHARS])
            elif cls == "tool_output":
                trigger_tools.update(self.call_tools.get(payload.get("call_id"), ["unknown_call"]))
                trigger_cell_markers.update(self.call_markers.get(payload.get("call_id"), ()))
                text = text_of(payload)
                for name, pattern in self.markers.items():
                    if pattern.search(text):
                        markers[name] += 1
                for incident in self.incidents.get(payload.get("call_id"), ()):
                    incidents.append(incident)
        text = "\n".join(self.summary)
        return {
            "line": number, "turn": self.turn, "usage": usage_key(usage, profile=False),
            "trigger": dict(classes), "developer_tags": dict(developer),
            "contextual_tags": dict(contextual), "markers": dict(markers),
            "incidents": incidents, "outputs": dict(self.outputs),
            "trigger_tools": dict(trigger_tools), "output_tools": dict(self.output_tools),
            "output_markers": dict(self.output_markers),
            "trigger_cell_markers": dict(trigger_cell_markers),
            "injections": injections, "warnings": self.warnings[:3],
            "events": sorted(set(self.events)), "summary_chars": len(text),
            "summary_head": text[:SUMMARY_HEAD_CHARS],
        }

    def rows(self) -> list[dict]:
        profiles: dict[str, tuple[str, dict]] = dict(self.terminals)
        for turn, merged in self.checkpoints.items():
            ordered = sorted(merged.values(), key=lambda request: (
                request.get("generationIndex") if type(request.get("generationIndex")) is int else -1))
            profiles.setdefault(turn, ("checkpoint", {"modelRequests": ordered}))
        by_turn: dict[str, list[dict]] = collections.defaultdict(list)
        for request in self.token_requests:
            by_turn[request["turn"]].append(request)
        rows = []
        fallback = next(reversed(self.settings.values()), {}) if self.settings else {}
        for turn, (terminal, timing) in profiles.items():
            candidates = by_turn.get(turn, [])
            for request in timing.get("modelRequests") or []:
                key = usage_key(request.get("tokenUsage"), profile=True)
                match = None
                if key is not None:
                    for candidate in candidates:
                        if not candidate.get("_joined") and candidate["usage"] == key:
                            match = candidate
                            candidate["_joined"] = True
                            break
                rows.append(self.row(turn, terminal, request, match, key, fallback))
        for turn, requests in by_turn.items():
            for request in requests:
                if not request.get("_joined"):
                    rows.append(self.row(turn, None, None, request, request["usage"], fallback))
        return rows

    def row(self, turn, terminal, profile, token, key, fallback) -> dict:
        settings = self.settings.get(turn, fallback)
        profile = profile or {}
        token = token or {}
        usage = key or (None, None, None, None)
        return {
            "file": self.path.name, **self.meta, "turn": turn, "terminal": terminal,
            "abort_reason": self.aborts.get(turn),
            "model": settings.get("model"), "effort": settings.get("effort"),
            "mode": settings.get("mode"), "summary_mode": settings.get("summary_mode"),
            "line": token.get("line"), "profiled": bool(profile), "token_count": bool(token),
            "input": usage[0], "cached": usage[1], "output": usage[2], "reasoning": usage[3],
            "purpose": profile.get("generationPurpose"), "reason": profile.get("generationReason"),
            "disposition": profile.get("disposition"), "attempt_kind": profile.get("attemptKind"),
            "attempts": len(profile.get("physicalAttemptIds") or []) if profile else None,
            "generation": profile.get("generationIndex"),
            "continuation": profile.get("isContinuation"),
            "progress": profile.get("progressKinds"),
            "unchanged_state": profile.get("unchangedRelevantState"),
            "next_action_changed": profile.get("nextStructuredActionChanged"),
            "tool_calls": profile.get("modelEmittedToolCallCount"),
            "trigger_tool_calls": profile.get("toolCallCount"),
            "state_fingerprint": profile.get("relevantStateFingerprint") is not None if profile else None,
            "stream_wait_ns": profile.get("modelStreamWaitNs"),
            "diagnostics": profile.get("requestDiagnosticsStatus"),
            **{name: token.get(name) for name in (
                "trigger", "developer_tags", "contextual_tags", "markers", "incidents",
                "outputs", "trigger_tools", "output_tools", "output_markers", "trigger_cell_markers",
                "injections", "warnings",
                "events", "summary_chars", "summary_head")},
        }


def discover(home: Path, stable_seconds: float) -> tuple[list[Path], list[Path]]:
    now = time.time()
    stable, live = [], []
    for root in (home / "sessions", home / "archived_sessions"):
        for path in sorted([*root.rglob("*.jsonl"), *root.rglob("*.jsonl.zst")]):
            (live if now - path.stat().st_mtime < stable_seconds else stable).append(path)
    return stable, live


def scan_incidents(preventable, path: Path) -> dict:
    """RolloutDetect's anchored incidents by call ID; its scanner reads plain JSONL only."""
    incidents: dict = collections.defaultdict(list)
    if preventable is None:
        return incidents
    plain, temporary = path, None
    if path.name.endswith(".zst"):
        with open_rollout(path) as source, tempfile.NamedTemporaryFile(
                "w", suffix=".jsonl", encoding="utf-8", delete=False) as target:
            shutil.copyfileobj(source, target)
            plain = temporary = Path(target.name)
    try:
        for incident in preventable.scan(plain)["all"]:
            incidents[incident.get("call_id")].append({
                key: incident.get(key) for key in ("origin", "mechanism", "preventable", "anchor")})
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return incidents


def extract(args) -> int:
    markers = {name: re.compile(rx, re.M) for name, rx in MARKERS.items()}
    preventable = load_preventable(args.rolloutdetect)
    stable, live = discover(args.home, args.stable_seconds)
    totals: collections.Counter = collections.Counter()
    failures = []
    with args.out.open("w", encoding="utf-8") as out:
        for path in stable:
            try:
                incidents = scan_incidents(preventable, path)
                extractor = Extractor(path, args.home, markers, incidents)
                rows = extractor.run()
            except Exception as error:  # Report and continue; never hide the file.
                failures.append({"file": str(path), "error": f"{type(error).__name__}: {error}"})
                continue
            for row in rows:
                out.write(json.dumps(row, ensure_ascii=False) + "\n")
            totals.update(extractor.stats)
            totals["files"] += 1
            totals["rows"] += len(rows)
    manifest = {"home": str(args.home), "stable_files": len(stable), "live_excluded": [str(p) for p in live],
                "failures": failures, "stats": dict(totals),
                "rolloutdetect_incidents": preventable is not None}
    args.out.with_suffix(".manifest.json").write_text(json.dumps(manifest, indent=1), encoding="utf-8")
    print(json.dumps({key: value for key, value in manifest.items() if key != "live_excluded"}
                     | {"live_excluded": len(live)}, indent=1))
    return 1 if failures else 0


# ---------------------------------------------------------------- report


def quantiles(values: list[int]) -> dict:
    if not values:
        return {"n": 0}
    ordered = sorted(values)

    def at(q: float) -> int:
        return ordered[min(len(ordered) - 1, int(q * len(ordered)))]

    return {"n": len(ordered), "sum": sum(ordered), "mean": round(sum(ordered) / len(ordered), 1),
            "p50": at(0.5), "p90": at(0.9), "p99": at(0.99), "max": ordered[-1],
            "zero_share": round(sum(1 for v in ordered if v == 0) / len(ordered), 3)}


def trigger_class(row: dict) -> str:
    trigger = row.get("trigger")
    if trigger is None:
        return "unrecorded"
    if trigger.get("user"):
        return "user_input"
    if trigger.get("tool_output"):
        return "tool_output"
    if trigger.get("agent_message"):
        return "agent_message"
    if trigger.get("developer") or trigger.get("contextual_user"):
        return "harness_injection_only"
    return "no_new_input" if not trigger else "other"


def nonprogress(row: dict) -> bool:
    return row.get("profiled") and not row.get("progress") and row.get("unchanged_state") is True


def compaction_role(row: dict) -> str | None:
    """RolloutDetect's split of compaction rows (rolloutdetect/metrics.py compaction_summary)."""
    purpose = row.get("purpose")
    if purpose == "compaction_summary":
        return "compaction_summary"
    if purpose != "compaction_recovery":
        return None
    if row.get("state_fingerprint"):
        return "compaction_recovery"
    if row.get("trigger_tool_calls") == 0 and not row.get("tool_calls"):
        return "compaction_summary"
    return "compaction_recovery"


def nested_trigger_tools(row: dict) -> set[str]:
    return {name for name in row.get("trigger_tools") or {} if name != "exec"}


def patch_artifact_work(row: dict) -> bool:
    """The generation wrote a .codex/gitdiff patch, or read the result of a cell that did."""
    return bool((row.get("output_markers") or {}).get("patch_artifact")
                or (row.get("trigger_cell_markers") or {}).get("patch_artifact"))


def categories(row: dict) -> list[str]:
    """Non-exclusive harness-addressable flags, in attribution priority order."""
    found = []
    purpose = row.get("purpose")
    incidents = row.get("incidents") or []
    if compaction_role(row):
        found.append(compaction_role(row))
    if row.get("attempt_kind") not in (None, "primary"):
        found.append("transport_replay")
    if patch_artifact_work(row):
        found.append("patch_artifact_work")
    if any(i.get("origin") == "harness" for i in incidents):
        found.append("after_harness_origin_failure")
    if purpose == "wait":
        found.append("wait_only_generation")
    if nested_trigger_tools(row) == {"exec>read_tool_output"}:
        found.append("truncation_recovery_only")
    if nonprogress(row):
        found.append("observational_nonprogress")
    if trigger_class(row) == "harness_injection_only":
        found.append("harness_injection_only")
    return found


def informational(row: dict) -> list[str]:
    """Flags that are not harness-addressable but bound the remaining classes."""
    found = []
    if any(i.get("origin") == "model_script" for i in row.get("incidents") or []):
        found.append("after_model_script_failure")
    if row.get("abort_reason") == "interrupted":
        found.append("turn_interrupted_by_user")
    if row.get("purpose") == "agent_coordination":
        found.append("agent_coordination")
    if not row.get("profiled"):
        found.append("unprofiled")
    return found


def patch_artifact_by_version(measured: list[dict]) -> dict:
    versions: dict = collections.defaultdict(list)
    for row in measured:
        versions[str(row.get("base_sha"))].append(row)
    result = {}
    for sha, members in sorted(versions.items()):
        patch = [r for r in members if patch_artifact_work(r)]
        result[sha] = {
            "sessions": len({r["file"] for r in members}), "patch_sessions": len({r["file"] for r in patch}),
            "requests": len(members), "patch_requests": len(patch),
            "reasoning": sum(r["reasoning"] for r in members),
            "patch_reasoning": sum(r["reasoning"] for r in patch),
            "patch_output": sum(r["output"] for r in patch),
        }
    return result


def emitted_tools(row: dict) -> set[str]:
    return {name.replace("exec>", "") for name in row.get("output_tools") or {} if name != "exec"}


def turn_sequences(rows: list[dict]) -> list[list[dict]]:
    """Provider-usage requests per turn in rollout order."""
    turns: dict = collections.defaultdict(list)
    for row in rows:
        if row.get("token_count"):
            turns[(row["file"], row["turn"])].append(row)
    return [sorted(seq, key=lambda r: r["line"] or 0) for seq in turns.values()]


def sums(rows: list[dict]) -> dict:
    measured = [r for r in rows if type(r.get("reasoning")) is int]
    return {"requests": len(rows), "measured": len(measured),
            "reasoning": sum(r["reasoning"] for r in measured),
            "output": sum(r["output"] for r in measured),
            "input": sum(r["input"] for r in measured),
            "uncached_input": sum(r["input"] - r["cached"] for r in measured),
            "sessions": len({r["file"] for r in rows})}


def round_trip_classes(rows: list[dict]) -> dict:
    """Single-purpose generations whose round trip the harness can remove or fold."""
    plan_only, plan_next = [], collections.Counter()
    recovery, recovery_next = [], []
    for seq in turn_sequences(rows):
        for index, row in enumerate(seq):
            following = seq[index + 1] if index + 1 < len(seq) else None
            if emitted_tools(row) == {"update_plan"}:
                plan_only.append(row)
                if following is None:
                    plan_next["turn_ends"] += 1
                elif any(k.startswith("message:final") for k in following.get("outputs") or {}):
                    plan_next["final_answer"] += 1
                else:
                    plan_next["other"] += 1
                plan_next["in_last_two"] += index >= len(seq) - 2
            if emitted_tools(row) == {"read_tool_output"}:
                recovery.append(row)
                if following is not None:
                    recovery_next.append(following)
    return {"plan_only": sums(plan_only) | {"followed_by": dict(plan_next)},
            "truncation_recovery": sums(recovery) | {"next_generation": sums(recovery_next)}}


def transport_replays(rows: list[dict]) -> dict:
    """Generations re-issued after a failed attempt; the abandoned attempt's usage is unrecorded."""
    turns: dict = collections.defaultdict(list)
    for row in rows:
        if row.get("profiled"):
            turns[(row["file"], row["turn"])].append(row)
    pairs = [(a, b) for seq in turns.values() for a, b in zip(seq, seq[1:])
             if a.get("generation") == b.get("generation") and b.get("attempt_kind") in ("fallback", "retry")]
    return {"replayed_generations": len(pairs),
            "replay_kinds": dict(collections.Counter(b.get("attempt_kind") for _, b in pairs)),
            "abandoned_attempts_with_usage": sum(1 for a, _ in pairs if type(a.get("reasoning")) is int),
            "abandoned_stream_seconds": round(sum(a.get("stream_wait_ns") or 0 for a, _ in pairs) / 1e9),
            "replays": sums([b for _, b in pairs])}


def unmeasured(rows: list[dict]) -> dict:
    missing = [r for r in rows if type(r.get("reasoning")) is not int]
    return {"requests": len(missing),
            "stream_seconds": round(sum(r.get("stream_wait_ns") or 0 for r in missing) / 1e9),
            "by_terminal": dict(collections.Counter(
                f"{r.get('terminal')}:{r.get('abort_reason')}:{r.get('attempt_kind')}" for r in missing).most_common())}


def reasoning_items(measured: list[dict]) -> dict:
    """Reasoning-token ranges per count of reasoning items the request emitted."""
    buckets: dict = collections.defaultdict(list)
    for row in measured:
        if row.get("token_count"):
            buckets[min((row.get("outputs") or {}).get("reasoning", 0), 3)].append(row["reasoning"])
    return {f"items={key}{'+' if key == 3 else ''}": {
        "n": len(values), "min": min(values), "max": max(values),
        "at_cap": sum(1 for v in values if v in (516, 1034, 1552)), "sum": sum(values)}
        for key, values in sorted(buckets.items())}


def report(args) -> int:
    rows = [json.loads(line) for line in args.ledger.open(encoding="utf-8")]
    measured = [r for r in rows if type(r.get("reasoning")) is int]
    total_reasoning = sum(r["reasoning"] for r in measured)
    total_output = sum(r["output"] for r in measured)

    def group(keyfn, population=measured) -> dict:
        buckets: dict = collections.defaultdict(list)
        for row in population:
            for key in keyfn(row):
                buckets[str(key)].append(row["reasoning"])
        return {key: quantiles(values) | {"share": round(sum(values) / max(total_reasoning, 1), 4)}
                for key, values in sorted(buckets.items(), key=lambda kv: -sum(kv[1]))}

    exclusive: dict = collections.defaultdict(list)
    for row in measured:
        flags = categories(row)
        exclusive[flags[0] if flags else "unflagged"].append(row["reasoning"])
    result = {
        "requests": len(rows), "measured_requests": len(measured),
        "unmeasured_requests": len(rows) - len(measured),
        "profiled_requests": sum(1 for r in rows if r.get("profiled")),
        "joined_requests": sum(1 for r in rows if r.get("profiled") and r.get("token_count")),
        "token_count_only": sum(1 for r in rows if r.get("token_count") and not r.get("profiled")),
        "reasoning_tokens": total_reasoning, "output_tokens": total_output,
        "reasoning_share_of_output": round(total_reasoning / max(total_output, 1), 4),
        "files": len({r["file"] for r in rows}),
        "by_effort": group(lambda r: [r.get("effort")]),
        "by_model_effort": group(lambda r: [f"{r.get('model')}/{r.get('effort')}"]),
        "by_source": group(lambda r: [r.get("source")]),
        "by_purpose": group(lambda r: [compaction_role(r) or r.get("purpose")]),
        "by_terminal": group(lambda r: [f"{r.get('terminal')}:{r.get('abort_reason')}"]),
        "by_trigger_tools": group(lambda r: ["+".join(sorted(k for k in r.get("trigger_tools") or {} if k != "exec"))
                                             or "-"]),
        "by_reason": group(lambda r: [r.get("reason")]),
        "by_purpose_effort": group(lambda r: [f"{r.get('purpose')}@{r.get('effort')}"]),
        "by_trigger": group(lambda r: [trigger_class(r)]),
        "by_marker": group(lambda r: list(r.get("markers") or {})),
        "by_incident": group(lambda r: sorted({f"{i.get('origin')}:{i.get('mechanism')}"
                                                for i in r.get("incidents") or []})),
        "by_developer_tag": group(lambda r: list(r.get("developer_tags") or {})),
        "by_contextual_tag": group(lambda r: list(r.get("contextual_tags") or {})),
        "by_event": group(lambda r: r.get("events") or []),
        "by_attempt_kind": group(lambda r: [r.get("attempt_kind")]),
        "by_progress": group(lambda r: ["none" if not r.get("progress") else "+".join(sorted(r["progress"]))]
                             if r.get("profiled") else ["unprofiled"]),
        "by_base_instructions": group(lambda r: [r.get("base_sha")]),
        "patch_artifact_by_base_instructions": patch_artifact_by_version(measured),
        "flags_nonexclusive": group(categories),
        "informational_flags": group(informational),
        "round_trip_classes": round_trip_classes(rows),
        "transport_replays": transport_replays(rows),
        "unmeasured": unmeasured(rows),
        "reasoning_items": reasoning_items(measured),
        "flags_exclusive": {key: quantiles(values) | {"share": round(sum(values) / max(total_reasoning, 1), 4)}
                            for key, values in sorted(exclusive.items(), key=lambda kv: -sum(kv[1]))},
        "top_requests": [
            {key: row.get(key) for key in (
                "file", "line", "source", "effort", "purpose", "reason", "reasoning", "output",
                "progress", "unchanged_state", "markers", "incidents", "summary_chars", "summary_head")}
            | {"trigger": trigger_class(row), "flags": categories(row)}
            for row in sorted(measured, key=lambda r: -r["reasoning"])[: args.top]],
    }
    if args.json:
        args.json.write_text(json.dumps(result, indent=1, ensure_ascii=False), encoding="utf-8")
    brief = {key: value for key, value in result.items() if key != "top_requests"}
    print(json.dumps(brief, indent=1, ensure_ascii=False))
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    ext = sub.add_parser("extract")
    ext.add_argument("--home", type=Path, default=DEFAULT_HOME)
    ext.add_argument("--out", type=Path, required=True)
    ext.add_argument("--rolloutdetect", type=Path, help="RolloutDetect checkout for failure-origin incidents")
    ext.add_argument("--stable-seconds", type=float, default=900.0)
    rep = sub.add_parser("report")
    rep.add_argument("ledger", type=Path)
    rep.add_argument("--json", type=Path)
    rep.add_argument("--top", type=int, default=40)
    args = parser.parse_args(argv)
    return extract(args) if args.command == "extract" else report(args)


if __name__ == "__main__":
    raise SystemExit(main())
