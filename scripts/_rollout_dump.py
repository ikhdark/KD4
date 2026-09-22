"""Dump a Codex rollout JSONL as a full, readable transcript (analysis scratch script)."""
import json
import sys
import datetime
import os


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
    rows = []
    for line in open(path, encoding='utf-8'):
        rows.append(json.loads(line))
    w = open(out_path, 'w', encoding='utf-8')
    P = lambda *a: print(*a, file=w)
    prev_t = None
    seen_manifest = False
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
            if m and not seen_manifest:
                seen_manifest = True
                mv = m.get('model_visible') or []
                P(f'--- model_visible tools ({len(mv)}) ---')
                for tdef in mv:
                    P(f'## TOOL {tdef.get("name")} type={tdef.get("type")} format={jd(tdef.get("format"))[:300]}')
                    P(str(tdef.get('description')))
                    for k, v in tdef.items():
                        if k not in ('name', 'type', 'format', 'description'):
                            P(f'   {k}: {jd(v)[:2000]}')
                reg = m.get('registered') or []
                P(f'--- registered ({len(reg)}) ---')
                for r in reg:
                    P(f'   {jd(r)}')
            elif m:
                P(f'  (manifest again, {len(jd(m))} bytes)')
            for k in ('added', 'removed'):
                if p.get(k):
                    P(f'  {k}: {jd(p.get(k))[:3000]}')
            for k, v in p.items():
                if k not in ('hash', 'base_hash', 'manifest', 'added', 'removed'):
                    P(f'  {k}: {jd(v)[:2000]}')
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


if __name__ == '__main__':
    outdir = sys.argv[1]
    os.makedirs(outdir, exist_ok=True)
    for a in sys.argv[2:]:
        name = os.path.basename(a).replace('.jsonl', '.txt')
        dump(a, os.path.join(outdir, name))
        print('wrote', os.path.join(outdir, name), os.path.getsize(os.path.join(outdir, name)))
