#!/usr/bin/env python3
"""Offline replay of recorded KD4 sessions under alternative context policies.

Question: would deterministic folding, a structured state ledger, or
cache-aware scheduling cost less than KD4's current context retirement and
compaction, once prompt-cache invalidation, recovery reads and extra model
requests are counted?

Method
------
1. ``extract``: read each rollout once (verified hydration through
   ``rollout_snapshot``) into a compact per-session timeline: model-visible
   history items in order (kind, size, call id, producing request, lexical
   evidence atoms), every model request with provider usage and timing, the
   recorded retirement checkpoints and compactions. Cached per file under
   ``--cache-dir``.
2. ``calibrate``: fit bytes-per-token per item kind from append-only request
   pairs (provider input delta against the bytes appended between them), and
   a per-request latency model from provider usage.
3. ``accounting``: decompose the *recorded* uncached input by cause. This uses
   provider-reported numbers only; no policy is simulated.
4. ``replay``: rebuild every request's prompt under each policy from the same
   item timeline, price prompt-cache reuse as the longest common prefix with
   the previous request (cold after ``--ttl-seconds`` idle), and add the
   policy's own generations (remote summaries) and estimated recovery work.

What replay can and cannot establish
------------------------------------
The replay holds the model's recorded actions fixed. It therefore measures
token, cache and request consequences of a context policy *given* those
actions. It cannot observe how the model would have acted with a different
context: extra discovery, different plans, changed reasoning length, or task
outcome. Recovery is estimated with a lexical proxy (see ``lost_references``)
calibrated against what the model actually did after recorded retirements and
compactions. Those estimates are bounds for live testing to confirm, not
measurements.

Usage (from the repository root)::

  python -I -B scripts/benchmark_scripts/context_policy_replay.py run \
      --until 2026-10-16T02-00-00 --out .codex/audits/context-policy-replay-2026-10-16
  # sensitivity: --reuse-alpha 1.0 (ideal prefix cache), --ttl-seconds 300
  python -I -B scripts/benchmark_scripts/context_policy_replay.py report \
      --results <result.json> [<result.json> ...] [--policies NAME ...]

This script reads recorded data only. It changes no harness behavior.
"""

from __future__ import annotations

if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))

import argparse
import gzip
import hashlib
import json
import math
import os
import re
import statistics
import sys
import tempfile
import time
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Iterable

from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records

SESSION_ROOT = Path(os.environ.get("CODEX_HOME", r"C:\Users\kuh\Desktop\LOCAL-KD")) / "sessions"
EXTRACT_VERSION = 7

# --------------------------------------------------------------------------
# Item kinds and lexical evidence atoms
# --------------------------------------------------------------------------

K_USER, K_DEV, K_ASST, K_REASON, K_CALL, K_OUTPUT, K_AGENT, K_COMPACTION, K_OTHER = (
    "user", "dev", "asst", "reasoning", "call", "output", "agent_msg", "compaction", "other")
MODEL_AUTHORED = {K_ASST, K_REASON, K_CALL}
EVIDENCE_KINDS = {K_OUTPUT, K_AGENT}

F_STABLE = 1        # msg_sctx_*: stable context (instructions, AGENTS.md, environment)
F_CHECKPOINT = 2    # context_pressure retirement checkpoint
F_BASE = 4          # part of a recorded compaction's replacement history
F_SUMMARY_TEXT = 8  # reasoning item carrying a visible summary

_PATH_RE = re.compile(r"(?:[A-Za-z]:)?/?(?:[\w.\-]+/)+[\w.\-]+\.[A-Za-z][A-Za-z0-9]{0,7}\b")
_IDENT_RE = re.compile(r"\b[A-Za-z_][A-Za-z0-9_]{5,63}\b")
_HEX_RE = re.compile(r"\b[0-9a-f]{12,64}\b")
_CAMEL_RE = re.compile(r"[a-z][A-Z]")
# Envelope keys and harness vocabulary present in nearly every output; they
# carry no task evidence. Document frequency removes the rest per session.
_STOP = {
    "exit_code", "execution_state", "output_lines", "text_lines", "call_id", "artifact_id",
    "current_chunk_display_complete", "cumulative_streams_complete", "read_tool_output",
    "exec_command", "read_file", "apply_patch", "list_files", "resolve_tool", "wall_time",
    "tool_identity", "semantic_class", "source_dependencies_current", "json_pointer",
}
PIN_HEAD_BYTES = 384  # a pin's bounded digest is ~96 tokens of the output
_PATCH_BLOCK_RE = re.compile(r"\*\*\* Begin Patch.*?(?:\*\*\* End Patch|$)", re.S)
_PATCH_FILE_RE = re.compile(r"\*\*\* (?:Update|Add|Delete) File: ([^\n\"'`]+)")


def patch_facts(text: str) -> tuple[int, list[str]]:
    """Bytes of apply_patch bodies in a cell and the normalized paths they edit."""
    if "*** Begin Patch" not in text:
        return 0, []
    blocks = _PATCH_BLOCK_RE.findall(text)
    size = sum(len(block.encode("utf-8")) for block in blocks)
    paths = set()
    for block in blocks:
        # Cell source carries patches inside JavaScript string literals.
        unescaped = block.replace("\\r\\n", "\n").replace("\\n", "\n")
        for match in _PATCH_FILE_RE.finditer(_normalize_text(unescaped)):
            parts = [x for x in match.group(1).strip().lower().split("/") if x and x not in (".", "..")]
            if parts:
                paths.add("p:" + "/".join(parts[-3:]))
    return size, sorted(paths)


def _normalize_text(text: str) -> str:
    return text.replace("\\\\", "/").replace("\\", "/")


def extract_atoms(text: str) -> set[str]:
    """Lexical identifiers a later action could depend on: paths, code
    identifiers (snake_case, camelCase, CONSTANTS) and long hex digests."""
    if not text:
        return set()
    text = _normalize_text(text)
    atoms: set[str] = set()
    for match in _PATH_RE.finditer(text):
        parts = [p for p in match.group(0).lower().split("/") if p and p not in (".", "..")]
        if parts:
            atoms.add("p:" + "/".join(parts[-3:]))
    for match in _IDENT_RE.finditer(text):
        token = match.group(0)
        if token in _STOP:
            continue
        inner = token.strip("_")
        if "_" in inner or _CAMEL_RE.search(token):
            atoms.add("i:" + token)
    for match in _HEX_RE.finditer(text):
        atoms.add("h:" + match.group(0))
    return atoms


def _content_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    parts = []
    if isinstance(content, list):
        for part in content:
            if isinstance(part, dict):
                for key in ("text", "output", "input"):
                    value = part.get(key)
                    if isinstance(value, str):
                        parts.append(value)
            elif isinstance(part, str):
                parts.append(part)
    return "\n".join(parts)


def classify_item(payload: dict[str, Any]) -> tuple[str, int, int, str | None, str, int, list[str]]:
    """(kind, visible_bytes, encrypted_bytes, call_id, text, flags, retired_ids)."""
    ptype = payload.get("type")
    flags = 0
    retired: list[str] = []
    if ptype == "message":
        role = payload.get("role") or ""
        text = _content_text(payload.get("content"))
        ident = str(payload.get("id") or "")
        if ident.startswith("msg_sctx_"):
            flags |= F_STABLE
        kind = {"user": K_USER, "developer": K_DEV, "system": K_DEV, "assistant": K_ASST}.get(role, K_OTHER)
        if kind == K_DEV and text.startswith("<completed_phase_checkpoint>"):
            body = text[len("<completed_phase_checkpoint>"):].strip()
            body = body[: -len("</completed_phase_checkpoint>")].strip() if body.endswith("</completed_phase_checkpoint>") else body
            try:
                parsed = json.loads(body)
            except json.JSONDecodeError:
                parsed = {}
            if isinstance(parsed, dict) and parsed.get("source") == "context_pressure":
                flags |= F_CHECKPOINT
                retired = sorted((parsed.get("receipts") or {}).keys())
        return kind, len(text.encode("utf-8")), 0, None, text, flags, retired
    if ptype == "reasoning":
        summary = _content_text(payload.get("summary"))
        enc = payload.get("encrypted_content") or ""
        if summary:
            flags |= F_SUMMARY_TEXT
        return K_REASON, len(summary.encode("utf-8")), len(enc), None, summary, flags, retired
    if ptype in ("custom_tool_call", "function_call", "local_shell_call", "tool_search_call", "web_search_call"):
        body = payload.get("input")
        if body is None:
            body = payload.get("arguments")
        if body is None:
            body = payload.get("action")
        text = body if isinstance(body, str) else json.dumps(body or {})
        name = payload.get("name") or ""
        return K_CALL, len(text.encode("utf-8")) + len(name), 0, payload.get("call_id"), text, flags, retired
    if ptype in ("custom_tool_call_output", "function_call_output", "tool_search_output"):
        out = payload.get("output")
        text = out if isinstance(out, str) else _content_text(out) if isinstance(out, list) else json.dumps(out or {})
        return K_OUTPUT, len(text.encode("utf-8")), 0, payload.get("call_id"), text, flags, retired
    if ptype == "agent_message":
        text = _content_text(payload.get("content"))
        return K_AGENT, len(text.encode("utf-8")), 0, None, text, flags, retired
    if ptype == "compaction":
        enc = payload.get("encrypted_content") or ""
        return K_COMPACTION, 0, len(enc), None, "", flags, retired
    text = json.dumps(payload)
    return K_OTHER, len(text.encode("utf-8")), 0, None, "", flags, retired


NON_MODEL_VISIBLE = {"ghost_snapshot"}
_TAG_RE = re.compile(r"\s*(<[A-Za-z_][\w\-]*|#\s*AGENTS\.md|You are `)")


def message_tag(kind: str, text: str) -> str | None:
    """Slot of a harness-injected message (leading tag), for latest-wins folding."""
    if kind not in (K_DEV, K_USER) or not text:
        return None
    match = _TAG_RE.match(text)
    return match.group(1) if match else None


# --------------------------------------------------------------------------
# Extraction
# --------------------------------------------------------------------------

def _request_key(request: dict[str, Any]) -> str:
    attempts = request.get("physicalAttemptIds") or []
    return f"{request.get('samplingRequestId')}|{attempts[0] if attempts else request.get('generationIndex')}"


