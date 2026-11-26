// Within a read-only wave, group unnamed requests by exact message bytes.
// Named producers keep their own ownership boundary. Never retry a spawn.
(() => {
  function tool(name) {
    const fn = resolve_tool(`agents.${name}`);
    if (typeof fn !== "function") throw new TypeError(`required tool unavailable: agents.${name}`);
    return fn;
  }

  Object.defineProperty(globalThis, "spawn_read_wave", {
    value: async function spawn_read_wave(requests) {
      if (!Array.isArray(requests) || requests.length < 1 || requests.length > 256)
        throw new TypeError("spawn_read_wave requires 1–256 requests");
      const ids = new Set(), named = new Map(), rows = [];
      for (const row of requests) {
        const { id, producer, message } = row ?? {};
        if (typeof id !== "string" || !id.trim() || ids.has(id))
          throw new TypeError("spawn_read_wave requires distinct nonempty string IDs");
        if (typeof message !== "string" || !message.trim() ||
            (producer !== undefined && (typeof producer !== "string" || !producer.trim() ||
              (named.has(producer) && named.get(producer) !== message))))
          throw new TypeError("Conflicting or empty producer assignment");
        ids.add(id);
        if (producer !== undefined) named.set(producer, message);
        rows.push({ id, producer, message });
      }
      const automatic = new Map(), messages = new Map();
      const prefix = `read_wave_${Math.random().toString(36).slice(2)}_`;
      let suffix = 0;
      for (const row of rows) {
        if (row.producer === undefined) {
          if (!automatic.has(row.message)) {
            let name;
            do { name = prefix + suffix++; } while (named.has(name));
            automatic.set(row.message, name);
          }
          row.producer = automatic.get(row.message);
        }
        messages.set(row.producer, row.message);
        delete row.message;
      }
      const spawn = tool("spawn_agent");
      const producers = [...messages.keys()];
      const calls = await Promise.allSettled(producers.map(async task_name =>
        spawn({ task_name, message: messages.get(task_name) })));
      const admitted = [], unresolved = [];
      for (const [i, call] of calls.entries()) {
        const producer = producers[i];
        if (call.status === "fulfilled" && call.value?.assignment_id && call.value?.task_name) {
          admitted.push({ producer, target: call.value.task_name, assignment_id: call.value.assignment_id });
        } else {
          unresolved.push({ producer, outcome: call.status === "rejected"
            ? String(call.reason) : "No confirmed assignment and task name",
            ...(call.status === "fulfilled" ? { response: call.value } : {}) });
        }
      }
      return { rows, admitted, unresolved };
    },
  });

  Object.defineProperty(globalThis, "collect_read_wave", {
    value: async function collect_read_wave({ rows, admitted, unresolved }, { timeout_ms, completion_mode } = {}) {
      const mode = completion_mode ?? "all";
      if (mode !== "all" && mode !== "any")
        throw new TypeError('collect_read_wave completion_mode must be "all" or "any"');
      const reports = [], unavailable = [], indices = new Map();
      let pending = [], reason = null, attempts = new Map();
      if (admitted.length) {
        const { completion } = await tool("wait_agent")({
          targets: admitted.map(a => a.target), completion_mode: mode,
          ...(timeout_ms === undefined ? {} : { timeout_ms }),
        });
        reason = completion.reason;
        pending = completion.pending;
        attempts = new Map(completion.outcomes.map(o => [o.assignment_id, o.attempt_id]));
        const ready = admitted.filter(a => attempts.get(a.assignment_id));
        if (ready.length) {
          const lookup = tool("get_agent_task");
          const reads = await Promise.allSettled(ready.map(async ({ assignment_id }) =>
            lookup({ assignment_id, attempt_id: attempts.get(assignment_id) })));
          for (const [i, read] of reads.entries()) {
            const { producer, assignment_id } = ready[i], attempt_id = attempts.get(assignment_id);
            const receipt = read.status === "fulfilled" ? read.value?.receipt : null;
            if (receipt?.assignment_id === assignment_id && receipt.attempt_id === attempt_id) {
              indices.set(producer, reports.length);
              reports.push({ producer, assignment_id, attempt_id, status: receipt.status, summary: receipt.summary });
            } else {
              unavailable.push({ producer, assignment_id, attempt_id, outcome: read.status === "rejected"
                ? String(read.reason) : "Exact terminal receipt unavailable" });
            }
          }
        }
      }
      const collected = {
        results: rows.map(({ id, producer }) => ({ id, producer, report_index: indices.get(producer) ?? null })),
        reports, unresolved, unavailable, pending, reason,
      };
      if (mode === "any") {
        // Admitted producers with no returned outcome yet, as a wave for the next collection.
        const left = admitted.filter(a => !attempts.get(a.assignment_id));
        const names = new Set(left.map(a => a.producer));
        collected.remaining = { rows: rows.filter(row => names.has(row.producer)), admitted: left, unresolved: [] };
      }
      return collected;
    },
  });
})();
