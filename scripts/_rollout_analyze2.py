import json, sys, collections, datetime, hashlib
from pathlib import Path

try:
    from scripts.kd4_timing_analysis import analyze_timing, timing_profile_valid
    from scripts.rollout_snapshot import read_rollout_records
except ImportError:
    from kd4_timing_analysis import analyze_timing, timing_profile_valid
    from rollout_snapshot import read_rollout_records

def ts(o):
    t = o.get('timestamp')
    if not t:
        return None
    return datetime.datetime.fromisoformat(t.replace('Z', '+00:00'))

def h(s):
    return hashlib.sha1(json.dumps(s, sort_keys=True, default=str).encode()).hexdigest()[:10]

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
        return '?'
    return f'{value:.1f}s' if duration else f'{value:,}'


def analyze(path):
    records = read_rollout_records(Path(path))
    rows = [row for row, _ in records]
    print('=' * 100)
    print(Path(path).name)
    # 1. bytes by type
    by = collections.Counter()
    for o, byte_length in records:
        p = o.get('payload', {})
        sub = p.get('type') if isinstance(p, dict) else None
        by[(o.get('type'), sub)] += byte_length
    print('bytes by type:')
    for k, v in sorted(by.items(), key=lambda x: -x[1])[:12]:
        print(f'   {v:>10,} {k}')
    # 2. world_state / tool_manifest / turn_context changes
    print('world_state entries:')
    prev = None
    state = {}
    for i, o in enumerate(rows):
        if o.get('type') == 'world_state':
            p = o['payload']
            patch = p.get('state', {})
            state = patch if p.get('full') else merge_patch(state, patch)
            keys = {k: h(v) for k, v in state.items()} if isinstance(state, dict) else {}
            changed = [k for k in sorted(keys.keys() | (prev or {}).keys()) if prev is None or prev.get(k) != keys.get(k)]
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
            changed = [k for k in sorted(keys.keys() | (prev or {}).keys()) if prev is None or prev.get(k) != keys.get(k)]
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
    turn_start = None; turn_i = None; n_samp = 0; user_msg = ''
    for i, o in enumerate(rows):
        t = ts(o); typ = o.get('type'); p = o.get('payload', {}); sub = p.get('type') if isinstance(p, dict) else None
        if typ == 'event_msg' and sub == 'task_started':
            turn_start = t; turn_i = i; n_samp = 0; user_msg = ''
        elif typ == 'event_msg' and sub == 'user_message':
            user_msg = (p.get('message') or '')[:60].replace('\n', ' ')
        elif typ == 'sampling_boundary':
            n_samp += 1
        elif typ == 'event_msg' and sub in ('task_complete', 'turn_aborted'):
            wall = (t - turn_start).total_seconds() if turn_start and t else None
            timing = p.get('timing')
            report = analyze_timing(timing, status=sub) if timing_profile_valid(timing) else None
            unions = timing.get('unions', {}) if report else {}
            counters = timing.get('counters', {}) if report else {}
            if report and seconds(timing.get('inclusiveDurationNs')) is not None:
                wall = seconds(timing['inclusiveDurationNs'])
            tokens = report['tokens'] if report else {}
            retention = report['requestRetention']['complete'] if report else None
            totals = tokens.get('providerTotals') if retention is True else None
            usage = 'complete' if totals is not None else 'partial' if tokens.get('available') else 'unavailable'
            observed = tokens.get('observedTotals', {}) if usage == 'partial' else {}
            totals = totals or {}
            # Event gaps overlap and token-count events are snapshots, not a ledger.
            # Missing native evidence must remain unknown rather than a plausible zero.
            print(f'   turn@{turn_i} {sub:13} wall={number(wall, True)} boundaries={n_samp} '
                  f'generations={number(counters.get("logicalGenerationCount"))} '
                  f'model_active={number(seconds(unions.get("modelActiveUnionNs")), True)} '
                  f'tool_active={number(seconds(unions.get("toolActiveUnionNs")), True)} '
                  f'interactive_wait={number(seconds(unions.get("interactiveWaitUnionNs")), True)} '
                  f'in={number(totals.get("inputTokens"))} cached={number(totals.get("cachedInputTokens"))} '
                  f'uncached={number(totals.get("nonCachedInputTokens"))} out={number(totals.get("outputTokens"))} '
                  f'usage={usage} request_retention={retention} '
                  + (f'observed_in={number(observed.get("inputTokens"))} observed_cached={number(observed.get("cachedInputTokens"))} '
                     f'observed_out={number(observed.get("outputTokens"))} ' if observed else '')
                  + f'user="{user_msg}"')
            turn_start = None; turn_i = None; n_samp = 0; user_msg = ''


if __name__ == '__main__':
    for path in sys.argv[1:]:
        analyze(path)