def extract_session(path: Path) -> dict[str, Any]:
    items: list[list[Any]] = []          # [kind, vbytes, encbytes, call_id, gen_sid, epoch, flags, turn_id, ts_ms]
    item_texts: list[str] = []
    retired_by_item: dict[int, list[str]] = {}
    boundaries: dict[str, tuple[int, int]] = {}   # sid -> (item_end, epoch)
    compactions: list[dict[str, Any]] = []
    requests: dict[str, dict[str, Any]] = {}
    windows: list[tuple[int, int]] = []           # (item_end, window)
    meta: dict[str, Any] = {"path": str(path), "name": path.name}
    epoch = 0
    current_sid: str | None = None
    current_turn: str | None = None
    effort = None
    model = None

    def add_requests(timing: dict[str, Any] | None) -> None:
        if not isinstance(timing, dict):
            return
        started = timing.get("startedAtUnixMs")
        for request in timing.get("modelRequests") or []:
            if not isinstance(request, dict):
                continue
            key = _request_key(request)
            entry = dict(request)
            entry["_turn_started_ms"] = started
            previous = requests.get(key)
            if previous is None or (entry.get("tokenUsage") and not previous.get("tokenUsage")) or (
                entry.get("completedMs") is not None and previous.get("completedMs") is None
            ):
                requests[key] = entry

    for record, _wire in iter_rollout_records(path):
        rtype = record.get("type")
        payload = record.get("payload") if isinstance(record.get("payload"), dict) else {}
        ts = record.get("timestamp") or ""
        if rtype == "session_meta":
            meta["session_id"] = payload.get("id") or payload.get("session_id")
            meta["cwd"] = payload.get("cwd")
            meta["source"] = payload.get("source") if not isinstance(payload.get("source"), (dict, list)) else json.dumps(payload.get("source"))[:200]
            meta["thread_source"] = payload.get("thread_source")
            meta["started"] = ts
            build = payload.get("harness_build") or {}
            meta["harness_commit"] = build.get("commit")
        elif rtype == "turn_context":
            current_turn = payload.get("turn_id") or current_turn
            effort = payload.get("effort") or effort
            model = payload.get("model") or model
        elif rtype == "response_item":
            if payload.get("type") in NON_MODEL_VISIBLE:
                continue
            kind, vbytes, encbytes, call_id, text, flags, retired = classify_item(payload)
            turn = (payload.get("internal_chat_message_metadata_passthrough") or {}).get("turn_id") or current_turn
            gen = current_sid if kind in MODEL_AUTHORED else None
            items.append([kind, vbytes, encbytes, call_id, gen, epoch, flags, turn, ts, message_tag(kind, text)])
            item_texts.append(text)
            if retired:
                retired_by_item[len(items) - 1] = retired
        elif rtype == "compacted":
            history = payload.get("replacement_history")
            epoch += 1
            base_start = len(items)
            if isinstance(history, list):
                for entry in history:
                    if not isinstance(entry, dict) or entry.get("type") in NON_MODEL_VISIBLE:
                        continue
                    kind, vbytes, encbytes, call_id, text, flags, _ = classify_item(entry)
                    items.append([kind, vbytes, encbytes, call_id, None, epoch, flags | F_BASE, current_turn, ts,
                                  message_tag(kind, text)])
                    item_texts.append(text)
            compactions.append({
                "epoch": epoch, "base_start": base_start, "base_end": len(items), "ts": ts,
                "has_replacement_history": isinstance(history, list),
                "item_end_before": base_start,
            })
        elif rtype == "sampling_boundary":
            sid = payload.get("sampling_request_id")
            if sid:
                boundaries[sid] = (len(items), epoch)
                current_sid = sid
            add_requests((payload.get("timing_checkpoint") or {}).get("timing"))
        elif rtype == "event_msg":
            ptype = payload.get("type")
            if ptype in ("task_complete", "turn_aborted"):
                add_requests(payload.get("timing"))
            elif ptype == "token_count":
                info = payload.get("info") or {}
                window = info.get("model_context_window")
                if window:
                    if not windows or windows[-1][1] != window:
                        windows.append((len(items), int(window)))
            elif ptype == "task_started":
                window = payload.get("model_context_window")
                if window and (not windows or windows[-1][1] != window):
                    windows.append((len(items), int(window)))

    meta["effort"] = effort
    meta["model"] = model

    # Lexical atoms per item.
    atom_ids: dict[str, int] = {}
    atom_list: list[str] = []
    item_atoms: list[list[int]] = []
    item_head_atoms: list[list[int]] = []

    def ids_for(values: Iterable[str]) -> list[int]:
        out = []
        for value in values:
            index = atom_ids.get(value)
            if index is None:
                index = len(atom_list)
                atom_ids[value] = index
                atom_list.append(value)
            out.append(index)
        return sorted(out)

    item_patch: dict[int, list[Any]] = {}
    for index, ((kind, *_rest), text) in enumerate(zip(items, item_texts)):
        if kind in (K_COMPACTION, K_OTHER) or not text:
            item_atoms.append([])
            item_head_atoms.append([])
            continue
        item_atoms.append(ids_for(extract_atoms(text)))
        item_head_atoms.append(ids_for(extract_atoms(text[:PIN_HEAD_BYTES])) if kind == K_OUTPUT else [])
        if kind == K_CALL:
            size, paths = patch_facts(text)
            if size:
                item_patch[index] = [size, ids_for(paths)]

    # Requests: order by dispatch, attach prompt prefix.
    ordered = []
    for entry in requests.values():
        started = entry.get("_turn_started_ms")
        dispatch = entry.get("dispatchMs")
        completed = entry.get("completedMs")
        usage = entry.get("tokenUsage") or {}
        cats = entry.get("requestTokenCategories") or {}
        sid = entry.get("samplingRequestId")
        boundary = boundaries.get(sid)
        is_summary = entry.get("generationReason") == "compaction" and boundary is None
        ordered.append({
            "sid": sid,
            "turn_started_ms": started,
            "dispatch_ms": (started + dispatch) if (started is not None and dispatch is not None) else None,
            "complete_ms": (started + completed) if (started is not None and completed is not None) else None,
            "input": usage.get("inputTokens") or 0,
            "cached": usage.get("cachedInputTokens") or 0,
            "out": usage.get("visibleOutputTokens") or 0,
            "reasoning": usage.get("reasoningTokens") or 0,
            "purpose": entry.get("generationPurpose"),
            "reason": entry.get("generationReason"),
            "attempt_kind": entry.get("attemptKind"),
            "stream_wait_s": (entry.get("modelStreamWaitNs") or 0) / 1e9,
            "hist_prev": cats.get("historyItemsPrevious"),
            "hist_div": cats.get("historyFirstDivergentIndex"),
            "conv_hist_est": cats.get("conversationHistory"),
            "logical_total": cats.get("logicalTotal"),
            "base_instructions": cats.get("baseInstructions"),
            "tool_schemas": cats.get("toolSchemas"),
            "other_injected": cats.get("otherInjectedContext"),
            "item_end": boundary[0] if boundary else None,
            "epoch": boundary[1] if boundary else None,
            "is_summary": is_summary,
            "progress": entry.get("progressKinds") or [],
            "chained": _chained(entry),
            "fixed_prefix_reuse_eligible": entry.get("fixedPrefixReuseEligible"),
            "prompt_cache_key_fp": entry.get("promptCacheKeyFingerprint"),
            "instructions_fp": ((entry.get("requestTokenCategories") or {}).get("promptSectionSha256") or {}).get("base_system"),
            "tools_fp": ((entry.get("requestTokenCategories") or {}).get("promptSectionSha256") or {}).get("tool_schemas"),
        })
    ordered.sort(key=lambda r: (r["dispatch_ms"] is None, r["dispatch_ms"] or 0))

    # Attach summary requests to the compaction that installed their output.
    for request in ordered:
        if not request["is_summary"] or request["complete_ms"] is None:
            continue
        best = None
        for compaction in compactions:
            if compaction.get("summary_matched"):
                continue
            ctime = _iso_ms(compaction["ts"])
            if ctime is not None and ctime >= request["complete_ms"] - 5_000:
                if best is None or ctime < _iso_ms(best["ts"]):
                    best = compaction
        if best is not None:
            best["summary_matched"] = True
            best["summary_input"] = request["input"]
            best["summary_out"] = request["out"]
            best["summary_wait_s"] = request["stream_wait_s"]
            request["item_end"] = best["item_end_before"]
            request["epoch"] = best["epoch"] - 1

    sid_ordinal = {r["sid"]: i for i, r in enumerate(ordered) if r["sid"]}
    for item in items:
        item[4] = sid_ordinal.get(item[4]) if item[4] is not None else None

    checkpoints = [{"item": index, "retired": ids} for index, ids in sorted(retired_by_item.items())]
    return {
        "version": EXTRACT_VERSION,
        "meta": meta,
        "items": items,
        "atoms": atom_list,
        "item_atoms": item_atoms,
        "item_head_atoms": item_head_atoms,
        "item_patch": {str(k): v for k, v in item_patch.items()},
        "requests": ordered,
        "checkpoints": checkpoints,
        "compactions": compactions,
        "windows": windows,
    }


def _chained(request: dict[str, Any]) -> bool | None:
    """Whether the first attempt continued provider state via previous_response_id."""
    sections = request.get("requestSectionSha256ByAttempt") or {}
    attempts = request.get("physicalAttemptIds") or list(sections)
    if not attempts or attempts[0] not in sections:
        return None
    return "previous_response_id" in sections[attempts[0]]


def _iso_ms(value: str) -> int | None:
    if not value:
        return None
    try:
        from datetime import datetime
        return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)
    except ValueError:
        return None


def cache_path(cache_dir: Path, path: Path) -> Path:
    stat = path.stat()
    key = hashlib.sha256(f"{path.resolve()}|{stat.st_size}|{stat.st_mtime_ns}|{EXTRACT_VERSION}".encode()).hexdigest()[:24]
    return cache_dir / f"{path.stem[:60]}-{key}.json.gz"


def load_or_extract(path: Path, cache_dir: Path) -> dict[str, Any] | None:
    target = cache_path(cache_dir, path)
    if target.exists():
        with gzip.open(target, "rt", encoding="utf-8") as handle:
            return json.load(handle)
    try:
        session = extract_session(path)
    except (ValueError, OSError) as error:
        print(f"skip {path.name}: {error}", file=sys.stderr)
        return None
    cache_dir.mkdir(parents=True, exist_ok=True)
    tmp = target.with_suffix(".tmp")
    with gzip.open(tmp, "wt", encoding="utf-8") as handle:
        json.dump(session, handle)
    os.replace(tmp, target)
    return session


def select_paths(root: Path, until: str | None, since: str | None) -> list[Path]:
    paths = []
    for path in discover_rollouts(root):
        name = path.name
        if not name.startswith("rollout-"):
            continue
        stamp = name[len("rollout-"): len("rollout-") + 19]
        if until and stamp >= until:
            continue
        if since and stamp < since:
            continue
        paths.append(path)
    return sorted(paths)


# --------------------------------------------------------------------------
# Small linear algebra (no numpy dependency)
# --------------------------------------------------------------------------

def least_squares(rows: list[list[float]], ys: list[float], ridge: float = 1e-9) -> list[float]:
    """Ordinary least squares through the normal equations."""
    n = len(rows[0])
    a = [[0.0] * n for _ in range(n)]
    b = [0.0] * n
    for x, y in zip(rows, ys):
        for i in range(n):
            xi = x[i]
            if xi == 0.0:
                continue
            b[i] += xi * y
            row = a[i]
            for j in range(n):
                row[j] += xi * x[j]
    for i in range(n):
        a[i][i] += ridge
    m = [a[i] + [b[i]] for i in range(n)]
    for c in range(n):
        p = max(range(c, n), key=lambda r: abs(m[r][c]))
        m[c], m[p] = m[p], m[c]
        if abs(m[c][c]) < 1e-12:
            continue
        for r in range(n):
            if r != c and m[r][c] != 0.0:
                f = m[r][c] / m[c][c]
                for k in range(c, n + 1):
                    m[r][k] -= f * m[c][k]
    return [m[i][n] / m[i][i] if abs(m[i][i]) > 1e-12 else 0.0 for i in range(n)]


def r_squared(ys: list[float], preds: list[float]) -> float:
    mean = sum(ys) / len(ys)
    ss_tot = sum((y - mean) ** 2 for y in ys)
    ss_res = sum((y - p) ** 2 for y, p in zip(ys, preds))
    return 1.0 - ss_res / ss_tot if ss_tot else 0.0


# --------------------------------------------------------------------------
# Calibration
# --------------------------------------------------------------------------

DEFAULT_PIN_TOKENS = 200          # current tool_history_artifact_pin (96-token digest + identity)
INDEX_LINE_TOKENS = 40            # ledger index line per folded output (artifact id, tool, digest line)
PATCH_STUB_BYTES = 160            # per folded patch: header naming the files; body recoverable from diff
CHECKPOINT_BASE_TOKENS = 70
CHECKPOINT_PER_ID_TOKENS = 14
LEDGER_BASE_TOKENS = 400
LEDGER_PER_FILE_TOKENS = 25
RECOVERY_SOURCE_CAP_TOKENS = 8_000
RECOVERY_REQUEST_OUT, RECOVERY_REQUEST_REASONING = 250, 100


class TokenModel:
    def __init__(self, rates: dict[str, float], pin_tokens: float, fit: dict[str, Any]):
        self.rates = rates
        self.pin_tokens = pin_tokens
        self.fit = fit

    def item(self, item: list[Any]) -> float:
        kind, vbytes, encbytes, flags = item[0], item[1], item[2], item[6]
        rates = self.rates
        if kind in (K_USER, K_DEV, K_ASST):
            return vbytes * (rates["stable"] if flags & F_STABLE else rates["msg"])
        if kind == K_CALL:
            return vbytes * rates["call"]
        if kind == K_OUTPUT:
            return vbytes * rates["output"]
        if kind == K_AGENT:
            return vbytes * rates["agent"]
        if kind == K_REASON:
            return encbytes * rates["reasoning_enc"]
        if kind == K_COMPACTION:
            return encbytes * rates["compaction_enc"]
        return vbytes / 4.0

    def summary_text(self, item: list[Any]) -> float:
        """Visible reasoning-summary text, rendered as ledger prose."""
        return item[1] * self.rates["msg"]


def _append_only_pairs(session: dict[str, Any]) -> Iterable[tuple[dict, dict, int, int]]:
    checkpoints = {c["item"] for c in session["checkpoints"]}
    requests = session["requests"]
    for r0, r1 in zip(requests, requests[1:]):
        if r0["is_summary"] or r1["is_summary"] or r0["item_end"] is None or r1["item_end"] is None:
            continue
        if r0["epoch"] != r1["epoch"] or not r0["input"] or not r1["input"]:
            continue
        if r1["hist_prev"] is None or r1["hist_div"] != r1["hist_prev"]:
            continue
        lo, hi = r0["item_end"], r1["item_end"]
        if hi <= lo or any(lo <= c < hi for c in checkpoints) or r1["input"] < r0["input"]:
            continue
        yield r0, r1, lo, hi


