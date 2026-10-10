"""Render rollout summaries, diagnostics, narrative views, and snapshot reports.

The summary, diagnostics, narrative, and default dump views are intentionally
lossy. Use dump --complete OUTDIR ROLLOUT... for a complete plaintext snapshot
report. Reports retain every JSON field except opaque encrypted_content strings,
recorded by UTF-8 length and SHA-256 instead. Exact duplicate subtrees become null
with an entry in references: RFC 6901 pointers into records map to earlier source
nodes. Resolve references before treating a placeholder as a source null. Counts
prove report construction, not that a model has read the report. Read retained
reports in bounded batches rather than regenerating them for each page.

Narrative accepts ROLLOUT@INDEX to start at a zero-based record index and honors
NARR_OUT_LIMIT (default 4000 characters) for bounded tool outputs.
"""

import argparse
import collections
import contextlib
import datetime
import hashlib
import io
import json
import os
import tempfile
from pathlib import Path

try:
    from scripts.atomic_json import write_json_atomic, write_stream_atomic
    from scripts.kd4_timing_analysis import analyze_timing, timing_profile_valid
    from scripts.rollout_snapshot import (
        existing_rollout_path,
        hydrate_rollout_record,
        read_rollout_records,
        read_rollout_snapshot,
    )
except ImportError:
    from atomic_json import write_json_atomic, write_stream_atomic
    from kd4_timing_analysis import analyze_timing, timing_profile_valid
    from rollout_snapshot import (
        existing_rollout_path,
        hydrate_rollout_record,
        read_rollout_records,
        read_rollout_snapshot,
    )

OUT_LIMIT = 4000


def dump_complete(path, out_path):
    """Build one deduplicated report from one immutable snapshot; never truncate."""
    snapshot = read_rollout_snapshot(Path(path))
    with contextlib.closing(snapshot.stream):
        output = Path(out_path)
        if output.resolve() == snapshot.path or (
            output.exists() and output.samefile(snapshot.path)
        ):
            raise ValueError("report output must not overwrite the source rollout")
        seen = {}
        references = {}
        opaque = {}

        def render(value, pointer, key=None):
            if key == "encrypted_content" and isinstance(value, str):
                raw = value.encode("utf-8")
                opaque[pointer] = {
                    "utf8Bytes": len(raw),
                    "sha256": hashlib.sha256(raw).hexdigest(),
                }
                return None
            encoded = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
            if len(encoded) >= 256:
                if encoded in seen:
                    references[pointer] = seen[encoded]
                    return None
                seen[encoded] = pointer
            if isinstance(value, dict):
                return {
                    name: render(
                        child,
                        pointer + "/" + name.replace("~", "~0").replace("/", "~1"),
                        name,
                    )
                    for name, child in value.items()
                }
            if isinstance(value, list):
                return [
                    render(child, f"{pointer}/{index}")
                    for index, child in enumerate(value)
                ]
            return value

        records = []
        with snapshot.open_lines() as lines:
            for number, line in enumerate(lines, 1):
                try:
                    row = json.loads(line.decode("utf-8"))
                except (json.JSONDecodeError, UnicodeDecodeError) as error:
                    raise ValueError(
                        f"incomplete or invalid rollout record {snapshot.path}:{number}"
                    ) from error
                if not isinstance(row, dict):
                    raise ValueError(  # noqa: TRY004 - retain the rollout reader error contract.
                        f"rollout record {snapshot.path}:{number} is not an object"
                    )
                records.append(
                    render(hydrate_rollout_record(row, snapshot.path), f"/{number - 1}")
                )
        write_json_atomic(
            output,
            {
                "format": "codex-rollout-plaintext-v1",
                "snapshot": snapshot.metadata(),
                "recordCount": len(records),
                "references": references,
                "opaqueEncryptedFields": opaque,
                "records": records,
            },
        )


def ts(o, *, strict=False):
    t = o.get("timestamp")
    if not t:
        return None
    try:
        return datetime.datetime.fromisoformat(t.replace("Z", "+00:00"))
    except Exception:
        if strict:
            raise
        return None


def jd(v, indent=None):
    return json.dumps(v, indent=indent, ensure_ascii=False, default=str)


def text_of(content):
    out = []
    if isinstance(content, list):
        for c in content:
            if isinstance(c, dict):
                if "text" in c:
                    out.append(str(c.get("text")))
                else:
                    out.append(jd(c))
            else:
                out.append(str(c))
    elif content is not None:
        out.append(str(content))
    return "\n".join(out)


