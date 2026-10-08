#!/usr/bin/env node
// No external model: execute the production JS helpers and their shared V8
// regression fixture. Timings are local helper/test overhead, NOT turn speedups.
import { readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { performance } from 'node:perf_hooks';
import vm from 'node:vm';

const args = process.argv.slice(2);
if (args.includes('--help')) {
  console.log('Usage: node scripts/benchmark_code_mode_handoffs.mjs [--runs N] [--output PATH] [--profile orchestration|critical-path] [--source-snapshot PATH] [--model-evaluations PATH]\nDefault: the native V8 orchestration fixture. critical-path: ten production-owner probes; --source-snapshot selects authenticated historical source instead of current files. No model, network, or repository mutation. Optionally compare independently reviewed live-model trials against fixtures/uncertainty_evaluation.json. Missing trials never establish semantic safety.');
  process.exit(0);
}
let runs = 5, output, modelEvaluations, sourceSnapshot, profile = 'orchestration';
for (let i = 0; i < args.length; i += 2) {
  if (args[i] === '--runs') runs = Number(args[i + 1]);
  else if (args[i] === '--output' && args[i + 1]) output = args[i + 1];
  else if (args[i] === '--model-evaluations' && args[i + 1]) modelEvaluations = args[i + 1];
  else if (args[i] === '--profile' && args[i + 1]) profile = args[i + 1];
  else if (args[i] === '--source-snapshot' && args[i + 1]) sourceSnapshot = args[i + 1];
  else throw Error(`unknown/missing argument: ${args[i]}`);
}
if (!Number.isInteger(runs) || runs < 1 || runs > 100) throw Error('--runs must be 1–100');
if (!['orchestration', 'critical-path'].includes(profile)) throw Error('unknown benchmark profile');
if (sourceSnapshot && profile !== 'critical-path') throw Error('--source-snapshot requires --profile critical-path');
const root = new URL('../codex-rs/code-mode/src/runtime/', import.meta.url);
let files = ['dependency_graph.js', 'orchestration.js', 'orchestration_tests.js'];
let sources, scenarios;
const wallMs = [];
if (profile === 'critical-path') {
  const { measureCriticalPath } = await import('./benchmark_code_mode_critical_path.mjs');
  const snapshot = sourceSnapshot ? JSON.parse(await readFile(sourceSnapshot, 'utf8')) : undefined;
  const measured = await measureCriticalPath(root, runs, snapshot);
  ({ files, sources, scenarios } = measured);
  wallMs.push(...measured.wallMs);
} else {
  sources = await Promise.all(files.map(name => readFile(new URL(name, root), 'utf8')));
  const script = new vm.Script(`(async () => {\n${sources.join('\n')}\n})()`);
  for (let i = 0; i < runs; i++) {
    const messages = [];
    const context = vm.createContext({ tools: {}, ALL_TOOL_NAMES: [], setTimeout, clearTimeout,
      text: value => messages.push(value) });
    const start = performance.now();
    // vm's timeout bounds synchronous evaluation, not the returned promise.
    // Bound stalled async fixtures too, without leaving a timer after success.
    let deadline;
    try {
      await Promise.race([
        new Promise((_, reject) => {
          deadline = setTimeout(() => reject(Error(`fixture run ${i + 1} timed out after 5000 ms`)), 5000);
        }),
        script.runInContext(context, { timeout: 5000 }),
      ]);
    } finally {
      clearTimeout(deadline);
    }
    wallMs.push(performance.now() - start);
    if (messages.length !== 1 || messages[0] !== 'orchestration scenarios passed') {
      throw Error(`incomplete fixture: ${JSON.stringify(messages)}`);
    }
  }
}
const sorted = [...wallMs].sort((a, b) => a - b);
let uncertaintyEvaluation = { status: 'unmeasured', accepted: false,
  reason: 'Scripted helper/provider fixtures cannot establish model uncertainty handling.' };
if (modelEvaluations) {
  const fixtureBytes = await readFile(new URL('./fixtures/uncertainty_evaluation.json', import.meta.url));
  const fixture = JSON.parse(fixtureBytes);
  const trialBytes = await readFile(modelEvaluations);
  const trials = JSON.parse(trialBytes);
  const metrics = ['wallMs', 'modelRequests', 'toolCalls', 'validationMs', 'recoveries', 'retries'];
  if (!Array.isArray(trials) || !trials.length) throw Error('model evaluations require paired trial records');
  const groups = new Map();
  for (const trial of trials) {
    const scenario = fixture.cases.find(test => test.id === trial.caseId);
    if (!scenario || !['baseline', 'candidate'].includes(trial.variant)
        || (scenario.modes && !scenario.modes.includes(trial.compactionMode))
        || !Number.isInteger(trial.pair) || trial.pair < 0
        || ['model', 'provider', 'revision', 'reviewer'].some(key => typeof trial[key] !== 'string' || !trial[key].trim())
        || !/^[a-f0-9]{64}$/.test(trial.transcriptSha256 ?? '')
        || !/^[a-f0-9]{64}$/.test(trial.inputSha256 ?? '')
        || metrics.some(key => !Number.isFinite(trial[key]) || trial[key] < 0)
        || ['modelRequests', 'toolCalls', 'recoveries', 'retries', 'unsupportedConclusions'].some(key => !Number.isInteger(trial[key]) || trial[key] < 0)
        || !['correct', 'complete'].every(key => typeof trial[key] === 'boolean')
        || !scenario.assertions.every(key => typeof trial.assertions?.[key] === 'boolean')) {
      throw Error('incomplete or invalid reviewed model trial');
    }
    const key = `${trial.caseId}:${trial.compactionMode ?? 'none'}:${trial.pair}`;
    const group = groups.get(key) ?? {};
    if (group[trial.variant]) throw Error(`duplicate trial ${key}:${trial.variant}`);
    group[trial.variant] = trial;
    groups.set(key, group);
  }
  const comparisons = [];
  for (const [key, { baseline, candidate }] of groups) {
    if (!baseline || !candidate || ['model', 'provider', 'inputSha256'].some(field => baseline[field] !== candidate[field])) {
      throw Error(`unmatched model/input trial ${key}`);
    }
    const quality = trial => trial.correct && trial.complete && trial.unsupportedConclusions === 0
      && Object.values(trial.assertions).every(value => value === true);
    comparisons.push({ key, baselineCorrectAndComplete: quality(baseline),
      candidateCorrectAndComplete: quality(candidate),
      regressions: metrics.filter(metric => candidate[metric] > baseline[metric]),
      metrics: Object.fromEntries(metrics.map(metric => [metric, {
        baseline: baseline[metric], candidate: candidate[metric], delta: candidate[metric] - baseline[metric],
      }])) });
  }
  if (fixture.cases.some(test => (test.modes ?? [undefined]).some(mode =>
      !trials.some(trial => trial.caseId === test.id && trial.compactionMode === mode)))) {
    throw Error('missing required uncertainty case');
  }
  uncertaintyEvaluation = { status: 'reviewed_trials',
    accepted: comparisons.every(row => row.candidateCorrectAndComplete && row.regressions.length === 0),
    fixtureSha256: createHash('sha256').update(fixtureBytes).digest('hex'),
    trialsSha256: createHash('sha256').update(trialBytes).digest('hex'), comparisons,
    limitation: 'Scores are supplied independent review, not inferred from call counts; pairing does not eliminate provider variance.' };
}
const report = {
  schemaVersion: 1, engine: process.version, runs, wallMs,
  ...(scenarios ? {profile, scenarios} : {}),
  ...(sourceSnapshot ? {sourceSnapshot, historicalSources: true} : {}),
  medianMs: sorted.length % 2 ? sorted[Math.floor(sorted.length / 2)]
    : (sorted[sorted.length / 2 - 1] + sorted[sorted.length / 2]) / 2,
  sources: files.map((path, i) => ({ path, bytes: Buffer.byteLength(sources[i]),
    sha256: createHash('sha256').update(sources[i]).digest('hex') })),
  passed: true,
  uncertaintyEvaluation,
  coverage: scenarios ? scenarios.map(scenario => scenario.name) : ['bounded concurrency', 'batch deduplication', 'settled partial failure',
    'immutable UTF-8/CRLF recovery', 'gaps/hash drift/no-progress stops',
    'passive command drainage', 'all output packets retained', 'failure/input/handle stops'],
  limitations: ['Scripted tool responses, not real provider inference or native tool dispatch.',
    'No claim about live model request selection, tokens, cost, or turn wall-clock speedup.',
    'Native core direct-delivery tests separately assert actual mock-provider request counts and answer persistence.',
    ...(scenarios ? ['Scenario mechanisms overlap; timings are not additive turn savings.',
      'Scripted clock values are simulated work, not measured process wall time.'] : [])],
};
const text = JSON.stringify(report, null, 2) + '\n';
if (output) await writeFile(output, text, { flag: 'wx' });
console.log(text);