def fit_token_model(sessions: list[dict[str, Any]]) -> TokenModel:
    names = ["msg", "stable", "call", "output", "agent", "reasoning_enc"]
    rows, ys = [], []
    for session in sessions:
        items = session["items"]
        for r0, r1, lo, hi in _append_only_pairs(session):
            features = [0.0] * len(names)
            for item in items[lo:hi]:
                kind, vbytes, encbytes, flags = item[0], item[1], item[2], item[6]
                if kind in (K_USER, K_DEV, K_ASST):
                    features[1 if flags & F_STABLE else 0] += vbytes
                elif kind == K_CALL:
                    features[2] += vbytes
                elif kind == K_OUTPUT:
                    features[3] += vbytes
                elif kind == K_AGENT:
                    features[4] += vbytes
                elif kind == K_REASON:
                    features[5] += encbytes
            rows.append(features)
            ys.append(float(r1["input"] - r0["input"]))
    coef = least_squares(rows, ys)
    preds = [sum(a * b for a, b in zip(row, coef)) for row in rows]
    rates = {name: max(value, 0.0) for name, value in zip(names, coef)}
    return TokenModel(rates | {"compaction_enc": 1 / 16.0}, DEFAULT_PIN_TOKENS, {
        "pairs": len(rows), "r2": r_squared(ys, preds),
        "bytes_per_token": {n: (1 / v if v > 0 else None) for n, v in rates.items()},
        "sum_observed_delta": sum(ys), "sum_predicted_delta": sum(preds),
    })


def session_static_prefix(session: dict[str, Any], tm: TokenModel) -> float:
    """Provider tokens outside history (instructions, tools), from the first request."""
    items = session["items"]
    for request in session["requests"]:
        if request["is_summary"] or request["item_end"] is None or not request["input"]:
            continue
        epoch = request["epoch"]
        history = sum(tm.item(items[i]) for i in range(request["item_end"]) if items[i][5] == epoch)
        return max(2_000.0, request["input"] - history)
    return 10_000.0


def calibrate_compaction_and_pins(sessions: list[dict[str, Any]], tm: TokenModel) -> dict[str, Any]:
    comp_rates, pin_estimates, summary_rows = [], [], []
    for session in sessions:
        items = session["items"]
        static = session_static_prefix(session, tm)
        requests = [r for r in session["requests"] if r["item_end"] is not None and not r["is_summary"] and r["input"]]
        for compaction in session["compactions"]:
            if compaction.get("summary_out"):
                enc_all = sum(items[i][2] for i in range(compaction["base_start"], compaction["base_end"]) if items[i][0] == K_COMPACTION)
                summary_rows.append((compaction["summary_out"], compaction.get("summary_wait_s") or 0.0,
                                     compaction.get("summary_input") or 0, enc_all))
            if not compaction.get("has_replacement_history"):
                continue
            first = next((r for r in requests if r["epoch"] == compaction["epoch"]), None)
            if first is None:
                continue
            enc = sum(items[i][2] for i in range(compaction["base_start"], compaction["base_end"]) if items[i][0] == K_COMPACTION)
            if not enc:
                continue
            other = sum(tm.item(items[i]) for i in range(compaction["base_start"], first["item_end"])
                        if items[i][5] == compaction["epoch"] and items[i][0] != K_COMPACTION)
            rate = (first["input"] - static - other) / enc
            if 0 < rate < 1:
                comp_rates.append(rate)
        call_item = {}
        for index, item in enumerate(items):
            if item[0] == K_OUTPUT and item[3]:
                call_item.setdefault((item[5], item[3]), index)
        for checkpoint in session["checkpoints"]:
            ci = checkpoint["item"]
            epoch = items[ci][5]
            before = [r for r in requests if r["epoch"] == epoch and r["item_end"] <= ci]
            after = [r for r in requests if r["epoch"] == epoch and r["item_end"] > ci]
            if not before or not after:
                continue
            r0, r1 = before[-1], after[0]
            retired = [call_item.get((epoch, cid)) for cid in checkpoint["retired"]]
            retired = [i for i in retired if i is not None and i < r0["item_end"]]
            if not retired:
                continue
            appended = sum(tm.item(items[i]) for i in range(r0["item_end"], r1["item_end"]) if items[i][5] == epoch)
            removed = sum(tm.item(items[i]) for i in retired)
            pin_estimates.append((r1["input"] - r0["input"] - appended + removed) / len(retired))
    out: dict[str, Any] = {}
    if comp_rates:
        tm.rates["compaction_enc"] = statistics.median(comp_rates)
    out["compaction_tokens_per_enc_byte"] = tm.rates["compaction_enc"]
    out["compaction_rate_samples"] = len(comp_rates)
    # Per-event implied pin sizes are dominated by per-output estimation noise
    # (they range from negative to thousands of tokens), so the structural pin
    # size is kept and the noisy distribution is only reported.
    out["pin_tokens"] = tm.pin_tokens
    out["implied_pin_samples"] = len(pin_estimates)
    out["implied_pin_median"] = statistics.median(pin_estimates) if pin_estimates else None
    out["summary_out_tokens"] = sorted(row[0] for row in summary_rows)
    if len(summary_rows) >= 3:
        a, b = least_squares([[1.0, row[0]] for row in summary_rows], [row[1] for row in summary_rows])
        out["summary_latency"] = {"intercept_s": a, "per_output_token_s": b, "n": len(summary_rows),
                                  "r2": r_squared([row[1] for row in summary_rows], [a + b * row[0] for row in summary_rows])}
        ratios = [row[3] * tm.rates["compaction_enc"] / row[0] for row in summary_rows if row[0] and row[3]]
        out["compaction_item_tokens_per_summary_token"] = statistics.median(ratios) if ratios else 1.0
    else:
        out["summary_latency"] = {"intercept_s": 22.0, "per_output_token_s": 0.0177, "n": 0, "r2": None}
        out["compaction_item_tokens_per_summary_token"] = 1.0
    return out


