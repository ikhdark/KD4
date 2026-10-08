#!/usr/bin/env node
// Production-source no-model probes. Timings are not task/turn speedups.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFile, writeFile} from 'node:fs/promises';
import {performance} from 'node:perf_hooks';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
import vm from 'node:vm';
const args = process.argv.slice(2);
if (args.includes('--help')) {
  console.log('Usage: node scripts/benchmark_harness_serialization.mjs [--runs N] [--baseline-dir PATH] [--scenario NAME] [--output PATH]\nA baseline directory supplies baseline-dependency_graph.js, baseline-orchestration.js and baseline-output_projection.rs. Runs production helpers/projector with no-model deterministic fixtures; no end-to-end speedup claim.');
  process.exit(0);
}
let runs = 7, baseline, output, selectedScenario;
for (let i = 0; i < args.length; i += 2) {
  if (args[i] === '--runs') runs = Number(args[i + 1]);
  else if (args[i] === '--baseline-dir' && args[i + 1]) baseline = args[i + 1];
  else if (args[i] === '--output' && args[i + 1]) output = args[i + 1];
  else if (args[i] === '--scenario' && args[i + 1]) selectedScenario = args[i + 1];
  else throw Error('unknown/missing argument: ' + args[i]);
}
assert(Number.isInteger(runs) && runs >= 1 && runs <= 100, 'runs must be 1–100');
const runtime = fileURLToPath(new URL('../codex-rs/code-mode/src/runtime/', import.meta.url));
const names = ['dependency_graph.js', 'orchestration.js', 'output_projection.rs'];
const sources = await Promise.all(names.map(name =>
  readFile(path.join(baseline || runtime, baseline ? 'baseline-' + name : name), 'utf8')));
