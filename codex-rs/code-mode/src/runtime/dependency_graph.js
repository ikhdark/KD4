// Cell-local dependency execution. Tool dispatch, permissions, cancellation and
// resource admission remain owned by the normal nested-tool runtime.
Object.defineProperty(globalThis, "run_graph", {
  value: async function run_graph(nodes, { concurrency = 4 } = {}) {
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
      graph.set(node.id, { deps, run: node.run, accept: node.accept, step_id: node.step_id });
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

    const results = Object.create(null);
    const pending = new Set(graph.keys());
    const running = new Map();
    async function execute(id, node) {
      try {
        const dependencies = Object.create(null);
        for (const dep of node.deps) dependencies[dep] = results[dep].value;
        const value = await node.run(Object.freeze(dependencies));
        // Promise resolution is not tool success. The caller must explicitly
        // establish the node's behavioral postcondition (including exit codes).
        if (await node.accept(value) !== true) {
          results[id] = { status: "rejected", value, reason: "postcondition failed" };
        } else {
          results[id] = { status: "fulfilled", value };
        }
      } catch (reason) {
        results[id] = { status: "rejected", reason };
      }
    }
    while (pending.size || running.size) {
      for (const id of pending) {
        const node = graph.get(id);
        if (!node.deps.every(dep => Object.hasOwn(results, dep))) continue;
        if (node.deps.some(dep => results[dep].status !== "fulfilled")) {
          results[id] = { status: "skipped", reason: "dependency failed" };
          pending.delete(id);
        } else if (running.size < concurrency) {
          pending.delete(id);
          // Defer callbacks until the task is registered, even when run throws.
          const task = Promise.resolve().then(() => execute(id, node))
            .finally(() => running.delete(id));
          running.set(id, task);
        }
      }
      if (running.size) await Promise.race(running.values());
    }
    const ordered = Object.create(null);
    for (const [id, node] of graph) {
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
