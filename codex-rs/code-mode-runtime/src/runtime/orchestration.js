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
      throw new TypeError(`invalid ${name}: expected an integer from ${low} through ${high}`);
  };
  const nonAscii = RegExp.prototype.exec.bind(/[^\x00-\x7f]/);
  const charCodeAt = Function.prototype.call.bind(String.prototype.charCodeAt);
  const bytes = (text) => {
    // Most source is ASCII; validate that in native regex code rather than
    // visiting every byte in JavaScript. Non-ASCII retains exact UTF-8 accounting.
    if (nonAscii(text) === null) return text.length;
    // UTF-16 indexing avoids allocating an iterator result / substring for
    // every source character. Lone surrogates still charge three UTF-8 bytes.
    let size = text.length;
    for (let i = 0; i < text.length; ++i) {
      const cp = charCodeAt(text, i);
      if (cp < 0x80) continue;
      if (cp < 0x800) {
        ++size;
        continue;
      }
      size += 2;
      if (cp >= 0xd800 && cp <= 0xdbff && i + 1 < text.length) {
        const next = charCodeAt(text, i + 1);
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

  // A packet may be retained by its caller and mutated during a native await.
  // Cache its rendered cost only while its plain data descriptors still match.
  // No getters, proxies, custom prototypes or toJSON hooks enter this path.
  const nativeIsProxy = globalThis.__codex_is_proxy;
  const ownNames = Object.getOwnPropertyNames;
  const ownDescriptor = Object.getOwnPropertyDescriptor;
  const getPrototype = Object.getPrototypeOf;
  const nativeIsArray = Array.isArray;
  const sameValue = Object.is;
  const nativeString = String;
  const NativeSet = Set;
  const setHas = Function.prototype.call.bind(Set.prototype.has);
  const setAdd = Function.prototype.call.bind(Set.prototype.add);
  const setDelete = Function.prototype.call.bind(Set.prototype.delete);
  const arrayPush = Function.prototype.call.bind(Array.prototype.push);
  const arrayMap = Function.prototype.call.bind(Array.prototype.map);
  const arraySome = Function.prototype.call.bind(Array.prototype.some);
  const arrayEvery = Function.prototype.call.bind(Array.prototype.every);
  const objectPrototype = Object.prototype;
  const arrayPrototype = Array.prototype;
  const nativeJSON = JSON;
  const nativeStringify = JSON.stringify;
  const bigintPrototype = BigInt.prototype;
  const bigintToString = ownDescriptor(bigintPrototype, "toString").value;
  const withoutToJSON = (value) => {
    for (let depth = 0; value !== null && depth < 128; ++depth) {
      if (nativeIsProxy(value) || ownDescriptor(value, "toJSON") !== undefined)
        return false;
      value = getPrototype(value);
    }
    return value === null;
  };
  const stableCommandJSON = () =>
    ownDescriptor(globalThis, "JSON")?.value === nativeJSON &&
    ownDescriptor(nativeJSON, "stringify")?.value === nativeStringify &&
    ownDescriptor(bigintPrototype, "toString")?.value === bigintToString &&
    withoutToJSON(bigintPrototype) && withoutToJSON(objectPrototype) &&
    withoutToJSON(arrayPrototype);
  const commandPacketSnapshot = (packet) => {
    const records = [];
    const seen = new NativeSet();
    const active = new NativeSet();
    const visit = (value, depth = 0) => {
      if (value === null || typeof value !== "object")
        return typeof value !== "function" && typeof value !== "symbol";
      if (nativeIsProxy(value) || depth >= 128 || setHas(active, value)) return false;
      if (setHas(seen, value)) return true;
      if (records.length >= 4096) return false;
      const prototype = getPrototype(value);
      const array = nativeIsArray(value);
      if ((prototype !== null && prototype !== (array ? arrayPrototype : objectPrototype)) ||
          !withoutToJSON(value)) return false;
      const names = ownNames(value);
      const fields = arrayMap(names, name => ownDescriptor(value, name));
      if (arraySome(fields, field => !field || !("value" in field))) return false;
      // JSON reads inherited numeric properties for sparse array holes.
      if (array) {
        const length = ownDescriptor(value, "length").value;
        if (length > 4096) return false;
        for (let index = 0; index < length; ++index)
          if (!ownDescriptor(value, nativeString(index))) return false;
      }
      arrayPush(records, {value, prototype, names, fields});
      setAdd(seen, value);
      setAdd(active, value);
      const valid = arrayEvery(fields, field => visit(field.value, depth + 1));
      setDelete(active, value);
      return valid;
    };
    return visit(packet) ? records : null;
  };
  const unchangedCommandPacket = (records) => arrayEvery(records, record => {
    if (getPrototype(record.value) !== record.prototype || !withoutToJSON(record.value))
      return false;
    const names = ownNames(record.value);
    return names.length === record.names.length && arrayEvery(names, (name, index) => {
      if (name !== record.names[index]) return false;
      const field = ownDescriptor(record.value, name), prior = record.fields[index];
      return field && "value" in field && sameValue(field.value, prior.value) &&
        field.enumerable === prior.enumerable && field.writable === prior.writable &&
        field.configurable === prior.configurable;
    });
  });

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
      { concurrency = 4, full = false, revision, source_hashes, environment_id } = {},
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
      // Accept only an immutable declared commit, never resolve a mutable ref
      // independently for each file. Native reads retain their normal gates.
      if (revision !== undefined &&
          (typeof revision !== "string" || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/i.test(revision)))
        throw new TypeError("revision must be a full immutable Git commit OID");
      if (environment_id !== undefined &&
          (typeof environment_id !== "string" || !environment_id.trim()))
        throw new TypeError("environment_id must be a nonempty string");
      const expected = new Map();
      if (source_hashes !== undefined) {
        if (!source_hashes || typeof source_hashes !== "object" || Array.isArray(source_hashes))
          throw new TypeError("source_hashes must declare a SHA-256 for every requested path");
        for (const path of unique) {
          const hash = ownDescriptor(source_hashes, path)?.value;
          if (typeof hash !== "string" || !/^[0-9a-f]{64}$/i.test(hash))
            throw new TypeError(`missing or invalid source_hashes identity: ${path}`);
          expected.set(path, hash.toLowerCase());
        }
      }
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
          const initial = await schedule(() => read({ path, revision, environment_id }));
          const evidence = {
            initial,
            pages: [],
            file_complete: initial.file_complete === true,
            consistency: {
              kind: revision !== undefined ? "git_commit" : expected.size ? "declared_identities" : "independent",
              status: "unknown",
              atomic_live_workspace: false,
              reason: "Independent file observations do not establish an atomic live-workspace snapshot.",
            },
          };
          if (revision !== undefined) {
            evidence.consistency.revision = revision.toLowerCase();
            if (initial.git_source?.commit_oid !== revision.toLowerCase() ||
                initial.git_source?.path !== path ||
                typeof initial.git_source?.repository_root !== "string" ||
                typeof initial.git_source?.blob_oid !== "string" ||
                typeof initial.environment_id !== "string")
              fail("read did not establish the declared immutable Git snapshot", evidence);
            evidence.consistency.status = "verified";
            evidence.consistency.reason = "Read from the declared immutable commit, not the live workspace.";
          }
          if (expected.has(path)) {
            evidence.consistency.source_sha256 = expected.get(path);
            if (typeof initial.source_sha256 !== "string")
              fail("read has no source identity to verify", evidence);
            if (initial.source_sha256.toLowerCase() !== expected.get(path)) {
              evidence.consistency.status = "inconsistent";
              fail("file differs from its declared source identity", evidence);
            }
            evidence.consistency.status = "verified";
            if (revision === undefined)
              evidence.consistency.reason = "Per-file source identities match; sequential or concurrent opens are not atomic.";
          }
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
          // Recovery addresses the retained UTF-8 snapshot, not the original
          // file bytes (which differ for decoded UTF-16 sources).
          const hash = initial.canonical_sha256 ?? initial.source_sha256;
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
              // Budget and automatic page-limit stops are both bounded,
              // resumable pages of this same snapshot.
              if (
                (stop?.reason !== "budget" && stop?.reason !== "page_limit") ||
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
      if (revision !== undefined) {
        const anchor = settled.find(result => result.status === "fulfilled")?.value.initial;
        for (let i = 0; anchor && i < settled.length; ++i) {
          const result = settled[i];
          if (result.status !== "fulfilled") continue;
          const evidence = result.value;
          if (evidence.initial.git_source.repository_root !== anchor.git_source.repository_root ||
              evidence.initial.environment_id !== anchor.environment_id) {
            evidence.consistency.status = "inconsistent";
            const reason = new Error("batch crossed repository or environment identities");
            reason.evidence = evidence;
            settled[i] = { status: "rejected", reason };
          }
        }
      }
      // Retain all started work, including failures and successful siblings.
      const byPath = new Map(unique.map((path, i) => [path, settled[i]]));
      return requested.map((path) => ({ path, ...byPath.get(path) }));
    },
  });

  Object.defineProperty(globalThis, "search_files", {
    value: __codex_paginate_search(async function search_files(
      pattern,
      {
        path = ".",
        glob = [],
        ignore_case = false,
        fixed = Array.isArray(pattern),
        hidden = false,
        no_ignore = false,
        lines = false,
        max_per_file = 10,
        force_fresh = false,
        hydrate = false,
        enclosing = false,
        context_lines = 3,
        max_hydrated_files = 8,
        count_only = false,
        overlapping = false,
      } = {},
      paged = false,
    ) {
      // ripgrep through the argv command shape: patterns and paths are never
      // shell-parsed, and structured results stay in the cell until printed.
      const multi = Array.isArray(pattern);
      const semantics = __codex_search_semantics(pattern, { fixed, ignore_case, overlapping });
      const patterns = (multi ? pattern : [pattern]).map((value, index) => {
        const item = typeof value === "string" ? { id: String(index), pattern: value } : value;
        if (!item || typeof item.id !== "string" || !item.id ||
          typeof item.pattern !== "string" || !item.pattern ||
          ((multi || overlapping) && bytes(item.pattern) > 1024))
          throw new TypeError("patterns require a nonempty id and 1–1024 UTF-8 bytes, without newlines or lone surrogates");
        if (item.pattern.includes("\0") ||
          /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/.test(item.pattern))
          throw new TypeError("patterns cannot contain NUL or lone surrogates; argv must preserve the supplied text");
        if (fixed && item.pattern.includes("\n"))
          throw new TypeError("fixed-string search requires single-line text; use an array for separate literal patterns");
        return { id: item.id, pattern: item.pattern };
      });
      if ((!multi && typeof pattern !== "string") || patterns.length < 1 ||
        patterns.length > 64 || new Set(patterns.map(p => p.id)).size !== patterns.length)
        throw new TypeError("search_files requires a string or 1–64 literal patterns with unique ids");
      if (count_only && (lines || hydrate || enclosing))
        throw new TypeError("count_only cannot include lines or hydration");
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
        hidden,
        no_ignore,
        lines,
        force_fresh,
        hydrate,
        enclosing,
        count_only,
      })) {
        if (typeof value !== "boolean")
          throw new TypeError(`${name} must be boolean`);
      }
      integer(max_per_file, 1, 1000, "max_per_file");
      integer(context_lines, 0, 20, "context_lines");
      integer(max_hydrated_files, 1, 32, "max_hydrated_files");
      hydrate ||= enclosing;
      lines ||= hydrate;
      const jsonSearch = lines || multi || count_only || overlapping;
      // rg visits the tree once and emits candidate lines. A bounded literal
      // automaton classifies each line once, retaining duplicate pattern ids
      // and matches hidden by rg's leftmost union (e.g. he / she / hers).
      // No JS regex executes user patterns. Multi-literal folding is ASCII;
      // single-pattern regex searches retain ripgrep's Unicode semantics.
      const literal = fixed && (multi || overlapping);
      const fold = text => ignore_case ? text.replace(/[A-Z]/g, c => c.toLowerCase()) : text;
      const trie = [{ next: new Map(), fail: 0, output: [] }];
      if (literal) {
        patterns.forEach((item, index) => {
          let state = 0;
          for (const char of fold(item.pattern)) {
            if (!trie[state].next.has(char)) {
              trie[state].next.set(char, trie.length);
              trie.push({ next: new Map(), fail: 0, output: [] });
            }
            state = trie[state].next.get(char);
          }
          trie[state].output.push(index);
        });
        const queue = [...trie[0].next.values()];
        for (let at = 0; at < queue.length; ++at) {
          const state = queue[at];
          for (const [char, child] of trie[state].next) {
            let fallback = trie[state].fail;
            while (fallback && !trie[fallback].next.has(char)) fallback = trie[fallback].fail;
            trie[child].fail = trie[fallback].next.get(char) ?? 0;
            trie[child].output.push(...trie[trie[child].fail].output);
            queue.push(child);
          }
        }
      }
      const classify = text => {
        const counts = patterns.map(() => 0), next = patterns.map(() => 0);
        let state = 0, end = 0;
        for (const char of fold(text)) {
          end += char.length;
          while (state && !trie[state].next.has(char)) state = trie[state].fail;
          state = trie[state].next.get(char) ?? 0;
          for (const index of trie[state].output) {
            const start = end - patterns[index].pattern.length;
            if (overlapping || start >= next[index]) {
              ++counts[index];
              next[index] = end;
            }
          }
        }
        return counts;
      };
      // Extend the existing authorized snapshot/structure owner, not rg's
      // previews: one bounded read per file, normalized/deduplicated selectors,
      // whole-source hashes and immutable recovery for oversized selections.
      const read = hydrate ? capability("read_file") : undefined;
      const run = capability("exec_command");
      // Fixed flags keep the record format independent of user configuration:
      // Search NUL-containing files as text: otherwise rg can silently stop at
      // a NUL without a per-file JSON event, even for a negative result.
      const args = [
        "--no-config",
        "--text",
        "--color",
        "never",
        "--null",
        "--path-separator",
        "/",
        "--with-filename",
      ];
      args.push(ignore_case ? "--ignore-case" : "--case-sensitive");
      if (literal && ignore_case) args.push("--no-unicode");
      if (fixed) args.push("--fixed-strings");
      if (hidden) args.push("--hidden");
      if (no_ignore) args.push("--no-ignore");
      for (const g of globs) args.push("--glob", g);
      // One lookahead distinguishes an exact limit from omitted matches; it is
      // never included in legacy counts, previews or hydration selections.
      args.push(...(jsonSearch
        ? ["--json", ...(lines && !paged ? ["--max-count", String(max_per_file + 1)] : [])]
        : ["--count"]));
      for (const item of patterns) args.push("--regexp", item.pattern);
      args.push("--", ...new Set(paths));
      const scopeOptions = { path: paths, glob: globs, hidden, no_ignore, lines, max_per_file,
        ...(paged ? { page_size: null } : {}) };
      const evidence = { program: "rg", args, semantics,
        coverage: { complete: false, scope: __codex_search_scope(scopeOptions) } };
      // The host advertises this contract in its existing tool metadata. Older
      // hosts (or implicit-ignore searches) retain the safe fresh-execution path.
      const typedReplay =
        no_ignore &&
        (typeof globalThis.resolve_tool === "function"
          ? globalThis.resolve_tool("exec_command")?.description?.includes("(search_miss_v1)")
          : typeof ALL_TOOLS !== "undefined" && ALL_TOOLS.some(
          (tool) =>
            tool.name === "exec_command" &&
            tool.description?.includes("(search_miss_v1)"),
        ));
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
          result = drained.streams_complete === true ? {
            ...drained.terminal, stdout: drained.stdout, stderr: drained.stderr, streams_complete: true,
          } : drained.terminal;
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
            result = cause.evidence.streams_complete === true ? {
              ...terminal, stdout: cause.evidence.stdout, stderr: cause.evidence.stderr, streams_complete: true,
            } : terminal;
          } else {
            evidence.cause = cause;
            fail("search observation needs a decision; resume only its retained handle", evidence);
          }
        }
        evidence.result = result;
      }
      // 1 is ripgrep's no-match status; 2 reports errors, possibly beside partial results.
      const code = result?.exit_code;
      const scope = __codex_search_scope(scopeOptions, result);
      if (code !== 0 && code !== 1 && code !== 2) {
        fail(
          typeof result === "string"
            ? `rg did not run: ${result}`
            : `rg failed (exit_code=${code ?? "unknown"})`,
          evidence,
        );
      }
      // Running observations are display packets. Only the terminal carries
      // exact cumulative streams; intermediate packets need not be complete.
      if (result.streams_complete !== true ||
        typeof result.stdout !== "string" ||
        (result.stderr != null && typeof result.stderr !== "string")) {
        evidence.coverage = { complete: false, lower_bound: true,
          scope, scope_exhausted: null,
          result_limit_hit: null, read_errors: { present: code === 2 ? true : null },
          reasons: ["exact_capture_incomplete"] };
        evidence.delivery = { complete: false, truncated: true, exact_capture_complete: false };
        fail(
          "search output exceeded exact capture; narrow path or glob, or omit lines",
          evidence,
        );
      }
      const errors = (result.stderr ?? "")
        .split("\n")
        .map((line) => line.trimEnd())
        .filter(Boolean);
      // Keep producer diagnostics separate from representation/hydration errors.
      // rg does not provide a structured unreadable-file count; do not invent it.
      const readErrors = { present: code === 2 || errors.length > 0,
        diagnostics: errors.slice(0, 20), omitted_diagnostics: Math.max(0, errors.length - 20) };
      const files = new Map();
      const occurrences = new Map();
      const hydrationHits = new Map();
      const normalizePath = (file) => file.replace(/^(\.\/)+/, "");
      let unparsed = false;
      let binary = false;
      let summarySeen = false;
      let filesSearched = null;
      if (!jsonSearch) {
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
          if (event.type === "summary") {
            summarySeen = true;
            if (Number.isSafeInteger(data?.stats?.searches) && data.stats.searches >= 0)
              filesSearched = data.stats.searches;
          }
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
            ...(lines ? { lines: [] } : {}),
            pattern_counts: patterns.map(item => ({ ...item, occurrences: 0, matching_lines: 0 })),
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
            if (lines && !paged && entry.count >= max_per_file) {
              entry.capped = true;
              files.set(key, entry);
              continue;
            }
            const counts = literal ? classify(data.lines.text.replace(/\n$/, "")) : [data.submatches?.length];
            if (counts.some(count => !Number.isSafeInteger(count) || count < 0)) {
              unparsed = true;
              continue;
            }
            counts.forEach((count, index) => {
              entry.pattern_counts[index].occurrences += count;
              entry.pattern_counts[index].matching_lines += Number(count > 0);
            });
            if (hydrate) {
              const hits = hydrationHits.get(entry) ?? [];
              hydrationHits.set(entry, hits);
              // Context plus enclosing uses at most the owner's 64 selectors.
              if (hits.length < 32)
                hits.push({ line: data.line_number, start: data.absolute_offset,
                  text: data.lines.text });
            }
            if (lines) {
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
              if (paged || entry.lines.length < max_per_file) {
                entry.lines.push({
                  line: data.line_number,
                  text: preview,
                  pattern_counts: counts.flatMap((count, index) => count ? [{ id: patterns[index].id, occurrences: count }] : []),
                  ...(truncated ? { truncated: true,
                    line_byte_start: bytes(text.slice(0, start)),
                    line_byte_end: bytes(text.slice(0, end)),
                    omitted_prefix: start > 0, omitted_suffix: end < text.length } : {}),
                });
              }
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
      const capped = sorted.some(entry => entry.capped === true);
      // Exhaustion describes traversal, not successful reads or full delivery.
      const scopeExhausted = (jsonSearch ? summarySeen || result.search_no_match === true : code !== 2) &&
        !capped && !unparsed && !binary;
      const searchComplete = scopeExhausted && !readErrors.present;
      const counts = jsonSearch ? {
        occurrences: sorted.reduce((sum, entry) => sum + entry.pattern_counts.reduce((n, p) => n + p.occurrences, 0), 0),
        matching_lines: sorted.reduce((sum, entry) => sum + entry.count, 0),
        matching_files: sorted.filter(entry => entry.count > 0).length,
        complete: searchComplete,
        lower_bound: !searchComplete,
      } : undefined;
      const pattern_counts = jsonSearch ? patterns.map((item, index) => ({
        ...item,
        occurrences: sorted.reduce((sum, entry) => sum + entry.pattern_counts[index].occurrences, 0),
        matching_lines: sorted.reduce((sum, entry) => sum + entry.pattern_counts[index].matching_lines, 0),
        matching_files: sorted.filter(entry => entry.pattern_counts[index].matching_lines > 0).length,
        complete: searchComplete,
        lower_bound: !searchComplete,
      })) : undefined;
      let hydration;
      if (hydrate) {
        hydration = { complete: true, truncated: false, files_read: 0,
          max_files: max_hydrated_files, max_matches_per_file: 32,
          max_bytes_per_file: 1024 * 1024 };
        const encoder = new TextEncoder();
        // Serial reads add no detached tasks or new scheduler/lock lifetime.
        // A failed file never discards successful search/read siblings.
        for (const entry of sorted) {
          const hits = hydrationHits.get(entry) ?? [];
          const reasons = [];
          let truncated = false;
          if (entry.binary || !entry.path || !hits.length) {
            reasons.push("source_unavailable");
          } else if (hydration.files_read >= max_hydrated_files) {
            reasons.push("file_limit");
            truncated = true;
          } else {
            ++hydration.files_read;
            const structural = enclosing && /\.(rs|py)$/i.test(entry.path);
            if (enclosing && !structural) reasons.push("enclosing_unsupported");
            if (entry.count > hits.length) {
              reasons.push("match_limit");
              truncated = true;
            }
            const selectors = hits.flatMap(hit => [
              { kind: "lines", start: Math.max(1, hit.line - context_lines),
                end: hit.line + context_lines },
              ...(structural ? [{ kind: "enclosing", line: hit.line }] : []),
            ]);
            try {
              const source = entry.source = await read({ path: entry.path, selectors });
              if (source.complete !== true) reasons.push("selection_incomplete");
              truncated ||= source.continuation != null || source.results?.some(row =>
                ["selector_too_large", "aggregate_omitted"].includes(row.status) ||
                row.continuation != null) === true;
              // rg and read_file observe mutable files at different instants.
              // Bind only exact hit bytes at their original UTF-8 offsets to
              // this hash; never stamp the rg previews with an unverified hash.
              const ranges = (source.results ?? []).filter(row =>
                row.status === "ok" && typeof row.text === "string" && row.canonical_range
              ).map(row => ({ ...row.canonical_range, bytes: encoder.encode(row.text) }));
              let verified = 0;
              for (const hit of hits) {
                if (!Number.isSafeInteger(hit.start) || hit.start < 0) continue;
                const expected = encoder.encode(hit.text);
                const range = ranges.find(range => range.start <= hit.start &&
                  range.end >= hit.start + expected.length);
                if (!range) continue;
                const offset = hit.start - range.start;
                if (expected.every((byte, i) => byte === range.bytes[offset + i])) ++verified;
                else if (!reasons.includes("source_changed")) reasons.push("source_changed");
              }
              entry.matches_verified = verified;
              if (verified !== hits.length && !reasons.includes("source_changed"))
                reasons.push("match_unavailable");
            } catch (cause) {
              reasons.push("read_error");
              entry.hydration_error = { message: cause?.message ?? String(cause),
                ...(cause?.tool_failure ? { tool_failure: cause.tool_failure } : {}) };
            }
          }
          entry.hydration = { complete: reasons.length === 0, truncated, reasons,
            matches_selected: entry.source ? hits.length : 0,
            matches_omitted: entry.count - (entry.source ? hits.length : 0) };
          hydration.complete &&= entry.hydration.complete;
          hydration.truncated ||= truncated;
        }
      }
      const previewTruncated = sorted.some(entry => entry.lines?.some(line => line.truncated));
      const delivery = { complete: !previewTruncated && (!hydration || hydration.complete),
        truncated: previewTruncated || hydration?.truncated === true,
        exact_capture_complete: true, preview_truncated: previewTruncated,
        hydration_truncated: hydration?.truncated === true };
      return {
        pattern,
        ...(count_only ? {} : { files: sorted }),
        file_count: sorted.length,
        match_count: sorted.reduce((sum, entry) => sum + entry.count, 0),
        search_complete: searchComplete,
        coverage: { complete: searchComplete, lower_bound: !searchComplete,
          scope,
          observed: { files_searched: filesSearched, matching_files: sorted.length,
            file_paths: "matching_files_only; nonmatching_and_inaccessible_paths_not_enumerated",
            complete: searchComplete },
          scope_exhausted: scopeExhausted, result_limit_hit: capped, read_errors: readErrors,
          reasons: [ ...(readErrors.present ? ["search_error"] : []), ...(capped ? ["line_limit"] : []),
            ...(unparsed ? ["unrepresentable_output"] : []), ...(binary ? ["binary"] : []) ] },
        semantics: { ...semantics, ...(counts ? {
            overlapping, cross_pattern_overlaps: "independent",
            occurrences: "sum_per_pattern", matching_lines: "union", matching_files: "union" } : {}) },
        ...(counts ? { counts, pattern_counts } : {}),
        delivery,
        complete: searchComplete && (!hydration || hydration.complete),
        ...(hydration ? { hydration,
          truncated: capped || hydration.truncated } : {}),
        ...(errors.length ? { errors: errors.slice(0, 20) } : {}),
        // The host's advisory for this search, such as existing paths near a
        // guessed one that rg could not open. Dropping it cost another search.
        ...(typeof result.repair === "string" && result.repair
          ? { hint: result.repair }
          : {}),
      };
    }),
  });

  Object.defineProperty(globalThis, "await_command", {
    value: async function await_command(
      initial,
      { max_observations = 256, max_wait_ms = 300_000, max_bytes = 4 * 1024 * 1024, on_progress } = {},
    ) {
      try {
        integer(max_observations, 1, 1024, "max_observations");
        integer(max_wait_ms, 5_000, 300_000, "max_wait_ms");
        integer(max_bytes, 65536, 4 * 1024 * 1024, "max_bytes");
        if (on_progress !== undefined && typeof on_progress !== "function")
          throw new TypeError("on_progress must be a function");
      } catch (error) {
        // The producer may already be running. A bad collection option must
        // not erase the caller's only receipt or invite a second launch.
        error.evidence = { terminal: initial, observations: [initial] };
        throw error;
      }
      // Capture the host's steady clock for this observation chain. Wall-clock
      // corrections must not lengthen the budget or invalidate a live handle.
      const now = globalThis.__codex_monotonic_now;
      const deadline = now() + max_wait_ms;
      const observations = [];
      const streamParts = [[], []], streamEnds = [0, 0];
      let streamsExact = true, streamsObserved = false;
      // The evidence budget counts the unique observation representation.
      // Preserve the public terminal alias in JSON/storage; serializing both
      // fields can be larger than this explicitly scoped retained-byte budget.
      let observationBytes = 0;
      const packetSnapshots = [];
      let incremental = on_progress === undefined && typeof nativeIsProxy === "function";
      const evidenceFramingBytes = bytes('{"observations":[]}');
      let current = initial;
      let session;
      let incarnation;
      const reject = (message, evidence, code = "failed", cause) => {
        const error = new Error(message);
        error.evidence = evidence;
        const inherited = cause?.tool_failure ?? current?.tool_failure;
        const handles = [...(inherited?.live_operations ?? [])];
        if (Number.isInteger(current?.session_id) &&
            current.session_capabilities?.polling === true &&
            typeof current.session_capabilities.incarnation === "string") {
          const arguments_ = { session_id: current.session_id,
            incarnation: current.session_capabilities.incarnation, chars: "" };
          if (!handles.some(handle => handle.tool === "write_stdin" &&
              handle.arguments?.session_id === arguments_.session_id &&
              handle.arguments?.incarnation === arguments_.incarnation))
            handles.push({ tool: "write_stdin", arguments: arguments_ });
        }
        error.tool_failure = {
          ...inherited,
          code: inherited?.code ?? code,
          message,
          execution_state: inherited?.execution_state ?? (
            !cause && current?.execution_state === "exited" && current?.process_exited === true
              ? "completed" : "unknown"),
          effects: inherited?.effects ?? "unknown",
          retry_safe: false,
          live_operations: handles,
          safe_next_actions: [...(inherited?.safe_next_actions ?? []),
            "A wait ending does not establish process termination. Preserve the evidence and resume only an owner-issued live handle; do not relaunch the command."],
          details: { ...inherited?.details,
            last_observation: { execution_state: current?.execution_state ?? "unknown",
              process_exited: current?.process_exited ?? null,
              exit_code: current?.exit_code ?? null } },
        };
        throw error;
      };
      while (true) {
        arrayPush(observations, current);
        const evidence = { terminal: current, observations };
        // Concatenate only attributed, contiguous producer bytes. Equal text
        // at later offsets is new output; gaps/unknown slices never certify a
        // complete stream. The terminal receipt itself remains a delta.
        if (current?.stream_chunks_complete === true && Array.isArray(current.stream_ranges) &&
            current.stream_ranges.length === 2) {
          for (const [index, field] of ["stdout", "stderr"].entries()) {
            const range = current.stream_ranges[index], value = current[field];
            if (typeof value !== "string" || !Number.isSafeInteger(range?.start) ||
                !Number.isSafeInteger(range?.end) || range.start !== streamEnds[index] ||
                range.end < range.start || bytes(value) !== range.end - range.start) {
              streamsExact = false;
            } else {
              streamParts[index].push(value);
              streamEnds[index] = range.end;
            }
          }
          streamsObserved = true;
        } else if (current?.stream_ranges?.some(range => range.end !== range.start) || current?.output) {
          streamsExact = false;
        }
        if (current?.execution_state === "exited" && current?.process_exited === true && current.session_id == null) {
          if (streamsExact && streamsObserved) {
            evidence.stdout = streamParts[0].join("");
            evidence.stderr = streamParts[1].join("");
            evidence.streams_complete = true;
          } else if (current.streams_complete === true && typeof current.stdout === "string" && typeof current.stderr === "string") {
            // Compatibility with hosts whose terminal receipt owns both whole
            // streams, rather than independent per-receipt ranges.
            evidence.stdout = current.stdout;
            evidence.stderr = current.stderr;
            evidence.streams_complete = true;
          }
        }
        const replacer = (_key, value) => typeof value === 'bigint' ? value.toString() : value;
        let retainedBytes;
        if (incremental) {
          const snapshot = stableCommandJSON() && commandPacketSnapshot(current);
          incremental = !!snapshot && arrayEvery(packetSnapshots, unchangedCommandPacket);
          if (incremental) {
            arrayPush(packetSnapshots, snapshot);
            const serialized = JSON.stringify(current, replacer);
            // Undefined is null in the observations array.
            const packetBytes = serialized === undefined ? 4 : bytes(serialized);
            observationBytes += packetBytes + (observations.length > 1 ? 1 : 0);
            retainedBytes = observationBytes + evidenceFramingBytes;
          }
        }
        if (!incremental)
          retainedBytes = bytes(JSON.stringify({ observations }, replacer));
        if (
          !current ||
          current.error != null ||
          (current.pending_deferred_completions != null &&
            (!Array.isArray(current.pending_deferred_completions) ||
              current.pending_deferred_completions.length))
        ) {
          reject("command needs a decision", evidence);
        }
        if (current.completion_recovered === true && current.success !== true) {
          reject("retained command outcome is failed or unresolved; reuse its provenance and output reference, not a dead handle", evidence,
            current.success === false ? "failed" : "uncertain_effects");
        }
        const exited =
          current.process_exited === true ||
          current.execution_state === "exited";
        if (exited) {
          if (
            current.process_exited !== true ||
            current.execution_state !== "exited"
          ) {
            reject("inconsistent command exit state", evidence);
          }
          // An exited process can still own unread output. Drain its authorized
          // handle before interpreting the final exit code, including failures.
          if (current.session_id == null) {
            if (
              current.exit_code === 0 ||
              (current.exit_code === 1 && current.search_no_match === true)
            ) {
              if (retainedBytes >= max_bytes)
                reject("command succeeded but its completed evidence reached the byte limit; retain these observations, do not restart the command", evidence);
              return evidence;
            }
            reject(
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
          reject("missing or changed live command handle", evidence);
        }
        if (retainedBytes >= max_bytes)
          reject("command evidence byte limit reached; retain these observations and resume only the retained handle", evidence);
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
          reject("command observation limit reached", evidence);
        try {
          if (on_progress && (await on_progress(current)) !== true)
            reject("command progress needs review", evidence);
          const remaining = Math.floor(deadline - now());
          // Empty native polls have a five-second floor. Do not launch a
          // fresh observation that cannot fit in the remaining wait budget.
          if (remaining < 5_000)
            reject(
              silent
                ? "command wait budget reached with no output observed; inspect and resume the retained handle"
                : "command wait budget reached; inspect and resume the retained handle",
              evidence, "timed_out",
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
          reject(
            "command observation failed; resume only the retained handle",
            evidence, "failed", cause,
          );
        }
      }
    },
  });
})();
