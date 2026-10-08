// Narrow production-helper benchmarks. No model, network, or synthetic inference delay.
// Usage: node scripts/benchmark_critical_path.mjs [--capture DIR | --baseline DIR]
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import vm from 'node:vm';

const [mode, directory] = process.argv.slice(2);
assert(!mode || (['--capture', '--baseline'].includes(mode) && directory));
const files = ['dependency_graph.js', 'orchestration.js'];
const current = files.map(name => readFileSync(new URL('../codex-rs/code-mode/src/runtime/' + name, import.meta.url), 'utf8'));
if (mode === '--capture') {
  mkdirSync(directory, { recursive: true });
  files.forEach((name, i) => writeFileSync(join(directory, name), current[i], { flag: 'wx' }));
}
const versions = mode === '--baseline'
  ? { baseline: files.map(name => readFileSync(join(directory, name), 'utf8')), candidate: current }
  : { baseline: current };
const median = samples => [...samples].sort((a, b) => a - b)[Math.floor(samples.length / 2)];
const results = [];
for (const scenario of ['dense-readiness', 'long-tail-wakeups', 'capability-preflight', 'utf8-recovery', 'recovery-admission']) {
  const samples = Object.fromEntries(Object.keys(versions).map(name => [name, []]));
  for (let trial = 0; trial < 9; trial++) {
    const order = Object.keys(versions);
    if (trial % 2) order.reverse();
    for (const version of order) {
      const context = vm.createContext({ tools: {}, ALL_TOOL_NAMES: [], setImmediate });
      for (const source of versions[version]) vm.runInContext(source, context);
      const fixture = new vm.Script(`(async () => {
        let thenCalls = 0, effects = 0, scheduling;
        const originalThen = Promise.prototype.then;
        Promise.prototype.then = function(...args) { thenCalls++; return originalThen.apply(this, args); };
        const node = (id, extra = {}) => ({id, run: () => ++effects, accept: () => true, ...extra});
        if (${JSON.stringify(scenario)} === 'dense-readiness') {
          const nodes = Array.from({length:160}, (_, i) => node(String(i), {
            deps:Array.from({length:i}, (_, j) => String(j))}));
          const r = await run_graph(nodes.reverse(), {concurrency:16});
          if (Object.keys(r).length !== 160 || effects !== 160) throw Error('lost graph results');
        } else if (${JSON.stringify(scenario)} === 'long-tail-wakeups') {
          let release;
          const held = new Promise(resolve => {release=resolve});
          const nodes = [node('held', {run:async () => {await held; effects++;}}),
            ...Array.from({length:255}, (_, i) => node('n'+i, {
              deps:i ? ['n'+(i-1)] : [], run:() => {effects++; if(i===254) release();}}))];
          const r = await run_graph(nodes, {concurrency:16});
          if (Object.keys(r).length !== 256 || effects !== 256) throw Error('detached graph');
        } else if (${JSON.stringify(scenario)} === 'capability-preflight') {
          ALL_TOOL_NAMES = Array.from({length:4096}, (_, i) => 'tool'+i);
          const requires = ALL_TOOL_NAMES.slice(-128);
          await run_graph(Array.from({length:128}, (_, i) => node(String(i), {requires})), {concurrency:16});
          if (effects !== 128) throw Error('missing capability work');
        } else if (${JSON.stringify(scenario)} === 'recovery-admission') {
          // A deterministic service-time model, not measured I/O or model latency.
          let now = 0, smallStarted, reads = 0, recoveries = 0, active = 0, peak = 0;
          const queue = [];
          const wait = ms => new Promise(resolve => queue.push({at:now+ms,resolve}));
          const part = (start,end) => ({status:'ok',complete:true,text:'x'.repeat(end-start),
            canonical_range:{start,end}});
          ALL_TOOL_NAMES = ['read_file','read_tool_output'];
          tools.read_file = async ({path}) => {
            ++reads; peak = Math.max(peak, ++active);
            if (path === 'small') smallStarted = now;
            await wait(path === 'small' ? 50 : 1); --active;
            const size = path === 'small' ? 1 : 6;
            return {complete:true,file_complete:size===1,canonical_bytes:size,
              source_sha256:path,artifact_id:path,retained_artifact_complete:true,
              results:[part(0,1)],continuation:{kind:'bytes',start:1,end:size}};
          };
          tools.read_tool_output = async ({artifact_id,selectors}) => {
            ++recoveries; peak = Math.max(peak, ++active);
            await wait(5); --active;
            const start = selectors[0].start, end = start+1;
            return {artifact_id,canonical_sha256:artifact_id,canonical_bytes:6,complete:end===6,
              results:[part(start,end)],...(end<6 ? {continuation_stop:{reason:'budget',
                resumable:true,selector:{kind:'bytes',start:end,end:6}}} : {})};
          };
          let finished = false;
          const work = read_files(['large-a','large-b','small','large-a'], {full:true,concurrency:2})
            .then(rows => { finished = true; return rows; });
          for (let turn=0; !finished && turn<100; turn++) {
            await new Promise(setImmediate);
            if (!queue.length) continue;
            now = Math.min(...queue.map(item => item.at));
            for (const item of queue.filter(item => item.at===now)) {
              queue.splice(queue.indexOf(item),1); item.resolve();
            }
          }
          if (!finished) throw Error('recovery scheduling did not settle');
          const rows = await work;
          if (reads!==3 || recoveries!==10 || active!==0 || peak>2 || rows.length!==4 ||
              rows.some(row => row.status!=='fulfilled' || !row.value.file_complete) ||
              rows[0].value!==rows[3].value) throw Error('recovery evidence or admission lost');
          scheduling = {modeledCompletionMs:now,modeledSmallReadStartMs:smallStarted,reads,recoveries,peak};
        } else {
          const tail = ('ascii text λ😀\\r\\n').repeat(200000);
          const bytes = 19 * 200000;
          const part = (text,start,end) => ({status:'ok',complete:true,text,canonical_range:{start,end}});
          ALL_TOOL_NAMES = ['read_file','read_tool_output'];
          tools.read_file = async () => ({complete:true,file_complete:false,canonical_bytes:bytes+1,
            source_sha256:'hash',artifact_id:'snapshot',retained_artifact_complete:true,
            results:[part('x',0,1)],continuation:{kind:'bytes',start:1,end:bytes+1}});
          tools.read_tool_output = async () => ({complete:true,canonical_bytes:bytes+1,
            canonical_sha256:'hash',artifact_id:'snapshot',results:[part(tail,1,bytes+1)]});
          const [row] = await read_files(['file'], {full:true});
          if (row.status !== 'fulfilled' || !row.value.file_complete) throw Error('invalid recovery '+row.reason);
        }
        Promise.prototype.then = originalThen;
        return {thenCalls,effects,...(scheduling ? {scheduling} : {})};
      })()`);
      const started = performance.now();
      const proof = await fixture.runInContext(context, { timeout: 10000 });
      const wall_ms = performance.now() - started;
      if (trial) samples[version].push({ wall_ms, ...proof }); // one untimed warmup per variant
    }
  }
  results.push({ scenario, samples, medians_ms: Object.fromEntries(Object.entries(samples).map(([name, rows]) => [name, median(rows.map(r => r.wall_ms))])) });
}
console.log(JSON.stringify({ node:process.version, scope:'Production JS only; no native dispatch or live-model latency claim',
  sources:Object.fromEntries(Object.entries(versions).map(([name, sources]) => [name, sources.map(source => createHash('sha256').update(source).digest('hex'))])), results }, null, 2));