def fit_latency(sessions: list[dict[str, Any]]) -> dict[str, Any]:
    rows, ys = [], []
    controlled: dict[int, list[float]] = defaultdict(list)
    for session in sessions:
        for request in session["requests"]:
            if request["is_summary"] or not request["input"] or request["stream_wait_s"] <= 0:
                continue
            uncached = request["input"] - request["cached"]
            rows.append([1.0, request["out"], request["reasoning"], uncached / 1000.0, request["input"] / 1000.0])
            ys.append(request["stream_wait_s"])
            if 100 <= request["out"] <= 300 and request["reasoning"] <= 200 and uncached <= 20_000:
                controlled[min(request["input"] // 50_000, 8)].append(request["stream_wait_s"])
    coef = least_squares(rows, ys)
    preds = [sum(a * b for a, b in zip(row, coef)) for row in rows]
    medians = {bucket * 50: statistics.median(values) for bucket, values in sorted(controlled.items()) if len(values) >= 30}
    xs = [k + 25 for k in medians]
    slope = least_squares([[1.0, x] for x in xs], list(medians.values()))[1] if len(xs) >= 3 else 0.0075
    return {
        "ols": {"intercept_s": coef[0], "per_out_token_s": coef[1], "per_reasoning_token_s": coef[2],
                "per_1k_uncached_s": coef[3], "per_1k_input_s": coef[4], "n": len(rows), "r2": r_squared(ys, preds)},
        "controlled_bucket_median_s": medians,
        "context_slope_per_1k_s": slope,
    }


def calibrate_cache(prepared: list["Prepared"], tm: TokenModel, ttl: float) -> dict[str, Any]:
    """Post-rewrite prefix reuse and random cold-miss rates from recorded requests."""
    buckets = [(10.0, []), (60.0, []), (300.0, []), (float("inf"), [])]
    for p in prepared:
        for k in range(1, len(p.requests)):
            r0, r1 = p.requests[k - 1], p.requests[k]
            gap = p.gap_s(k)
            if gap > ttl or r0["input"] < 20_000 or r1["hist_prev"] is None or r1["hist_div"] != r1["hist_prev"]:
                continue
            if r1["epoch"] != r0["epoch"] or r1["input"] < r0["input"]:
                continue
            cold = r1["cached"] < 0.2 * r0["input"]
            for upper, values in buckets:
                if gap < upper:
                    values.append(1.0 if cold else 0.0)
                    break
    miss = [(upper, (sum(v) / len(v)) if v else 0.0) for upper, v in buckets]
    samples = []
    zero_calib = {"compaction": {"summary_latency": {"intercept_s": 0, "per_output_token_s": 0},
                                 "summary_out_tokens": [], "compaction_item_tokens_per_summary_token": 1.0},
                  "latency": {"context_slope_per_1k_s": 0.0, "ols": {"per_1k_uncached_s": 0.0}},
                  "cache": {"reuse_alpha": 1.0, "random_miss_by_gap": []}}
    for p in prepared:
        if p.peak_input < 50_000:
            continue
        eng = Engine(p, RecordedView(), zero_calib, ttl, False)
        previous_end = 0
        for k, request in enumerate(p.requests):
            eng.k = k
            eng.policy.on_items(eng, previous_end, request["item_end"])
            previous_end = request["item_end"]
            if k in p.projection_rewrites:
                eng.mark_dirty(p.projection_rewrites[k])
            if k in p.relayouts:
                eng.relayout(p.relayouts[k])
            if eng.dirty is not None and p.gap_s(k) <= ttl and k > 0:
                lcp = min(eng.prefix_tokens(eng.dirty), eng.prev_total)
                if lcp - p.static > 20_000:
                    samples.append(max(0.0, min(1.0, (request["cached"] - p.static) / (lcp - p.static))))
            eng.price_request(k)
    alpha = statistics.median(samples) if samples else 1.0
    return {"reuse_alpha": alpha, "reuse_alpha_samples": len(samples),
            "reuse_alpha_quartiles": (statistics.quantiles(samples, n=4) if len(samples) >= 4 else None),
            "random_miss_by_gap": miss}


# --------------------------------------------------------------------------
# Recorded accounting: where uncached input actually came from
# --------------------------------------------------------------------------

def turn_start_flags(requests: list[dict[str, Any]]) -> list[bool]:
    flags, previous = [], None
    for request in requests:
        flags.append(request["turn_started_ms"] != previous)
        previous = request["turn_started_ms"]
    return flags


def recorded_accounting(session: dict[str, Any], tm: TokenModel, ttl: float) -> dict[str, float]:
    items = session["items"]
    checkpoints = sorted(c["item"] for c in session["checkpoints"])
    requests = [r for r in session["requests"] if r["input"]]
    starts = turn_start_flags(requests)
    acc: Counter = Counter()
    previous = None
    for request, is_start in zip(requests, starts):
        uncached = request["input"] - request["cached"]
        acc["requests"] += 1
        acc["input"] += request["input"]
        acc["cached"] += request["cached"]
        acc["uncached"] += uncached
        acc["out"] += request["out"]
        acc["reasoning"] += request["reasoning"]
        acc["model_wait_s"] += request["stream_wait_s"]
        if request["is_summary"]:
            acc["summary_requests"] += 1
            acc["summary_out"] += request["out"]
            acc["summary_wait_s"] += request["stream_wait_s"]
            acc["uncached:compaction_generation"] += uncached
            previous = request
            continue
        if previous is None:
            acc["uncached:session_start"] += uncached
            previous = request
            continue
        gap = ((request["dispatch_ms"] or 0) - (previous["complete_ms"] or 0)) / 1000.0
        fresh = 0.0
        if request["item_end"] is not None and previous.get("item_end") is not None and request["epoch"] == previous.get("epoch"):
            fresh = sum(tm.item(items[i]) for i in range(previous["item_end"], request["item_end"]) if items[i][5] == request["epoch"])
        fresh_part = min(uncached, fresh)
        reread = uncached - fresh_part
        acc["uncached:fresh_content"] += fresh_part
        if reread > 0:
            lo = previous.get("item_end") or 0
            hi = request["item_end"] or lo
            if request["epoch"] is not None and previous.get("epoch") is not None and request["epoch"] != previous["epoch"]:
                cause = "compaction_reread"
            elif any(lo <= c < hi for c in checkpoints):
                cause = "retirement_reread"
            elif gap > ttl:
                cause = "ttl_expiry"
            elif is_start:
                cause = "turn_start_projection"
            elif request["hist_prev"] is not None and request["hist_div"] is not None and request["hist_div"] < request["hist_prev"]:
                cause = "midturn_projection"
            else:
                cause = "estimate_residual"
            acc["uncached:" + cause] += reread
            acc["events:" + cause] += 1
        previous = request
    return dict(acc)


# --------------------------------------------------------------------------
# Replay engine
# --------------------------------------------------------------------------

FULL, PIN, STUB = 0, 1, 2


class Prepared:
    """Policy-independent view of one session's timeline."""

    def __init__(self, session: dict[str, Any], tm: TokenModel):
        self.session = session
        self.items = session["items"]
        self.tm = tm
        self.static = session_static_prefix(session, tm)
        self.tokens = [tm.item(item) for item in self.items]
        requests = [r for r in session["requests"] if not r["is_summary"] and r["item_end"] is not None and r["input"]]
        requests.sort(key=lambda r: (r["dispatch_ms"] or 0))
        # Item ends must be monotonic for prefix replay; drop the rare reordered entry.
        cleaned, last = [], -1
        for request in requests:
            if request["item_end"] >= last:
                cleaned.append(request)
                last = request["item_end"]
        self.requests = cleaned
        self.turn_start = turn_start_flags(self.requests)
        self.summaries = [r for r in session["requests"] if r["is_summary"]]
        self.recorded_summary_sizes = [c.get("summary_out") for c in session["compactions"] if c.get("summary_out")]
        n = len(self.items)
        self.slot = [-1] * n
        for k, request in enumerate(self.requests):
            lo = request["item_end"]
            hi = self.requests[k + 1]["item_end"] if k + 1 < len(self.requests) else n
            for i in range(lo, hi):
                self.slot[i] = k
        self.patch = {int(k): v for k, v in (session.get("item_patch") or {}).items()}
        self.windows = session.get("windows") or []
        self.failed = set()
        # Atoms: postings over non-base items; references per request.
        item_atoms = session["item_atoms"]
        self.item_atoms = item_atoms
        self.head_atoms = [set(x) for x in session["item_head_atoms"]]
        always: set[int] = set()
        first_seen: dict[int, int] = {}
        postings: dict[int, list[int]] = defaultdict(list)
        for i, item in enumerate(self.items):
            if item[6] & F_BASE:
                continue
            kind = item[0]
            if kind == K_USER and not item[6] & F_STABLE or item[6] & F_STABLE:
                always.update(item_atoms[i])
                continue
            for atom in item_atoms[i]:
                postings[atom].append(i)
                first_seen.setdefault(atom, i)
        self.postings = postings
        self.refs: list[list[int]] = [[] for _ in self.requests]
        for i, item in enumerate(self.items):
            if item[6] & F_BASE or item[0] not in MODEL_AUTHORED:
                continue
            k = self.slot[i]
            if k < 0:
                continue
            boundary = self.requests[k]["item_end"]
            for atom in item_atoms[i]:
                if atom in always:
                    continue
                first = first_seen.get(atom)
                if first is not None and first < boundary:
                    self.refs[k].append(atom)
        self.refs = [sorted(set(r)) for r in self.refs]
        # Recorded harness projection rewrites (not retirement, not compaction).
        checkpoints = sorted(c["item"] for c in session["checkpoints"])
        self.projection_rewrites: dict[int, int] = {}
        for k in range(1, len(self.requests)):
            r0, r1 = self.requests[k - 1], self.requests[k]
            if r1["hist_prev"] is None or r1["hist_div"] is None or r1["hist_div"] >= r1["hist_prev"]:
                continue
            if r1["epoch"] != r0["epoch"] or any(r0["item_end"] <= c < r1["item_end"] for c in checkpoints):
                continue
            self.projection_rewrites[k] = r1["hist_div"]
        # Recorded turn-start re-layouts: the provider prompt shrank by more
        # than the replay explains (stale-evidence consolidation and slot
        # retirement in the current projection), with an uncached re-read.
        self.relayouts: dict[int, float] = {}
        for k in range(1, len(self.requests)):
            r0, r1 = self.requests[k - 1], self.requests[k]
            if not self.turn_start[k] or r1["epoch"] != r0["epoch"]:
                continue
            if any(r0["item_end"] <= c < r1["item_end"] for c in checkpoints):
                continue
            fresh = sum(self.tokens[i] for i in range(r0["item_end"], r1["item_end"])
                        if self.items[i][5] == r1["epoch"] and not self.items[i][6] & F_CHECKPOINT)
            shrink = fresh - (r1["input"] - r0["input"])
            if shrink > 10_000:
                self.relayouts[k] = shrink
        self.peak_input = max((r["input"] for r in self.requests), default=0)

    def window(self, k: int) -> int:
        end = self.requests[k]["item_end"]
        value = 272_000
        for index, window in self.windows:
            if index <= end:
                value = window
        return value

    def gap_s(self, k: int) -> float:
        if k == 0:
            return 0.0
        dispatch = self.requests[k]["dispatch_ms"]
        complete = self.requests[k - 1]["complete_ms"]
        if dispatch is None or complete is None:
            return 0.0
        return max(0.0, (dispatch - complete) / 1000.0)


class Policy:
    name = "policy"
    include_base = False          # replay recorded compaction bases and checkpoints (recorded view only)
    harness_projection = False    # keep the current harness's turn-start/mid-turn projection rewrites
    description = ""

    def on_items(self, eng: "Engine", lo: int, hi: int) -> None:
        for i in range(lo, hi):
            item = eng.p.items[i]
            if item[6] & (F_BASE | F_CHECKPOINT):
                continue
            eng.append_item(i)

    def before_request(self, eng: "Engine", k: int) -> None:
        pass


class Engine:
    def __init__(self, prepared: Prepared, policy: Policy, calib: dict[str, Any], ttl: float, recovery: bool,
                 cache_weight: float = 0.1, reuse_alpha: float | None = None):
        self.reuse_alpha = calib.get("cache", {}).get("reuse_alpha", 1.0) if reuse_alpha is None else reuse_alpha
        self.miss_rates = calib.get("cache", {}).get("random_miss_by_gap", [])
        self.p = prepared
        self.policy = policy
        self.calib = calib
        self.ttl = ttl
        self.recovery = recovery
        self.cache_weight = cache_weight
        self.entries: list[list[Any]] = []    # [key, tokens, atoms(set|None), meta]
        self.form: dict[int, int] = {}
        self.hist_tokens = 0.0
        self.dirty: int | None = None
        self.prev_total = 0.0
        self.prev_complete_ms: int | None = None
        self.summary_cursor = 0
        self.k = 0
        self.synthetic_seq = 0
        self.lost_seen: set[tuple[int, int]] = set()
        self.hide_epoch = 0
        self.m: Counter = Counter()
        self.per_request: list[tuple[float, float]] = []
        self._extra_atoms: set[int] | None = None
        self._fresh_since_last = 0.0

    # ---- view mutation -------------------------------------------------
    def append_item(self, i: int, form: int = FULL, tokens: float | None = None) -> None:
        tok = self.p.tokens[i] if tokens is None else tokens
        self.entries.append([("i", i, form), tok, None, None])
        self.form[i] = form
        self.hist_tokens += tok

    def append_synthetic(self, kind: str, tokens: float, atoms: set[int] | None = None, meta: Any = None) -> None:
        self.synthetic_seq += 1
        self.entries.append([("s", kind, self.synthetic_seq), tokens, atoms, meta])
        self.hist_tokens += tokens
        if atoms:
            self._extra_atoms = None

    def mark_dirty(self, position: int) -> None:
        position = max(0, min(position, len(self.entries)))
        self.dirty = position if self.dirty is None else min(self.dirty, position)

    def _full_outputs(self) -> set[int]:
        items = self.p.items
        return {e[0][1] for e in self.entries if e[0][0] == "i" and e[0][2] == FULL and items[e[0][1]][0] in EVIDENCE_KINDS}

    def replace_entries(self, new_entries: list[list[Any]]) -> None:
        items = self.p.items
        before_full = self._full_outputs()
        after_full = {e[0][1] for e in new_entries if e[0][0] == "i" and e[0][2] == FULL and items[e[0][1]][0] in EVIDENCE_KINDS}
        self.m["hidden_outputs"] += len(before_full - after_full)
        old = self.entries
        first = 0
        limit = min(len(old), len(new_entries))
        while first < limit and old[first][0] == new_entries[first][0]:
            first += 1
        if first < len(old):
            self.mark_dirty(first)
        self.entries = new_entries
        self.form = {}
        for entry in new_entries:
            key = entry[0]
            if key[0] == "i":
                self.form[key[1]] = key[2]
        self.hist_tokens = sum(entry[1] for entry in new_entries)
        self.hide_epoch += 1
        self._extra_atoms = None

    def total(self) -> float:
        return self.p.static + self.hist_tokens

    def limit(self, k: int) -> float:
        return 0.9 * self.p.window(k)

    def prefix_tokens(self, position: int) -> float:
        return self.p.static + sum(entry[1] for entry in self.entries[:position])

    # ---- pricing -----------------------------------------------------------
    def _cached_for_next(self, gap: float) -> float:
        if gap > self.ttl:
            return 0.0
        if self.dirty is None:
            return min(self.prev_total, self.total())
        lcp = min(self.prefix_tokens(self.dirty), self.prev_total, self.total())
        static = min(self.p.static, lcp)
        # Observed: after an in-place rewrite the provider reuses only part of
        # the common prefix the harness reports (reuse_alpha, calibrated).
        return static + self.reuse_alpha * (lcp - static)

    def random_miss_rate(self, gap: float) -> float:
        for upper, rate in self.miss_rates:
            if gap < upper:
                return rate
        return self.miss_rates[-1][1] if self.miss_rates else 0.0

    def price_extra(self, kind: str, out: float, reasoning: float, latency: float, extra_input: float = 0.0) -> None:
        """A request the policy itself adds (summary, recovery)."""
        gap = 0.0
        total = self.total() + extra_input
        cached = self._cached_for_next(gap)
        uncached = total - cached
        m = self.m
        m["requests"] += 1
        m[f"requests:{kind}"] += 1
        m["input"] += total
        m["cached"] += cached
        m["uncached"] += uncached
        m["out"] += out
        m["reasoning"] += reasoning
        m[f"out:{kind}"] += out
        m["model_s"] += latency
        m[f"model_s:{kind}"] += latency
        self.prev_total = total
        self.dirty = None

    def price_request(self, k: int) -> None:
        request = self.p.requests[k]
        gap = self.p.gap_s(k)
        total = self.total()
        cached = self._cached_for_next(gap)
        if self.dirty is None and gap <= self.ttl:
            # Expected provider-side cold misses on append-only prompts.
            miss = self.random_miss_rate(gap) * cached
            cached -= miss
            self.m["random_miss_uncached"] += miss
        uncached = total - cached
        lat = self.calib["latency"]
        slope_ctx = lat["context_slope_per_1k_s"]
        slope_unc = max(0.0, lat["ols"]["per_1k_uncached_s"])
        recorded_uncached = request["input"] - request["cached"]
        latency = max(0.5, request["stream_wait_s"] + slope_ctx * (total - request["input"]) / 1000.0
                      + slope_unc * (uncached - recorded_uncached) / 1000.0)
        m = self.m
        m["requests"] += 1
        m["requests:recorded"] += 1
        m["input"] += total
        m["cached"] += cached
        m["uncached"] += uncached
        m["out"] += request["out"]
        m["reasoning"] += request["reasoning"]
        m["model_s"] += latency
        m["model_s:recorded"] += latency
        m["peak"] = max(m["peak"], total)
        m["context_sum"] += total
        if self.dirty is not None and gap <= self.ttl:
            m["reread"] += max(0.0, total - cached - self._fresh_since_last)
        if gap > self.ttl:
            m["ttl_cold_requests"] += 1
        self.per_request.append((total, cached))
        self.prev_total = total
        self.dirty = None

    # ---- lexical exposure ------------------------------------------------
    def atom_visible(self, atom: int, boundary: int) -> bool:
        postings = self.p.postings.get(atom)
        if postings:
            form = self.form
            for i in reversed(postings):
                if i >= boundary:
                    continue
                f = form.get(i)
                if f is None:
                    continue
                if f == FULL:
                    return True
                if f == PIN and atom in self.p.head_atoms[i]:
                    return True
                if f == STUB:
                    patch = self.p.patch.get(i)
                    if patch and atom in patch[1]:
                        return True
        return atom in self.extra_atoms()

    def extra_atoms(self) -> set[int]:
        if self._extra_atoms is None:
            atoms: set[int] = set()
            for entry in self.entries:
                if entry[2]:
                    atoms.update(entry[2])
            self._extra_atoms = atoms
        return self._extra_atoms

    def lost_references(self, k: int) -> list[int]:
        boundary = self.p.requests[k]["item_end"]
        lost = []
        for atom in self.p.refs[k]:
            if not self.atom_visible(atom, boundary):
                lost.append(atom)
        return lost

    def recover(self, k: int, lost: list[int]) -> None:
        boundary = self.p.requests[k]["item_end"]
        sources: dict[int, float] = {}
        atoms: set[int] = set()
        for atom in lost:
            postings = self.p.postings.get(atom) or []
            source = next((i for i in reversed(postings) if i < boundary), None)
            if source is None:
                continue
            sources[source] = min(self.p.tokens[source], RECOVERY_SOURCE_CAP_TOKENS)
            atoms.update(self.p.item_atoms[source])
        if not sources:
            return
        tokens = sum(sources.values())
        lat = self.calib["latency"]["ols"]
        slope_ctx = self.calib["latency"]["context_slope_per_1k_s"]
        latency = (max(lat["intercept_s"], 0.5) + RECOVERY_REQUEST_OUT * lat["per_out_token_s"]
                   + RECOVERY_REQUEST_REASONING * lat["per_reasoning_token_s"]
                   + slope_ctx * self.total() / 1000.0 + max(0.0, lat["per_1k_uncached_s"]) * tokens / 1000.0)
        self.price_extra("recovery", RECOVERY_REQUEST_OUT, RECOVERY_REQUEST_REASONING, latency)
        self.append_synthetic("recovery", tokens, atoms, {"k": k})
        self.m["recovery_tokens"] += tokens
        self.m["recovery_sources"] += len(sources)

    # ---- main loop ---------------------------------------------------------
    def run(self) -> Counter:
        p = self.p
        previous_end = 0
        for k, request in enumerate(p.requests):
            self.k = k
            before = self.hist_tokens
            self.policy.on_items(self, previous_end, request["item_end"])
            previous_end = request["item_end"]
            self._fresh_since_last = self.hist_tokens - before
            if self.policy.harness_projection and k in p.projection_rewrites:
                self.mark_dirty(p.projection_rewrites[k])
                self.m["projection_rewrites"] += 1
            if self.policy.harness_projection and k in p.relayouts:
                self.relayout(p.relayouts[k])
            self.policy.before_request(self, k)
            lost = self.lost_references(k)
            fresh_lost = [a for a in lost if (a, self.hide_epoch) not in self.lost_seen]
            for atom in fresh_lost:
                self.lost_seen.add((atom, self.hide_epoch))
            if fresh_lost:
                self.m["lost_refs"] += len(fresh_lost)
                self.m["lost_ref_requests"] += 1
                if self.recovery:
                    self.recover(k, fresh_lost)
            self.m["refs"] += len(p.refs[k])
            self.price_request(k)
            self.prev_complete_ms = request["complete_ms"]
        return self.m

    def relayout(self, shrink: float) -> None:
        """Emulate the current projection's turn-start re-layout: the oldest
        full outputs (stale evidence) become receipts until the recorded
        shrink is reached; the prompt diverges at the first one."""
        receipt = self.p.tm.pin_tokens
        saved, first = 0.0, None
        for index, entry in enumerate(self.entries):
            if saved >= shrink:
                break
            key = entry[0]
            if key[0] != "i" or key[2] != FULL or self.p.items[key[1]][0] != K_OUTPUT or entry[1] <= receipt:
                continue
            saved += entry[1] - receipt
            self.m["hidden_outputs"] += 1
            self.hist_tokens += receipt - entry[1]
            self.entries[index] = [("i", key[1], PIN), receipt, None, None]
            self.form[key[1]] = PIN
            first = index if first is None else first
        if first is not None:
            self.mark_dirty(first)
            self.hide_epoch += 1
        self.m["relayouts"] += 1
        self.m["relayout_saved"] += saved

    # ---- shared policy building blocks -----------------------------------
    def retire(self, call_items: list[int], checkpoint: bool = True) -> float:
        """Replace outputs in place with pins; optionally append a checkpoint item."""
        if not call_items:
            return 0.0
        positions = {entry[0][1]: index for index, entry in enumerate(self.entries) if entry[0][0] == "i"}
        saved = 0.0
        first = None
        for i in call_items:
            index = positions.get(i)
            if index is None:
                continue
            entry = self.entries[index]
            pin = self.p.tm.pin_tokens
            if entry[0][2] == FULL:
                self.m["hidden_outputs"] += 1
            saved += entry[1] - pin
            self.hist_tokens += pin - entry[1]
            self.entries[index] = [("i", i, PIN), pin, None, None]
            self.form[i] = PIN
            first = index if first is None else min(first, index)
        if first is not None:
            self.mark_dirty(first)
        if checkpoint:
            self.append_synthetic("checkpoint", CHECKPOINT_BASE_TOKENS + CHECKPOINT_PER_ID_TOKENS * len(call_items))
        self.hide_epoch += 1
        return saved

    def retirement_selection(self, keep_generations: int, target: float) -> tuple[list[int], float]:
        """Mirror of pressure_retirement_selection: consumed outputs outside the
        newest generations, oldest first, whose pin is smaller."""
        k = self.k
        protected_from = k - keep_generations  # outputs consumed by requests > protected_from stay
        selected, saved = [], 0.0
        pin = self.p.tm.pin_tokens
        for entry in self.entries:
            if saved >= target:
                break
            key = entry[0]
            if key[0] != "i" or key[2] != FULL:
                continue
            i = key[1]
            if self.p.items[i][0] != K_OUTPUT:
                continue
            consumed_by = self.p.slot[i] + 1
            if consumed_by > protected_from or consumed_by > k:
                continue
            if entry[1] <= pin:
                continue
            selected.append(i)
            saved += entry[1] - pin
        return selected, saved

    def task_input_checkpoint(self, with_summary_tokens: float | None) -> list[list[Any]]:
        """Bounded task input retained by compaction and the fresh-window path."""
        stable, users, agents, last_asst, last_plan = [], [], [], None, None
        for entry in self.entries:
            key = entry[0]
            if key[0] != "i":
                continue
            i = key[1]
            item = self.p.items[i]
            kind, flags = item[0], item[6]
            if flags & F_STABLE:
                stable.append(entry)
            elif kind == K_USER:
                users.append(entry)
            elif kind == K_AGENT:
                agents.append(entry)
            elif kind == K_ASST:
                last_asst = entry
            elif kind == K_DEV and not flags & F_CHECKPOINT:
                last_plan = entry
        kept_users, budget = [], 16_000.0
        if users:
            for entry in [users[0]] + list(reversed(users[1:])):
                if entry[1] <= budget:
                    kept_users.append(entry)
                    budget -= entry[1]
        kept_agents, budget = [], 8_000.0
        for entry in reversed(agents):
            if entry[1] <= budget:
                kept_agents.append(entry)
                budget -= entry[1]
        retained = sorted(kept_users + kept_agents, key=lambda e: e[0][1])
        new = list(stable) + retained
        if last_asst is not None:
            new.append([last_asst[0], min(last_asst[1], 2_400.0), None, None])
        self.synthetic_seq += 1
        new.append([("s", "recovery_pointer", self.synthetic_seq), 300.0, None, None])
        if with_summary_tokens is not None:
            self.synthetic_seq += 1
            new.append([("s", "summary", self.synthetic_seq), with_summary_tokens, None, None])
        if last_plan is not None:
            new.append(last_plan)
        return new

    def next_summary_size(self) -> float:
        sizes = self.p.recorded_summary_sizes
        if self.summary_cursor < len(sizes):
            size = sizes[self.summary_cursor]
        else:
            corpus = self.calib["compaction"]["summary_out_tokens"]
            size = statistics.median(corpus) if corpus else 6_000
        self.summary_cursor += 1
        return float(size)

    def remote_compact(self) -> None:
        sigma = self.next_summary_size()
        lat = self.calib["compaction"]["summary_latency"]
        latency = lat["intercept_s"] + lat["per_output_token_s"] * sigma
        self.price_extra("summary", sigma, 0.0, latency, extra_input=20.0)
        ratio = self.calib["compaction"]["compaction_item_tokens_per_summary_token"]
        self.replace_entries(self.task_input_checkpoint(ratio * sigma))
        self.m["compactions"] += 1

    def fresh_window(self) -> None:
        self.replace_entries(self.task_input_checkpoint(None))
        self.m["fresh_windows"] += 1

    def fold_entries(self, keep_generations: int, mode: dict[str, Any]) -> list[list[Any]]:
        p = self.p
        k = self.k
        window_start = p.requests[max(0, k - keep_generations)]["item_end"] if keep_generations > 0 else p.requests[k]["item_end"]
        folded: list[list[Any]] = []
        window: list[list[Any]] = []
        ledger_files: set[int] = set(mode.get("_ledger_files", set()))
        index_lines = 0
        pin = p.tm.pin_tokens
        latest_by_tag: dict[str, int] = {}
        for entry in self.entries:
            key = entry[0]
            if key[0] == "i" and key[1] < window_start:
                tag = p.items[key[1]][9] if len(p.items[key[1]]) > 9 else None
                if tag:
                    latest_by_tag[tag] = key[1]
        for entry in self.entries:
            key = entry[0]
            if key[0] == "s":
                kind = key[1]
                if kind in ("checkpoint", "ledger"):
                    continue
                if kind == "recovery":
                    if entry[3] and entry[3].get("k", 0) >= k - keep_generations:
                        window.append(entry)
                    continue
                folded.append(entry)
                continue
            i, form = key[1], key[2]
            if i >= window_start:
                window.append(entry)
                continue
            item = p.items[i]
            kind, flags = item[0], item[6]
            tag = item[9] if len(item) > 9 else None
            if tag and latest_by_tag.get(tag) != i and (flags & F_STABLE or kind == K_DEV):
                continue  # superseded harness context: latest value wins at a fold
            if flags & F_CHECKPOINT:
                continue
            if flags & F_STABLE or kind in (K_USER, K_ASST) or kind == K_DEV:
                folded.append(entry)
            elif kind == K_REASON:
                rule = mode.get("reasoning", "all")
                if rule == "all":
                    folded.append(entry)
                elif rule == "summary" and item[1] > 0:
                    folded.append([("i", i, STUB), p.tm.summary_text(item), set(p.item_atoms[i]), None])
            elif kind == K_CALL:
                rule = mode.get("calls", "full")
                patch = p.patch.get(i)
                if patch:
                    ledger_files.update(patch[1])
                if rule == "full" or form != FULL:
                    folded.append(entry)
                elif rule == "stub":
                    if patch:
                        stub_bytes = max(0, item[1] - patch[0]) + PATCH_STUB_BYTES
                        folded.append([("i", i, STUB), stub_bytes * p.tm.rates["call"], None, None])
                    else:
                        folded.append(entry)
                elif rule == "drop":
                    pass
            elif kind == K_OUTPUT:
                rule = mode.get("outputs", "pin")
                if form != FULL or entry[1] <= pin:
                    if rule == "index" and form == PIN:
                        index_lines += 1
                    else:
                        folded.append(entry)
                elif rule == "pin":
                    folded.append([("i", i, PIN), pin, None, None])
                elif rule == "index":
                    index_lines += 1
            elif kind == K_AGENT:
                # Sub-agent reports are evidence: folded like tool outputs.
                rule = mode.get("outputs", "pin")
                if form != FULL or entry[1] <= pin:
                    folded.append(entry)
                elif rule == "pin":
                    folded.append([("i", i, PIN), pin, None, None])
                else:
                    index_lines += 1
            else:
                folded.append(entry)
        if mode.get("ledger"):
            mode["_ledger_files"] = ledger_files
            mode["_index_lines"] = mode.get("_index_lines", 0) + index_lines
            tokens = LEDGER_BASE_TOKENS + LEDGER_PER_FILE_TOKENS * len(ledger_files) + INDEX_LINE_TOKENS * mode["_index_lines"]
            self.synthetic_seq += 1
            folded.append([("s", "ledger", self.synthetic_seq), tokens, set(ledger_files), None])
        return folded + window

    def fold(self, keep_generations: int, mode: dict[str, Any]) -> None:
        self.replace_entries(self.fold_entries(keep_generations, mode))
        self.m["folds"] += 1

    def dry_run_fold(self, keep_generations: int, mode: dict[str, Any]) -> tuple[float, float]:
        """(total after fold, uncached re-read the fold would cause)."""
        trial_mode = dict(mode)
        if "_ledger_files" in mode:
            trial_mode["_ledger_files"] = set(mode["_ledger_files"])
        seq = self.synthetic_seq
        new = self.fold_entries(keep_generations, trial_mode)
        self.synthetic_seq = seq
        first = 0
        limit = min(len(self.entries), len(new))
        while first < limit and self.entries[first][0] == new[first][0]:
            first += 1
        total_after = self.p.static + sum(e[1] for e in new)
        unchanged = self.p.static + sum(e[1] for e in new[:first])
        reused = self.p.static + self.reuse_alpha * (unchanged - self.p.static)
        return total_after, total_after - reused


# --------------------------------------------------------------------------
# Policies
# --------------------------------------------------------------------------

LIMIT_OF_REPORTED_WINDOW = 0.9 / 0.95   # 90% of the raw window; token_count reports 95% of it
PROACTIVE_TARGET = 2.0 / 3.0
PROACTIVE_MIN_SAVED = 20_000
KEPT_GENERATIONS = 3


def harness_limit(eng: Engine, k: int) -> float:
    return LIMIT_OF_REPORTED_WINDOW * eng.p.window(k)


class RecordedView(Policy):
    """What the recorded harness did, rebuilt from the timeline: recorded
    compactions (with their recorded summary requests) and recorded
    retirement checkpoints. Validates the replay against provider numbers."""
    name = "REC"
    description = "as recorded (compactions and retirements replayed from the rollout)"
    include_base = True
    harness_projection = True

    def __init__(self) -> None:
        self.epoch = 0
        self.outputs_by_call: dict[tuple[int, str], int] = {}

    def on_items(self, eng: Engine, lo: int, hi: int) -> None:
        p = eng.p
        i = lo
        while i < hi:
            item = p.items[i]
            epoch = item[5]
            if epoch != self.epoch:
                self.epoch = epoch
                compaction = next((c for c in p.session["compactions"] if c["epoch"] == epoch), None)
                if compaction and compaction.get("summary_out"):
                    eng.price_extra("summary", compaction["summary_out"], 0.0, compaction.get("summary_wait_s") or 0.0, 20.0)
                    eng.m["compactions"] += 1
                base_end = compaction["base_end"] if compaction else i
                new_entries = []
                for j in range(i, min(base_end, len(p.items))):
                    if p.items[j][6] & F_BASE:
                        new_entries.append([("i", j, FULL), p.tokens[j], None, None])
                eng.replace_entries(new_entries)
                i = max(base_end, i + 1) if compaction else i + 1
                continue
            if item[6] & F_BASE:
                i += 1
                continue
            if item[6] & F_CHECKPOINT:
                checkpoint = next((c for c in p.session["checkpoints"] if c["item"] == i), None)
                targets = []
                for call_id in (checkpoint or {}).get("retired", []):
                    j = self.outputs_by_call.get((epoch, call_id))
                    if j is not None and eng.form.get(j) == FULL:
                        targets.append(j)
                eng.retire(targets, checkpoint=False)
                eng.append_item(i)
                eng.m["retirements"] += 1
                eng.m["retired_outputs"] += len(targets)
                i += 1
                continue
            if item[0] == K_OUTPUT and item[3]:
                self.outputs_by_call[(epoch, item[3])] = i
            eng.append_item(i)
            i += 1


class CurrentHarness(Policy):
    """Current working tree: proactive retirement at 2/3 of the limit (>=20k
    saved, newest 3 generations kept), limit-triggered retirement when it
    fits, otherwise remote compaction v2."""
    name = "CUR"
    description = "current code: proactive + limit retirement, remote v2 compaction"
    harness_projection = True

    def __init__(self, proactive: bool = True, retire_at_limit: bool = True, projection: bool = True,
                 name: str | None = None, description: str | None = None) -> None:
        self.proactive = proactive
        self.retire_at_limit = retire_at_limit
        self.harness_projection = projection
        if name:
            self.name = name
        if description:
            self.description = description

    def before_request(self, eng: Engine, k: int) -> None:
        total = eng.total()
        limit = harness_limit(eng, k)
        target = limit * PROACTIVE_TARGET
        if total >= limit:
            if self.retire_at_limit:
                selection, saved = eng.retirement_selection(KEPT_GENERATIONS, total - target)
                if selection and total - saved < limit:
                    eng.retire(selection)
                    eng.m["retirements"] += 1
                    eng.m["retired_outputs"] += len(selection)
                    return
            eng.remote_compact()
            return
        if self.proactive and not eng.p.turn_start[k] and total > target:
            selection, saved = eng.retirement_selection(KEPT_GENERATIONS, total - target)
            if selection and saved >= PROACTIVE_MIN_SAVED:
                eng.retire(selection)
                eng.m["retirements"] += 1
                eng.m["retired_outputs"] += len(selection)


class FreshWindowAtLimit(Policy):
    """Existing non-generative token-budget path: at the limit, install a
    fresh window (bounded task input + recovery pointer), no summary."""
    name = "TB"
    description = "token-budget fresh window at the limit (no generation)"
    harness_projection = True

    def before_request(self, eng: Engine, k: int) -> None:
        if eng.total() >= harness_limit(eng, k):
            eng.fresh_window()


class DeterministicFold(Policy):
    """Batched deterministic fold at a watermark. Older outputs become pins
    (or ledger index lines); optional patch-body stubs and reasoning rules;
    the newest ``keep`` generations stay verbatim. Append-only between folds
    (no turn-start or mid-turn projection rewrites)."""
    harness_projection = False

    def __init__(self, name: str, high: float, keep: int, mode: dict[str, Any], description: str,
                 high_is_fraction: bool = False, aggressive: bool = True, goal_fraction: float = 0.6) -> None:
        self.name = name
        self.high = high
        self.high_is_fraction = high_is_fraction
        self.keep = keep
        self.mode = dict(mode)
        self.description = description
        self.aggressive = aggressive
        self.goal_fraction = goal_fraction

    def watermark(self, eng: Engine, k: int) -> float:
        high = self.high * eng.p.window(k) if self.high_is_fraction else self.high
        return min(high, harness_limit(eng, k))

    last_fold = -10**9
    MIN_SPACING = 5

    def plan_fold(self, eng: Engine, k: int, goal_watermark: float | None = None) -> tuple[int, dict[str, Any], bool, float, float]:
        """Size one fold in dry runs so the installed view leaves room:
        shrink the verbatim window first, then fall back to the aggressive
        ledger mode for this fold (unless disabled, which keeps every
        reasoning item and call). Returns (keep, mode, aggressive, total
        after, re-read)."""
        goal = self.goal_fraction * (self.watermark(eng, k) if goal_watermark is None else goal_watermark)
        keep, mode = self.keep, self.mode
        after, reread = eng.dry_run_fold(keep, mode)
        while after > goal and keep > 2:
            keep = max(2, keep // 2)
            after, reread = eng.dry_run_fold(keep, mode)
        if after > goal and not self.aggressive:
            eng.m["fold_over_goal"] += 1
        aggressive = after > goal and self.aggressive
        if aggressive:
            mode = dict(FOLD_MODES["ledger-norsn"])
            mode["_ledger_files"] = set(self.mode.get("_ledger_files", set()))
            mode["_index_lines"] = self.mode.get("_index_lines", 0)
            after, reread = eng.dry_run_fold(keep, mode)
        return keep, mode, aggressive, after, reread

    def do_fold(self, eng: Engine, k: int, reason: str, plan: tuple[int, dict[str, Any], bool, float, float] | None = None,
                goal_watermark: float | None = None) -> None:
        """One fold; a single replace keeps one cache break."""
        keep, mode, aggressive, _, _ = plan or self.plan_fold(eng, k, goal_watermark)
        if aggressive:
            eng.m["fold_aggressive"] += 1
        eng.m["fold_before_tokens"] += eng.total()
        eng.fold(keep, mode)
        eng.m["fold_after_tokens"] += eng.total()
        if mode is not self.mode:
            self.mode["_ledger_files"] = mode.get("_ledger_files", set())
            self.mode["_index_lines"] = mode.get("_index_lines", 0)
        eng.m[f"folds:{reason}"] += 1
        self.last_fold = k
        if eng.total() >= harness_limit(eng, k):
            eng.fresh_window()
            eng.m["fold_fallback_fresh_window"] += 1

    def before_request(self, eng: Engine, k: int) -> None:
        total = eng.total()
        if total >= harness_limit(eng, k) or (total >= self.watermark(eng, k) and k - self.last_fold >= self.MIN_SPACING):
            self.do_fold(eng, k, "watermark")


class CacheAwareFold(DeterministicFold):
    """The plain watermark fold plus two cache-aware rules. Cold fold: from
    ``cold_floor`` up, fold when the provider cache is cold anyway (idle
    beyond TTL), so the re-read is already paid. Deferral: at the watermark,
    skip the fold while the expected remaining requests cannot repay its
    one-time uncached re-read (the harness limit still forces it). Folds are
    sized like the plain fold (0.6 x the watermark)."""

    def __init__(self, name: str, high: float, keep: int, mode: dict[str, Any], description: str,
                 remaining: "RemainingModel", cold_floor: float | None = None, defer: bool = False,
                 oracle: bool = False) -> None:
        super().__init__(name, high, keep, mode, description)
        self.cold_floor = cold_floor
        self.defer = defer
        self.remaining = remaining
        self.oracle = oracle

    def before_request(self, eng: Engine, k: int) -> None:
        total = eng.total()
        if total >= harness_limit(eng, k):
            self.do_fold(eng, k, "limit")
            return
        if k - self.last_fold < self.MIN_SPACING:
            return
        if self.cold_floor is not None and total >= self.cold_floor and eng.p.gap_s(k) > eng.ttl:
            self.do_fold(eng, k, "cold_cache")
            return
        if total < self.watermark(eng, k):
            return
        plan = self.plan_fold(eng, k)
        if self.defer:
            _, _, _, after, reread = plan
            n = (len(eng.p.requests) - k) if self.oracle else self.remaining.expected(k)
            c = eng.cache_weight
            if n * c * (total - after) <= (1.0 - c) * reread:
                eng.m["folds_deferred"] += 1
                return
        self.do_fold(eng, k, "watermark", plan)


class ShallowEarlyFold(DeterministicFold):
    """Superseded cache-aware rule, kept for comparison: fold at the hard
    watermark always; between ``soft`` and hard, fold when the cache is cold
    or the expected remaining requests repay the re-read, but size every fold
    against the hard watermark (so early folds are shallow)."""

    def __init__(self, name: str, soft: float, hard: float, keep: int, mode: dict[str, Any], description: str,
                 remaining: "RemainingModel") -> None:
        super().__init__(name, hard, keep, mode, description)
        self.soft = soft
        self.remaining = remaining

    def before_request(self, eng: Engine, k: int) -> None:
        total = eng.total()
        if total >= harness_limit(eng, k) or (total >= self.watermark(eng, k) and k - self.last_fold >= self.MIN_SPACING):
            self.do_fold(eng, k, "hard")
            return
        if total < self.soft or k - self.last_fold < self.MIN_SPACING:
            return
        if eng.p.gap_s(k) > eng.ttl:
            self.do_fold(eng, k, "cold_cache")
            return
        after, reread = eng.dry_run_fold(self.keep, self.mode)
        if after >= total:
            return
        c = eng.cache_weight
        if self.remaining.expected(k) * c * (total - after) > (1.0 - c) * reread:
            self.do_fold(eng, k, "scheduled")


class SlidingMask(Policy):
    """Observation masking applied every request (no batching): outputs not
    consumed by the newest ``keep`` generations become pins immediately."""
    harness_projection = False

    def __init__(self, keep: int) -> None:
        self.keep = keep
        self.name = f"SLIDE-K{keep}"
        self.description = f"per-request observation masking, newest {keep} generations verbatim"

    def before_request(self, eng: Engine, k: int) -> None:
        selection, _ = eng.retirement_selection(self.keep, float("inf"))
        if selection:
            eng.retire(selection, checkpoint=False)
        if eng.total() >= harness_limit(eng, k):
            eng.fresh_window()


def _no_projection(policy: Policy, name: str | None = None, description: str | None = None) -> Policy:
    policy.harness_projection = False
    if name:
        policy.name = name
    if description:
        policy.description = description
    return policy


class RemainingModel:
    """Online expectation of remaining requests in a session given requests
    so far, from the corpus distribution of session lengths."""

    def __init__(self, lengths: list[int]):
        self.lengths = sorted(lengths)

    def expected(self, k: int) -> float:
        longer = [n - k - 1 for n in self.lengths if n > k + 1]
        if not longer:
            return 1.0
        return statistics.median(longer)


FOLD_MODES = {
    "mask": {"outputs": "pin", "calls": "full", "reasoning": "all"},
    "stub": {"outputs": "pin", "calls": "stub", "reasoning": "all"},
    "stub-rsum": {"outputs": "pin", "calls": "stub", "reasoning": "summary"},
    "ledger": {"outputs": "index", "calls": "stub", "reasoning": "all", "ledger": True},
    "ledger-rsum": {"outputs": "index", "calls": "stub", "reasoning": "summary", "ledger": True},
    "ledger-norsn": {"outputs": "index", "calls": "drop", "reasoning": "none", "ledger": True},
}


def policy_factories(remaining: RemainingModel) -> list[Any]:
    factories: list[Any] = [
        RecordedView,
        CurrentHarness,
        lambda: CurrentHarness(proactive=False, name="LIMRET", description="retirement only at the limit, remote v2 fallback"),
        lambda: CurrentHarness(proactive=False, retire_at_limit=False, name="REMOTE", description="remote v2 compaction only (upstream behaviour)"),
        lambda: CurrentHarness(projection=False, name="CUR-noproj", description="current code without turn-start/mid-turn projection rewrites"),
        FreshWindowAtLimit,
        lambda: _no_projection(CurrentHarness(proactive=False, retire_at_limit=False, name="REMOTE-noproj",
                                              description="remote v2 only, append-only projection")),
        lambda: _no_projection(FreshWindowAtLimit(), "TB-noproj", "token-budget fresh window, append-only projection"),
        lambda: SlidingMask(8),
    ]
    for mode in ("mask", "stub", "stub-rsum", "ledger", "ledger-rsum", "ledger-norsn"):
        for high in (150_000, 200_000, 300_000):
            for keep in (8,):
                factories.append(lambda mode=mode, high=high, keep=keep: DeterministicFold(
                    f"FOLD-{mode}-H{high // 1000}k-K{keep}", high, keep, FOLD_MODES[mode],
                    f"fold at {high // 1000}k, mode {mode}, newest {keep} generations verbatim"))
    for mode in ("mask", "ledger-rsum", "ledger-norsn"):
        for high in (100_000, 120_000):
            factories.append(lambda mode=mode, high=high: DeterministicFold(
                f"FOLD-{mode}-H{high // 1000}k-K8", high, 8, FOLD_MODES[mode],
                f"fold at {high // 1000}k, mode {mode}, newest 8 generations verbatim"))
    for mode in ("mask", "ledger-rsum"):
        factories.append(lambda mode=mode: DeterministicFold(
            f"FOLD-{mode}-H150k-K8-keeprsn", 150_000, 8, FOLD_MODES[mode],
            f"fold at 150k, mode {mode}, never drops reasoning or calls (no aggressive fallback)", aggressive=False))
        factories.append(lambda mode=mode: DeterministicFold(
            f"FOLD-{mode}-H150k-K8-keeprsn-g75", 150_000, 8, FOLD_MODES[mode],
            f"as -keeprsn, folds sized to 75% of the watermark", aggressive=False, goal_fraction=0.75))
    for mode in ("stub", "ledger"):
        for keep in (3, 16):
            factories.append(lambda mode=mode, keep=keep: DeterministicFold(
                f"FOLD-{mode}-H200k-K{keep}", 200_000, keep, FOLD_MODES[mode],
                f"fold at 200k, mode {mode}, newest {keep} generations verbatim"))
    for mode in ("mask", "ledger-norsn"):
        factories.append(lambda mode=mode: CacheAwareFold(
            f"CA-{mode}-H150k-cold80k", 150_000, 8, FOLD_MODES[mode],
            f"fold at 150k plus cold-cache folds from 80k, mode {mode}", remaining, cold_floor=80_000))
        factories.append(lambda mode=mode: CacheAwareFold(
            f"CA-{mode}-H150k-cold80k-defer", 150_000, 8, FOLD_MODES[mode],
            f"cold-cache folds from 80k; defer watermark folds the expected remaining requests cannot repay, mode {mode}",
            remaining, cold_floor=80_000, defer=True))
        factories.append(lambda mode=mode: CacheAwareFold(
            f"CA-{mode}-H150k-cold80k-defer-oracle", 150_000, 8, FOLD_MODES[mode],
            f"as -defer with oracle remaining-request knowledge (bound), mode {mode}",
            remaining, cold_floor=80_000, defer=True, oracle=True))
    for soft in (100_000, 150_000):
        factories.append(lambda soft=soft: ShallowEarlyFold(
            f"SHALLOW-stub-S{soft // 1000}k-H300k-K8", soft, 300_000, 8, FOLD_MODES["stub"],
            f"superseded early fold from {soft // 1000}k sized against a 300k hard watermark", remaining))
    return factories


# --------------------------------------------------------------------------
# Recovery calibration from recorded hiding events
# --------------------------------------------------------------------------

def recorded_rerequest_rates(prepared: Prepared, horizon: int = 40) -> Counter:
    """Targeted recovery behaviour in the recorded sessions.

    At each recorded hiding event (retirement, turn-start re-layout,
    compaction) collect the tool outputs that stopped being visible in full.
    For each, take the file paths its own call named and look ahead
    ``horizon`` requests for a later model call naming any of those paths
    again (a re-request). The control group is outputs still visible in full
    at the same instant whose consumption is at least KEPT_GENERATIONS old.
    The attributable recovery propensity is the difference in re-request
    rates; the re-read volume is the size of the re-requesting call's output.
    """
    p = prepared
    stats: Counter = Counter()
    call_of: dict[str, int] = {}
    for i, item in enumerate(p.items):
        if item[0] == K_CALL and item[3] and not item[6] & F_BASE:
            call_of.setdefault(item[3], i)
    atoms = p.session["atoms"]

    def targets(output_index: int) -> set[int]:
        call_id = p.items[output_index][3]
        call = call_of.get(call_id) if call_id else None
        if call is None:
            return set()
        return {a for a in p.item_atoms[call] if atoms[a].startswith("p:")}

    # Later model calls per request slot: (slot, atoms, output tokens of that call).
    output_tokens_by_call: dict[str, float] = {}
    for i, item in enumerate(p.items):
        if item[0] == K_OUTPUT and item[3] and not item[6] & F_BASE:
            output_tokens_by_call.setdefault(item[3], p.tokens[i])
    calls_by_slot: dict[int, list[tuple[set[int], float]]] = defaultdict(list)
    for i, item in enumerate(p.items):
        if item[0] == K_CALL and not item[6] & F_BASE and p.slot[i] >= 0:
            path_atoms = {a for a in p.item_atoms[i] if atoms[a].startswith("p:")}
            if path_atoms:
                calls_by_slot[p.slot[i]].append((path_atoms, output_tokens_by_call.get(item[3] or "", 0.0)))

    def classify(group: str, outputs: list[int], k: int) -> None:
        for o in outputs:
            wanted = targets(o)
            if not wanted:
                continue
            stats[f"{group}:outputs"] += 1
            for slot in range(k, min(len(p.requests), k + horizon)):
                hit = next((tokens for path_atoms, tokens in calls_by_slot.get(slot, ()) if path_atoms & wanted), None)
                if hit is not None:
                    stats[f"{group}:rerequested"] += 1
                    stats[f"{group}:reread_tokens"] += hit
                    stats[f"{group}:delay_requests"] += slot - k
                    break

    zero_calib = {"compaction": {"summary_latency": {"intercept_s": 0, "per_output_token_s": 0},
                                 "summary_out_tokens": [], "compaction_item_tokens_per_summary_token": 1.0},
                  "latency": {"context_slope_per_1k_s": 0.0, "ols": {"per_1k_uncached_s": 0.0}},
                  "cache": {"reuse_alpha": 1.0, "random_miss_by_gap": []}}
    eng = Engine(p, RecordedView(), zero_calib, 1e12, False)
    previous_end = 0
    for k, request in enumerate(p.requests):
        eng.k = k
        full_before = {key[1] for key, *_ in eng.entries if key[0] == "i" and key[2] == FULL and p.items[key[1]][0] == K_OUTPUT}
        eng.policy.on_items(eng, previous_end, request["item_end"])
        previous_end = request["item_end"]
        if k in p.relayouts:
            eng.relayout(p.relayouts[k])
        full_after = {key[1] for key, *_ in eng.entries if key[0] == "i" and key[2] == FULL and p.items[key[1]][0] == K_OUTPUT}
        hidden = sorted(full_before - full_after)
        if hidden and k > 0:
            kind = ("compaction" if any(previous_end > c["base_start"] >= p.requests[k - 1]["item_end"] for c in p.session["compactions"])
                    else "relayout" if k in p.relayouts else "retirement")
            stats[f"{kind}:events"] += 1
            classify(f"{kind}:hidden", hidden, k)
            control = [o for o in full_after if o < request["item_end"] and p.slot[o] + 1 <= k - KEPT_GENERATIONS]
            classify(f"{kind}:visible_old", control, k)
        eng.price_request(k)
    return stats


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------

WEIGHTINGS = {"w(0.1,8)": (1.0, 0.1, 8.0), "w(0.25,8)": (1.0, 0.25, 8.0), "w(0.1,4)": (1.0, 0.1, 4.0)}


def weighted(m: dict[str, float], weights: tuple[float, float, float]) -> float:
    uncached_w, cached_w, out_w = weights
    return uncached_w * m.get("uncached", 0) + cached_w * m.get("cached", 0) + out_w * (m.get("out", 0) + m.get("reasoning", 0))


def run(paths: list[Path], args: argparse.Namespace) -> int:
    started = time.monotonic()
    sessions = [s for s in (load_or_extract(p, args.cache_dir) for p in paths) if s]
    tm = fit_token_model(sessions)
    compaction = calibrate_compaction_and_pins(sessions, tm)
    latency = fit_latency(sessions)
    calib = {"tokens": {"rates": tm.rates, "pin_tokens": tm.pin_tokens, "fit": tm.fit},
             "compaction": compaction, "latency": latency}
    prepared = [Prepared(s, tm) for s in sessions]
    prepared = [p for p in prepared if p.requests]
    calib["cache"] = calibrate_cache(prepared, tm, args.ttl_seconds)
    if args.reuse_alpha is not None:
        calib["cache"]["reuse_alpha_calibrated"] = calib["cache"]["reuse_alpha"]
        calib["cache"]["reuse_alpha"] = args.reuse_alpha
    print(f"calibrated in {time.monotonic() - started:.0f}s: pin={tm.pin_tokens:.0f} tokens, "
          f"token-fit R2={tm.fit['r2']:.3f}, latency R2={latency['ols']['r2']:.3f}, "
          f"reuse_alpha={calib['cache']['reuse_alpha']:.3f}", file=sys.stderr)
    remaining = RemainingModel([len(p.requests) for p in prepared])
    accounting: Counter = Counter()
    accounting_long: Counter = Counter()
    for p in prepared:
        acc = recorded_accounting(p.session, tm, args.ttl_seconds)
        accounting.update(acc)
        if p.peak_input >= 150_000:
            accounting_long.update(acc)
    recovery_stats: Counter = Counter()
    for p in prepared:
        if p.peak_input >= 100_000:
            recovery_stats.update(recorded_rerequest_rates(p))

    selected = [p for p in prepared if p.peak_input >= args.min_peak]
    factories = policy_factories(remaining)
    results: dict[str, dict[str, Any]] = {}
    for recovery in (False, True):
        for factory in factories:
            probe = factory()
            if recovery and probe.name == "REC":
                continue
            total: Counter = Counter()
            total_long: Counter = Counter()
            per_session: dict[str, float] = {}
            for p in selected:
                policy = factory()
                eng = Engine(p, policy, calib, args.ttl_seconds, recovery)
                m = eng.run()
                peak = m.pop("peak", 0)
                if p.peak_input >= 150_000:
                    per_session[p.session["meta"]["name"]] = weighted(m, WEIGHTINGS["w(0.1,8)"])
                total.update(m)
                total["peak_max"] = max(total["peak_max"], peak)
                if p.peak_input >= 150_000:
                    total_long.update(m)
                    total_long["sessions"] += 1
                total["sessions"] += 1
            key = probe.name + ("+recovery" if recovery else "")
            results[key] = {"description": probe.description, "all": dict(total), "long": dict(total_long),
                            "per_session_long_w": per_session}
            for scope in ("all", "long"):
                metrics = results[key][scope]
                metrics.update({name: weighted(metrics, w) for name, w in WEIGHTINGS.items()})
        print(f"replayed {'with' if recovery else 'without'} recovery in {time.monotonic() - started:.0f}s", file=sys.stderr)

    recorded_totals: Counter = Counter()
    for p in selected:
        for r in p.session["requests"]:
            if not r["input"]:
                continue
            recorded_totals["input"] += r["input"]
            recorded_totals["cached"] += r["cached"]
            recorded_totals["uncached"] += r["input"] - r["cached"]
            recorded_totals["out"] += r["out"]
            recorded_totals["reasoning"] += r["reasoning"]
            recorded_totals["model_s"] += r["stream_wait_s"]
            recorded_totals["requests"] += 1
    recorded_totals.update({name: weighted(recorded_totals, w) for name, w in WEIGHTINGS.items()})

    rho = recovery_propensities(recovery_stats)
    attach_recovery_estimates(results, rho, calib)
    attach_session_distribution(results)
    report = {
        "recovery_propensity": rho,
        "corpus": {"rollouts": len(paths), "sessions_with_requests": len(prepared), "replayed": len(selected),
                   "long_sessions": sum(1 for p in selected if p.peak_input >= 150_000),
                   "until": args.until, "since": args.since, "ttl_seconds": args.ttl_seconds},
        "calibration": calib,
        "recorded_accounting": {"all": dict(accounting), "long": dict(accounting_long)},
        "recorded_totals_replayed_sessions": dict(recorded_totals),
        "recovery_events": dict(recovery_stats),
        "policies": results,
    }
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)
        alpha_tag = "calibrated" if args.reuse_alpha is None else f"alpha{args.reuse_alpha:g}"
        target = args.out / f"context_policy_replay_ttl{int(args.ttl_seconds)}_{alpha_tag}.json"
        target.write_text(json.dumps(report, indent=1, default=float), encoding="utf-8")
        print(f"wrote {target}", file=sys.stderr)
    print_summary(report)
    return 0


def recovery_propensities(stats: Counter) -> dict[str, float]:
    def rate(group: str) -> float:
        n = stats.get(f"{group}:outputs", 0)
        return stats.get(f"{group}:rerequested", 0) / n if n else 0.0

    def mean_tokens(group: str) -> float:
        n = stats.get(f"{group}:rerequested", 0)
        return stats.get(f"{group}:reread_tokens", 0) / n if n else 0.0

    control = rate("retirement:visible_old")
    return {
        "retirement_hidden_rate": rate("retirement:hidden"),
        "retirement_visible_old_rate": control,
        "pin_attributable": max(0.0, rate("retirement:hidden") - control),
        "compaction_hidden_rate": rate("compaction:hidden"),
        # No visible control survives a compaction; the retirement control stands in.
        "reset_attributable": max(0.0, rate("compaction:hidden") - control),
        "relayout_hidden_rate": rate("relayout:hidden"),
        "relayout_visible_old_rate": rate("relayout:visible_old"),
        "pin_reread_tokens": mean_tokens("retirement:hidden"),
        "reset_reread_tokens": mean_tokens("compaction:hidden"),
        "samples": {k: v for k, v in stats.items() if k.endswith(":outputs") or k.endswith(":events")},
    }


def policy_family(name: str) -> str:
    if name.startswith(("REMOTE", "TB")) or "norsn" in name:
        return "reset"
    return "pin"


def attach_recovery_estimates(results: dict[str, dict[str, Any]], rho: dict[str, float], calib: dict[str, Any]) -> None:
    """Mid-point recovery cost: hidden outputs beyond the recorded view,
    times the attributable re-request propensity of the policy's hiding
    style, each re-read paid uncached once and carried ~10 cached requests,
    plus half an extra request (bounds 0 and 1 are reported by +recovery)."""
    base_hidden = results.get("REC", {}).get("all", {}).get("hidden_outputs", 0)
    lat = calib["latency"]["ols"]
    for name, result in results.items():
        if name.endswith("+recovery") or name == "REC":
            continue
        family = policy_family(name)
        propensity = rho["reset_attributable"] if family == "reset" else rho["pin_attributable"]
        tokens = rho["reset_reread_tokens"] if family == "reset" else rho["pin_reread_tokens"]
        for scope in ("all", "long"):
            m = result[scope]
            excess = max(0.0, m.get("hidden_outputs", 0) - (base_hidden if scope == "all" else results["REC"]["long"].get("hidden_outputs", 0)))
            rereads = propensity * excess
            extra_requests = 0.5 * rereads
            w_extra = rereads * tokens * (1.0 + 0.1 * 10) + extra_requests * (8.0 * (RECOVERY_REQUEST_OUT + RECOVERY_REQUEST_REASONING))
            s_extra = extra_requests * (lat["intercept_s"] + RECOVERY_REQUEST_OUT * lat["per_out_token_s"]
                                        + RECOVERY_REQUEST_REASONING * lat["per_reasoning_token_s"]) + rereads * tokens / 1000 * max(0.0, lat["per_1k_uncached_s"])
            m["recovery_mid"] = {"family": family, "propensity": propensity, "excess_hidden_outputs": excess,
                                 "rereads": rereads, "extra_requests": extra_requests, "w_extra": w_extra, "model_s_extra": s_extra,
                                 "w_total": m["w(0.1,8)"] + w_extra, "model_s_total": m.get("model_s", 0) + s_extra}


def attach_session_distribution(results: dict[str, dict[str, Any]]) -> None:
    base = results.get("REC", {}).get("per_session_long_w", {})
    for result in results.values():
        sessions = result.get("per_session_long_w", {})
        ratios = sorted(sessions[n] / base[n] for n in sessions if base.get(n))
        if ratios:
            result["long_session_distribution"] = {
                "sessions": len(ratios), "share_cheaper_than_rec": sum(1 for r in ratios if r < 1.0) / len(ratios),
                "median_ratio": statistics.median(ratios), "p10_ratio": ratios[len(ratios) // 10],
                "p90_ratio": ratios[(9 * len(ratios)) // 10], "worst_ratio": ratios[-1]}


def print_summary(report: dict[str, Any]) -> None:
    base = report["policies"].get("CUR", {}).get("all", {})
    print(f"corpus: {report['corpus']}")
    print(f"{'policy':<42} {'reqs':>7} {'input M':>8} {'uncached M':>10} {'out k':>7} {'w(0.1,8) M':>10} {'vs CUR':>7} {'model h':>8} {'lost refs':>9}")
    rec = report["recorded_totals_replayed_sessions"]
    print(f"{'(recorded provider totals)':<42} {rec['requests']:>7.0f} {rec['input'] / 1e6:>8.1f} {rec['uncached'] / 1e6:>10.2f} "
          f"{(rec['out'] + rec['reasoning']) / 1e3:>7.0f} {rec['w(0.1,8)'] / 1e6:>10.2f} {'':>7} {rec['model_s'] / 3600:>8.1f}")
    for name, result in report["policies"].items():
        m = result["all"]
        rel = (m["w(0.1,8)"] / base["w(0.1,8)"] - 1) * 100 if base else 0.0
        print(f"{name:<42} {m.get('requests', 0):>7.0f} {m.get('input', 0) / 1e6:>8.1f} {m.get('uncached', 0) / 1e6:>10.2f} "
              f"{(m.get('out', 0) + m.get('reasoning', 0)) / 1e3:>7.0f} {m['w(0.1,8)'] / 1e6:>10.2f} {rel:>+6.1f}% "
              f"{m.get('model_s', 0) / 3600:>8.1f} {m.get('lost_refs', 0):>9.0f}")


def _rel(value: float, base: float) -> str:
    return f"{(value / base - 1) * 100:+.1f}%" if base else "n/a"


def report_tables(paths: list[Path], policies: list[str] | None) -> int:
    """Markdown tables from result JSON files: per-run policy table (costs
    relative to the same run's REC, without recovery / mid estimate / high
    bound), then a cross-run sensitivity table."""
    runs = [(path.stem.replace("context_policy_replay_", ""), json.loads(path.read_text(encoding="utf-8"))) for path in paths]
    for tag, rep in runs:
        pol = rep["policies"]
        rec = pol["REC"]["all"]
        names = policies or [n for n in pol if not n.endswith("+recovery")]
        print(f"\n#### {tag}\n")
        print("| policy | w(0.1,8) M | vs REC | mid | high | w(0.25,8) vs REC | w(0.1,4) vs REC "
              "| model h none/mid/high | requests | lost refs | long: cheaper / median / p90 / worst |")
        print("|---|---:|---:|---:|---:|---:|---:|---|---:|---:|---|")
        for name in names:
            if name not in pol:
                continue
            m = pol[name]["all"]
            high = pol.get(name + "+recovery", {}).get("all")
            mid = m.get("recovery_mid")
            dist = pol[name].get("long_session_distribution")
            hours = [m.get("model_s", 0) / 3600, (mid["model_s_total"] / 3600) if mid else None,
                     (high["model_s"] / 3600) if high else None]
            print(f"| {name} | {m['w(0.1,8)'] / 1e6:.1f} | {_rel(m['w(0.1,8)'], rec['w(0.1,8)'])} "
                  f"| {_rel(mid['w_total'], rec['w(0.1,8)']) if mid else 'n/a'} "
                  f"| {_rel(high['w(0.1,8)'], rec['w(0.1,8)']) if high else 'n/a'} "
                  f"| {_rel(m['w(0.25,8)'], rec['w(0.25,8)'])} | {_rel(m['w(0.1,4)'], rec['w(0.1,4)'])} "
                  f"| {' / '.join('n/a' if h is None else f'{h:.1f}' for h in hours)} "
                  f"| {m.get('requests', 0):.0f} | {m.get('lost_refs', 0):.0f} "
                  f"| {'n/a' if not dist else f'{dist['share_cheaper_than_rec']:.0%} / {dist['median_ratio']:.2f} / {dist['p90_ratio']:.2f} / {dist['worst_ratio']:.2f}'} |")
    if len(runs) > 1:
        names = policies or [n for n in runs[0][1]["policies"] if not n.endswith("+recovery")]
        print("\n#### sensitivity: w(0.1,8) vs the same run's REC (no recovery / high bound)\n")
        print("| policy | " + " | ".join(tag for tag, _ in runs) + " |")
        print("|---|" + "---:|" * len(runs))
        for name in names:
            cells = []
            for _, rep in runs:
                pol = rep["policies"]
                if name not in pol:
                    cells.append("—")
                    continue
                base = pol["REC"]["all"]["w(0.1,8)"]
                high = pol.get(name + "+recovery", {}).get("all")
                cells.append(_rel(pol[name]["all"]["w(0.1,8)"], base)
                             + (f" / {_rel(high['w(0.1,8)'], base)}" if high else ""))
            print(f"| {name} | " + " | ".join(cells) + " |")
    return 0


def _extract_worker(args: tuple[str, str]) -> tuple[str, bool]:
    path, cache_dir = args
    session = load_or_extract(Path(path), Path(cache_dir))
    return path, session is not None


def extract_all(paths: list[Path], cache_dir: Path, workers: int) -> None:
    pending = [p for p in paths if not cache_path(cache_dir, p).exists()]
    if not pending:
        return
    started = time.monotonic()
    if workers <= 1:
        for path in pending:
            load_or_extract(path, cache_dir)
    else:
        from concurrent.futures import ProcessPoolExecutor
        with ProcessPoolExecutor(max_workers=workers) as pool:
            for _ in pool.map(_extract_worker, [(str(p), str(cache_dir)) for p in pending], chunksize=1):
                pass
    print(f"extracted {len(pending)} sessions in {time.monotonic() - started:.0f}s", file=sys.stderr)


def default_cache_dir() -> Path:
    return Path(tempfile.gettempdir()) / "kd4-context-policy-replay"


def _cli() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=["extract", "run", "report"])
    parser.add_argument("--results", type=Path, nargs="+", help="result JSON files (report)")
    parser.add_argument("--policies", nargs="+", help="policy names to tabulate (report; default all)")
    parser.add_argument("--root", type=Path, default=SESSION_ROOT)
    parser.add_argument("--until", help="exclude rollouts whose name stamp is >= this (YYYY-MM-DDTHH-MM-SS)")
    parser.add_argument("--since", help="exclude rollouts whose name stamp is < this")
    parser.add_argument("--cache-dir", type=Path, default=default_cache_dir())
    parser.add_argument("--workers", type=int, default=6)
    parser.add_argument("--out", type=Path, help="directory for JSON results (run)")
    parser.add_argument("--ttl-seconds", type=float, default=3600.0)
    parser.add_argument("--min-peak", type=int, default=0,
                        help="replay only sessions whose recorded peak input reaches this many tokens")
    parser.add_argument("--reuse-alpha", type=float,
                        help="override the calibrated post-rewrite prefix reuse fraction (1.0 = ideal prefix cache)")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _cli().parse_args(argv)
    if args.command == "report":
        if not args.results:
            print("report needs --results", file=sys.stderr)
            return 2
        return report_tables(args.results, args.policies)
    paths = select_paths(args.root, args.until, args.since)
    if not paths:
        print("no rollouts selected", file=sys.stderr)
        return 2
    extract_all(paths, args.cache_dir, args.workers)
    if args.command == "extract":
        print(f"{len(paths)} sessions cached under {args.cache_dir}")
        return 0
    return run(paths, args)


if __name__ == "__main__":
    raise SystemExit(main())
