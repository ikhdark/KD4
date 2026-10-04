#!/usr/bin/env node
// No external model: execute the production JS helpers and their shared V8
// regression fixture. Timings are local helper/test overhead, NOT turn speedups.
import { readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import vm from 'node:vm';

const args = process.argv.slice(2);
if (args.includes('--help')) {
  console.log('Usage: node scripts/benchmark_code_mode_handoffs.mjs [--runs N] [--output PATH]\nRuns the same orchestration fixture as the native V8 test. No model, network, or repository mutation. Output includes source hashes, individual timings and coverage limits.');
  process.exit(0);
}
let runs = 5, output;
for (let i = 0; i < args.length; i += 2) {
  if (args[i] === '--runs') runs = Number(args[i + 1]);
  else if (args[i] === '--output' && args[i + 1]) output = args[i + 1];
  else throw Error(`unknown/missing argument: ${args[i]}`);
}
if (!Number.isInteger(runs) || runs < 1 || runs > 100) throw Error('--runs must be 1–100');
const root = new URL('../codex-rs/code-mode/src/runtime/', import.meta.url);
const files = ['dependency_graph.js', 'orchestration.js', 'orchestration_tests.js'];
const sources = await Promise.all(files.map(name => readFile(new URL(name, root), 'utf8')));
const script = new vm.Script(`(async () => {\n${sources.join('\n')}\n})()`);
const wallMs = [];
for (let i = 0; i < runs; i++) {
  const messages = [];
  const context = vm.createContext({ tools: {}, ALL_TOOL_NAMES: [], setTimeout, clearTimeout,
    text: value => messages.push(value) });
  const start = performance.now();
  await script.runInContext(context, { timeout: 5000 });
  wallMs.push(performance.now() - start);
  if (messages.length !== 1 || messages[0] !== 'orchestration scenarios passed') {
    throw Error(`incomplete fixture: ${JSON.stringify(messages)}`);
  }
}
const sorted = [...wallMs].sort((a, b) => a - b);
const report = {
  schemaVersion: 1, engine: process.version, runs, wallMs,
  medianMs: sorted.length % 2 ? sorted[Math.floor(sorted.length / 2)]
    : (sorted[sorted.length / 2 - 1] + sorted[sorted.length / 2]) / 2,
  sources: files.map((path, i) => ({ path, bytes: Buffer.byteLength(sources[i]),
    sha256: createHash('sha256').update(sources[i]).digest('hex') })),
  passed: true,
  coverage: ['bounded concurrency', 'batch deduplication', 'settled partial failure',
    'immutable UTF-8/CRLF recovery', 'gaps/hash drift/no-progress stops',
    'passive command drainage', 'all output packets retained', 'failure/input/handle stops'],
  limitations: ['Scripted tool responses, not real provider inference or native tool dispatch.',
    'No claim about live model request selection, tokens, cost, or turn wall-clock speedup.',
    'Native core direct-delivery tests separately assert actual mock-provider request counts and answer persistence.'],
};
const text = JSON.stringify(report, null, 2) + '\n';
if (output) await writeFile(output, text, { flag: 'wx' });
console.log(text);
