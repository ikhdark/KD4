#!/usr/bin/env node
// Production-helper microbenchmarks. No model/provider, process launch or live
// workspace evidence is simulated as an end-to-end speedup.
import {readFile, writeFile} from 'node:fs/promises';
import {resolve} from 'node:path';
import {createHash} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {types} from 'node:util';
import vm from 'node:vm';

let runs = 5, output, runtime = new URL('../codex-rs/code-mode/src/runtime/', import.meta.url);
const args = process.argv.slice(2);
if (args.includes('--help')) {
  console.log('Usage: node scripts/benchmark_critical_path_projection.mjs [--runs N] [--runtime-dir PATH] [--output PATH]\nTen bounded production-helper probes; reports timing, operation counts and content fingerprints. Output is exclusive-create. Timings are not live turn speedups.');
  process.exit(0);
}
for (let i = 0; i < args.length; i += 2) {
  if (args[i] === '--runs') runs = Number(args[i + 1]);
  else if (args[i] === '--output' && args[i + 1]) output = args[i + 1];
  else if (args[i] === '--runtime-dir' && args[i + 1]) runtime = resolve(args[i + 1]);
  else throw Error(`unknown/missing argument: ${args[i]}`);
}
if (!Number.isInteger(runs) || runs < 1 || runs > 30) throw Error('--runs must be 1–30');
const names = ['dependency_graph.js', 'orchestration.js', 'output_projection.rs'];
const sources = await Promise.all(names.map(name => readFile(runtime instanceof URL ? new URL(name, runtime) : resolve(runtime, name), 'utf8')));
const projector = sources[2].match(/const PROJECTOR: &str = r#"([\s\S]*?)"#;/)?.[1];
if (!projector) throw Error('production projector not found');
const common = `${sources[0]}\n${sources[1]}\nconst project = (${projector})(isProxy);
const check = (ok, message) => { if (!ok) throw Error(message); };
const node = (id, run = () => id, extra = {}) => ({id, run, accept: () => true, ...extra});
const raw = (text, start = 0) => ({status:'ok', complete:true, text, canonical_range:{start, end:start + utf8Length(text)}});
const live = () => ({execution_state:'running', process_exited:false, session_id:7,
  session_capabilities:{polling:true, incarnation:'test'}});
const register = value => {const original = JSON.parse(JSON.stringify(value)); project(value, original, original, true); return value;};`;
const cases = [
  {id:'capability_preflight', setup:String.raw`
    ALL_TOOL_NAMES = Array.from({length:4096}, (_, i) => 'tool-'+i);
    const requires = ALL_TOOL_NAMES.slice(-128);
    const nodes = Array.from({length:256}, (_, i) => node(String(i), undefined, {requires}));`,
    body:String.raw`const result = await run_graph(nodes, {concurrency:16}); check(Object.keys(result).length === 256, 'lost nodes'); return {nodes:256, requiredNames:32768};`},
  {id:'completion_subscriptions', setup:String.raw`
    let subscriptions = 0, release;
    const gate = new Promise(r => release = r), originalRace = Promise.race;
    Promise.race = function(values) { const rows = [...values]; subscriptions += rows.length; return originalRace.call(this, rows); };
    const nodes = [node('slow', () => gate), ...Array.from({length:255}, (_, i) => node(String(i)))];`,
    body:String.raw`const pending = run_graph(nodes, {concurrency:2}); setTimeout(release, 10); const result = await pending;
      check(Object.keys(result).length === 256, 'unsettled nodes'); return {subscriptions, nodes:256};`},
  {id:'dependency_scans', setup:String.raw`
    let probes = 0; const hasOwn = Object.hasOwn;
    Object.hasOwn = (...args) => { ++probes; return hasOwn(...args); };
    const nodes = Array.from({length:256}, (_, i) => node(String(i), undefined, {deps:i ? [String(i-1)] : []})).reverse();`,
    body:String.raw`const result = await run_graph(nodes, {concurrency:1}); check(Object.keys(result).length === 256, 'lost dependency'); return {probes, nodes:256};`},
  {id:'read_recovery_head_of_line', setup:String.raw`
    ALL_TOOL_NAMES = ['read_file','read_tool_output']; let reads = 0, recoveries = 0, smallStarted;
    const delay = ms => new Promise(r => setTimeout(r, ms));
    tools.read_file = async ({path}) => {
      ++reads;
      if (path === 'small') { smallStarted = performance.now(); await delay(30); return {complete:true,file_complete:true,canonical_bytes:1,results:[raw('s')]}; }
      return {complete:true,file_complete:false,canonical_bytes:4,source_sha256:path,artifact_id:path,
        retained_artifact_complete:true,results:[raw('x')],continuation:{kind:'bytes',start:1,end:4}};
    };
    tools.read_tool_output = async ({artifact_id,selectors}) => {
      ++recoveries; await delay(10); const start = selectors[0].start, end = start+1;
      return {artifact_id,canonical_sha256:artifact_id,canonical_bytes:4,complete:end===4,results:[raw('x',start)],
        ...(end<4 ? {continuation_stop:{reason:'budget',resumable:true,selector:{kind:'bytes',start:end,end:4}}} : {})};
    };`, body:String.raw`const start = performance.now(); const rows = await read_files(['a','b','small'], {full:true,concurrency:2});
      check(rows.every(r => r.status==='fulfilled' && r.value.file_complete), 'incomplete batch');
      return {reads,recoveries,smallStartMs:smallStarted-start};`},
  {id:'utf8_recovery', setup:String.raw`
    ALL_TOOL_NAMES = ['read_file','read_tool_output']; const body = 'ASCII λ😀\r\n'.repeat(300000), size = utf8Length(body)+1;
    tools.read_file = async () => ({complete:true,file_complete:false,canonical_bytes:size,source_sha256:'h',artifact_id:'a',
      retained_artifact_complete:true,results:[raw('x')],continuation:{kind:'bytes',start:1,end:size}});
    tools.read_tool_output = async () => ({complete:true,canonical_bytes:size,canonical_sha256:'h',artifact_id:'a',results:[raw(body,1)]});`,
    body:String.raw`const [row] = await read_files(['a'], {full:true}); check(row.value?.file_complete, 'UTF-8 coverage'); return {bytes:size,content:row.value.pages[0].results[0].text};`},
  {id:'wait_budget_floor', setup:String.raw`
    let now = 0, polls = 0; Date.now = () => now; ALL_TOOL_NAMES = ['write_stdin'];
    tools.write_stdin = async args => { ++polls; now += args.yield_time_ms; return live(); };`,
    body:String.raw`let stopped; try { await await_command(live(), {max_wait_ms:5000,on_progress:()=>{now=Math.max(now,4999);return true;}}); }
      catch (error) { stopped = error; } check(stopped?.evidence.terminal.session_id===7, 'lost resumable handle');
      return {polls,elapsedVirtualMs:now,overshootMs:Math.max(0,now-5000)};`},
  {id:'escape_detection', setup:String.raw`const value = register({output:'command output\n'.repeat(70000)});`,
    body:String.raw`let result; for(let i=0;i<12;i++) result=project(value); check(result.endsWith(value.output) && result.includes('"output_lines":70001'), 'changed command output'); return {content:result};`},
  {id:'line_counting', setup:String.raw`const value = register({complete:true,results:[raw('a\n'.repeat(250000))]});`,
    body:String.raw`let result; for(let i=0;i<8;i++) result=project(value); check(result.includes('"text_lines":250001'), 'line count'); return {content:result};`},
  {id:'repeated_projection', setup:String.raw`const value=register({complete:true,results:[raw('source\n'.repeat(20000))]}); const batch=Array(32).fill(value);`,
    body:String.raw`const result=project(batch); check(result.includes('"same_as_body":1'), 'lost duplicate provenance'); return {content:result};`},
  {id:'shared_range_hydration', setup:String.raw`
    const body = 'xλ😀\n'.repeat(100000), size=utf8Length(body);
    const value = register({artifact_id:'a',source_sha256:'h',results:[raw(body),...Array.from({length:80},(_,i)=>({
      status:'ok',complete:true,shared:true,canonical_range:{start:size-(80-i)*8,end:size-(79-i)*8}}))]});
    const selected = value.results.slice(1);`,
    body:String.raw`const result=project(selected); check(!result.includes('"shared":true') && result.includes('xλ😀\n'), 'unresolved shared ranges'); return {content:result};`},
];
const results = [];
for (const test of cases) {
  const wallMs = [], observations = [];
  for (let iteration = 0; iteration < runs; iteration++) {
    const context = vm.createContext({tools:{},ALL_TOOL_NAMES:[],setTimeout,clearTimeout,performance,
      isProxy:types.isProxy,utf8Length:value=>Buffer.byteLength(value)});
    vm.runInContext(common + '\n' + test.setup, context, {timeout:10000});
    const start = performance.now();
    let deadline, result;
    try {
      result = await Promise.race([vm.runInContext(`(async()=>{${test.body}})()`,context,{timeout:10000}),
        new Promise((_,reject)=>{deadline=setTimeout(()=>reject(Error(`${test.id} timed out`)),15000);})]);
    } finally { clearTimeout(deadline); }
    wallMs.push(performance.now()-start);
    if (result.content !== undefined) {
      result.contentBytes=Buffer.byteLength(result.content);
      result.contentSha256=createHash('sha256').update(result.content).digest('hex'); delete result.content;
    }
    observations.push(result);
  }
  const sorted=[...wallMs].sort((a,b)=>a-b), half=Math.floor(runs/2);
  results.push({id:test.id,wallMs,medianMs:runs%2?sorted[half]:(sorted[half-1]+sorted[half])/2,observations});
}
const report={schemaVersion:1,engine:process.version,runs,
  sources:names.map((path,i)=>({path,sha256:createHash('sha256').update(sources[i]).digest('hex')})),results,
  limitations:['Scripted production JS helpers in Node/V8; no provider or native tool dispatch.',
    'Operation counts and byte fingerprints are exact for these fixtures, not audit recall or task completion.',
    'No assertion of live model round, validation duration, or end-to-end turn speedup.']};
const serialized=JSON.stringify(report,null,2)+'\n';
if(output) await writeFile(output,serialized,{flag:'wx'});
console.log(serialized);
