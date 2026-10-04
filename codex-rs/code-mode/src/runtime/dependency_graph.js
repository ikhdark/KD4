// Cell-local dependency execution. Tool dispatch, permissions, cancellation and
// resource admission remain owned by the normal nested-tool runtime.
Object.defineProperty(globalThis, "run_graph", {
  value: async function run_graph(nodes, { concurrency = 4, targets } = {}) {
    if (!Array.isArray(nodes) || nodes.length === 0 || nodes.length > 256) {
      throw new TypeError("run_graph requires 1–256 nodes");
    }
    if (!Number.isInteger(concurrency) || concurrency < 1 || concurrency > 16) {
      throw new TypeError("run_graph concurrency must be an integer from 1 to 16");
    }
    // Capture definitions before running callbacks. Reject the entire graph
    // before starting any effect, including errors in disconnected components.
    const graph = new Map();
    for (const node of nodes) {
      if (!node || typeof node.id !== "string" || !node.id.trim() ||
          node.id.length > 256 || graph.has(node.id) ||
          typeof node.run !== "function" || typeof node.accept !== "function" ||
          (node.deps !== undefined && !Array.isArray(node.deps)) ||
          (node.requires !== undefined && !Array.isArray(node.requires)) ||
          (node.step_id !== undefined && (typeof node.step_id !== "string" ||
            !node.step_id.trim() || node.step_id.length > 256))) {
        throw new TypeError("run_graph nodes require unique IDs, deps, run and accept callbacks");
      }
      const deps = [...(node.deps ?? [])];
      if (deps.some(id => typeof id !== "string") || new Set(deps).size !== deps.length) {
        throw new TypeError(`invalid dependencies for ${node.id}`);
      }
      const requires = [...(node.requires ?? [])];
      if (requires.length > 128 || requires.some(name =>
          typeof name !== "string" || !ALL_TOOL_NAMES.includes(name))) {
        throw new TypeError(`required tool capability is unavailable for ${node.id}`);
      }
      const estimated_ms = node.estimated_ms ?? 0;
      if (!Number.isFinite(estimated_ms) || estimated_ms < 0 || estimated_ms > 86_400_000) {
        throw new TypeError(`invalid estimated_ms for ${node.id}`);
      }
      const resources = node.resources ?? {};
      if (typeof resources !== "object" || Array.isArray(resources) ||
          Object.keys(resources).some(key => key !== "read" && key !== "write")) {
        throw new TypeError(`invalid resources for ${node.id}`);
      }
      const claims = {};
      for (const mode of ["read", "write"]) {
        const keys = resources[mode] ?? [];
        if (!Array.isArray(keys) || keys.length > 128 || keys.some(key =>
            typeof key !== "string" || !key.trim() || key.length > 256) ||
            new Set(keys).size !== keys.length) {
          throw new TypeError(`invalid ${mode} resources for ${node.id}`);
        }
        claims[mode] = [...keys];
      }
      if (claims.read.some(key => claims.write.includes(key))) {
        throw new TypeError(`duplicate read/write resource for ${node.id}`);
      }
      graph.set(node.id, { deps, run: node.run, accept: node.accept,
        step_id: node.step_id, estimated_ms, claims, ordinal: graph.size });
    }
    for (const [id, node] of graph) {
      if (node.deps.some(dep => dep === id || !graph.has(dep))) {
        throw new TypeError(`missing or self dependency for ${id}`);
      }
    }
    const visited = new Set();
    const visiting = new Set();
    function visit(id) {
      if (visiting.has(id)) throw new TypeError(`dependency cycle at ${id}`);
      if (visited.has(id)) return;
      visiting.add(id);
      for (const dep of graph.get(id).deps) visit(dep);
      visiting.delete(id);
      visited.add(id);
    }
    for (const id of graph.keys()) visit(id);

    // Selection is explicit and happens before effects, never by abandoning
    // already-started work. Validate even disconnected definitions above.
    if (targets !== undefined && (!Array.isArray(targets) || !targets.length ||
        targets.some(id => typeof id !== "string" || !graph.has(id)) ||
        new Set(targets).size !== targets.length)) {
      throw new TypeError("run_graph targets require distinct known node IDs");
    }
    const selected = new Set();
    function select(id) {
      if (selected.has(id)) return;
      selected.add(id);
      for (const dep of graph.get(id).deps) select(dep);
    }
    for (const id of targets ?? graph.keys()) select(id);
    // Longest remaining dependency path first; zero estimates retain the
    // original input-order policy. Estimates affect admission, never results.
    const ranks = new Map();
    for (const id of [...visited].reverse()) {
      if (!selected.has(id)) continue;
      const node = graph.get(id);
      const rank = (ranks.get(id) ?? 0) + node.estimated_ms;
      ranks.set(id, rank);
      for (const dep of node.deps) ranks.set(dep, Math.max(ranks.get(dep) ?? 0, rank));
    }
    const admissionOrder = [...graph.keys()].filter(id => selected.has(id)).sort((a, b) =>
      ranks.get(b) - ranks.get(a) || graph.get(a).ordinal - graph.get(b).ordinal);
    const results = Object.create(null);
    const pending = new Set(admissionOrder);
    const running = new Map();
    // All claims are acquired together on this JS thread. No partial leases,
    // lock-order cycles, or cross-cell authority. Normal tool gates still apply.
    const readers = new Map();
    const writers = new Set();
    function available({ claims }) {
      return claims.read.every(key => !writers.has(key)) &&
        claims.write.every(key => !writers.has(key) && !readers.has(key));
    }
    function acquire({ claims }) {
      for (const key of claims.read) readers.set(key, (readers.get(key) ?? 0) + 1);
      for (const key of claims.write) writers.add(key);
    }
    function release({ claims }) {
      for (const key of claims.read) {
        if (readers.get(key) === 1) readers.delete(key);
        else readers.set(key, readers.get(key) - 1);
      }
      for (const key of claims.write) writers.delete(key);
    }
    async function execute(id, node) {
      let value, produced = false;
      try {
        const dependencies = Object.create(null);
        for (const dep of node.deps) dependencies[dep] = results[dep].value;
        value = await node.run(Object.freeze(dependencies));
        produced = true;
        // Promise resolution is not tool success. The caller must explicitly
        // establish the node's behavioral postcondition (including exit codes).
        if (await node.accept(value) !== true) {
          results[id] = { status: "rejected", value, reason: "postcondition failed" };
        } else {
          results[id] = { status: "fulfilled", value };
        }
      } catch (reason) {
        // Acceptance can fail after a successful, expensive effect. Keep its
        // evidence/live handle so recovery never needs to repeat that effect.
        results[id] = produced ? { status: "rejected", value, reason }
          : { status: "rejected", reason };
      }
    }
    while (pending.size || running.size) {
      for (const id of pending) {
        const node = graph.get(id);
        if (!node.deps.every(dep => Object.hasOwn(results, dep))) continue;
        if (node.deps.some(dep => results[dep].status !== "fulfilled")) {
          results[id] = { status: "skipped", reason: "dependency failed" };
          pending.delete(id);
        } else if (running.size < concurrency && available(node)) {
          pending.delete(id);
          acquire(node);
          // Defer callbacks until the task is registered, even when run throws.
          const task = Promise.resolve().then(() => execute(id, node))
            .finally(() => { release(node); running.delete(id); });
          running.set(id, task);
        }
      }
      if (running.size) await Promise.race(running.values());
    }
    const ordered = Object.create(null);
    for (const [id, node] of graph) {
      if (!selected.has(id)) continue;
      ordered[id] = results[id];
      if (node.step_id !== undefined) ordered[id].step_id = node.step_id;
    }
    if (Object.values(ordered).some(result => result.status !== "fulfilled")) {
      const error = new Error("dependency graph failed; all started nodes settled; no node was retried");
      error.results = ordered;
      throw error;
    }
    return ordered;
  },
});
