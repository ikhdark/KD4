import json, sys, collections, datetime

def ts(o):
    t = o.get('timestamp')
    if not t:
        return None
    try:
        return datetime.datetime.fromisoformat(t.replace('Z', '+00:00'))
    except Exception:
        return None

def short(s, n=160):
    s = str(s).replace('\n', '\\n')
    return s if len(s) <= n else s[:n] + f'...(+{len(s)-n})'

def analyze(path, verbose):
    rows = []
    for line in open(path, encoding='utf-8'):
        try:
            rows.append(json.loads(line))
        except Exception as e:
            print('BAD LINE', e)
    print('=' * 100)
    print(path.split('\\')[-1])
    first = rows[0]
    meta = first.get('payload', {}) if first.get('type') == 'session_meta' else {}
    print('session_meta keys:', list(meta.keys())[:30])
    for k in ('id', 'cwd', 'originator', 'cli_version', 'model', 'model_provider', 'source', 'timestamp'):
        if k in meta:
            print(f'  {k}: {short(meta[k])}')
    if 'instructions' in meta:
        print('  instructions len:', len(meta['instructions'] or ''))
    t0 = ts(rows[0]); tN = ts(rows[-1])
    print('start', t0, 'end', tN, 'wall', (tN - t0) if t0 and tN else None)

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
        typ = o.get('type')
        p = o.get('payload', {})
        sub = p.get('type') if isinstance(p, dict) else None
        desc = f'{typ}/{sub}'
        extra = ''
        if typ == 'response_item':
            if sub in ('custom_tool_call', 'function_call'):
                name = p.get('name')
                args = p.get('input') if sub == 'custom_tool_call' else p.get('arguments')
                cid = p.get('call_id')
                tool_starts[cid] = (t, name, i)
                ncalls[name] += 1
                extra = f' {name} call_id={cid} args={short(args, 300)}'
            elif sub in ('custom_tool_call_output', 'function_call_output'):
                cid = p.get('call_id')
                out = p.get('output')
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
                extra = f' call_id={cid} dur={dur} outlen={len(outs)} out={short(outs, 300)}'
            elif sub == 'message':
                role = p.get('role')
                content = p.get('content')
                txt = ''
                if isinstance(content, list):
                    for c in content:
                        if isinstance(c, dict):
                            txt += c.get('text', '') or ''
                extra = f' role={role} len={len(txt)} text={short(txt, 400)}'
            elif sub == 'reasoning':
                summ = p.get('summary') or []
                txt = ''
                for s in summ:
                    if isinstance(s, dict):
                        txt += s.get('text', '') or ''
                enc = p.get('encrypted_content')
                extra = f' summary_len={len(txt)} enc_len={len(enc) if enc else 0} summ={short(txt, 200)}'
        elif typ == 'event_msg':
            if sub == 'token_count':
                info = p.get('info') or {}
                tu = info.get('last_token_usage') or {}
                tot = info.get('total_token_usage') or {}
                tokens_series.append((t, tu, tot, info.get('model_context_window')))
                extra = f' last={tu} total_in={tot.get("input_tokens")} total_out={tot.get("output_tokens")} ctx_window={info.get("model_context_window")}'
            elif sub in ('user_message', 'agent_message'):
                extra = f' msg={short(p.get("message"), 500)}'
            elif sub == 'warning':
                extra = f' {short(p, 500)}'
            elif sub in ('task_started', 'task_complete', 'turn_aborted', 'error', 'stream_error', 'background_event', 'context_compacted', 'compaction', 'thread_settings_applied'):
                extra = f' {short({k: v for k, v in p.items() if k != "type"}, 400)}'
            elif sub == 'agent_reasoning':
                extra = f' {short(p.get("text"), 200)}'
            elif sub == 'item_completed':
                extra = f' {short(p, 200)}'
            else:
                extra = f' {short(p, 200)}'
        elif typ == 'sampling_boundary':
            n_sampling += 1
            extra = f' {short(p, 200)}'
        elif typ == 'turn_context':
            extra = f' {short({k: v for k, v in p.items() if k not in ("instructions",)}, 600)}'
        elif typ == 'tool_manifest':
            extra = f' n_tools={len(p.get("tools", [])) if isinstance(p, dict) else "?"} keys={list(p.keys())[:8] if isinstance(p, dict) else ""}'
        elif typ == 'world_state':
            extra = f' {short(p, 300)}'
        elif typ == 'compacted':
            extra = f' {short(p, 400)}'
        gap = (t - prev_t).total_seconds() if (t and prev_t) else None
        if gap is not None:
            gaps.append((gap, i, prev_desc, desc))
        timeline.append((i, t.strftime('%H:%M:%S') if t else '?', f'{gap:8.1f}' if gap is not None else '        ', desc, extra))
        prev_t = t if t else prev_t
        prev_desc = desc
    print('samplings:', n_sampling, 'tool calls:', dict(ncalls))
    if tokens_series:
        last = tokens_series[-1]
        print('final total usage:', last[2])
        print('ctx window:', last[3])
        print('per-sampling input tokens (last_token_usage):')
        for t, tu, tot, cw in tokens_series:
            print(f'   {t.strftime("%H:%M:%S") if t else "?"} in={tu.get("input_tokens")} cached={tu.get("cached_input_tokens")} out={tu.get("output_tokens")} reason={tu.get("reasoning_output_tokens")} tot={tu.get("total_tokens")}')
    print('top gaps (sec, idx, prev -> cur):')
    for g in sorted(gaps, reverse=True)[:15]:
        print('   %8.1f  idx=%d  %s -> %s' % g)
    print('top tool durations:')
    for d in sorted(tool_durations, reverse=True)[:10]:
        print('   %8.1f  %s idx=%d' % d)
    print('sum tool durations: %.1f' % sum(d[0] for d in tool_durations))
    print('largest outputs:', sorted(out_sizes, reverse=True)[:10])
    if verbose:
        print('--- timeline ---')
        for row in timeline:
            print('%4d %s %s %-42s%s' % row)

if __name__ == '__main__':
    verbose = '-v' in sys.argv
    for a in sys.argv[1:]:
        if a == '-v':
            continue
        analyze(a, verbose)
