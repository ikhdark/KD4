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
  check(results.postcondition.value.exit_code === 7 && results["accept-throw"].reason === cause,
    "failure evidence lost");
  check(Object.keys(results).join() === "throw,blocked,postcondition,accept-throw,independent",
    "failure aggregation not deterministic");
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
text("dependency graph scenarios passed");
