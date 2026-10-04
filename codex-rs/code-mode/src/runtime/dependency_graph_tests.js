// Runs unchanged in the owning V8 runtime and the standalone timing probe.
const check = (condition, message) => { if (!condition) throw Error(message); };
const deferred = () => {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
};
const node = (id, run = () => id, extra = {}) => ({ id, run, accept: () => true, ...extra });

// Bounded fan-out: no model round or unbounded Promise.all over tool calls.
{
  const release = deferred(), admitted = deferred();
  let active = 0, peak = 0, finished = 0;
  const nodes = Array.from({ length: 9 }, (_, i) => node(`read-${i}`, async () => {
    peak = Math.max(peak, ++active);
    if (active === 3) admitted.resolve();
    await release.promise;
    --active; ++finished;
    return i;
  }));
  const result = run_graph(nodes, { concurrency: 3 });
  await admitted.promise;
  check(active === 3 && finished === 0, "fan-out was not bounded/concurrent");
  release.resolve();
  const results = await result;
  check(peak === 3 && active === 0 && finished === 9, "lost fan-out work");
  check(Object.keys(results).join() === nodes.map(n => n.id).join(), "unstable result order");
}

// A fast branch's consumer must not wait at an unrelated whole-batch barrier.
{
  const release = deferred(), consumed = deferred();
  let slowFinished = false;
  const evidence = { sha256: "fixture-digest", source: "native-file", complete: true };
  const result = run_graph([
    node("slow", async () => { await release.promise; slowFinished = true; }),
    node("read", () => evidence),
    node("consume", deps => {
      check(!slowFinished, "consumer waited for an unrelated branch");
      check(deps.read === evidence && Object.isFrozen(deps), "evidence/dependencies changed");
      consumed.resolve();
      return deps.read;
    }, { deps: ["read"], step_id: "existing-plan-step" }),
  ], { concurrency: 2 });
  await consumed.promise;
  release.resolve();
  const results = await result;
  check(results.consume.value === evidence && results.consume.step_id === "existing-plan-step",
    "aggregation lost evidence identity or plan linkage");
  check(Object.keys(results).join() === "slow,read,consume", "completion order leaked");
}

// Estimates prioritize the full downstream path, with stable input-order ties.
{
  const calls = [];
  const record = (id, extra) => node(id, () => calls.push(id), extra);
  const results = await run_graph([
    record("short-a", { estimated_ms: 5 }),
    record("short-b", { estimated_ms: 5 }),
    record("prepare", { estimated_ms: 1 }),
    record("validate", { deps: ["prepare"], estimated_ms: 100 }),
    record("final", { deps: ["short-a", "short-b", "validate"] }),
  ], { concurrency: 1 });
  check(calls.join() === "prepare,validate,short-a,short-b,final", "not critical-path first");
  check(Object.keys(results).join() === "short-a,short-b,prepare,validate,final", "rank reordered results");
  calls.length = 0;
  await run_graph([record("a"), record("b"), record("c")], { concurrency: 1 });
  check(calls.join() === "a,b,c", "default admission compatibility changed");
}

// Shared reads overlap; writers wait through acceptance; unrelated work bypasses
// occupied resources without bypassing the graph's global concurrency bound.
{
  const releaseReads = deferred(), bothReads = deferred(), accepted = deferred();
  const independent = deferred(), releaseAccept = deferred();
  let readers = 0, writes = 0, accepting = false;
  const read = id => node(id, async () => {
    check(writes === 0, "reader overlaps writer");
    if (++readers === 2) bothReads.resolve();
    await releaseReads.promise;
    --readers;
  }, { resources: { read: ["repo"] } });
  const writer = node("write", () => {
    check(readers === 0, "writer overlaps readers");
    ++writes;
  }, { resources: { write: ["repo", "cargo-target"] }, accept: async () => {
    accepting = true;
    accepted.resolve();
    await releaseAccept.promise;
    accepting = false; --writes;
    return true;
  } });
  const result = run_graph([
    read("read-a"), read("read-b"), writer,
    node("second-write", () => {
      check(writes === 0 && !accepting, "lease released before accept completed");
    }, { resources: { write: ["cargo-target", "repo"] } }),
    node("independent", () => independent.resolve()),
  ], { concurrency: 3 });
  await bothReads.promise;
  await independent.promise;
  check(readers === 2 && writes === 0, "unrelated work blocked behind resource");
  releaseReads.resolve();
  await accepted.promise;
  check(accepting, "writer not in acceptance");
  releaseAccept.resolve();
  await result;
}