@contextlib.contextmanager
def _report_writer(path, out_path):
    """Render completely before publication, with bounded in-memory staging."""
    source = existing_rollout_path(Path(path)).resolve()
    output = Path(out_path)
    if output.resolve() == source or (output.exists() and output.samefile(source)):
        raise ValueError("report output must not overwrite the source rollout")
    with tempfile.SpooledTemporaryFile(max_size=4 * 1024 * 1024) as raw:
        with io.TextIOWrapper(raw, encoding="utf-8") as writer:
            yield writer
            writer.flush()
            raw.seek(0)
            write_stream_atomic(output, raw)


def dump(path, out_path):
    rows = [row for row, _ in read_rollout_records(Path(path))]
    with _report_writer(path, out_path) as w:
        def P(*a):
            print(*a, file=w)

        prev_t = None
        seen_manifests = set()
        cache_keys = {}
        prev_settings = None
        prev_turn_ctx = None
        for i, o in enumerate(rows):
            t = ts(o)
            gap = (t - prev_t).total_seconds() if (t and prev_t) else 0.0
            prev_t = t or prev_t
            typ = o.get("type")
            p = o.get("payload", {})
            sub = p.get("type") if isinstance(p, dict) else None
            hdr = (
                f"#### [{i}] {t.strftime('%H:%M:%S.%f')[:-3] if t else '?'} (+{gap:.1f}s) {typ}"
                + (f"/{sub}" if sub else "")
            )
            if typ == "session_meta":
                P(hdr)
                for k, v in p.items():
                    if k == "base_instructions":
                        P(f"--- base_instructions ({len(v)} chars) ---")
                        P(v)
                        P("--- end base_instructions ---")
                    else:
                        P(f"  {k}: {jd(v)}")
            elif typ == "turn_context":
                P(hdr)
                if prev_turn_ctx is None:
                    P(jd(p, 1))
                else:
                    for k, v in p.items():
                        if prev_turn_ctx.get(k) != v:
                            P(f"  CHANGED {k}: {jd(v)}")
                prev_turn_ctx = p
            elif typ == "tool_manifest":
                P(hdr + f" hash={p.get('hash')} base={p.get('base_hash')}")
                m = p.get("manifest")
                manifest_id = p.get("hash") or jd(m)
                if m and manifest_id not in seen_manifests:
                    seen_manifests.add(manifest_id)
                    mv = m.get("model_visible") or []
                    P(f"--- model_visible tools ({len(mv)}) ---")
                    for tdef in mv:
                        P(
                            f"## TOOL {tdef.get('name')} type={tdef.get('type')} format={jd(tdef.get('format'))}"
                        )
                        P(str(tdef.get("description")))
                        for k, v in tdef.items():
                            if k not in ("name", "type", "format", "description"):
                                P(f"   {k}: {jd(v)}")
                    reg = m.get("registered") or []
                    P(f"--- registered ({len(reg)}) ---")
                    for r in reg:
                        P(f"   {jd(r)}")
                elif m:
                    P(f"  (manifest already shown, {len(jd(m))} chars)")
                for k in ("added", "removed"):
                    if p.get(k):
                        P(f"  {k}: {jd(p.get(k))}")
                for k, v in p.items():
                    if k not in ("hash", "base_hash", "manifest", "added", "removed"):
                        P(f"  {k}: {jd(v)}")
            elif typ == "world_state":
                P(hdr + f" full={p.get('full')}")
                st = p.get("state", {})
                for k, v in st.items():
                    P(f"--- world_state.{k} ---")
                    P(jd(v, 1) if not isinstance(v, str) else v)
                for k, v in p.items():
                    if k not in ("full", "state"):
                        P(f"  {k}: {jd(v)[:2000]}")
            elif typ == "compacted":
                P(hdr)
                for k, v in p.items():
                    if k == "replacement_history":
                        P(
                            f"--- replacement_history ({len(v) if isinstance(v, list) else '?'} items) ---"
                        )
                        if isinstance(v, list):
                            for it in v:
                                if isinstance(it, dict) and it.get("type") == "message":
                                    P(f"[{it.get('role')}]")
                                    P(text_of(it.get("content")))
                                else:
                                    P(jd(it, 1))
                        else:
                            P(jd(v, 1))
                    else:
                        P(f"  {k}: {jd(v)}")
            elif typ == "sampling_boundary":
                P(hdr + " " + jd(p)[:600])
            elif typ == "response_item":
                if sub == "message":
                    role = p.get("role")
                    P(hdr + f" role={role}")
                    P(text_of(p.get("content")))
                    for k, v in p.items():
                        if k not in ("type", "role", "content", "id", "status"):
                            P(f"  {k}: {jd(v)[:500]}")
                elif sub == "reasoning":
                    summ = p.get("summary") or []
                    txt = "\n".join(
                        s.get("text", "") for s in summ if isinstance(s, dict)
                    )
                    P(
                        hdr
                        + f" id={p.get('id')} enc_len={len(p.get('encrypted_content') or '')}"
                    )
                    if txt:
                        P("<<reasoning summary>>")
                        P(txt)
                    cont = p.get("content")
                    if cont:
                        P("<<reasoning content>>")
                        P(text_of(cont))
                elif sub in ("custom_tool_call", "function_call"):
                    args = (
                        p.get("input")
                        if sub == "custom_tool_call"
                        else p.get("arguments")
                    )
                    P(hdr + f" name={p.get('name')} call_id={p.get('call_id')}")
                    P(">>> INPUT:")
                    P(str(args))
                elif sub in ("custom_tool_call_output", "function_call_output"):
                    out = p.get("output")
                    if isinstance(out, (dict, list)):
                        outs = jd(out, 1)
                    else:
                        outs = str(out)
                    P(hdr + f" call_id={p.get('call_id')} outlen={len(outs)}")
                    P("<<< OUTPUT:")
                    P(outs)
                else:
                    P(hdr)
                    P(jd(p, 1))
            elif typ == "event_msg":
                if sub == "token_count":
                    info = p.get("info") or {}
                    tu = info.get("last_token_usage") or {}
                    tot = info.get("total_token_usage") or {}
                    P(
                        hdr
                        + f" last={jd(tu)} total_in={tot.get('input_tokens')} total_out={tot.get('output_tokens')} ctx={info.get('model_context_window')} rate_limits={jd(p.get('rate_limits'))[:300]}"
                    )
                elif sub in ("task_complete", "turn_aborted"):
                    P(hdr)
                    for k, v in p.items():
                        if k == "timing" and isinstance(v, dict):
                            P("  timing summary:")
                            for tk in (
                                "exclusive",
                                "unions",
                                "local",
                                "milestones",
                                "terminalization",
                                "toolClosure",
                                "observationalNonprogressTokens",
                                "observationalNonprogressLatency",
                                "toolCallTimingOverflow",
                                "deterministicContinuationReceiptOverflow",
                                "inclusiveDurationMs",
                                "machineDurationMs",
                                "profileValid",
                                "classificationComplete",
                            ):
                                if tk in v:
                                    P(f"    {tk}: {jd(v[tk])}")
                            c = v.get("counters") or {}
                            P("    counters:")
                            for ck, cv in c.items():
                                P(f"      {ck}: {jd(cv)[:3000]}")
                            mr = v.get("modelRequests") or []
                            P(f"    modelRequests ({len(mr)}):")
                            for r in mr:
                                keep = {
                                    k2: r.get(k2)
                                    for k2 in (
                                        "generationIndex",
                                        "generationReason",
                                        "generationPurpose",
                                        "disposition",
                                        "attemptKind",
                                        "isContinuation",
                                        "modelStreamWaitNs",
                                        "decisionLatencyNs",
                                        "toolCallCount",
                                        "modelEmittedToolCallCount",
                                        "outputTokens",
                                        "reasoningOutputTokens",
                                        "tokenUsage",
                                        "requestTokenCategories",
                                        "fixedPrefixReuseEligible",
                                        "nextStructuredActionChanged",
                                        "unchangedRelevantState",
                                        "physicalAttemptIds",
                                        "retryCount",
                                        "errorKind",
                                        "error",
                                        "status",
                                    )
                                }
                                fingerprint = r.get("promptCacheKeyFingerprint")
                                if isinstance(fingerprint, str):
                                    if fingerprint not in cache_keys:
                                        cache_keys[fingerprint] = (
                                            f"cache-key-{len(cache_keys) + 1}"
                                        )
                                        P(
                                            f"      {cache_keys[fingerprint]} fingerprint={fingerprint}"
                                        )
                                    keep["promptCacheKey"] = cache_keys[fingerprint]
                                else:
                                    keep["promptCacheKey"] = None
                                other = {
                                    k2: r.get(k2)
                                    for k2 in r
                                    if k2 not in keep
                                    and k2
                                    not in (
                                        "relevantStateFingerprint",
                                        "samplingRequestId",
                                        "promptCacheKeyFingerprint",
                                    )
                                }
                                P(f"      {jd(keep)}  other={jd(other)[:1500]}")
                            tc = v.get("toolCalls") or []
                            P(f"    toolCalls ({len(tc)}):")
                            for r in tc:
                                keep = {
                                    k2: r.get(k2)
                                    for k2 in (
                                        "callId",
                                        "parentCallId",
                                        "toolName",
                                        "source",
                                        "outcome",
                                        "generationIndex",
                                        "acceptedAtMs",
                                        "handlerDurationMs",
                                        "totalDurationMs",
                                        "parallelGateWaitMs",
                                        "workspaceEvidenceBeforeMs",
                                        "eager",
                                        "backgroundProcessExpected",
                                        "outputProjectionMs",
                                    )
                                }
                                P(f"      {jd(keep)}")
                            for tk, tv in v.items():
                                if tk not in (
                                    "exclusive",
                                    "unions",
                                    "local",
                                    "milestones",
                                    "terminalization",
                                    "toolClosure",
                                    "observationalNonprogressTokens",
                                    "observationalNonprogressLatency",
                                    "counters",
                                    "modelRequests",
                                    "toolCalls",
                                    "toolCallTimingOverflow",
                                    "deterministicContinuationReceiptOverflow",
                                    "inclusiveDurationMs",
                                    "machineDurationMs",
                                    "profileValid",
                                    "classificationComplete",
                                    "schemaVersion",
                                    "startedAtUnixMs",
                                    "completedAtUnixMs",
                                    "inclusiveDurationNs",
                                    "machineDurationNs",
                                ):
                                    P(f"    {tk}: {jd(tv)[:3000]}")
                        else:
                            P(f"  {k}: {jd(v) if not isinstance(v, str) else v}")
                elif sub == "thread_settings_applied":
                    s = p.get("thread_settings")
                    if prev_settings is None:
                        P(hdr)
                        P(jd(s, 1))
                    elif s != prev_settings:
                        P(hdr + " (changed)")
                        if isinstance(s, dict) and isinstance(prev_settings, dict):
                            for k in set(list(s.keys()) + list(prev_settings.keys())):
                                if s.get(k) != prev_settings.get(k):
                                    P(f"  CHANGED {k}:")
                                    P(f"    old: {jd(prev_settings.get(k))}")
                                    P(f"    new: {jd(s.get(k))}")
                        else:
                            P(jd(s, 1))
                    else:
                        P(hdr + " (unchanged)")
                    prev_settings = s
                elif sub == "agent_reasoning":
                    P(
                        hdr
                        + f" len={len(p.get('text') or '')} (see response_item reasoning)"
                    )
                elif sub == "agent_message":
                    P(
                        hdr
                        + f" len={len(p.get('message') or '')} (see response_item message)"
                    )
                elif sub == "user_message":
                    P(hdr)
                    P(str(p.get("message")))
                    for k, v in p.items():
                        if k not in ("type", "message"):
                            P(f"  {k}: {jd(v)[:2000]}")
                else:
                    P(hdr)
                    P(jd({k: v for k, v in p.items() if k != "type"}, 1))
            else:
                P(hdr)
                P(jd(p, 1))


