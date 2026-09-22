"""Narrative dump of a Codex rollout: full model-visible messages, reasoning summaries, tool inputs,
developer interventions; tool outputs bounded to a char limit; timing blocks reduced to counters."""
import json
import sys
import datetime
import os

OUT_LIMIT = int(os.environ.get('NARR_OUT_LIMIT', '4000'))


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
                out.append(str(c.get('text')) if 'text' in c else jd(c))
            else:
                out.append(str(c))
    elif content is not None:
        out.append(str(content))
    return '\n'.join(out)


def bounded(s, limit=OUT_LIMIT):
    if len(s) <= limit:
        return s
    head = s[: int(limit * 0.75)]
    tail = s[-int(limit * 0.25):]
    return f'{head}\n...[{len(s) - limit} chars omitted]...\n{tail}'


def dump(path, out_path, start_idx=0):
    rows = [json.loads(l) for l in open(path, encoding='utf-8')]
    w = open(out_path, 'w', encoding='utf-8')
    P = lambda *a: print(*a, file=w)
    prev_t = None
    for i, o in enumerate(rows):
        t = ts(o)
        gap = (t - prev_t).total_seconds() if (t and prev_t) else 0.0
        prev_t = t or prev_t
        if i < start_idx:
            continue
        typ = o.get('type')
        p = o.get('payload', {})
        sub = p.get('type') if isinstance(p, dict) else None
        hdr = f'#### [{i}] {t.strftime("%H:%M:%S") if t else "?"} (+{gap:.1f}s) {typ}' + (f'/{sub}' if sub else '')
        if typ in ('session_meta', 'tool_manifest', 'sampling_boundary'):
            if typ == 'tool_manifest' and (p.get('added') or p.get('removed')):
                P(hdr + f' added={jd(p.get("added"))[:600]} removed={jd(p.get("removed"))[:300]}')
            continue
        if typ == 'turn_context':
            P(hdr + f' turn_id={p.get("turn_id")} cwd={p.get("cwd")}')
            continue
        if typ == 'world_state':
            P(hdr + f' full={p.get("full")} keys={list((p.get("state") or {}).keys())}')
            st = p.get('state') or {}
            if 'agents_md' in st and not p.get('full'):
                P('  agents_md changed: ' + bounded(jd(st['agents_md']), 1500))
            continue
        if typ == 'compacted':
            P(hdr)
            for k, v in p.items():
                if k == 'replacement_history' and isinstance(v, list):
                    P(f'--- replacement_history ({len(v)} items) ---')
                    for it in v:
                        if isinstance(it, dict) and it.get('type') == 'message':
                            P(f'[{it.get("role")}]')
                            P(text_of(it.get('content')))
                        else:
                            P(bounded(jd(it), 3000))
                else:
                    P(f'  {k}: {jd(v)}')
            continue
        if typ == 'response_item':
            if sub == 'message':
                P(hdr + f' role={p.get("role")} phase={p.get("phase")}')
                P(text_of(p.get('content')))
            elif sub == 'reasoning':
                summ = p.get('summary') or []
                txt = '\n'.join(s.get('text', '') for s in summ if isinstance(s, dict))
                if txt:
                    P(hdr + ' <<reasoning summary>> ' + txt.replace('\n', ' | '))
                else:
                    P(hdr + ' (no summary)')
            elif sub in ('custom_tool_call', 'function_call'):
                args = p.get('input') if sub == 'custom_tool_call' else p.get('arguments')
                P(hdr + f' name={p.get("name")} call_id={p.get("call_id")}')
                P('>>> ' + bounded(str(args), 6000))
            elif sub in ('custom_tool_call_output', 'function_call_output'):
                out = p.get('output')
                outs = jd(out, 1) if isinstance(out, (dict, list)) else str(out)
                P(hdr + f' call_id={p.get("call_id")} outlen={len(outs)}')
                P('<<< ' + bounded(outs))
            else:
                P(hdr)
                P(bounded(jd(p, 1)))
            continue
        if typ == 'event_msg':
            if sub == 'token_count':
                info = p.get('info') or {}
                tu = info.get('last_token_usage') or {}
                P(hdr + f' in={tu.get("input_tokens")} cached={tu.get("cached_input_tokens")} out={tu.get("output_tokens")} reasoning={tu.get("reasoning_output_tokens")}')
            elif sub in ('task_complete', 'turn_aborted'):
                t2 = p.get('timing') or {}
                c = t2.get('counters') or {}
                P(hdr + f' reason={p.get("reason")} duration_ms={p.get("duration_ms")} generations={c.get("logicalGenerationCount")} toolCalls={c.get("toolCallCount")} drops={c.get("toolOutputBudgetDropCount")}/{c.get("toolOutputBudgetDroppedTokenCount")} noProgress={c.get("noProgressDirectiveCount")} waitOnly={c.get("waitOnlyGenerationCount")} suppressedDet={c.get("suppressedDeterministicContinuationCount")} validations={c.get("executedValidationCount")}')
                if p.get('last_agent_message'):
                    P('  last_agent_message: ' + bounded(str(p.get('last_agent_message')), 3000))
            elif sub in ('agent_reasoning', 'agent_message', 'item_completed', 'thread_settings_applied'):
                continue
            elif sub == 'user_message':
                P(hdr)
                P(str(p.get('message')))
            elif sub == 'patch_apply_end':
                P(hdr + f' success={p.get("success")} stdout={bounded(str(p.get("stdout")), 400)} stderr={bounded(str(p.get("stderr")), 800)}')
            else:
                P(hdr)
                P(bounded(jd({k: v for k, v in p.items() if k != 'type'}, 1), 3000))
            continue
        P(hdr)
        P(bounded(jd(p, 1), 2000))
    w.close()


if __name__ == '__main__':
    outdir = sys.argv[1]
    os.makedirs(outdir, exist_ok=True)
    for a in sys.argv[2:]:
        start = 0
        if '@' in a:
            a, s = a.rsplit('@', 1)
            start = int(s)
        name = os.path.basename(a).replace('.jsonl', f'.narr{start or ""}.txt')
        dump(a, os.path.join(outdir, name), start)
        print('wrote', os.path.join(outdir, name), os.path.getsize(os.path.join(outdir, name)))
