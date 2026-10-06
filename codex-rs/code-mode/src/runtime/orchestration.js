// Mechanical continuations only. These helpers use normal nested-tool dispatch;
// they do not grant permissions, retry effects, or establish semantic completion.
(() => {
  const fail = (message, evidence) => {
    const error = new Error(message);
    error.evidence = evidence;
    throw error;
  };
  const integer = (n, low, high, name) => {
    if (!Number.isInteger(n) || n < low || n > high) throw new TypeError(`invalid ${name}`);
  };
  const bytes = text => {
    let size = 0;
    for (const ch of text) {
      const cp = ch.codePointAt(0);
      size += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    }
    return size;
  };
  const capability = name => {
    if (!ALL_TOOL_NAMES.includes(name)) throw new TypeError(`required tool unavailable: ${name}`);
    return tools[name];
  };

  Object.defineProperty(globalThis, "read_files", {
    value: async function read_files(paths, { concurrency = 4, full = false } = {}) {
      // Strings deliberately exclude ambiguous selector merges and environment
      // aliases. Repeated exact paths share one observation only within this batch.
      if (!Array.isArray(paths) || paths.length < 1 || paths.length > 32 ||
          paths.some(path => typeof path !== "string" || !path.trim())) {
        throw new TypeError("read_files requires 1–32 nonempty paths");
      }
      integer(concurrency, 1, 16, "concurrency");
      if (typeof full !== "boolean") throw new TypeError("full must be boolean");
      const read = capability("read_file");
      const recover = full ? capability("read_tool_output") : undefined;
      const requested = [...paths];
      const unique = [...new Set(requested)];
      // Bound aggregate full-recovery data to 8 MiB, in addition to the normal
      // per-call payload cap. Oversize results retain their initial receipt.
      const fileLimit = Math.floor(8 * 1024 * 1024 / unique.length);
      const nodes = unique.map((path, index) => ({
        id: String(index),
        run: async () => {
          const initial = await read({ path });
          const evidence = { initial, pages: [], file_complete: initial.file_complete === true };
          if (initial.complete !== true || !Array.isArray(initial.results) ||
              initial.results.some(r => r.status !== "ok" || r.complete !== true)) {
            fail("incomplete file read", evidence);
          }
          if (!full) return evidence;
          const size = initial.canonical_bytes;
          if (!Number.isSafeInteger(size) || size > fileLimit || size < 0) {
            fail("full read exceeds its bounded snapshot scope", evidence);
          }
          if (evidence.file_complete) return evidence;
          const hash = initial.source_sha256;
          if (typeof hash !== "string" || !hash || !initial.artifact_id ||
              initial.retained_artifact_complete !== true) {
            fail("full read requires a bounded, retained snapshot", evidence);
          }
          let offset = 0;
          const consume = page => {
            if (!Array.isArray(page.results) || page.results.length === 0) {
              fail("missing recovery ranges", evidence);
            }
            const before = offset;
            for (const part of page.results) {
              // An oversized selector is an envelope, not delivered source.
              // Native recovery appends its exact fragments as separate results.
              if (part.status === "selector_too_large" && part.complete === false &&
                  part.text === undefined && Array.isArray(part.child_selectors)) continue;
              const range = part.canonical_range;
              if (part.status !== "ok" || part.complete !== true || typeof part.text !== "string" || !range ||
                  range.start !== offset || !Number.isSafeInteger(range.end) ||
                  range.end > size || range.end <= offset ||
                  bytes(part.text) !== range.end - range.start) {
                fail("noncontiguous or incomplete snapshot evidence", evidence);
              }
              offset = range.end;
            }
            if (offset <= before) fail("snapshot recovery made no progress", evidence);
          };
          consume(initial);
          const tail = initial.continuation;
          if (tail?.kind !== "bytes" || tail.start !== offset || tail.end !== size) {
            fail("missing exact file continuation", evidence);
          }
          // The full-read scope is already authorized. Recover only the unread
          // suffix of the original artifact, never reopen a mutable source file.
          for (let call = 0; offset < size && call < 64; call++) {
            let page;
            try {
              page = await recover({ artifact_id: initial.artifact_id,
                selectors: [{ kind: "bytes", start: offset, end: size }], max_bytes: 1024 * 1024 });
            } catch (cause) {
              evidence.cause = cause;
              fail("snapshot recovery failed", evidence);
            }
            evidence.pages.push(page);
            if (page.canonical_sha256 !== hash || page.canonical_bytes !== size ||
                page.artifact_id !== initial.artifact_id) {
              fail("snapshot identity changed", evidence);
            }
            consume(page);
            if (offset < size) {
              const stop = page.continuation_stop;
              if (stop?.reason !== "budget" || stop.resumable !== true ||
                  stop.selector?.kind !== "bytes" || stop.selector.start !== offset ||
                  stop.selector.end !== size) fail("recovery needs a decision", evidence);
            } else if (page.complete !== true) {
              fail("recovery did not certify completion", evidence);
            }
          }
          if (offset !== size) fail("recovery call limit reached", evidence);
          evidence.file_complete = true;
          return evidence;
        },
        accept: () => true,
      }));
      // Reuse the graph's bounded scheduler and all-settled failure isolation.
      // No successful sibling is discarded or rerun when another read fails.
      let settled;
      try { settled = await run_graph(nodes, { concurrency }); }
      catch (error) {
        if (!error.results) throw error;
        settled = error.results;
      }
      const byPath = new Map(unique.map((path, i) => [path, settled[String(i)]]));
      return requested.map(path => ({ path, ...byPath.get(path) }));
    },
  });

  Object.defineProperty(globalThis, "await_command", {
    value: async function await_command(initial, { max_observations = 256, on_progress } = {}) {
      integer(max_observations, 1, 1024, "max_observations");
      if (on_progress !== undefined && typeof on_progress !== "function") {
        throw new TypeError("on_progress must be a function");
      }
      const observations = [];
      // A pending exec_command promise is accepted so one expression can start
      // a command and drain it to exit in the same cell.
      let current;
      try { current = await initial; }
      catch (cause) { fail("command did not start", { terminal: undefined, observations, cause }); }
      let session;
      while (true) {
        observations.push(current);
        const evidence = { terminal: current, observations };
        if (!current || current.error != null ||
            (current.pending_deferred_completions != null &&
              (!Array.isArray(current.pending_deferred_completions) || current.pending_deferred_completions.length))) {
          fail("command needs a decision", evidence);
        }
        const exited = current.process_exited === true || current.execution_state === "exited";
        if (exited) {
          if (!Number.isInteger(current.exit_code) || current.exit_code !== 0 ||
              current.session_id != null || current.process_exited !== true ||
              current.execution_state !== "exited") {
            fail("command did not succeed", evidence);
          }
          return evidence;
        }
        if (current.execution_state !== "running" ||
            !Number.isInteger(current.session_id) || current.session_id < 0 ||
            current.session_capabilities?.polling !== true ||
            (session !== undefined && current.session_id !== session)) {
          fail("missing or changed live command handle", evidence);
        }
        session = current.session_id;
        if (observations.length >= max_observations) fail("command observation limit reached", evidence);
        try {
          if (on_progress && await on_progress(current) !== true) fail("command progress needs review", evidence);
          // Only observe the existing process. Passive waits stay steerable through
          // the normal cell owner; no shell restart, stdin, cancellation or retry.
          current = await capability("write_stdin")({ session_id: session, wait_for_output: true });
        } catch (cause) {
          if (cause?.evidence === evidence) throw cause;
          evidence.cause = cause;
          fail("command observation failed; resume only the retained handle", evidence);
        }
      }
    },
  });
})();