// Failed effects/acceptance settle once, release claims, skip descendants and
// preserve every independent result, even when completion order differs.
{
  let calls = 0;
  const cause = Error("tool failure");
  const results = await run_graph([
    node("throw", () => { ++calls; throw cause; }, { resources: { write: ["r"] } }),
    node("blocked", () => { throw Error("failed dependent dispatched"); }, { deps: ["throw"] }),
    node("postcondition", () => ({ exit_code: 7 }), { accept: r => r.exit_code === 0,
      resources: { write: ["r"] } }),
    node("accept-throw", () => 3, { accept: () => { throw cause; }, resources: { write: ["r"] } }),
    node("independent", () => ++calls, { resources: { write: ["r"] } }),
  ]).then(() => { throw Error("failure was swallowed"); }, error => error.results);
  check(calls === 2 && results.throw.reason === cause, "retry or lost original failure");
  check(results.blocked.status === "skipped" && results.independent.status === "fulfilled",
    "failure isolation broken");
  check(results.postcondition.value.exit_code === 7 && results["accept-throw"].reason === cause &&
    results["accept-throw"].value === 3 && !Object.hasOwn(results.throw, "value"),
    "failure evidence lost");
  check(Object.keys(results).join() === "throw,blocked,postcondition,accept-throw,independent",
    "failure aggregation not deterministic");
}

// Rejected asynchronous acceptance must retain the exact returned object, not
// just its error. Recover from this receipt without redispatch or lost handles.
{
  const receipt = { session_id: 7, artifact_id: "retained", complete: false };
  let effects = 0, dependent = false;
  const cause = Error("acceptance failed");
  const results = await run_graph([
    node("producer", () => { ++effects; return receipt; }, {
      accept: async () => { await Promise.resolve(); throw cause; },
      resources: { write: ["process"] },
    }),
    node("dependent", () => { dependent = true; }, { deps: ["producer"] }),
    node("independent", () => "done", { resources: { write: ["process"] } }),
  ]).then(() => { throw Error("acceptance failure swallowed"); }, error => error.results);
  check(effects === 1 && !dependent && results.producer.value === receipt &&
    results.producer.reason === cause && results.independent.value === "done",
    "recovery lost evidence, reran an effect, or failed to release its claim");
}

// Explicit targets prune only unneeded work before dispatch. They never detach
// a started obligation, and resource/dependency inputs are copied before effects.
{
  const calls = [], deps = ["read"], targets = ["final"], resources = { write: ["r"] };
  const nodes = [
    node("optional", () => { throw Error("unselected work started"); }, { estimated_ms: 1000 }),
    node("read", () => {
      calls.push("read"); deps.push("optional"); targets.push("optional");
      resources.write.push("other");
      return { source: "read", hash: "abc" };
    }),
    node("final", d => { calls.push("final"); return d.read; }, { deps, resources }),
  ];
  const results = await run_graph(nodes, { targets });
  check(calls.join() === "read,final" && Object.keys(results).join() === "read,final",
    "target closure or definition snapshot failed");
  check(results.final.value === results.read.value, "selected provenance lost");
  const special = await run_graph([node("__proto__", () => 7),
    node("constructor", d => d.__proto__ + 1, { deps: ["__proto__"] })], { targets: ["constructor"] });
  check(special.constructor.value === 8, "special IDs are not safe");
}

// All configuration, including excluded branches, is rejected before any effect.
{
  let calls = 0;
  const good = () => node("good", () => ++calls);
  const cases = [
    [[good()], { targets: [] }], [[good()], { targets: ["missing"] }],
    [[good()], { targets: ["good", "good"] }], [[good()], { concurrency: 0 }],
    [[good(), node("bad", undefined, { estimated_ms: -1 })], {}],
    [[good(), node("bad", undefined, { estimated_ms: Infinity })], {}],
    [[good(), node("bad", undefined, { estimated_ms: "1" })], {}],
    [[good(), node("bad", undefined, { resources: [] })], {}],
    [[good(), node("bad", undefined, { resources: { typo: ["r"] } })], {}],
    [[good(), node("bad", undefined, { resources: { read: "r" } })], {}],
    [[good(), node("bad", undefined, { resources: { write: ["r", "r"] } })], {}],
    [[good(), node("bad", undefined, { resources: { read: ["r"], write: ["r"] } })], {}],
    [[good(), node("bad", undefined, { deps: ["bad"] })], { targets: ["good"] }],
    [[good(), node("bad", undefined, { requires: ["unavailable-capability"] })], { targets: ["good"] }],
  ];
  for (const [nodes, options] of cases) {
    const error = await run_graph(nodes, options).then(() => null, error => error);
    check(error instanceof TypeError && calls === 0, "invalid graph partially executed");
  }
}
// A known recovery chain runs within its branch, without waiting for unrelated
// discovery. Acceptance must retain immutable snapshot identity, not merely a
// successful tool transport. A mismatched recovery cannot reach the answer.
for (const recoveredHash of ["snapshot-a", "snapshot-b"]) {
  const releaseDiscovery = deferred(), recovered = deferred();
  let discoveryFinished = false, answered = false;
  const snapshot = { hash: "snapshot-a", artifact: "retained-a", complete: false };
  const result = run_graph([
    node("discovery", async () => {
      await releaseDiscovery.promise; discoveryFinished = true;
      return "independent evidence";
    }),
    node("read", () => snapshot),
    node("recover", deps => {
      check(!discoveryFinished, "recovery waited for whole-batch discovery");
      check(deps.read === snapshot, "recovery lost source provenance");
      recovered.resolve();
      return { hash: recoveredHash, artifact: deps.read.artifact, complete: true };
    }, { deps: ["read"], accept: value => value.complete && value.hash === snapshot.hash }),
    node("answer", deps => { answered = true; return deps.recover; }, { deps: ["recover"] }),
  ], { concurrency: 2 }).then(value => ({ value }), error => ({ error }));
  await recovered.promise;
  releaseDiscovery.resolve();
  const outcome = await result;
  check(discoveryFinished, "failure abandoned independent discovery");
  if (recoveredHash === snapshot.hash) {
    check(answered && outcome.value.answer.value.artifact === snapshot.artifact,
      "valid recovery did not reach its consumer");
  } else {
    check(!answered && outcome.error.results.answer.status === "skipped" &&
      outcome.error.results.discovery.status === "fulfilled", "unverified recovery was delivered");
  }
}