def short(s, n=160):
    s = str(s).replace("\n", "\\n")
    return s if len(s) <= n else s[:n] + f"...(+{len(s) - n})"


def summarize(path, verbose=False):
    rows = [row for row, _ in read_rollout_records(Path(path))]
    print("=" * 100)
    print(path.split("\\")[-1])
    if not rows:
        print("no complete records")
        return
    first = rows[0]
    meta = first.get("payload", {}) if first.get("type") == "session_meta" else {}
    print("session_meta keys:", list(meta.keys())[:30])
    for k in (
        "id",
        "cwd",
        "originator",
        "cli_version",
        "model",
        "model_provider",
        "source",
        "timestamp",
    ):
        if k in meta:
            print(f"  {k}: {short(meta[k])}")
    if "instructions" in meta:
        print("  instructions len:", len(meta["instructions"] or ""))
    t0 = ts(rows[0])
    tN = ts(rows[-1])
    print("start", t0, "end", tN, "wall", (tN - t0) if t0 and tN else None)

    # Timeline with gaps
    prev_t = None
    prev_desc = None
    gaps = []
    tool_starts = {}
    tokens_series = []
    n_sampling = 0
    ncalls = collections.Counter()
    tool_durations = []
    out_sizes = []
    timeline = []
    for i, o in enumerate(rows):
        t = ts(o)
        typ = o.get("type")
        p = o.get("payload", {})
        sub = p.get("type") if isinstance(p, dict) else None
        desc = f"{typ}/{sub}"
        extra = ""
        if typ == "response_item":
            if sub in ("custom_tool_call", "function_call"):
                name = p.get("name")
                args = (
                    p.get("input") if sub == "custom_tool_call" else p.get("arguments")
                )
                cid = p.get("call_id")
                tool_starts[cid] = (t, name, i)
                ncalls[name] += 1
                extra = f" {name} call_id={cid} args={short(args, 300)}"
            elif sub in ("custom_tool_call_output", "function_call_output"):
                cid = p.get("call_id")
                out = p.get("output")
                if isinstance(out, dict):
                    outs = json.dumps(out)
                else:
                    outs = str(out)
                out_sizes.append(len(outs))
                st = tool_starts.get(cid)
                dur = None
                if st and st[0] and t:
                    dur = (t - st[0]).total_seconds()
                    tool_durations.append((dur, st[1], i))
                extra = f" call_id={cid} dur={dur} outlen={len(outs)} out={short(outs, 300)}"
            elif sub == "message":
                role = p.get("role")
                content = p.get("content")
                txt = ""
                if isinstance(content, list):
                    for c in content:
                        if isinstance(c, dict):
                            txt += c.get("text", "") or ""
                extra = f" role={role} len={len(txt)} text={short(txt, 400)}"
            elif sub == "reasoning":
                summ = p.get("summary") or []
                txt = ""
                for s in summ:
                    if isinstance(s, dict):
                        txt += s.get("text", "") or ""
                enc = p.get("encrypted_content")
                extra = f" summary_len={len(txt)} enc_len={len(enc) if enc else 0} summ={short(txt, 200)}"
        elif typ == "event_msg":
            if sub == "token_count":
                info = p.get("info") or {}
                tu = info.get("last_token_usage") or {}
                tot = info.get("total_token_usage") or {}
                tokens_series.append((t, tu, tot, info.get("model_context_window")))
                extra = f" last={tu} total_in={tot.get('input_tokens')} total_out={tot.get('output_tokens')} ctx_window={info.get('model_context_window')}"
            elif sub in ("user_message", "agent_message"):
                extra = f" msg={short(p.get('message'), 500)}"
            elif sub == "warning":
                extra = f" {short(p, 500)}"
            elif sub in (
                "task_started",
                "task_complete",
                "turn_aborted",
                "error",
                "stream_error",
                "background_event",
                "context_compacted",
                "compaction",
                "thread_settings_applied",
            ):
                extra = f" {short({k: v for k, v in p.items() if k != 'type'}, 400)}"
            elif sub == "agent_reasoning":
                extra = f" {short(p.get('text'), 200)}"
            elif sub == "item_completed":
                extra = f" {short(p, 200)}"
            else:
                extra = f" {short(p, 200)}"
        elif typ == "sampling_boundary":
            n_sampling += 1
            extra = f" {short(p, 200)}"
        elif typ == "turn_context":
            extra = f" {short({k: v for k, v in p.items() if k not in ('instructions',)}, 600)}"
        elif typ == "tool_manifest":
            extra = f" n_tools={len(p.get('tools', [])) if isinstance(p, dict) else '?'} keys={list(p.keys())[:8] if isinstance(p, dict) else ''}"
        elif typ == "world_state":
            extra = f" {short(p, 300)}"
        elif typ == "compacted":
            extra = f" {short(p, 400)}"
        gap = (t - prev_t).total_seconds() if (t and prev_t) else None
        if gap is not None:
            gaps.append((gap, i, prev_desc, desc))
        timeline.append(
            (
                i,
                t.strftime("%H:%M:%S") if t else "?",
                f"{gap:8.1f}" if gap is not None else "        ",
                desc,
                extra,
            )
        )
        prev_t = t if t else prev_t
        prev_desc = desc
    print("samplings:", n_sampling, "tool calls:", dict(ncalls))
    if tokens_series:
        last = tokens_series[-1]
        print("final total usage:", last[2])
        print("ctx window:", last[3])
        print("per-sampling input tokens (last_token_usage):")
        for t, tu, tot, cw in tokens_series:
            print(
                f"   {t.strftime('%H:%M:%S') if t else '?'} in={tu.get('input_tokens')} cached={tu.get('cached_input_tokens')} out={tu.get('output_tokens')} reason={tu.get('reasoning_output_tokens')} tot={tu.get('total_tokens')}"
            )
    print("top gaps (sec, idx, prev -> cur):")
    for g in sorted(gaps, reverse=True)[:15]:
        print(f"   {g[0]:8.1f}  idx={g[1]}  {g[2]} -> {g[3]}")
    print("top tool durations:")
    for d in sorted(tool_durations, reverse=True)[:10]:
        print(f"   {d[0]:8.1f}  {d[1]} idx={d[2]}")
    print(f"sum tool durations: {sum(d[0] for d in tool_durations):.1f}")
    print("largest outputs:", sorted(out_sizes, reverse=True)[:10])
    if verbose:
        print("--- timeline ---")
        for row in timeline:
            print(f"{row[0]:4d} {row[1]} {row[2]} {row[3]:<42}{row[4]}")


