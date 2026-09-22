import json, sys, collections, datetime, hashlib

def ts(o):
    t = o.get('timestamp')
    if not t:
        return None
    return datetime.datetime.fromisoformat(t.replace('Z', '+00:00'))

def h(s):
    return hashlib.sha1(json.dumps(s, sort_keys=True, default=str).encode()).hexdigest()[:10]

for path in sys.argv[1:]:
    rows = [json.loads(l) for l in open(path, encoding='utf-8')]
    print('=' * 100)
    print(path.split('\\')[-1])
    # 1. bytes by type
    by = collections.Counter()
    for l in open(path, encoding='utf-8'):
        o = json.loads(l)
        p = o.get('payload', {})
        sub = p.get('type') if isinstance(p, dict) else None
        by[(o.get('type'), sub)] += len(l)
    print('bytes by type:')
    for k, v in sorted(by.items(), key=lambda x: -x[1])[:12]:
        print(f'   {v:>10,} {k}')
    # 2. world_state / tool_manifest / turn_context changes
    print('world_state entries:')
    prev = None
    for i, o in enumerate(rows):
        if o.get('type') == 'world_state':
            p = o['payload']
            st = p.get('state', {})
            keys = {k: h(v) for k, v in st.items()} if isinstance(st, dict) else {}
            changed = [k for k in keys if prev and prev.get(k) != keys[k]] if prev else list(keys)
            print(f'   idx={i} full={p.get("full")} keys={list(keys)} changed={changed} otherkeys={[k for k in p if k not in ("full","state")]}')
            prev = keys
    print('tool_manifest entries:')
    for i, o in enumerate(rows):
        if o.get('type') == 'tool_manifest':
            p = o['payload']
            print(f'   idx={i} hash={str(p.get("hash"))[:16]} base={str(p.get("base_hash"))[:16]} added={[a.get("name") if isinstance(a, dict) else a for a in (p.get("added") or [])][:10]} removed={p.get("removed")} manifest_len={len(json.dumps(p.get("manifest"))) if p.get("manifest") else 0}')
    print('turn_context entries:')
    prev = None
    for i, o in enumerate(rows):
        if o.get('type') == 'turn_context':
            p = dict(o['payload'])
            keys = {k: h(v) for k, v in p.items()}
            changed = [k for k in keys if prev and prev.get(k) != keys[k]] if prev else list(keys)
            print(f'   idx={i} changed={changed}')
            prev = keys
    # 3. reasoning ids
    ids = collections.Counter()
    for o in rows:
        if o.get('type') == 'response_item' and o['payload'].get('type') == 'reasoning':
            ids[o['payload'].get('id')] += 1
    dup = {k: v for k, v in ids.items() if v > 1}
    print('reasoning items:', sum(ids.values()), 'distinct ids:', len(ids), 'dups:', len(dup), list(dup.items())[:5])
    # 4. per-turn timing
    print('per-turn timing:')
    turn_start = None; turn_i = None; n_samp = 0; tool_t = 0.0; model_t = 0.0; wait_t = 0.0; last_t = None; user_msg = ''
    call_start = {}
    last_boundary = None
    in_tokens = 0; cached = 0; out_tokens = 0
    for i, o in enumerate(rows):
        t = ts(o); typ = o.get('type'); p = o.get('payload', {}); sub = p.get('type') if isinstance(p, dict) else None
        if typ == 'event_msg' and sub == 'task_started':
            turn_start = t; turn_i = i; n_samp = 0; tool_t = 0; model_t = 0; wait_t = 0; user_msg = ''; in_tokens = 0; cached = 0; out_tokens = 0
        elif typ == 'event_msg' and sub == 'user_message':
            user_msg = (p.get('message') or '')[:60].replace('\n', ' ')
        elif typ == 'sampling_boundary':
            n_samp += 1; last_boundary = t
        elif typ == 'response_item' and sub in ('custom_tool_call', 'function_call'):
            call_start[p.get('call_id')] = (t, p.get('name'))
            if last_boundary and t:
                model_t += (t - last_boundary).total_seconds()
        elif typ == 'response_item' and sub in ('custom_tool_call_output', 'function_call_output'):
            st = call_start.get(p.get('call_id'))
            if st and st[0] and t:
                d = (t - st[0]).total_seconds()
                if st[1] == 'wait':
                    wait_t += d
                else:
                    tool_t += d
        elif typ == 'response_item' and sub == 'message' and p.get('role') == 'assistant':
            if last_boundary and t:
                model_t += (t - last_boundary).total_seconds()
        elif typ == 'event_msg' and sub == 'token_count':
            tu = (p.get('info') or {}).get('last_token_usage') or {}
            in_tokens += tu.get('input_tokens', 0) or 0
            cached += tu.get('cached_input_tokens', 0) or 0
            out_tokens += tu.get('output_tokens', 0) or 0
        elif typ == 'event_msg' and sub in ('task_complete', 'turn_aborted'):
            wall = (t - turn_start).total_seconds() if turn_start and t else None
            print(f'   turn@{turn_i} {sub:13} wall={wall:8.1f}s samplings={n_samp:3} model_gen={model_t:7.1f}s tools={tool_t:7.1f}s wait={wait_t:7.1f}s in={in_tokens:,} cached={cached:,} uncached={in_tokens-cached:,} out={out_tokens:,} user="{user_msg}"')