// The validation node owns launch -> live handle -> terminal exit. Returning a
// session handle is not completion and must not release a shared Cargo target.
// Review overlaps validation only after the edit; final delivery needs both.
for (const exitCode of [0, 7]) {
  const terminal = deferred(), launched = deferred(), reviewed = deferred();
  let edited = false, alive = false, finalCalls = 0, validations = 0;
  const result = run_graph([
    node("edit", () => { edited = true; return { revision: "edited" }; },
      { resources: { write: ["repo"] } }),
    node("review", deps => {
      check(edited && alive && deps.edit.revision === "edited", "review did not overlap validation");
      reviewed.resolve(); return { complete: true, revision: deps.edit.revision };
    }, { deps: ["edit"], resources: { read: ["repo"] } }),
    node("validate", async deps => {
      check(edited && deps.edit.revision === "edited", "validation preceded the final edit");
      ++validations; alive = true; launched.resolve();
      await terminal.promise;
      alive = false; return { exit_code: exitCode };
    }, { deps: ["edit"], estimated_ms: 100,
      resources: { read: ["repo"], write: ["cargo-target"] }, accept: r => r.exit_code === 0 }),
    node("same-target", () => { check(!alive, "live process lost its target lease"); },
      { deps: ["edit"], resources: { write: ["cargo-target"] } }),
    node("final", deps => {
      ++finalCalls;
      check(!alive && deps.review.complete && deps.validate.exit_code === 0,
        "final preceded checked proof");
    }, { deps: ["review", "validate", "same-target"] }),
  ], { concurrency: 2 }).then(value => ({ value }), error => ({ error }));
  await launched.promise;
  await reviewed.promise;
  check(alive && finalCalls === 0, "live handle was treated as final proof");
  terminal.resolve();
  const outcome = await result;
  check(validations === 1 && !alive, "validation retried or detached");
  const results = outcome.value ?? outcome.error.results;
  check(Object.keys(results).join() === "edit,review,validate,same-target,final",
    "critical-path launch changed aggregation order");
  check(results["same-target"].status === "fulfilled", "failure leaked a target claim");
  check(finalCalls === (exitCode === 0 ? 1 : 0), "failed validation reached final delivery");
}

// Only explicitly unneeded work may be pruned. Choosing final still retains
// every transitive proof obligation, including failed ones; no success shortcut.
{
  let optionalCalls = 0, finalCalls = 0;
  const results = await run_graph([
    node("optional-cleanup", () => ++optionalCalls),
    node("source", () => ({ hash: "source-v1" })),
    node("proof", deps => ({ source: deps.source, exit_code: 1 }),
      { deps: ["source"], accept: value => value.exit_code === 0 }),
    node("final", () => ++finalCalls, { deps: ["proof"] }),
  ], { targets: ["final"] }).then(() => { throw Error("failed proof was hidden"); }, error => error.results);
  check(optionalCalls === 0 && finalCalls === 0 &&
    Object.keys(results).join() === "source,proof,final", "target closure omitted a required obligation");
  check(results.proof.value.source === results.source.value && results.final.status === "skipped",
    "target pruning lost failed evidence provenance");
}
text("dependency graph scenarios passed");