def h(s):
    return hashlib.sha1(
        json.dumps(s, sort_keys=True, default=str).encode()
    ).hexdigest()[:10]


def merge_patch(target, patch):
    # Match the runtime's recursive JSON merge-patch semantics, including deletion.
    if not isinstance(patch, dict):
        return patch
    target = dict(target) if isinstance(target, dict) else {}
    for key, value in patch.items():
        if value is None:
            target.pop(key, None)
        else:
            target[key] = merge_patch(target.get(key), value)
    return target


def seconds(value):
    return value / 1_000_000_000 if type(value) is int and value >= 0 else None


def number(value, duration=False):
    if value is None:
        return "?"
    return f"{value:.1f}s" if duration else f"{value:,}"


def analyze(path):
    records = read_rollout_records(Path(path))
    rows = [row for row, _ in records]
    print("=" * 100)
    print(Path(path).name)
    # 1. bytes by type
    by = collections.Counter()
    for o, byte_length in records:
        p = o.get("payload", {})
        sub = p.get("type") if isinstance(p, dict) else None
        by[(o.get("type"), sub)] += byte_length
    print("bytes by type:")
    for k, v in sorted(by.items(), key=lambda x: -x[1])[:12]:
        print(f"   {v:>10,} {k}")
    # 2. world_state / tool_manifest / turn_context changes
    print("world_state entries:")
    prev = None
    state = {}
    for i, o in enumerate(rows):
        if o.get("type") == "world_state":
            p = o["payload"]
            patch = p.get("state", {})
            state = patch if p.get("full") else merge_patch(state, patch)
            keys = (
                {k: h(v) for k, v in state.items()} if isinstance(state, dict) else {}
            )
            changed = [
                k
                for k in sorted(keys.keys() | (prev or {}).keys())
                if prev is None or prev.get(k) != keys.get(k)
            ]
            print(
                f"   idx={i} full={p.get('full')} keys={list(keys)} changed={changed} otherkeys={[k for k in p if k not in ('full', 'state')]}"
            )
            prev = keys
    print("tool_manifest entries:")
    for i, o in enumerate(rows):
        if o.get("type") == "tool_manifest":
            p = o["payload"]
            print(
                f"   idx={i} hash={str(p.get('hash'))[:16]} base={str(p.get('base_hash'))[:16]} added={[a.get('name') if isinstance(a, dict) else a for a in (p.get('added') or [])][:10]} removed={p.get('removed')} manifest_len={len(json.dumps(p.get('manifest'))) if p.get('manifest') else 0}"
            )
    print("turn_context entries:")
    prev = None
    for i, o in enumerate(rows):
        if o.get("type") == "turn_context":
            p = dict(o["payload"])
            keys = {k: h(v) for k, v in p.items()}
            changed = [
                k
                for k in sorted(keys.keys() | (prev or {}).keys())
                if prev is None or prev.get(k) != keys.get(k)
            ]
            print(f"   idx={i} changed={changed}")
            prev = keys
    # 3. reasoning ids
    ids = collections.Counter()
    for o in rows:
        if o.get("type") == "response_item" and o["payload"].get("type") == "reasoning":
            ids[o["payload"].get("id")] += 1
    dup = {k: v for k, v in ids.items() if v > 1}
    print(
        "reasoning items:",
        sum(ids.values()),
        "distinct ids:",
        len(ids),
        "dups:",
        len(dup),
        list(dup.items())[:5],
    )
    # 4. per-turn timing
    print("per-turn timing:")
    turn_start = None
    turn_i = None
    n_samp = 0
    user_msg = ""
    for i, o in enumerate(rows):
        t = ts(o, strict=True)
        typ = o.get("type")
        p = o.get("payload", {})
        sub = p.get("type") if isinstance(p, dict) else None
        if typ == "event_msg" and sub == "task_started":
            turn_start = t
            turn_i = i
            n_samp = 0
            user_msg = ""
        elif typ == "event_msg" and sub == "user_message":
            user_msg = (p.get("message") or "")[:60].replace("\n", " ")
        elif typ == "sampling_boundary":
            n_samp += 1
        elif typ == "event_msg" and sub in ("task_complete", "turn_aborted"):
            wall = (t - turn_start).total_seconds() if turn_start and t else None
            timing = p.get("timing")
            report = (
                analyze_timing(timing, status=sub)
                if timing_profile_valid(timing)
                else None
            )
            unions = timing.get("unions", {}) if report else {}
            counters = timing.get("counters", {}) if report else {}
            if report and seconds(timing.get("inclusiveDurationNs")) is not None:
                wall = seconds(timing["inclusiveDurationNs"])
            tokens = report["tokens"] if report else {}
            retention = report["requestRetention"]["complete"] if report else None
            totals = tokens.get("providerTotals") if retention is True else None
            usage = (
                "complete"
                if totals is not None
                else "partial"
                if tokens.get("available")
                else "unavailable"
            )
            observed = tokens.get("observedTotals", {}) if usage == "partial" else {}
            totals = totals or {}
            # Event gaps overlap and token-count events are snapshots, not a ledger.
            # Missing native evidence must remain unknown rather than a plausible zero.
            print(
                f"   turn@{turn_i} {sub:13} wall={number(wall, True)} boundaries={n_samp} "
                f"generations={number(counters.get('logicalGenerationCount'))} "
                f"model_active={number(seconds(unions.get('modelActiveUnionNs')), True)} "
                f"tool_active={number(seconds(unions.get('toolActiveUnionNs')), True)} "
                f"interactive_wait={number(seconds(unions.get('interactiveWaitUnionNs')), True)} "
                f"in={number(totals.get('inputTokens'))} cached={number(totals.get('cachedInputTokens'))} "
                f"uncached={number(totals.get('nonCachedInputTokens'))} out={number(totals.get('outputTokens'))} "
                f"usage={usage} request_retention={retention} "
                + (
                    f"observed_in={number(observed.get('inputTokens'))} observed_cached={number(observed.get('cachedInputTokens'))} "
                    f"observed_out={number(observed.get('outputTokens'))} "
                    if observed
                    else ""
                )
                + f'user="{user_msg}"'
            )
            turn_start = None
            turn_i = None
            n_samp = 0
            user_msg = ""


