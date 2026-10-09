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
  const nonAscii = RegExp.prototype.exec.bind(/[^\x00-\x7f]/);
  const bytes = text => {
    // Most source is ASCII; validate that in native regex code rather than
    // visiting every byte in JavaScript. Non-ASCII retains exact UTF-8 accounting.
    if (nonAscii(text) === null) return text.length;
    // UTF-16 indexing avoids allocating an iterator result / substring for
    // every source character. Lone surrogates still charge three UTF-8 bytes.
    let size = text.length;
    for (let i = 0; i < text.length; ++i) {
      const cp = text.charCodeAt(i);
      if (cp < 0x80) continue;
      if (cp < 0x800) { ++size; continue; }
      size += 2;
      if (cp >= 0xd800 && cp <= 0xdbff && i + 1 < text.length) {
        const next = text.charCodeAt(i + 1);
        if (next >= 0xdc00 && next <= 0xdfff) ++i;
      }
    }
    return size;
  };
  const capability = name => {
    if (!ALL_TOOL_NAMES.includes(name)) throw new TypeError(`required tool unavailable: ${name}`);
    return tools[name];
  };

  Object.defineProperty(globalThis, "read_status", {
    value: async function read_status(paths, options = {}) {
      if (!Array.isArray(paths) || paths.length < 1 || paths.length > 32 ||
          paths.some(path => typeof path !== "string" || !path.trim())) {
        throw new TypeError("read_status requires 1–32 nonempty paths");
      }
      return capability("read_status")({ ...options, paths: [...paths] });
    },
  });

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
      const requested = [...paths];
      const unique = [...new Set(requested)];
      // Bound aggregate full-recovery data to 8 MiB, in addition to the normal
      // per-call payload cap. Oversize results retain their initial receipt.
      let remainingBytes = 8 * 1024 * 1024;
      // A file awaiting several recovery pages must not occupy a worker for
      // its entire lifetime. FIFO admission bounds individual nested calls,
      // allowing queued initial reads to overlap the first file's recovery.
      let activeCalls = 0;
      const queue = [];
      const schedule = action => new Promise((resolve, reject) => {
        const start = async () => {
          ++activeCalls;
          try { resolve(await action()); }
          catch (error) { reject(error); }
          finally {
            --activeCalls;
            queue.shift()?.();
          }
        };
        if (activeCalls < concurrency) void start();
        else queue.push(start);
      });
      const settled = await Promise.allSettled(unique.map(async path => {
          const initial = await schedule(() => read({ path }));
          const evidence = { initial, pages: [], file_complete: initial.file_complete === true };
          if (initial.complete !== true || !Array.isArray(initial.results) ||
              initial.results.some(r => r.status !== "ok" || r.complete !== true)) {
            fail("incomplete file read", evidence);
          }
          if (!full) return evidence;
          const size = initial.canonical_bytes;
          if (!Number.isSafeInteger(size) || size > remainingBytes || size < 0) {
            fail("full read exceeds its bounded snapshot scope", evidence);
          }
          // Reserve the whole snapshot synchronously before any recovery await.
          // Duplicate paths share this reservation; uneven files share the budget.
          remainingBytes -= size;
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
          const recovery = () => ({ tool: "read_tool_output", arguments: {
            artifact_id: initial.artifact_id,
            selectors: [{ kind: "bytes", start: offset, end: size }], max_bytes: 1024 * 1024,
          } });
          let recover;
          for (let call = 0; offset < size && call < 64; call++) {
            let page;
            try {
              recover ??= capability("read_tool_output");
              page = await schedule(() => recover({ artifact_id: initial.artifact_id,
                selectors: [{ kind: "bytes", start: offset, end: size }], max_bytes: 1024 * 1024 }));
            } catch (cause) {
              evidence.cause = cause;
              // Only transport failure exposes the last verified cursor. A
              // returned page with drift/gaps must not advertise a safe retry.
              evidence.recovery = recovery();
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
          if (offset !== size) {
            evidence.recovery = recovery();
            fail("recovery call limit reached", evidence);
          }
          evidence.file_complete = true;
          return evidence;
      }));
      // Retain all started work, including failures and successful siblings.
      const byPath = new Map(unique.map((path, i) => [path, settled[i]]));
      return requested.map(path => ({ path, ...byPath.get(path) }));
    },
  });

  Object.defineProperty(globalThis, "await_command", {
    value: async function await_command(initial, { max_observations = 256, max_wait_ms = 300_000, on_progress } = {}) {
      integer(max_observations, 1, 1024, "max_observations");
      integer(max_wait_ms, 5_000, 300_000, "max_wait_ms");
      const deadline = Date.now() + max_wait_ms;
      if (on_progress !== undefined && typeof on_progress !== "function") {
        throw new TypeError("on_progress must be a function");
      }
      const observations = [];
      let current = initial;
      let session;
      let incarnation;
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
          if (current.process_exited !== true ||
              current.execution_state !== "exited") {
            fail("inconsistent command exit state", evidence);
          }
          // An exited process can still own unread output. Drain its authorized
          // handle before interpreting the final exit code, including failures.
          if (current.session_id == null) {
            if (current.exit_code === 0 ||
                (current.exit_code === 1 && current.search_no_match === true)) return evidence;
            fail(`command did not succeed (exit_code=${current.exit_code ?? "unknown"}, ` +
              `execution_state=${current.execution_state ?? "unknown"}, ` +
              `process_exited=${current.process_exited ?? "unknown"})`, evidence);
          }
        }
        if ((!exited && current.execution_state !== "running") ||
            !Number.isInteger(current.session_id) || current.session_id < 0 ||
            current.session_capabilities?.polling !== true ||
            typeof current.session_capabilities?.incarnation !== "string" ||
            !current.session_capabilities.incarnation ||
            (session !== undefined && (current.session_id !== session ||
              current.session_capabilities.incarnation !== incarnation))) {
          fail("missing or changed live command handle", evidence);
        }
        session = current.session_id;
        incarnation = current.session_capabilities.incarnation;
        // Silence is a decision boundary, not proof of failure. Keep the live
        // owner and all completed output; do not hide the notice in another poll.
        if (!exited && current.session_capabilities.observation?.reason === "no_output_observed") {
          fail("command reported no output; inspect and resume the retained handle", evidence);
        }
        if (observations.length >= max_observations) fail("command observation limit reached", evidence);
        try {
          if (on_progress && await on_progress(current) !== true) fail("command progress needs review", evidence);
          const remaining = deadline - Date.now();
          // Empty native polls have a five-second floor. Do not launch a
          // fresh observation that cannot fit in the remaining wait budget.
          if (remaining < 5_000) fail("command wait budget reached; inspect and resume the retained handle", evidence);
          // A passive output wait can remain pending forever. Bound the existing
          // poll instead; never race/detach a tool, kill the process, or restart it.
          current = await capability("write_stdin")({ session_id: session, incarnation,
            wait_for_output: false, yield_time_ms: remaining });
        } catch (cause) {
          if (cause?.evidence === evidence) throw cause;
          evidence.cause = cause;
          fail("command observation failed; resume only the retained handle", evidence);
        }
      }
    },
  });
})();
