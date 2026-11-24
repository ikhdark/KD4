// Explicit decision inputs are leaf nodes. The existing graph owns local
// dispatch, scheduling, acceptance and retention of partial results.
(() => {
  const graph = globalThis.run_graph;
  Object.defineProperty(graph, "frontier", {
    value: async function frontier(steps, decisions, { concurrency } = {}) {
      if (!Array.isArray(steps) || !Array.isArray(decisions) ||
          decisions.length > 32 || steps.length + decisions.length > 256 ||
          steps.length + decisions.length === 0) {
        throw new TypeError("run_graph.frontier requires 1–256 total nodes and at most 32 decisions");
      }

      const stepIds = new Set(steps.map(step => step?.id));
      const reasons = new Map();
      const leaves = Array.from(decisions, decision => {
        const { id, reason, deps = [], collect } = decision ?? {};
        if (typeof id !== "string" || !id.trim() || id.length > 256 ||
            stepIds.has(id) || reasons.has(id) ||
            typeof reason !== "string" || !reason.trim() || reason.length > 512 ||
            typeof collect !== "function" || !Array.isArray(deps) ||
            deps.some(dep => !stepIds.has(dep))) {
          throw new TypeError("each frontier decision needs a distinct ID, a reason, and only local step dependencies");
        }
        reasons.set(id, reason);
        return {
          id, deps,
          run: async inputs => {
            const evidence = await collect(inputs);
            if (evidence === undefined) {
              throw new TypeError("a frontier collector must return evidence or explicitly return null");
            }
            return evidence;
          },
        };
      });
      if (steps.some(step => Array.isArray(step?.deps) &&
          step.deps.some(dep => reasons.has(dep)))) {
        throw new TypeError("local steps cannot depend on a decision that has not been made");
      }

      const results = await graph([...steps, ...leaves], { concurrency });
      const handoff = leaves.flatMap(({ id }) => {
        const evidence = results[id].value;
        return evidence === null ? [] : [{ id, reason: reasons.get(id), evidence }];
      });
      return { state: handoff.length ? "decision_required" : "settled", decisions: handoff, results };
    },
  });
})();
