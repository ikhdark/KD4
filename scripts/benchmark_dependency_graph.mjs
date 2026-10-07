// Controlled scheduling measurements, not a live-model or repository-I/O benchmark.
// Usage: node scripts/benchmark_dependency_graph.mjs [runs] > report.json
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
import vm from 'node:vm';

const runs = Number(process.argv[2] ?? 5);
assert(Number.isInteger(runs) && runs >= 1 && runs <= 100, 'runs must be 1..100');
const owner = new URL('../codex-rs/code-mode/src/runtime/dependency_graph.js', import.meta.url);
const source = readFileSync(owner, 'utf8');
const context = vm.createContext({ ALL_TOOL_NAMES: [] });
vm.runInContext(source, context, { filename: owner.pathname });
const graph = context.run_graph;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const digest = value => createHash('sha256').update(value).digest('hex');
const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)];

// Inputs are fixed synthetic service times. Baseline and candidate use the same
// production scheduler, changing only explicitly recorded false dependencies,
// admission limits, resource claims, or target selection. No work is detached.
const scenarios = [
  {
    name: 'independent-queries',
    expectedAnswer: 10,
    nodes: ['source', 'tests', 'policy', 'status'].map(id => ({ id, ms: 20 })),
    finalDeps: ['source', 'tests', 'policy', 'status'],
    baselineConcurrency: 1,
  },
  {
    name: 'branch-local-recovery',
    expectedAnswer: 6,
    nodes: [{ id: 'discovery', ms: 60 }, { id: 'read', ms: 10 },
      { id: 'recover', ms: 40, deps: ['read'], baselineDeps: ['read', 'discovery'] }],
    finalDeps: ['discovery', 'recover'],
  },
  {
    name: 'validation-before-report',
    expectedAnswer: 7,
    nodes: [{ id: 'edit', ms: 10 }, { id: 'report', ms: 40, deps: ['edit'] },
      { id: 'validate', ms: 80, deps: ['edit'], baselineDeps: ['edit', 'report'] }],
    finalDeps: ['report', 'validate'],
  },
  {
    name: 'independent-review-bypasses-target-wait',
    expectedAnswer: 6,
    nodes: [
      { id: 'validate', ms: 60, resources: { read: ['repo'], write: ['cargo'] } },
      { id: 'same-target', ms: 10, resources: { write: ['cargo'] } },
      { id: 'review', ms: 40, resources: { read: ['repo'] }, baselineDeps: ['same-target'] },
    ],
    finalDeps: ['validate', 'same-target', 'review'],
  },
  {
    name: 'exclude-explicitly-optional-work',
    expectedAnswer: 2,
    nodes: [{ id: 'optional', ms: 80 }, { id: 'proof', ms: 20 }],
    finalDeps: ['proof'],
    candidateTargets: ['final'],
  },
];

function union(spans) {
  const sorted = spans.map(s => [s.start_ms, s.end_ms]).sort((a, b) => a[0] - b[0]);
  let total = 0, end = 0;
  for (const [start, stop] of sorted) {
    total += Math.max(0, stop - Math.max(start, end));
    end = Math.max(end, stop);
  }
  return total;
}

async function measure(scenario, candidate, corruptId = null) {
  const started = performance.now();
  const spans = [];
  let active = 0, peak = 0;
  const definitions = [...scenario.nodes, { id: 'final', ms: 0, deps: scenario.finalDeps }];
  const nodes = definitions.map((def, index) => ({
    id: def.id,
    deps: (!candidate && def.baselineDeps) || def.deps || [],
    resources: def.resources,
    run: async dependencies => {
      const span = { id: def.id, start_ms: performance.now() - started };
      spans.push(span);
      peak = Math.max(peak, ++active);
      try {
        assert(Object.isFrozen(dependencies));
        for (const value of Object.values(dependencies)) assert.equal(value.complete, true);
        if (def.ms && !corruptId) await sleep(def.ms);
        // False baseline barriers order work but are not semantic inputs.
        const answer = (def.id === 'final' ? 0 : index + 1)
          + (def.deps || []).reduce((sum, id) => sum + dependencies[id].answer, 0)
          + Number(def.id === corruptId);
        return { id: def.id, complete: true, answer };
      } finally {
        span.end_ms = performance.now() - started;
        --active;
      }
    },
    accept: value => value.complete === true,
  }));
  const results = await graph(nodes, {
    concurrency: (!candidate && scenario.baselineConcurrency) || 4,
    targets: candidate ? scenario.candidateTargets : undefined,
  });
  const wall_ms = performance.now() - started;
  assert.equal(active, 0);
  assert.equal(results.final.status, 'fulfilled');
  assert.equal(results.final.value.answer, scenario.expectedAnswer, 'prerequisite-derived answer');
  const selected = new Set(Object.keys(results));
  assert.deepEqual(Object.keys(results), definitions.filter(d => selected.has(d.id)).map(d => d.id));
  for (const node of nodes.filter(n => selected.has(n.id))) {
    const span = spans.find(s => s.id === node.id);
    for (const dep of node.deps) {
      assert(spans.find(s => s.id === dep).end_ms <= span.start_ms, 'dependency ordering');
    }
  }
  return {
    wall_ms, aggregate_node_ms: spans.reduce((sum, s) => sum + s.end_ms - s.start_ms, 0),
    active_union_ms: union(spans), peak, selected: [...selected],
    result_sha256: digest(JSON.stringify(results.final.value)),
    spans: definitions.flatMap(d => spans.filter(s => s.id === d.id)),
  };
}

const measurements = [];
for (const scenario of scenarios) {
  // Corruption preserves scheduling/completion flags but must break correctness.
  await assert.rejects(measure(scenario, true, scenario.finalDeps[0]), /prerequisite-derived answer/);
  const baseline = [], candidate = [];
  for (let i = 0; i < runs; ++i) {
    // Alternate order to avoid consistently favoring one side's warmup state.
    for (const optimized of i % 2 ? [true, false] : [false, true]) {
      (optimized ? candidate : baseline).push(await measure(scenario, optimized));
    }
  }
  assert.equal(new Set([...baseline, ...candidate].map(r => r.result_sha256)).size, 1);
  measurements.push({
    fixture: scenario, baseline, candidate,
    median_baseline_ms: median(baseline.map(r => r.wall_ms)),
    median_candidate_ms: median(candidate.map(r => r.wall_ms)),
    median_baseline_aggregate_ms: median(baseline.map(r => r.aggregate_node_ms)),
    median_candidate_aggregate_ms: median(candidate.map(r => r.aggregate_node_ms)),
  });
}
console.log(JSON.stringify({
  scope: 'Synthetic scheduling only; not five new production changes or a measured live-model speedup',
  node: process.version, platform: process.platform, runs,
  scheduler_sha256: digest(source), measurements,
}, null, 2));