def bounded(s, limit=OUT_LIMIT):
    if limit < 0:
        raise ValueError("output limit must be nonnegative")
    if len(s) <= limit:
        return s
    head_length = limit * 3 // 4
    tail_length = limit - head_length
    head = s[:head_length]
    tail = s[-tail_length:] if tail_length else ""
    return f"{head}\n...[{len(s) - limit} chars omitted]...\n{tail}"


def dump_narrative(path, out_path, start_idx=0):
    output_limit = int(os.environ.get("NARR_OUT_LIMIT", OUT_LIMIT))
    if output_limit < 0:
        raise ValueError("NARR_OUT_LIMIT must be nonnegative")
    rows = [row for row, _ in read_rollout_records(Path(path))]
    with _report_writer(path, out_path) as w:
        def P(*a):
            print(*a, file=w)

        prev_t = None
        for i, o in enumerate(rows):
            t = ts(o)
            gap = (t - prev_t).total_seconds() if (t and prev_t) else 0.0
            prev_t = t or prev_t
            if i < start_idx:
                continue
            typ = o.get("type")
            p = o.get("payload", {})
            sub = p.get("type") if isinstance(p, dict) else None
            hdr = (
                f"#### [{i}] {t.strftime('%H:%M:%S') if t else '?'} (+{gap:.1f}s) {typ}"
                + (f"/{sub}" if sub else "")
            )
            if typ in ("session_meta", "tool_manifest", "sampling_boundary"):
                if typ == "tool_manifest" and (p.get("added") or p.get("removed")):
                    P(
                        hdr
                        + f" added={jd(p.get('added'))[:600]} removed={jd(p.get('removed'))[:300]}"
                    )
                continue
            if typ == "turn_context":
                P(hdr + f" turn_id={p.get('turn_id')} cwd={p.get('cwd')}")
                continue
            if typ == "world_state":
                P(
                    hdr
                    + f" full={p.get('full')} keys={list((p.get('state') or {}).keys())}"
                )
                st = p.get("state") or {}
                if "agents_md" in st and not p.get("full"):
                    P("  agents_md changed: " + bounded(jd(st["agents_md"]), 1500))
                continue
            if typ == "compacted":
                P(hdr)
                for k, v in p.items():
                    if k == "replacement_history" and isinstance(v, list):
                        P(f"--- replacement_history ({len(v)} items) ---")
                        for it in v:
                            if isinstance(it, dict) and it.get("type") == "message":
                                P(f"[{it.get('role')}]")
                                P(text_of(it.get("content")))
                            elif isinstance(it, dict) and it.get("type") in (
                                "custom_tool_call",
                                "function_call",
                            ):
                                P(jd(it))
                            else:
                                P(bounded(jd(it), 3000))
                    else:
                        P(f"  {k}: {jd(v)}")
                continue
            if typ == "response_item":
                if sub == "message":
                    P(hdr + f" role={p.get('role')} phase={p.get('phase')}")
                    P(text_of(p.get("content")))
                elif sub == "reasoning":
                    summ = p.get("summary") or []
                    txt = "\n".join(
                        s.get("text", "") for s in summ if isinstance(s, dict)
                    )
                    if txt:
                        P(hdr + " <<reasoning summary>> " + txt.replace("\n", " | "))
                    else:
                        P(hdr + " (no summary)")
                elif sub in ("custom_tool_call", "function_call"):
                    args = (
                        p.get("input")
                        if sub == "custom_tool_call"
                        else p.get("arguments")
                    )
                    P(hdr + f" name={p.get('name')} call_id={p.get('call_id')}")
                    P(">>> " + str(args))
                elif sub in ("custom_tool_call_output", "function_call_output"):
                    out = p.get("output")
                    outs = jd(out, 1) if isinstance(out, (dict, list)) else str(out)
                    P(hdr + f" call_id={p.get('call_id')} outlen={len(outs)}")
                    P("<<< " + bounded(outs, output_limit))
                else:
                    P(hdr)
                    P(bounded(jd(p, 1), output_limit))
                continue
            if typ == "event_msg":
                if sub == "token_count":
                    info = p.get("info") or {}
                    tu = info.get("last_token_usage") or {}
                    P(
                        hdr
                        + f" in={tu.get('input_tokens')} cached={tu.get('cached_input_tokens')} out={tu.get('output_tokens')} reasoning={tu.get('reasoning_output_tokens')}"
                    )
                elif sub in ("task_complete", "turn_aborted"):
                    t2 = p.get("timing") or {}
                    c = t2.get("counters") or {}
                    P(
                        hdr
                        + f" reason={p.get('reason')} duration_ms={p.get('duration_ms')} generations={c.get('logicalGenerationCount')} toolCalls={c.get('toolCallCount')} drops={c.get('toolOutputBudgetDropCount')}/{c.get('toolOutputBudgetDroppedTokenCount')} noProgress={c.get('noProgressDirectiveCount')} waitOnly={c.get('waitOnlyGenerationCount')} suppressedDet={c.get('suppressedDeterministicContinuationCount')} validations={c.get('executedValidationCount')}"
                    )
                    if p.get("last_agent_message"):
                        P(
                            "  last_agent_message: "
                            + bounded(str(p.get("last_agent_message")), 3000)
                        )
                elif sub in (
                    "agent_reasoning",
                    "agent_message",
                    "item_completed",
                    "thread_settings_applied",
                ):
                    continue
                elif sub == "user_message":
                    P(hdr)
                    P(str(p.get("message")))
                elif sub == "patch_apply_end":
                    P(
                        hdr
                        + f" success={p.get('success')} stdout={bounded(str(p.get('stdout')), 400)} stderr={bounded(str(p.get('stderr')), 800)}"
                    )
                else:
                    P(hdr)
                    P(bounded(jd({k: v for k, v in p.items() if k != "type"}, 1), 3000))
                continue
            P(hdr)
            P(bounded(jd(p, 1), 2000))


