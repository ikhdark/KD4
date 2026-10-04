// Controlled tool-service fixtures. No model/provider requests or checkout writes.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
import vm from 'node:vm';

if (process.argv.includes('--help')) {
  console.log('Usage: node scripts/benchmark_harness_continuations.mjs [runs=5]\n'
    + 'Prints JSON. Synthetic 5 ms tool services; wall time excludes model inference.\n'
    + 'Compares duplicate reads, snapshot recovery and command draining with identical evidence.');
  process.exit(0);
}
const runs = Number(process.argv[2] ?? 5);
assert(Number.isInteger(runs) && runs >= 1 && runs <= 100);
const sha = value => createHash('sha256').update(value).digest('hex');
const sources = Object.fromEntries(['dependency_graph.js', 'orchestration.js'].map(name =>
  [name, readFileSync(new URL('../codex-rs/code-mode/src/runtime/' + name, import.meta.url), 'utf8')]));
const paths = ['source', 'tests', 'config', 'guide'];
const contents = Object.fromEntries(paths.map(path => [path, path + '\r\nλ😀\r\n']));
const source = 'first\r\nλ😀\r\nlast\r\n';
const chunks = ['first\r\n', 'λ😀\r\n', 'last\r\n'];
const size = Buffer.byteLength(source);
const part = (text, start = 0) => ({ status: 'ok', complete: true, text,
  canonical_range: { start, end: start + Buffer.byteLength(text) } });
const packet = index => index < 3
  ? { execution_state: 'running', process_exited: false, session_id: 7,
      session_capabilities: { polling: true }, output: `diagnostic-${index}\n` }
  : { execution_state: 'exited', process_exited: true, exit_code: 0, output: '' };
const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)];

async function measure(scenario, candidate) {
  const counts = { read_file: 0, read_tool_output: 0, write_stdin: 0 };
  let polls = 0, active = 0, peak = 0, boundaries = 0;
  const service = (name, operation) => async args => {
    ++counts[name]; peak = Math.max(peak, ++active);
    try {
      await new Promise(resolve => setTimeout(resolve, 5));
      return operation(args);
    } finally { --active; }
  };
  const tools = {
    read_file: service('read_file', ({ path }) => scenario !== 'snapshot-recovery'
      ? { complete: true, file_complete: true, results: [part(contents[path])] }
      : { complete: true, file_complete: false, source_sha256: sha(source),
          canonical_bytes: size, artifact_id: 'snapshot', retained_artifact_complete: true,
          results: [part(chunks[0])], continuation: { kind: 'bytes',
            start: Buffer.byteLength(chunks[0]), end: size } }),
    read_tool_output: service('read_tool_output', ({ artifact_id, selectors }) => {
      assert.equal(artifact_id, 'snapshot');
      const offset = selectors[0].start;
      const index = offset === Buffer.byteLength(chunks[0]) ? 1 : 2;
      assert.equal(offset, Buffer.byteLength(chunks.slice(0, index).join('')));
      assert.equal(selectors[0].end, size);
      const end = offset + Buffer.byteLength(chunks[index]);
      return { artifact_id, canonical_sha256: sha(source), canonical_bytes: size,
        complete: end === size, results: [part(chunks[index], offset)],
        ...(end < size ? { continuation_stop: { reason: 'budget', resumable: true,
          selector: { kind: 'bytes', start: end, end: size } } } : {}) };
    }),
    write_stdin: service('write_stdin', args => {
      assert.equal(args.session_id, 7); assert.equal(args.wait_for_output, true);
      return packet(++polls);
    }),
  };
  const context = vm.createContext({ tools, ALL_TOOL_NAMES: Object.keys(tools) });
  for (const text of Object.values(sources)) vm.runInContext(text, context);
  const started = performance.now();
  let evidence;
  if (scenario === 'retain-complete-batch') {
    if (candidate) {
      const rows = await context.read_files(paths);
      assert(rows.every(row => row.status === 'fulfilled' && row.value.file_complete));
      evidence = Array.from(rows, row => row.value.initial.results[0].text);
      ++boundaries;
    } else {
      // Reproduce the observed early-break loss: three completed siblings are
      // not retained, so the next cell fetches them again. Fixed source only.
      const batch = await Promise.all(paths.map(path => tools.read_file({ path })));
      ++boundaries;
      const reread = await Promise.all(paths.slice(1).map(path => tools.read_file({ path })));
      evidence = [batch[0], ...reread].map(row => row.results[0].text);
      ++boundaries;
    }
    assert.deepEqual(evidence, Object.values(contents));
  } else if (scenario === 'snapshot-recovery') {
    let pages;
    if (candidate) {
      const [row] = await context.read_files(['source'], { full: true });
      assert.equal(row.status, 'fulfilled'); assert(row.value.file_complete);
      pages = [row.value.initial, ...row.value.pages];
      ++boundaries;
    } else {
      pages = [await tools.read_file({ path: 'source' })]; ++boundaries;
      for (let start = Buffer.byteLength(chunks[0]); start < size;) {
        const page = await tools.read_tool_output({ artifact_id: 'snapshot',
          selectors: [{ kind: 'bytes', start, end: size }] });
        pages.push(page); ++boundaries;
        start = page.results[0].canonical_range.end;
      }
    }
    evidence = pages.flatMap(page => page.results).map(p => p.text).join('');
    assert.equal(evidence, source);
  } else {
    let observations;
    if (candidate) {
      const result = await context.await_command(packet(0));
      observations = Array.from(result.observations); ++boundaries;
    } else {
      observations = [packet(0)]; ++boundaries;
      while (observations.at(-1).session_id != null) {
        observations.push(await tools.write_stdin({ session_id: 7, wait_for_output: true }));
        ++boundaries;
      }
    }
    assert.equal(observations.at(-1).exit_code, 0);
    evidence = observations.map(p => p.output).join('');
    assert.equal(evidence, 'diagnostic-0\ndiagnostic-1\ndiagnostic-2\n');
  }
  assert.equal(active, 0);
  return { wall_ms: performance.now() - started, tool_calls: counts, peak,
    scripted_cell_boundaries: boundaries, evidence_sha256: sha(JSON.stringify(evidence)) };
}

const measurements = [];
for (const scenario of ['retain-complete-batch', 'snapshot-recovery', 'command-drain']) {
  const baseline = [], candidate = [];
  for (let i = 0; i < runs; ++i) {
    for (const optimized of i % 2 ? [true, false] : [false, true]) {
      (optimized ? candidate : baseline).push(await measure(scenario, optimized));
    }
  }
  assert.equal(new Set([...baseline, ...candidate].map(row => row.evidence_sha256)).size, 1);
  measurements.push({ scenario, baseline, candidate,
    median_baseline_ms: median(baseline.map(row => row.wall_ms)),
    median_candidate_ms: median(candidate.map(row => row.wall_ms)) });
}
console.log(JSON.stringify({ scope: 'Synthetic service timings; scripted boundaries are not measured model requests. No live-model speedup claim.',
  runs, node: process.version, platform: process.platform,
  source_sha256: Object.fromEntries(Object.entries(sources).map(([name, text]) => [name, sha(text)])),
  measurements }, null, 2));
