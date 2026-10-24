// Mechanical continuations only. These helpers use normal nested-tool dispatch;
// they do not grant permissions, retry effects, or establish semantic completion.
(() => {
  const fail = (message, evidence) => {
    const error = new Error(message);
    error.evidence = evidence;
    throw error;
  };
  const integer = (n, low, high, name) => {
    if (!Number.isInteger(n) || n < low || n > high)
      throw new TypeError(`invalid ${name}`);
  };
  const nonAscii = RegExp.prototype.exec.bind(/[^\x00-\x7f]/);
  const bytes = (text) => {
    // Most source is ASCII; validate that in native regex code rather than
    // visiting every byte in JavaScript. Non-ASCII retains exact UTF-8 accounting.
    if (nonAscii(text) === null) return text.length;
    // UTF-16 indexing avoids allocating an iterator result / substring for
    // every source character. Lone surrogates still charge three UTF-8 bytes.
    let size = text.length;
    for (let i = 0; i < text.length; ++i) {
      const cp = text.charCodeAt(i);
      if (cp < 0x80) continue;
      if (cp < 0x800) {
        ++size;
        continue;
      }
      size += 2;
      if (cp >= 0xd800 && cp <= 0xdbff && i + 1 < text.length) {
        const next = text.charCodeAt(i + 1);
        if (next >= 0xdc00 && next <= 0xdfff) ++i;
      }
    }
    return size;
  };
  const capability = (name) => {
    if (!ALL_TOOL_NAMES.includes(name))
      throw new TypeError(`required tool unavailable: ${name}`);
    return tools[name];
  };

  Object.defineProperty(globalThis, "read_status", {
    value: async function read_status(paths, options = {}) {
      if (
        !Array.isArray(paths) ||
        paths.length < 1 ||
        paths.length > 32 ||
        paths.some((path) => typeof path !== "string" || !path.trim())
      ) {
        throw new TypeError("read_status requires 1–32 nonempty paths");
      }
      return capability("read_status")({ ...options, paths: [...paths] });
    },
  });

  Object.defineProperty(globalThis, "read_files", {
    value: async function read_files(
      paths,
      { concurrency = 4, full = false } = {},
    ) {
      // Strings deliberately exclude ambiguous selector merges and environment
      // aliases. Repeated exact paths share one observation only within this batch.
      if (
        !Array.isArray(paths) ||
        paths.length < 1 ||
        paths.length > 32 ||
        paths.some((path) => typeof path !== "string" || !path.trim())
      ) {
        throw new TypeError("read_files requires 1–32 nonempty paths");
      }
      integer(concurrency, 1, 16, "concurrency");
      if (typeof full !== "boolean")
        throw new TypeError("full must be boolean");
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
      const schedule = (action) =>
        new Promise((resolve, reject) => {
          const start = async () => {
            ++activeCalls;
            try {
              resolve(await action());
            } catch (error) {
              reject(error);
            } finally {
              --activeCalls;
              queue.shift()?.();
            }
          };
          if (activeCalls < concurrency) void start();
          else queue.push(start);
        });
      const settled = await Promise.allSettled(
        unique.map(async (path) => {
          const initial = await schedule(() => read({ path }));
          const evidence = {
            initial,
            pages: [],
            file_complete: initial.file_complete === true,
          };
          if (
            initial.complete !== true ||
            !Array.isArray(initial.results) ||
            initial.results.some(
              (r) => r.status !== "ok" || r.complete !== true,
            )
          ) {
            fail("incomplete file read", evidence);
          }
          if (!full) return evidence;
          const size = initial.canonical_bytes;
          if (
            !Number.isSafeInteger(size) ||
            size > remainingBytes ||
            size < 0
          ) {
            fail("full read exceeds its bounded snapshot scope", evidence);
          }
          // Reserve the whole snapshot synchronously before any recovery await.
          // Duplicate paths share this reservation; uneven files share the budget.
          remainingBytes -= size;
          if (evidence.file_complete) return evidence;
          const hash = initial.source_sha256;
          if (
            typeof hash !== "string" ||
            !hash ||
            !initial.artifact_id ||
            initial.retained_artifact_complete !== true
          ) {
            fail("full read requires a bounded, retained snapshot", evidence);
          }
          let offset = 0;
          const consume = (page) => {
            if (!Array.isArray(page.results) || page.results.length === 0) {
              fail("missing recovery ranges", evidence);
            }
            const before = offset;
            for (const part of page.results) {
              // An oversized selector is an envelope, not delivered source.
              // Native recovery appends its exact fragments as separate results.
              if (
                part.status === "selector_too_large" &&
                part.complete === false &&
                part.text === undefined &&
                Array.isArray(part.child_selectors)
              )
                continue;
              const range = part.canonical_range;
              if (
                part.status !== "ok" ||
                part.complete !== true ||
                typeof part.text !== "string" ||
                !range ||
                range.start !== offset ||
                !Number.isSafeInteger(range.end) ||
                range.end > size ||
                range.end <= offset ||
                bytes(part.text) !== range.end - range.start
              ) {
                fail("noncontiguous or incomplete snapshot evidence", evidence);
              }
              offset = range.end;
            }
            if (offset <= before)
              fail("snapshot recovery made no progress", evidence);
          };
          consume(initial);
          const tail = initial.continuation;
          if (
            tail?.kind !== "bytes" ||
            tail.start !== offset ||
            tail.end !== size
          ) {
            fail("missing exact file continuation", evidence);
          }
          // The full-read scope is already authorized. Recover only the unread
          // suffix of the original artifact, never reopen a mutable source file.
          const recovery = () => ({
            tool: "read_tool_output",
            arguments: {
              artifact_id: initial.artifact_id,
              selectors: [{ kind: "bytes", start: offset, end: size }],
              max_bytes: 1024 * 1024,
            },
          });
          let recover;
          for (let call = 0; offset < size && call < 64; call++) {
            let page;
            try {
              recover ??= capability("read_tool_output");
              page = await schedule(() =>
                recover({
                  artifact_id: initial.artifact_id,
                  selectors: [{ kind: "bytes", start: offset, end: size }],
                  max_bytes: 1024 * 1024,
                }),
              );
            } catch (cause) {
              evidence.cause = cause;
              // Only transport failure exposes the last verified cursor. A
              // returned page with drift/gaps must not advertise a safe retry.
              evidence.recovery = recovery();
              fail("snapshot recovery failed", evidence);
            }
            evidence.pages.push(page);
            if (
              page.canonical_sha256 !== hash ||
              page.canonical_bytes !== size ||
              page.artifact_id !== initial.artifact_id
            ) {
              fail("snapshot identity changed", evidence);
            }
            consume(page);
            if (offset < size) {
              const stop = page.continuation_stop;
              if (
                stop?.reason !== "budget" ||
                stop.resumable !== true ||
                stop.selector?.kind !== "bytes" ||
                stop.selector.start !== offset ||
                stop.selector.end !== size
              )
                fail("recovery needs a decision", evidence);
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
        }),
      );
      // Retain all started work, including failures and successful siblings.
      const byPath = new Map(unique.map((path, i) => [path, settled[i]]));
      return requested.map((path) => ({ path, ...byPath.get(path) }));
    },
  });

  Object.defineProperty(globalThis, "search_files", {
    value: async function search_files(
      pattern,
      {
        path = ".",
        glob = [],
        ignore_case = false,
        fixed = false,
        hidden = false,
        no_ignore = false,
        lines = false,
        max_per_file = 10,
        force_fresh = false,
      } = {},
    ) {
      // ripgrep through the argv command shape: patterns and paths are never
      // shell-parsed, and structured results stay in the cell until printed.
      if (typeof pattern !== "string" || !pattern)
        throw new TypeError("search_files requires a nonempty pattern");
      const paths = typeof path === "string" ? [path] : path;
      if (
        !Array.isArray(paths) ||
        paths.length < 1 ||
        paths.length > 32 ||
        paths.some((p) => typeof p !== "string" || !p.trim())
      ) {
        throw new TypeError(
          "path must be a nonempty string or 1–32 nonempty strings",
        );
      }
      const globs = typeof glob === "string" ? [glob] : glob;
      if (
        !Array.isArray(globs) ||
        globs.length > 64 ||
        globs.some((g) => typeof g !== "string" || !g)
      ) {
        throw new TypeError(
          "glob must be a nonempty string or at most 64 nonempty strings",
        );
      }
      for (const [name, value] of Object.entries({
        ignore_case,
        fixed,
        hidden,
        no_ignore,
        lines,
        force_fresh,
      })) {
        if (typeof value !== "boolean")
          throw new TypeError(`${name} must be boolean`);
      }
      integer(max_per_file, 1, 1000, "max_per_file");
      const run = capability("exec_command");
      // Fixed flags keep the record format independent of user configuration:
      // Counts are NUL-framed; JSON line events preserve arbitrary path spelling
      // and report binary detection without manufacturing a negative result.
      const args = [
        "--no-config",
        "--color",
        "never",
        "--null",
        "--path-separator",
        "/",
        "--with-filename",
      ];
      if (ignore_case) args.push("--ignore-case");
      if (fixed) args.push("--fixed-strings");
      if (hidden) args.push("--hidden");
      if (no_ignore) args.push("--no-ignore");
      for (const g of globs) args.push("--glob", g);
      args.push(
        ...(lines
          ? ["--json", "--max-count", String(max_per_file)]
          : ["--count"]),
      );
      args.push("--regexp", pattern, "--", ...new Set(paths));
      const evidence = { program: "rg", args };
      // The host advertises this contract in its existing tool metadata. Older
      // hosts (or implicit-ignore searches) retain the safe fresh-execution path.
      const typedReplay =
        no_ignore &&
        typeof ALL_TOOLS !== "undefined" &&
        ALL_TOOLS.some(
          (tool) =>
            tool.name === "exec_command" &&
            tool.description?.includes("(search_miss_v1)"),
        );
      try {
        evidence.result = await run({
          program: "rg",
          args,
          force_fresh: force_fresh || !typedReplay,
        });
      } catch (cause) {
        evidence.cause = cause;
        fail(
          `search_files could not run rg (ripgrep): ${cause?.message ?? cause}`,
          evidence,
        );
      }
      let result = evidence.result;
      if (
        result?.execution_suppressed === true &&
        (force_fresh ||
          !typedReplay ||
          result.retained_search_miss_version !== 1 ||
          result.executed !== false ||
          result.reused !== true ||
          !result.evidence_id ||
          !result.original_execution_id ||
          !result.validity_scope_fingerprint ||
          result.exit_code !== 1 ||
          result.search_no_match !== true ||
          typeof result.stderr !== "string")
      ) {
        fail("search replay lacks verified typed evidence", evidence);
      }
      if (result?.session_id != null) {
        try {
          const drained = await globalThis.await_command(result);
          evidence.observations = drained.observations;
          result = drained.terminal;
        } catch (cause) {
          const terminal = cause?.evidence?.terminal;
          // await_command deliberately rejects failed commands. rg's 1 and 2
          // still carry useful search evidence, but only after a normal drain.
          if (
            cause?.message?.startsWith("command did not succeed (") &&
            terminal?.execution_state === "exited" &&
            terminal.process_exited === true &&
            terminal.session_id == null &&
            [1, 2].includes(terminal.exit_code)
          ) {
            evidence.observations = cause.evidence.observations;
            result = terminal;
          } else {
            evidence.cause = cause;
            fail("search observation needs a decision; resume only its retained handle", evidence);
          }
        }
        evidence.result = result;
      }
      // 1 is ripgrep's no-match status; 2 reports errors, possibly beside partial results.
      const code = result?.exit_code;
      if (code !== 0 && code !== 1 && code !== 2) {
        fail(
          typeof result === "string"
            ? `rg did not run: ${result}`
            : `rg failed (exit_code=${code ?? "unknown"})`,
          evidence,
        );
      }
      const observations = evidence.observations ?? [result];
      if (observations.some((packet) => packet.streams_complete !== true ||
        typeof packet.stdout !== "string" ||
        (packet.stderr != null && typeof packet.stderr !== "string"))) {
        fail(
          "search output exceeded exact capture; narrow path or glob, or omit lines",
          evidence,
        );
      }
      // Native observations contain successive packets, not cumulative streams.
      // Join before parsing so NUL records and JSON events can cross packets.
      result = {...result,
        stdout: observations.map((packet) => packet.stdout).join(""),
        stderr: observations.map((packet) => packet.stderr ?? "").join(""),
      };
      const errors = result.stderr
        .split("\n")
        .map((line) => line.trimEnd())
        .filter(Boolean);
      if (code === 2 && !result.stdout)
        fail(
          `rg error: ${errors.slice(0, 5).join(" ") || "unknown"}`,
          evidence,
        );
      const files = new Map();
      const occurrences = new Map();
      const normalizePath = (file) => file.replace(/^(\.\/)+/, "");
      let unparsed = false;
      let binary = false;
      if (!lines) {
        let offset = 0;
        while (offset < result.stdout.length) {
          const nul = result.stdout.indexOf("\0", offset);
          const end = result.stdout.indexOf("\n", nul + 1);
          if (nul <= offset || end < 0) {
            unparsed = true;
            break;
          }
          const file = normalizePath(result.stdout.slice(offset, nul));
          const count = result.stdout.slice(nul + 1, end).replace(/\r$/, "");
          if (!/^\d+$/.test(count)) {
            unparsed = true;
            break;
          }
          const observed = Number(count);
          if (!Number.isSafeInteger(observed)) {
            unparsed = true;
            break;
          }
          const previous = files.get(file);
          if (previous && previous.count !== observed) unparsed = true;
          files.set(file, {
            path: file,
            count: Math.max(previous?.count ?? 0, observed),
          });
          offset = end + 1;
        }
      } else
        for (const record of result.stdout.split("\n")) {
          if (!record.trim()) continue;
          let event;
          try {
            event = JSON.parse(record);
          } catch {
            unparsed = true;
            continue;
          }
          if (
            !["begin", "match", "context", "end", "summary"].includes(
              event?.type,
            )
          ) {
            unparsed = true;
            continue;
          }
          const data = event.data;
          if (
            event.type !== "match" &&
            !(event.type === "end" && data?.binary_offset != null)
          )
            continue;
          // Byte-encoded paths cannot be losslessly represented by the string path
          // API. Retain their encoding as partial evidence rather than invent a path.
          const file =
            typeof data?.path?.text === "string"
              ? normalizePath(data.path.text)
              : null;
          if (file === null && typeof data?.path?.bytes !== "string") {
            unparsed = true;
            continue;
          }
          const key =
            file === null ? `bytes:${data.path.bytes}` : `text:${file}`;
          const entry = files.get(key) ?? {
            ...(file === null
              ? { path_bytes: data.path.bytes }
              : { path: file }),
            count: 0,
            lines: [],
          };
          if (file === null) unparsed = true;
          if (event.type === "end") {
            binary = entry.binary = true;
          } else if (
            Number.isInteger(data.line_number) &&
            typeof data.lines?.text === "string"
          ) {
            const text = data.lines.text.replace(/\r?\n$/, "");
            const seen = occurrences.get(key) ?? new Map();
            occurrences.set(key, seen);
            const identity = JSON.stringify([
              data.absolute_offset ?? null,
              text,
              data.submatches ?? null,
            ]);
            if (seen.has(data.line_number)) {
              if (seen.get(data.line_number) !== identity) unparsed = true;
              continue;
            }
            seen.set(data.line_number, identity);
            let start = 0;
            let end = Math.min(text.length, text.length > 240 ? 239 : 240);
            if (text.length > 240 && Array.isArray(data.submatches)) {
              const hit = data.submatches.find((match) =>
                Number.isSafeInteger(match.start) && match.start >= 0);
              if (hit) {
                // rg offsets count UTF-8 bytes, not JavaScript code units.
                let offset = 0, at = 0;
                while (at < text.length && offset < hit.start) {
                  const cp = text.codePointAt(at);
                  offset += cp < 128 ? 1 : cp < 2048 ? 2 : cp < 65536 ? 3 : 4;
                  at += cp > 65535 ? 2 : 1;
                }
                if (offset === hit.start) {
                  start = Math.max(0, at - 80);
                  if (start && /[\uDC00-\uDFFF]/.test(text[start])) --start;
                  end = Math.min(text.length, start + 238);
                }
              }
            }
            if (end < text.length && /[\uDC00-\uDFFF]/.test(text[end])) --end;
            const truncated = start > 0 || end < text.length;
            const preview = (start ? "…" : "") + text.slice(start, end) +
              (end < text.length ? "…" : "");
            if (entry.lines.length < max_per_file) {
              entry.lines.push({
                line: data.line_number,
                text: preview,
                ...(truncated ? { truncated: true,
                  line_byte_start: bytes(text.slice(0, start)),
                  line_byte_end: bytes(text.slice(0, end)),
                  omitted_prefix: start > 0, omitted_suffix: end < text.length } : {}),
              });
            }
            entry.count++;
          } else {
            unparsed = true;
            if (typeof data.lines?.bytes === "string")
              entry.binary = binary = true;
          }
          files.set(key, entry);
        }
      if (code === 0 && files.size === 0) unparsed = true;
      if (unparsed)
        errors.push(
          "Some positive or unsupported rg output could not be represented; this is not a complete match set.",
        );
      const sorted = [...files.values()].sort((a, b) => {
        const left = a.path ?? a.path_bytes,
          right = b.path ?? b.path_bytes;
        return left < right ? -1 : left > right ? 1 : 0;
      });
      let capped = false;
      for (const entry of sorted) {
        if (lines && entry.count >= max_per_file) capped = entry.capped = true;
      }
      return {
        pattern,
        files: sorted,
        file_count: sorted.length,
        match_count: sorted.reduce((sum, entry) => sum + entry.count, 0),
        complete: code !== 2 && !capped && !unparsed && !binary,
        ...(errors.length ? { errors: errors.slice(0, 20) } : {}),
        // The host's advisory for this search, such as existing paths near a
        // guessed one that rg could not open. Dropping it cost another search.
        ...(typeof result.repair === "string" && result.repair
          ? { hint: result.repair }
          : {}),
      };
    },
  });

  Object.defineProperty(globalThis, "await_command", {
    value: async function await_command(
      initial,
      { max_observations = 256, max_wait_ms = 300_000, max_bytes = 4 * 1024 * 1024, on_progress } = {},
    ) {
      integer(max_observations, 1, 1024, "max_observations");
      integer(max_wait_ms, 5_000, 300_000, "max_wait_ms");
      integer(max_bytes, 65536, 4 * 1024 * 1024, "max_bytes");
      // Capture the host's steady clock for this observation chain. Wall-clock
      // corrections must not lengthen the budget or invalidate a live handle.
      const now = globalThis.__codex_monotonic_now;
      const deadline = now() + max_wait_ms;
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
        const retainedBytes = bytes(JSON.stringify(evidence, (_key, value) =>
          typeof value === 'bigint' ? value.toString() : value));
        if (retainedBytes >= max_bytes)
          fail("command evidence byte limit reached; retain these observations and resume only the retained handle", evidence);
        if (
          !current ||
          current.error != null ||
          (current.pending_deferred_completions != null &&
            (!Array.isArray(current.pending_deferred_completions) ||
              current.pending_deferred_completions.length))
        ) {
          fail("command needs a decision", evidence);
        }
        const exited =
          current.process_exited === true ||
          current.execution_state === "exited";
        if (exited) {
          if (
            current.process_exited !== true ||
            current.execution_state !== "exited"
          ) {
            fail("inconsistent command exit state", evidence);
          }
          // An exited process can still own unread output. Drain its authorized
          // handle before interpreting the final exit code, including failures.
          if (current.session_id == null) {
            if (
              current.exit_code === 0 ||
              (current.exit_code === 1 && current.search_no_match === true)
            )
              return evidence;
            fail(
              `command did not succeed (exit_code=${current.exit_code ?? "unknown"}, ` +
                `execution_state=${current.execution_state ?? "unknown"}, ` +
                `process_exited=${current.process_exited ?? "unknown"})`,
              evidence,
            );
          }
        }
        if (
          (!exited && current.execution_state !== "running") ||
          !Number.isInteger(current.session_id) ||
          current.session_id < 0 ||
          current.session_capabilities?.polling !== true ||
          typeof current.session_capabilities?.incarnation !== "string" ||
          !current.session_capabilities.incarnation ||
          (session !== undefined &&
            (current.session_id !== session ||
              current.session_capabilities.incarnation !== incarnation))
        ) {
          fail("missing or changed live command handle", evidence);
        }
        session = current.session_id;
        incarnation = current.session_capabilities.incarnation;
        // Stop with the exact observations and live handle before consuming
        // another packet. The caller can page/retain these bytes and resume the
        // same command; a successful process is never restarted or silently
        // stripped of its earlier output to fit text()/store().
        // Silence is not failure. The owner's no-output notice stays in this
        // observation, so keep waiting on the same handle; a process that stays
        // silent stops at the wait budget with that silence named.
        const silent =
          !exited &&
          current.session_capabilities.observation?.reason ===
            "no_output_observed";
        if (observations.length >= max_observations)
          fail("command observation limit reached", evidence);
        try {
          if (on_progress && (await on_progress(current)) !== true)
            fail("command progress needs review", evidence);
          const remaining = Math.floor(deadline - now());
          // Empty native polls have a five-second floor. Do not launch a
          // fresh observation that cannot fit in the remaining wait budget.
          if (remaining < 5_000)
            fail(
              silent
                ? "command wait budget reached with no output observed; inspect and resume the retained handle"
                : "command wait budget reached; inspect and resume the retained handle",
              evidence,
            );
          // A passive output wait can remain pending forever. Bound the existing
          // poll instead; never race/detach a tool, kill the process, or restart it.
          current = await capability("write_stdin")({
            session_id: session,
            incarnation,
            wait_for_output: false,
            yield_time_ms: remaining,
          });
        } catch (cause) {
          if (cause?.evidence === evidence) throw cause;
          evidence.cause = cause;
          fail(
            "command observation failed; resume only the retained handle",
            evidence,
          );
        }
      }
    },
  });
})();
