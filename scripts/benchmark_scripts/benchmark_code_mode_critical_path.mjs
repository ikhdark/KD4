// Narrow no-model probes of production scheduler, recovery and projection owners.
// Loaded by benchmark_code_mode_handoffs.mjs; all tool responses are scripted.
import { readFile } from 'node:fs/promises';
import { performance } from 'node:perf_hooks';
import { types } from 'node:util';
import vm from 'node:vm';

const check = (ok, why) => { if (!ok) throw Error(why); };
const median = values => {
  const ordered = [...values].sort((a, b) => a - b), middle = ordered.length >> 1;
  return ordered.length % 2 ? ordered[middle] : (ordered[middle - 1] + ordered[middle]) / 2;
};

export async function measureCriticalPath(root, runs, snapshot) {
  const files = ['dependency_graph.js', 'orchestration.js', 'output_projection.rs'];
  let sources;
  if (snapshot) {
    check(snapshot.kind === 'retained_direct_file_read' && Array.isArray(snapshot.sources) &&
      snapshot.sources.length === files.length, 'invalid source snapshot');
    const {createHash} = await import('node:crypto');
    sources = files.map(path => {
      const row = snapshot.sources.find(source => source.path === path);
      check(row && typeof row.text === 'string' && Buffer.byteLength(row.text) === row.bytes &&
        createHash('sha256').update(row.text).digest('hex') === row.sha256, 'source snapshot identity mismatch');
      return row.text;
    });
  } else sources = await Promise.all(files.map(name => readFile(new URL(name, root), 'utf8')));
  const projector = sources[2].match(/const PROJECTOR: &str = r#"([\s\S]*?)"#;/)?.[1];
  check(projector, 'production output projector not found');
  const scenarios = [];
  const probe = async (name, source, setup = '') => {
    const timings = [], samples = [];
    const script = new vm.Script(`(async () => {
      ${setup}
      ${sources[0]}
      ${sources[1]}
      const project = (${projector})(isProxy);
      ${source}
    })()`);
    for (let i = 0; i < runs + 1; i++) {
      const context = vm.createContext({tools: {}, ALL_TOOL_NAMES: [], setTimeout, clearTimeout,
        setImmediate, isProxy: types.isProxy, check});
      const started = performance.now();
      let timer;
      try {
        const sample = await Promise.race([
          script.runInContext(context, {timeout: 5000}),
          new Promise((_, reject) => { timer = setTimeout(() => reject(Error(name + ' timed out')), 5000); }),
        ]);
        if (i) { timings.push(performance.now() - started); samples.push(sample); }
      } finally { clearTimeout(timer); }
    }
    scenarios.push({name, medianMs: median(timings), wallMs: timings, samples});
  };
  await probe('graph_race_subscriptions', `
    let release, subscriptions = 0;
    const originalRace = Promise.race;
    Promise.race = values => {
      const all = [...values]; subscriptions += all.length;
      return originalRace.call(Promise, all);
    };
    const slow = new Promise(resolve => { release = resolve; });
    let finished = 0;
    const nodes = [{id:'slow', run:()=>slow, accept:()=>true},
      ...Array.from({length:255}, (_,i) => ({id:String(i), run:async()=>{
        await new Promise(setImmediate);
        if (++finished === 255) release();
        return i;
      }, accept:()=>true}))];
    const results = await run_graph(nodes, {concurrency:16});
    check(Object.keys(results).length === 256 && finished === 255, 'graph lost started work');
    return {subscriptions, completed:256};
  `);
  await probe('graph_dependency_rescans', `
    let checks = 0;
    const originalHas = Object.hasOwn;
    Object.hasOwn = (...args) => { ++checks; return originalHas(...args); };
    for (let trial=0; trial<20; trial++) {
      const nodes = Array.from({length:256}, (_,i)=>({id:String(i),
        deps:i?[String(i-1)]:[], run:d=>i?d[String(i-1)]+1:1, accept:()=>true})).reverse();
      const results = await run_graph(nodes, {concurrency:16});
      check(results['255'].value === 256, 'reverse chain incomplete');
    }
    return {dependencyChecks:checks, completed:5120};
  `);
  await probe('graph_capability_preflight', `
    globalThis.ALL_TOOL_NAMES = Array.from({length:512}, (_,i)=>'tool-'+i);
    let lookups=0;
    const includes = ALL_TOOL_NAMES.includes.bind(ALL_TOOL_NAMES);
    ALL_TOOL_NAMES.includes = name => { ++lookups; return includes(name); };
    const required = ALL_TOOL_NAMES.slice(-128);
    for (let trial=0; trial<10; trial++) {
      const result = await run_graph(Array.from({length:64}, (_,i)=>({
        id:String(i), requires:required, run:()=>i, accept:()=>true})));
      check(Object.keys(result).length === 64, 'capability preflight lost work');
    }
    return {linearLookups:lookups, completed:640};
  `);
  await probe('ranked_writer_convoy', `
    let now=0, writerAt, proofAt;
    const queue=[];
    const wait = ms => new Promise(resolve=>queue.push({at:now+ms,resolve}));
    const node=(id,run,extra={})=>({id,run,accept:()=>true,...extra});
    let done=false;
    const result=run_graph([
      node('held-reader',()=>wait(15),{estimated_ms:1000,resources:{read:['repo']}}),
      node('writer',()=>{writerAt=now;},{resources:{write:['repo']}}),
      node('proof',()=>wait(100).then(()=>{proofAt=now;}),{deps:['writer'],estimated_ms:100}),
      ...Array.from({length:24}, (_,i)=>node('reader-'+i,()=>wait(10),{resources:{read:['repo']}})),
    ],{concurrency:4}).then(r=>{done=true;return r;});
    for (let turn=0; !done && turn<1000; turn++) {
      // Drain all JS continuations before advancing the scripted clock.
      await new Promise(setImmediate);
      if (!queue.length) continue;
      const at=Math.min(...queue.map(item=>item.at)); now=at;
      const due=queue.filter(item=>item.at===at);
      for (const item of due) { queue.splice(queue.indexOf(item),1); item.resolve(); }
    }
    const settled=await result;
    check(done && Object.keys(settled).length===27, 'resource graph incomplete');
    return {writerAtMs:writerAt, proofAtMs:proofAt, completionMs:now};
  `);
  await probe('full_read_utf8_accounting', `
    const size=8*1024*1024, body='x'.repeat(1024*1024);
    let recoveries=0;
    ALL_TOOL_NAMES.push('read_file','read_tool_output');
    tools.read_file=async()=>({complete:true,file_complete:false,canonical_bytes:size,
      source_sha256:'hash',artifact_id:'snapshot',retained_artifact_complete:true,
      results:[{status:'ok',complete:true,text:'x',canonical_range:{start:0,end:1}}],
      continuation:{kind:'bytes',start:1,end:size}});
    tools.read_tool_output=async({artifact_id,selectors})=>{
      ++recoveries;
      const start=selectors[0].start,end=Math.min(start+body.length,size);
      return {artifact_id,canonical_sha256:'hash',canonical_bytes:size,complete:end===size,
        results:[{status:'ok',complete:true,text:body.slice(0,end-start),canonical_range:{start,end}}],
        ...(end<size?{continuation_stop:{reason:'budget',resumable:true,selector:{kind:'bytes',start:end,end:size}}}:{})};
    };
    const [row]=await read_files(['fixture'],{full:true});
    check(row.status==='fulfilled' && row.value.file_complete && recoveries===8,'full coverage lost');
    return {verifiedBytes:size,recoveries};
  `);
  await probe('command_deadline_floor', `
    let now=0,polls=0;
    Date.now=()=>now;
    ALL_TOOL_NAMES.push('write_stdin');
    const handle={execution_state:'running',process_exited:false,session_id:7,
      session_capabilities:{polling:true,incarnation:'original'},output:'retained'};
    tools.write_stdin=async args=>{
      check(args.session_id===7 && args.incarnation==='original','process restarted');
      now+=++polls===1?4999:args.yield_time_ms;
      return handle;
    };
    const error=await await_command(handle,{max_wait_ms:5000}).catch(error=>error);
    check(error.evidence.terminal===handle && error.evidence.observations.length===polls+1,'wait lost evidence');
    return {polls,observedWaitMs:now,overshootMs:Math.max(0,now-5000)};
  `);
  const receipt = `
    const register = raw => {
      const value=JSON.parse(JSON.stringify(raw));
      project(value, JSON.parse(JSON.stringify(raw)), JSON.parse(JSON.stringify(raw)), true);
      return value;
    };
  `;
  await probe('projection_line_count', receipt + `
    const output='line\\n'.repeat(100000), value=register({output,metadata:{complete:true}});
    const rendered=project(value);
    check(rendered.endsWith(output) && rendered.includes('"text_lines":100001'),'line/body coverage lost');
    return {sourceBytes:output.length,renderedBytes:rendered.length,lineSplits:stats.lineSplits};
  `, `
    const stats={lineSplits:0}, split=String.prototype.split;
    String.prototype.split=function(...args){if(args[0]==='\\n') ++stats.lineSplits; return split.apply(this,args);};
  `);
  await probe('projection_escape_detection', receipt + `
    const output='quoted "body" '.repeat(40000), value=register({output});
    stats.stringSerializations=0;
    const rendered=project(value);
    check(rendered.endsWith(output),'escaped text evidence lost');
    return {sourceBytes:output.length,stringSerializations:stats.stringSerializations};
  `, `
    const stats={stringSerializations:0}, stringify=JSON.stringify;
    JSON.stringify=(value,...rest)=>{if(typeof value==='string') ++stats.stringSerializations;return stringify(value,...rest);};
  `);
  await probe('projection_shared_byte_slices', receipt + `
    const text='λ😀\\r\\n'.repeat(65536), size=65536*8;
    const raw={path:'fixture',artifact_id:'snapshot',source_sha256:'hash',
      results:[{status:'ok',complete:true,text,canonical_range:{start:0,end:size}},
        ...Array.from({length:32},(_,i)=>({status:'ok',complete:true,shared:true,
          canonical_range:{start:size-512*(i+1),end:size-512*i}}))]};
    const value=register(raw), rendered=project(value.results.slice(1));
    check(!rendered.includes('"recovery"') && rendered.split('λ😀').length-1===2048,
      'shared slice lost exact UTF-8 coverage');
    return {selectedBytes:32*512,parentBytes:size};
  `);
  await probe('projection_repeated_receipts', receipt + `
    const output='line\\n'.repeat(40000);
    const value=register({output,metadata:Array.from({length:256},(_,i)=>({index:i,complete:true}))});
    stats.descriptors=0;
    const rendered=project(Array(32).fill(value));
    check(rendered.split('"same_as_body"').length-1===31 && rendered.endsWith('\\n'),'repeat framing lost evidence');
    return {rows:32,descriptorChecks:stats.descriptors,renderedBytes:rendered.length};
  `, `
    const stats={descriptors:0}, descriptor=Object.getOwnPropertyDescriptor;
    Object.getOwnPropertyDescriptor=(...args)=>{++stats.descriptors;return descriptor(...args);};
  `);
  return {files,sources,scenarios,wallMs:Array.from({length:runs},(_,i)=>
    scenarios.reduce((sum,scenario)=>sum+scenario.wallMs[i],0))};
}

