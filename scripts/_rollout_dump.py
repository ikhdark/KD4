"""Render rollout reports; the default human-readable view is intentionally lossy.

Use --complete OUTDIR ROLLOUT... for a complete plaintext snapshot report.
Reports retain every JSON field except opaque encrypted_content strings, recorded
by UTF-8 length and SHA-256 instead. Exact duplicate subtrees become null with an
entry in references: RFC 6901 pointers into records map to earlier source nodes.
Resolve references before treating a placeholder as a source null. Counts prove
report construction, not that a model has read the report. Read retained reports
in bounded batches rather than regenerating them for each page.
"""
import argparse
import contextlib
import hashlib
import json
import datetime
import os
from pathlib import Path

try:
    from scripts.atomic_json import write_json_atomic
    from scripts.rollout_snapshot import read_rollout_records
    from scripts.rollout_snapshot import read_rollout_snapshot
except ImportError:
    from atomic_json import write_json_atomic
    from rollout_snapshot import read_rollout_records
    from rollout_snapshot import read_rollout_snapshot


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
                    raise ValueError(
                        f"rollout record {snapshot.path}:{number} is not an object"
                    )
                records.append(render(row, f"/{number - 1}"))
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


def ts(o):
    t = o.get('timestamp')
    if not t:
        return None
    try:
        return datetime.datetime.fromisoformat(t.replace('Z', '+00:00'))
    except Exception:
        return None


def jd(v, indent=None):
    return json.dumps(v, indent=indent, ensure_ascii=False, default=str)


def text_of(content):
    out = []
    if isinstance(content, list):
        for c in content:
            if isinstance(c, dict):
                if 'text' in c:
                    out.append(str(c.get('text')))
                else:
                    out.append(jd(c))
            else:
                out.append(str(c))
    elif content is not None:
        out.append(str(content))
    return '\n'.join(out)


