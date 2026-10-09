#!/usr/bin/env node
// No model or Cargo build. Exercise the production JSON codec, then a real
// child -> pipe collection -> codec -> registered display pipeline.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {readFile, writeFile} from 'node:fs/promises';
import {performance} from 'node:perf_hooks';
import {fileURLToPath} from 'node:url';
import {types} from 'node:util';
import vm from 'node:vm';

const fixtures = {
  small: () => ({output:'done\n', exit_code:0, process_exited:true}),
  rows: () => ({results:Array.from({length:10000}, (_,line) =>
    ({line, text:'source evidence λ😀\r\n', complete:true}))}),
  text: () => ({output:'source evidence λ😀\r\n'.repeat(24000), exit_code:0}),
  fallback: () => ({results:Array.from({length:10000}, (_,line) =>
    ({line, text:'1234567890123456', complete:true}))}),
};
const args = process.argv.slice(2);
if (args[0] === '--emit') {
  assert(Object.hasOwn(fixtures, args[1]));
  process.stdout.write(JSON.stringify(fixtures[args[1]]()));
} else if (args.includes('--help')) {
  console.log('Usage: node scripts/benchmark_tool_transport.mjs --baseline FILE --output FILE [--runs N]\nAlternates an immutable baseline output_projection.rs with current production source. Retains all samples and hashes; validates exact integer and JSON behavior. Pipeline excludes app-server/Rust dispatch, model/network latency and durable artifact I/O.');
} else {
  let baseline, output, runs = 9;
  for (let i = 0; i < args.length; i += 2) {
    if (args[i] === '--baseline' && args[i+1]) baseline = args[i+1];
    else if (args[i] === '--output' && args[i+1]) output = args[i+1];
    else if (args[i] === '--runs') runs = Number(args[i+1]);
    else throw Error('unknown or missing argument: ' + args[i]);
  }
  assert(baseline && output && Number.isInteger(runs) && runs >= 1 && runs <= 100);
  const sourcePath = new URL('../codex-rs/code-mode/src/runtime/output_projection.rs', import.meta.url);
  const sources = await Promise.all([readFile(baseline,'utf8'), readFile(sourcePath,'utf8')]);
  const codecs = sources.map(source => {
    const match = source.match(/const PROJECTOR: &str = r#"(.*?)"#;/s);
    assert(match, 'missing production projector');
    const context = vm.createContext({});
    const project = new vm.Script(match[1]).runInContext(context)(types.isProxy);
    return {context, project};
  });
  const digest = value => createHash('sha256').update(value).digest('hex');
  const encode = value => JSON.stringify(value, (_key,item) =>
    typeof item === 'bigint' ? {$bigint:item.toString()} : item);
  // Expected integers come from the exact JSON lexeme, never rounded Numbers.
  const corpus = ['null','true','"λ😀\\r\\n"','[]','{}','-0','1.5','1e100',
    '9007199254740991','9007199254740992','9007199254740993',
    '-9007199254740993','18446744073709551615','-9223372036854775808',
    '9007199254740993.0','9007199254740993e0',
    '{"__proto__":{"x":1},"digits":"12345678901234567890"}',
    '[{"x":42},null,"\\u0031\\u0032",-0,0.25,1e20]'];
  for (let i=0;i<1000;++i) corpus.push(String(9007199254740500n + BigInt(i)));
  for (const input of corpus) {
    const native = JSON.parse(input);
    const expected = /^-?[0-9]+$/.test(input) && typeof native === 'number' && !Number.isSafeInteger(native)
      ? BigInt(input) : native;
    for (const {project} of codecs) {
      const actual = project(input,'parse');
      assert.equal(encode(actual),encode(expected),input);
      if (input === '-0') assert(Object.is(actual,-0));
    }
  }
  for (const input of ['[','{"x":}','01','NaN','{"x":1,}']) {
    for (const {project} of codecs) assert.throws(() => project(input,'parse'),{name:'SyntaxError'});
  }
  // Generated nested packets check precision, strings, arrays and prototype keys
  // against the exact baseline transport behavior, not just display equality.
  for (const input of corpus) {
    const packet = '{"value":' + input + ',"nested":[' + input + ']}';
    assert.equal(encode(codecs[0].project(packet,'parse')),encode(codecs[1].project(packet,'parse')));
  }
  // User code may replace RegExp/JSON methods after runtime initialization.
  const guarded = codecs[1];
  new vm.Script(`RegExp.prototype.test = RegExp.prototype.exec = () => {throw Error('overridden regex');};
    JSON.parse = () => {throw Error('overridden parse');};`).runInContext(guarded.context);
  assert.equal(guarded.project('9007199254740993','parse'),9007199254740993n);
  assert.equal(guarded.project('{"x":42}','parse').x,42);

  const records = [];
  const self = fileURLToPath(import.meta.url);
  async function child(name) {
    const start = performance.now();
    return await new Promise((resolve,reject) => {
      const proc = spawn(process.execPath,[self,'--emit',name],{windowsHide:true,stdio:['ignore','pipe','pipe']});
      const stdout=[], stderr=[];
      let spawnMs, bytes=0, exceeded=false;
      const deadline = setTimeout(() => {exceeded=true; proc.kill();},30000);
      proc.once('spawn',() => {spawnMs=performance.now()-start;});
      proc.stdout.on('data',chunk => {
        bytes += chunk.length;
        if (bytes > 8*1024*1024) {exceeded=true; proc.kill();} else stdout.push(chunk);
      });
      proc.stderr.on('data',chunk => stderr.push(chunk));
      proc.once('error',error => {clearTimeout(deadline); reject(error);});
      proc.once('close',(code,signal) => {
        clearTimeout(deadline);
        try {
          assert(!exceeded,'bounded child exceeded deadline/output limit');
          assert.equal(code,0); assert.equal(signal,null); assert.equal(Buffer.concat(stderr).length,0);
          resolve({text:Buffer.concat(stdout).toString('utf8'),childWallMs:performance.now()-start,spawnMs});
        } catch(error) {reject(error);}
      });
    });
  }
  for (const name of Object.keys(fixtures)) {
    const input=JSON.stringify(fixtures[name]()), inputHash=digest(input);
    let expectedDisplay;
    for (let sample=-1;sample<runs;++sample) {
      for (const version of sample%2 ? [1,0] : [0,1]) {
        const {project}=codecs[version];
        const cpu=process.cpuUsage(), start=performance.now();
        let result;
        for (let i=0;i<20;++i) result=project(input,'parse');
        const wallMs=performance.now()-start, used=process.cpuUsage(cpu);
        assert.equal(JSON.stringify(result),input);
        if (sample>=0) records.push({phase:'parse',name,version,sample,iterations:20,wallMs,
          parentCpuMs:(used.user+used.system)/1000,bytes:Buffer.byteLength(input),inputHash});
        const pipelineCpu=process.cpuUsage(), pipelineStart=performance.now();
        const collected=await child(name);
        assert.equal(digest(collected.text),inputHash,'complete child bytes');
        const parseStart=performance.now();
        const value=project(collected.text,'parse');
        // Mirrors resolve_tool_response + register: live result, immutable raw,
        // and display result are parsed separately, as in production.
        const original=project(collected.text,'parse'), display=project(collected.text,'parse');
        const parseMs=performance.now()-parseStart;
        const projectStart=performance.now();
        project(value,original,display,true);
        const rendered=project(value);
        const projectionMs=performance.now()-projectStart;
        const totalMs=performance.now()-pipelineStart, pipelineUsed=process.cpuUsage(pipelineCpu);
        expectedDisplay ??= rendered;
        assert.equal(rendered,expectedDisplay,'output projection changed');
        if(sample>=0) records.push({phase:'pipeline',name,version,sample,wallMs:totalMs,
          parentCpuMs:(pipelineUsed.user+pipelineUsed.system)/1000,childWallMs:collected.childWallMs,
          spawnMs:collected.spawnMs,parseMs,projectionMs,inputHash,displayHash:digest(rendered)});
      }
    }
  }
  const groups={};
  for(const record of records) (groups[[record.phase,record.name,record.version].join('/')] ??= []).push(record.wallMs);
  const median = values => {const sorted=[...values].sort((a,b)=>a-b), i=sorted.length>>1;
    return sorted.length%2 ? sorted[i] : (sorted[i-1]+sorted[i])/2;};
  const mediansMs=Object.fromEntries(Object.entries(groups).map(([key,values]) => [key,median(values)]));
  const report={schemaVersion:1,engine:process.version,runs,variant:'0=baseline, 1=current; alternating paired order',
    sources:sources.map(source => ({bytes:Buffer.byteLength(source),sha256:digest(source)})),
    assertions:{corpus:corpus.length,malformed:5,nested:corpus.length,prototypeOverrides:true,completeChildBytes:true,identicalDisplay:true},
    limitations:['No model/network or app-server/Rust dispatch. Pipeline is child creation through registered display, not a turn speedup.',
      'Parent CPU excludes child CPU. Child wall includes startup, execution and collection. spawnMs is parent spawn-event latency, not OS scheduler time.',
      'No durable artifacts or slow-storage simulation; no process/scheduling policy is changed.'],mediansMs,records};
  await writeFile(output,JSON.stringify(report,null,2)+'\n',{flag:'wx'});
  console.log(JSON.stringify({report:output,mediansMs,assertions:report.assertions}));
}