const match = sources[2].match(/const PROJECTOR: &str = r#"(.*?)"#;/s);
assert(match, 'production projector not found');
assert.equal((match[1].match(/return function project\(/g) || []).length, 1);
const instrumented = match[1].replace('return function project(',
  'globalThis.probes = {escapes, lineCount}; return function project(');
const context = vm.createContext({tools:{}, ALL_TOOL_NAMES:[], setTimeout, clearTimeout,
  text(){}, fixtureDelay:ms => new Promise(resolve => setTimeout(resolve, ms))});
new vm.Script(sources[0] + '\n' + sources[1]).runInContext(context);
context.project = new vm.Script(instrumented).runInContext(context)(() => false);
assert.equal((match[1].match(/const lineCount =/g) || []).length, 1);
const counted = match[1].replace('const lineCount =', 'const nativeLineCount =')
  .replace('return function project(',
    'const lineCount = text => { ++globalThis.lineCountCalls; return nativeLineCount(text); }; return function project(');
context.countedProject = new vm.Script(counted).runInContext(context)(() => false);
const scenarios = [];
async function measure(name, source) {
  if (selectedScenario && selectedScenario !== name) return;
  const script = new vm.Script('(async () => {' + source + '})()');
  await script.runInContext(context);
  const wallMs = [], details = [];
  for (let i = 0; i < runs; ++i) {
    const start = performance.now();
    details.push(await script.runInContext(context));
    wallMs.push(performance.now() - start);
  }
  const sorted = [...wallMs].sort((a,b) => a-b), mid = Math.floor(sorted.length / 2);
  scenarios.push({name, wallMs, medianMs:sorted.length % 2 ? sorted[mid] :
    (sorted[mid-1] + sorted[mid]) / 2, details});
}
await measure('graph_completion_frontiers', String.raw`
  let release, races = 0, attached = 0;
  const held = new Promise(resolve => {release = resolve;});
  const original = Promise.race;
  Promise.race = function(values) {
    const batch = [...values]; ++races; attached += batch.length;
    return original.call(this, batch);
  };
  try {
    const nodes = [{id:'held', run:() => held, accept:() => true},
      ...Array.from({length:240}, (_,i) => ({id:String(i),
        run:() => {if(i === 239) release(); return i;}, accept:() => true}))];
    if(Object.keys(await run_graph(nodes,{concurrency:2})).length !== 241) throw Error('work lost');
    return {nodes:241, races, attached};
  } finally {Promise.race = original;}
`);
await measure('graph_dependency_readiness', String.raw`
  let scans = 0;
  const original = Object.hasOwn;
  Object.hasOwn = (...args) => {++scans; return original(...args);};
  try {
    const nodes = Array.from({length:256}, (_,i) => ({id:String(i),
      deps:i === 255 ? [] : [String(i+1)], run:() => i, accept:() => true}));
    const result = await run_graph(nodes,{concurrency:1});
    if(Object.keys(result).length !== 256 || result['0'].value !== 0) throw Error('chain lost');
    return {nodes:256, resultPresenceChecks:scans};
  } finally {Object.hasOwn = original;}
`);
await measure('graph_capability_preflight', String.raw`
  ALL_TOOL_NAMES = Array.from({length:1024}, (_,i) => 'cap-' + i);
  const requires = ALL_TOOL_NAMES.slice(-128);
  const nodes = Array.from({length:64}, (_,i) => ({id:String(i), requires,
    run:() => i, accept:() => true}));
  if(Object.keys(await run_graph(nodes)).length !== 64) throw Error('work lost');
  return {nodes:64, requiredNames:8192, catalogNames:1024};
`);
await measure('full_read_utf8_validation', String.raw`
  ALL_TOOL_NAMES = ['read_file','read_tool_output'];
  const text = 'abcdλ😀'.repeat(4096), size = 10*4096;
  let calls = 0;
  const raw = (text,start,end) => ({status:'ok',complete:true,text,canonical_range:{start,end}});
  tools = {
    read_file:async ({path}) => ({complete:true,file_complete:false,
      source_sha256:'fixture',artifact_id:path,retained_artifact_complete:true,
      canonical_bytes:size,results:[raw(text.slice(0,7),0,10)],
      continuation:{kind:'bytes',start:10,end:size}}),
    read_tool_output:async ({artifact_id}) => {++calls; return {complete:true,artifact_id,
      canonical_sha256:'fixture',canonical_bytes:size,results:[raw(text.slice(7),10,size)]};},
  };
  const result = await read_files(Array.from({length:32},(_,i) => String(i)),{full:true});
  if(!result.every(row => row.status === 'fulfilled' && row.value.file_complete) || calls !== 32)
    throw Error('UTF-8 coverage lost');
  return {files:32,recoveryCalls:calls,verifiedBytes:32*size};
`);
await measure('projection_escape_detection', String.raw`
  const text = 'x'.repeat(1024*1024);
  for(let i=0;i<100;++i) if(probes.escapes(text)) throw Error('unescaped text changed');
  for(const text of ['"','\\','\0','\n','\ud800','\udfff'])
    if(!probes.escapes(text)) throw Error('escaping lost');
  if(probes.escapes('λ😀')) throw Error('Unicode escaping changed');
  return {iterations:100,bytesPerText:text.length};
`);
await measure('projection_line_count', String.raw`
  const text = 'line\n'.repeat(30000);
  for(let i=0;i<100;++i) if(probes.lineCount(text) !== 30001) throw Error('line count changed');
  if(probes.lineCount('') !== 1 || probes.lineCount('\r\n') !== 2) throw Error('boundary changed');
  return {iterations:100,linesPerText:30001};
`);
await measure('projection_sparse_shared_ranges', String.raw`
  const body = 'x\n'.repeat(50000), count = 64;
  const source = {path:'fixture.rs',source_sha256:'fixture',artifact_id:'fixture',results:[]};
  source.results = Array.from({length:count},(_,i) => ({
    selector:{kind:'bytes',start:i*32,end:(i+1)*32},
    canonical_range:i === 0 ? {start:0,end:100000} :
      {start:80000+i*32,end:80000+(i+1)*32},
    complete:true,status:'ok',...(i === 0 ? {text:body} : {shared:true}),
  }));
  project(source,JSON.parse(JSON.stringify(source)),JSON.parse(JSON.stringify(source)),true);
  const printed = project(source.results.slice(1));
  if(printed.includes('"shared":true') || printed.includes('"recovery"') ||
      printed.split('[source ').length !== count) throw Error('shared coverage lost');
  return {ranges:count-1,parentBytes:body.length,renderedCharacters:printed.length};
`);
await measure('projection_frame_line_count', String.raw`
  const body = 'line\n'.repeat(1024);
  const value = {path:'fixture',results:[{status:'ok',complete:true,text:body,
    canonical_range:{start:0,end:body.length},selector:{kind:'bytes',start:0,end:body.length}}]};
  countedProject(value,JSON.parse(JSON.stringify(value)),JSON.parse(JSON.stringify(value)),true);
  globalThis.lineCountCalls = 0;
  const printed = countedProject(value);
  if(!printed.includes('"text_lines":1025') || printed.split('line\n').length !== 1025)
    throw Error('framed line coverage changed');
  return {lineCountCalls,sourceBytes:body.length,lines:1025};
`);
await measure('projection_repeated_frames', String.raw`
  const body = 'line\n'.repeat(30000);
  const value = {path:'fixture',results:[{status:'ok',complete:true,text:body,
    canonical_range:{start:0,end:body.length},selector:{kind:'bytes',start:0,end:body.length}}]};
  project(value,JSON.parse(JSON.stringify(value)),JSON.parse(JSON.stringify(value)),true);
  const printed = project(Array(32).fill(value));
  if((printed.match(/same_as_body/g)||[]).length !== 31 ||
      printed.split('line\n').length !== 30001) throw Error('duplicate evidence changed');
  return {presentations:32,sourceBytes:body.length,renderedCharacters:printed.length};
`);
await measure('full_read_initial_recovery_lane', String.raw`
  ALL_TOOL_NAMES = ['read_file','read_tool_output'];
  const started = Date.now(), calls = [];
  tools = {
    read_file:async ({path}) => {
      calls.push({path,atMs:Date.now()-started});
      await fixtureDelay(path === 'slow-source' ? 80 : 2);
      if(path === 'slow-source') return {complete:true,file_complete:true,canonical_bytes:1,
        results:[{status:'ok',complete:true,text:'z',canonical_range:{start:0,end:1}}]};
      return {complete:true,file_complete:false,canonical_bytes:2,source_sha256:'fixture',
        artifact_id:path,retained_artifact_complete:true,
        results:[{status:'ok',complete:true,text:'x',canonical_range:{start:0,end:1}}],
        continuation:{kind:'bytes',start:1,end:2}};
    },
    read_tool_output:async ({artifact_id}) => {
      await fixtureDelay(30);
      return {complete:true,artifact_id,canonical_sha256:'fixture',canonical_bytes:2,
        results:[{status:'ok',complete:true,text:'y',canonical_range:{start:1,end:2}}]};
    },
  };
  const results = await read_files(['a','b','slow-source'],{full:true,concurrency:2});
  if(!results.every(row => row.status === 'fulfilled' && row.value.file_complete))
    throw Error('pipeline coverage lost');
  return {initialReads:calls,allSettled:true};
`);
await measure('command_wait_budget_floor', String.raw`
  const originalNow = Date.now;
  let clock = 0, polls = 0;
  Date.now = () => clock;
  ALL_TOOL_NAMES = ['write_stdin'];
  const initial = {execution_state:'running',process_exited:false,session_id:1,
    session_capabilities:{polling:true,incarnation:'fixture'}};
  tools = {write_stdin:async args => {
    ++polls; clock += Math.max(5000,args.yield_time_ms); return initial;
  }};
  try {
    await await_command(initial,{max_wait_ms:5000,on_progress:() => {++clock; return true;}});
    throw Error('running process incorrectly completed');
  } catch(error) {
    if(!error.message.includes('wait budget reached') || error.evidence?.terminal !== initial)
      throw error;
    return {budgetMs:5000,progressMs:1,nativeWaitFloorMs:5000,polls,
      elapsedVirtualMs:clock,retainedHandle:true};
  } finally {Date.now = originalNow;}
`);
assert(scenarios.length, 'unknown scenario: ' + selectedScenario);
const report = {schemaVersion:1,engine:process.version,runs,
  variant:baseline ? 'captured-baseline' : 'current',
  sources:names.map((name,i) => ({name,bytes:Buffer.byteLength(sources[i]),
    sha256:createHash('sha256').update(sources[i]).digest('hex')})),
  scenarios,passed:true,limitations:[
    'Deterministic no-model fixtures, not provider/task/turn latency measurements.',
    'Timings include assertions; no claim about live-model request count or answer quality.',
    'Read-only completion contention is measured separately through the owning Rust test.']};
const serialized = JSON.stringify(report,null,2) + '\n';
if(output) await writeFile(output,serialized,{flag:'wx'});
console.log(serialized);