def dump(path, out_path):
    rows = [row for row, _ in read_rollout_records(Path(path))]
    w = open(out_path, 'w', encoding='utf-8')
    P = lambda *a: print(*a, file=w)
    prev_t = None
    seen_manifests = set()
    cache_keys = {}
    prev_settings = None
    prev_turn_ctx = None
    for i, o in enumerate(rows):
        t = ts(o)
        gap = (t - prev_t).total_seconds() if (t and prev_t) else 0.0
        prev_t = t or prev_t
        typ = o.get('type')
        p = o.get('payload', {})
        sub = p.get('type') if isinstance(p, dict) else None
        hdr = f'#### [{i}] {t.strftime("%H:%M:%S.%f")[:-3] if t else "?"} (+{gap:.1f}s) {typ}' + (f'/{sub}' if sub else '')
        if typ == 'session_meta':
            P(hdr)
            for k, v in p.items():
                if k == 'base_instructions':
                    P(f'--- base_instructions ({len(v)} chars) ---')
                    P(v)
                    P('--- end base_instructions ---')
                else:
                    P(f'  {k}: {jd(v)}')
        elif typ == 'turn_context':
            P(hdr)
            if prev_turn_ctx is None:
                P(jd(p, 1))
            else:
                for k, v in p.items():
                    if prev_turn_ctx.get(k) != v:
                        P(f'  CHANGED {k}: {jd(v)}')
            prev_turn_ctx = p
        elif typ == 'tool_manifest':
            P(hdr + f' hash={p.get("hash")} base={p.get("base_hash")}')
            m = p.get('manifest')
            manifest_id = p.get('hash') or jd(m)
            if m and manifest_id not in seen_manifests:
                seen_manifests.add(manifest_id)
                mv = m.get('model_visible') or []
                P(f'--- model_visible tools ({len(mv)}) ---')
                for tdef in mv:
                    P(f'## TOOL {tdef.get("name")} type={tdef.get("type")} format={jd(tdef.get("format"))}')
                    P(str(tdef.get('description')))
                    for k, v in tdef.items():
                        if k not in ('name', 'type', 'format', 'description'):
                            P(f'   {k}: {jd(v)}')
                reg = m.get('registered') or []
                P(f'--- registered ({len(reg)}) ---')
                for r in reg:
                    P(f'   {jd(r)}')
            elif m:
                P(f'  (manifest already shown, {len(jd(m))} chars)')
            for k in ('added', 'removed'):
                if p.get(k):
                    P(f'  {k}: {jd(p.get(k))}')
            for k, v in p.items():
                if k not in ('hash', 'base_hash', 'manifest', 'added', 'removed'):
                    P(f'  {k}: {jd(v)}')
        elif typ == 'world_state':
            P(hdr + f' full={p.get("full")}')
            st = p.get('state', {})
            for k, v in st.items():
                P(f'--- world_state.{k} ---')
                P(jd(v, 1) if not isinstance(v, str) else v)
            for k, v in p.items():
                if k not in ('full', 'state'):
                    P(f'  {k}: {jd(v)[:2000]}')
        elif typ == 'compacted':
            P(hdr)
            for k, v in p.items():
                if k == 'replacement_history':
                    P(f'--- replacement_history ({len(v) if isinstance(v, list) else "?"} items) ---')
                    if isinstance(v, list):
                        for it in v:
                            if isinstance(it, dict) and it.get('type') == 'message':
                                P(f'[{it.get("role")}]')
                                P(text_of(it.get('content')))
                            else:
                                P(jd(it, 1))
                    else:
                        P(jd(v, 1))
                else:
                    P(f'  {k}: {jd(v)}')
        elif typ == 'sampling_boundary':
            P(hdr + ' ' + jd(p)[:600])
        elif typ == 'response_item':
            if sub == 'message':
                role = p.get('role')
                P(hdr + f' role={role}')
                P(text_of(p.get('content')))
                for k, v in p.items():
                    if k not in ('type', 'role', 'content', 'id', 'status'):
                        P(f'  {k}: {jd(v)[:500]}')
            elif sub == 'reasoning':
                summ = p.get('summary') or []
                txt = '\n'.join(s.get('text', '') for s in summ if isinstance(s, dict))
                P(hdr + f' id={p.get("id")} enc_len={len(p.get("encrypted_content") or "")}')
                if txt:
                    P('<<reasoning summary>>')
                    P(txt)
                cont = p.get('content')
                if cont:
                    P('<<reasoning content>>')
                    P(text_of(cont))
            elif sub in ('custom_tool_call', 'function_call'):
                args = p.get('input') if sub == 'custom_tool_call' else p.get('arguments')
                P(hdr + f' name={p.get("name")} call_id={p.get("call_id")}')
                P('>>> INPUT:')
                P(str(args))
            elif sub in ('custom_tool_call_output', 'function_call_output'):
                out = p.get('output')
                if isinstance(out, (dict, list)):
                    outs = jd(out, 1)
                else:
                    outs = str(out)
                P(hdr + f' call_id={p.get("call_id")} outlen={len(outs)}')
                P('<<< OUTPUT:')
                P(outs)
            else:
                P(hdr)
                P(jd(p, 1))
        elif typ == 'event_msg':
            if sub == 'token_count':
                info = p.get('info') or {}
                tu = info.get('last_token_usage') or {}
                tot = info.get('total_token_usage') or {}
                P(hdr + f' last={jd(tu)} total_in={tot.get("input_tokens")} total_out={tot.get("output_tokens")} ctx={info.get("model_context_window")} rate_limits={jd(p.get("rate_limits"))[:300]}')
            elif sub in ('task_complete', 'turn_aborted'):
                P(hdr)
                for k, v in p.items():
                    if k == 'timing' and isinstance(v, dict):
                        P('  timing summary:')
                        for tk in ('exclusive', 'unions', 'local', 'milestones', 'terminalization', 'toolClosure',
                                   'observationalNonprogressTokens', 'observationalNonprogressLatency',
                                   'toolCallTimingOverflow', 'deterministicContinuationReceiptOverflow',
                                   'inclusiveDurationMs', 'machineDurationMs', 'profileValid', 'classificationComplete'):
                            if tk in v:
                                P(f'    {tk}: {jd(v[tk])}')
                        c = v.get('counters') or {}
                        P('    counters:')
                        for ck, cv in c.items():
                            P(f'      {ck}: {jd(cv)[:3000]}')
                        mr = v.get('modelRequests') or []
                        P(f'    modelRequests ({len(mr)}):')
                        for r in mr:
                            keep = {k2: r.get(k2) for k2 in ('generationIndex', 'generationReason', 'generationPurpose', 'disposition',
                                                             'attemptKind', 'isContinuation', 'modelStreamWaitNs', 'decisionLatencyNs',
                                                             'toolCallCount', 'modelEmittedToolCallCount', 'outputTokens',
                                                             'reasoningOutputTokens', 'tokenUsage', 'requestTokenCategories',
                                                             'fixedPrefixReuseEligible', 'nextStructuredActionChanged',
                                                             'unchangedRelevantState', 'physicalAttemptIds', 'retryCount',
                                                             'errorKind', 'error', 'status')}
                            fingerprint = r.get('promptCacheKeyFingerprint')
                            if isinstance(fingerprint, str):
                                if fingerprint not in cache_keys:
                                    cache_keys[fingerprint] = f'cache-key-{len(cache_keys) + 1}'
                                    P(f'      {cache_keys[fingerprint]} fingerprint={fingerprint}')
                                keep['promptCacheKey'] = cache_keys[fingerprint]
                            else:
                                keep['promptCacheKey'] = None
                            other = {k2: r.get(k2) for k2 in r if k2 not in keep and k2 not in ('relevantStateFingerprint', 'samplingRequestId', 'promptCacheKeyFingerprint')}
                            P(f'      {jd(keep)}  other={jd(other)[:1500]}')
                        tc = v.get('toolCalls') or []
                        P(f'    toolCalls ({len(tc)}):')
                        for r in tc:
                            keep = {k2: r.get(k2) for k2 in ('callId', 'parentCallId', 'toolName', 'source', 'outcome', 'generationIndex',
                                                             'acceptedAtMs', 'handlerDurationMs', 'totalDurationMs', 'parallelGateWaitMs',
                                                             'workspaceEvidenceBeforeMs', 'eager', 'backgroundProcessExpected', 'outputProjectionMs')}
                            P(f'      {jd(keep)}')
                        for tk, tv in v.items():
                            if tk not in ('exclusive', 'unions', 'local', 'milestones', 'terminalization', 'toolClosure',
                                          'observationalNonprogressTokens', 'observationalNonprogressLatency', 'counters',
                                          'modelRequests', 'toolCalls', 'toolCallTimingOverflow', 'deterministicContinuationReceiptOverflow',
                                          'inclusiveDurationMs', 'machineDurationMs', 'profileValid', 'classificationComplete',
                                          'schemaVersion', 'startedAtUnixMs', 'completedAtUnixMs', 'inclusiveDurationNs', 'machineDurationNs'):
                                P(f'    {tk}: {jd(tv)[:3000]}')
                    else:
                        P(f'  {k}: {jd(v) if not isinstance(v, str) else v}')
            elif sub == 'thread_settings_applied':
                s = p.get('thread_settings')
                if prev_settings is None:
                    P(hdr)
                    P(jd(s, 1))
                elif s != prev_settings:
                    P(hdr + ' (changed)')
                    if isinstance(s, dict) and isinstance(prev_settings, dict):
                        for k in set(list(s.keys()) + list(prev_settings.keys())):
                            if s.get(k) != prev_settings.get(k):
                                P(f'  CHANGED {k}:')
                                P(f'    old: {jd(prev_settings.get(k))}')
                                P(f'    new: {jd(s.get(k))}')
                    else:
                        P(jd(s, 1))
                else:
                    P(hdr + ' (unchanged)')
                prev_settings = s
            elif sub == 'agent_reasoning':
                P(hdr + f' len={len(p.get("text") or "")} (see response_item reasoning)')
            elif sub == 'agent_message':
                P(hdr + f' len={len(p.get("message") or "")} (see response_item message)')
            elif sub == 'user_message':
                P(hdr)
                P(str(p.get('message')))
                for k, v in p.items():
                    if k not in ('type', 'message'):
                        P(f'  {k}: {jd(v)[:2000]}')
            else:
                P(hdr)
                P(jd({k: v for k, v in p.items() if k != 'type'}, 1))
        else:
            P(hdr)
            P(jd(p, 1))
    w.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--complete", action="store_true")
    parser.add_argument("outdir", type=Path)
    parser.add_argument("rollouts", nargs="+")
    args = parser.parse_args(argv)
    args.outdir.mkdir(parents=True, exist_ok=True)
    for a in args.rollouts:
        name = os.path.basename(a).replace('.jsonl', '.txt')
        output = args.outdir / name
        (dump_complete if args.complete else dump)(a, output)
        print('wrote', output, output.stat().st_size)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