def rollout_argument(value):
    start = 0
    if "@" in value:
        value, suffix = value.rsplit("@", 1)
        try:
            start = int(suffix)
        except ValueError as error:
            raise argparse.ArgumentTypeError(
                "record index after @ must be an integer"
            ) from error
    return value, start


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    summary = commands.add_parser(
        "summary", help="Summarize calls, tokens, and event gaps"
    )
    summary.add_argument(
        "-v", "--verbose", action="store_true", help="Include the timeline"
    )
    summary.add_argument("rollouts", nargs="+")
    diagnostics = commands.add_parser(
        "diagnostics", help="Show payload changes and native per-turn timing"
    )
    diagnostics.add_argument("rollouts", nargs="+")
    report = commands.add_parser(
        "dump", help="Write detailed or complete snapshot reports"
    )
    report.add_argument("--complete", action="store_true")
    report.add_argument("outdir", type=Path)
    report.add_argument("rollouts", nargs="+")
    narrative = commands.add_parser(
        "narrative", help="Write narratives with bounded outputs"
    )
    narrative.add_argument("outdir", type=Path)
    narrative.add_argument(
        "rollouts", nargs="+", type=rollout_argument, metavar="ROLLOUT[@INDEX]"
    )
    args = parser.parse_args(argv)
    if args.command in ("summary", "diagnostics"):
        for path in args.rollouts:
            if args.command == "summary":
                summarize(path, args.verbose)
            else:
                analyze(path)
        return 0

    outputs = set()
    jobs = []
    for value in args.rollouts:
        if args.command == "narrative":
            path, start = value
            name = os.path.basename(path).replace(".jsonl", f".narr{start or ''}.txt")
        else:
            path, start = value, 0
            name = os.path.basename(value).replace(".jsonl", ".txt")
        output = args.outdir / name
        if output in outputs:
            parser.error(f"multiple rollouts map to the same report output: {output}")
        outputs.add(output)
        jobs.append((path, output, start))
    args.outdir.mkdir(parents=True, exist_ok=True)
    for path, output, start in jobs:
        if args.command == "narrative":
            dump_narrative(path, output, start)
        else:
            (dump_complete if args.complete else dump)(path, output)
        print("wrote", output, output.stat().st_size)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
