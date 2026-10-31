// Intended scope comes from the existing launch/path owner, never match paths.
// Missing old-host provenance remains unknown, including on empty searches.
const __codex_search_scope = (options, result) => {
  const receipt = result?.environment_receipt;
  const environments = [...(receipt?.roots ?? []), ...(receipt?.shared_instance_references ?? [])]
    .map(root => root.environment).filter(env => Array.isArray(env?.search_roots));
  if (Array.isArray(receipt?.unobserved_launch_environment?.search_roots))
    environments.push(receipt.unobserved_launch_environment);
  const environment = environments[0];
  const paths = typeof options.path === "string" ? [options.path] : options.path ?? ["."];
  const globs = typeof options.glob === "string" ? [options.glob] : options.glob ?? [];
  const roots = environment?.search_roots ?? paths.map(requested => ({ requested, resolved: null,
    error: "launch_scope_unavailable" }));
  return { kind: "intended", paths: [...paths], globs: [...globs],
    environment_id: environment?.environment_id ?? null, cwd: environment?.cwd ?? null,
    resolved_roots: roots, resolution: "lexical_not_canonical_or_access_verified",
    resolution_complete: !!environment && roots.every(root => typeof root.resolved === "string"),
    filters: { include: globs.filter(glob => !glob.startsWith("!")),
      exclude: globs.filter(glob => glob.startsWith("!")), precedence: "ordered_ripgrep_globs" },
    hidden: options.hidden ?? false, no_ignore: options.no_ignore ?? false, binary: "text",
    policies: { hidden: options.hidden ? "included" : "ripgrep_default; explicit_paths_or_globs_can_override",
      ignored: options.no_ignore ? "ignore_files_disabled; globs_still_apply"
        : "ripgrep_default_ignore_files; explicit_paths_or_globs_can_override",
      symlinks: "ripgrep_default_no_follow", configuration: "--no-config" },
    limits: { max_depth: null, max_directories: null,
      max_matches_per_file: options.page_size !== undefined ? null : options.lines ? options.max_per_file ?? 10 : null,
      page_size: options.page_size ?? null, inventory_files: options.page_size !== undefined ? 128 : null,
      diagnostic_lines: 20, exact_capture: "bounded_by_exec_command_and_await_command" } };
};

// Extend the existing rg helper, session store and guarded checksum owners.
// State is immutable after admission: concurrent/repeated cursors select the
// same page, without a shared advancing offset, detached tasks or new locks.
// Share effective matching settings with ordinary results and cursor refusals.
// Default only omitted values: null/non-booleans must never select another mode.
const __codex_search_semantics = (pattern, {
  fixed = Array.isArray(pattern), ignore_case = false, overlapping = false,
} = {}) => {
  for (const [name, value] of Object.entries({ fixed, ignore_case, overlapping }))
    if (typeof value !== "boolean") throw new TypeError(`${name} must be boolean`);
  if (Array.isArray(pattern) && !fixed)
    throw new TypeError("multi-pattern search supports fixed strings only");
  if (overlapping && !fixed) throw new TypeError("overlapping requires fixed strings");
  return { fixed, case_sensitive: !ignore_case,
    case_folding: !ignore_case ? "none" : fixed && (Array.isArray(pattern) || overlapping)
      ? "ascii" : "ripgrep_unicode" };
};
const __codex_paginate_search = (search) => {
  const prefix = "__search_files_v1_";
  const ttl = 15 * 60 * 1000;
  const token = () => Math.random().toString(36).slice(2) + Math.random().toString(36).slice(2);
  const inventory = async (options) => {
    const args = ["--no-config", "--files", "--null", "--path-separator", "/"];
    if (options.hidden) args.push("--hidden");
    if (options.no_ignore) args.push("--no-ignore");
    for (const glob of options.glob) args.push("--glob", glob);
    args.push("--", ...options.path);
    let result = await tools.exec_command({ program: "rg", args, force_fresh: true });
    if (result?.session_id != null) {
      try {
        const collected = await await_command(result);
        result = collected.streams_complete === true ? {
          ...collected.terminal, stdout: collected.stdout, stderr: collected.stderr, streams_complete: true,
        } : collected.terminal;
      } catch (cause) {
        const terminal = cause?.evidence?.terminal;
        if (!cause?.message?.startsWith("command did not succeed (") ||
            terminal?.execution_state !== "exited" || terminal.process_exited !== true ||
            terminal.session_id != null || terminal.exit_code !== 1) throw cause;
        result = cause.evidence.streams_complete === true ? {
          ...terminal, stdout: cause.evidence.stdout, stderr: cause.evidence.stderr, streams_complete: true,
        } : terminal;
      }
    }
    if (![0, 1].includes(result?.exit_code) || result.streams_complete !== true ||
        typeof result.stdout !== "string" || result.stderr ||
        (result.stdout && !result.stdout.endsWith("\0"))) {
      const error = new Error("search inventory is incomplete");
      error.evidence = { result, scope: __codex_search_scope(options, result) };
      throw error;
    }
    const paths = [...new Set(result.stdout.split("\0").filter(Boolean))].sort();
    const receipt = __codex_search_scope(options, result);
    if (paths.length > 128) {
      const error = new Error("paged search scope exceeds 128 files; narrow path or glob");
      error.evidence = { scope: receipt, inventory_files: paths.length, inventory_limit: 128 };
      throw error;
    }
    return { paths, receipt };
  };
  const identities = async (scope) => {
    const sources = [];
    let environment_id;
    // Each read is cancellable through its existing native tool owner. Keep
    // serial admission; no task can outlive a failed/cancelled page request.
    for (const path of scope.paths) {
      // The checksum owner resolves cwd-relative names and binds their absolute
      // paths/environment as well as their opened-object identities. Empty
      // inventories cannot yield a continuation and need no synthetic identity.
      const source = await tools.checksum_file({ path, ...(environment_id ? { environment_id } : {}) });
      if (typeof source.environment_id !== "string" ||
          (environment_id !== undefined && source.environment_id !== environment_id) || typeof source.path !== "string" ||
          !/^[0-9a-f]{64}$/.test(source.sha256) || typeof source.source_identity !== "string")
        throw new Error("source identity unavailable");
      environment_id = source.environment_id;
      sources.push({ path: source.path, environment_id, sha256: source.sha256,
        source_identity: source.source_identity, bytes_hashed: source.bytes_hashed });
    }
    return sources;
  };
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  return async function search_files(pattern, options = {}) {
    if (options.page_size === undefined && options.cursor === undefined)
      return search(pattern, options);
    if (!Number.isInteger(options.page_size) || options.page_size < 1 || options.page_size > 1000)
      throw new TypeError("paged search requires page_size from 1 through 1000 on every call");
    if (options.count_only) throw new TypeError("count_only cannot paginate results");
    if (options.cursor !== undefined && typeof options.cursor !== "string")
      throw new TypeError("cursor must be an opaque search continuation string");
    const semantics = __codex_search_semantics(pattern, options);
    let currentScope;
    const incomplete = (pattern, reason, message) => ({
      pattern, semantics, files: [], file_count: 0, match_count: 0,
      complete: false, search_complete: false, continuation: null,
      coverage: { complete: false, lower_bound: true, consistent: false,
        scope: currentScope, scope_exhausted: null, result_limit_hit: null,
        read_errors: { present: null }, reasons: [reason] },
      delivery: { complete: false, truncated: false },
      pagination: { consistent: false, reason },
      ...(message ? { errors: [message] } : {}),
    });
    // Copy the complete effective request before any native await, including
    // pattern identities and ordered globs. Later caller mutation cannot rebind it.
    const request = JSON.parse(JSON.stringify({ pattern, options: {
      path: typeof (options.path ?? ".") === "string" ? [options.path ?? "."] : options.path,
      glob: typeof (options.glob ?? []) === "string" ? [options.glob] : options.glob ?? [],
      ignore_case: !semantics.case_sensitive, fixed: semantics.fixed,
      hidden: options.hidden ?? false, no_ignore: options.no_ignore ?? false,
      lines: true, max_per_file: options.max_per_file ?? 10,
      force_fresh: true, hydrate: options.hydrate ?? false, enclosing: options.enclosing ?? false,
      context_lines: options.context_lines ?? 3, max_hydrated_files: options.max_hydrated_files ?? 8,
      count_only: false, overlapping: options.overlapping ?? false,
    }, page_size: options.page_size }));
    const opts = request.options;
    if (!Array.isArray(opts.path) || opts.path.length < 1 || opts.path.length > 32 ||
        opts.path.some(path => typeof path !== "string" || !path.trim()) ||
        !Array.isArray(opts.glob) || opts.glob.length > 64 ||
        opts.glob.some(glob => typeof glob !== "string" || !glob))
      throw new TypeError("invalid paged search paths or globs");
    currentScope = __codex_search_scope({ ...opts, page_size: request.page_size });
    let state, id, index = 0;
    if (options.cursor !== undefined) {
      const parts = /^search-v1:([a-z0-9]+):(\d+):([a-z0-9]+)$/.exec(options.cursor);
      if (!parts) return incomplete(pattern, "invalid_cursor");
      id = parts[1];
      state = load(prefix + id);
      index = Number(parts[2]);
      if (!state || state.version !== 1 || Date.now() >= state.expires_at) {
        // Reclaim only this helper's expired state, never unrelated session work.
        if (state?.version === 1 && Date.now() >= state.expires_at) deleteStored(prefix + id);
        return incomplete(pattern, "traversal_state_expired");
      }
      if (!Number.isSafeInteger(index) || index < 1 || state.cursors[index] !== options.cursor)
        return incomplete(pattern, "invalid_cursor");
      if (!same(request, state.request)) return incomplete(pattern, "search_parameters_changed");
    }
    try {
      const scope = await inventory(opts);
      currentScope = { ...scope.receipt, limits: { ...scope.receipt.limits,
        max_matches_per_file: null, page_size: request.page_size, inventory_files: 128 } };
      if (state && !same(scope, state.scope)) return incomplete(pattern, "search_scope_changed");
      const sources = await identities(scope);
      if (state && !same(sources, state.sources)) return incomplete(pattern, "source_changed");
      if (state && !same(scope, await inventory(opts))) return incomplete(pattern, "search_scope_changed");
      if (!state) {
        const result = await search(request.pattern, opts, true);
        if (!result.search_complete) return { ...result, continuation: null,
          coverage: { ...result.coverage, scope: currentScope },
          pagination: { consistent: false, reason: "search_incomplete" } };
        const after = await inventory(opts);
        if (!same(scope, after)) return incomplete(pattern, "search_scope_changed");
        if (!same(sources, await identities(after))) return incomplete(pattern, "source_changed");
        // Native parsing already deduplicates path/line identities. Sort once,
        // then retain exact page positions, independent of rg's worker order.
        for (const entry of result.files) entry.lines.sort((a, b) => a.line - b.line);
        do { id = token(); } while (load(prefix + id) !== undefined);
        const pages = Math.ceil(result.match_count / request.page_size);
        state = { version: 1, expires_at: Date.now() + ttl, request, scope, sources, result,
          cursors: Array.from({ length: pages }, (_, page) => `search-v1:${id}:${page}:${token()}`) };
        // Storage admission is transactional and bounded by the existing 8 MiB
        // session quota. Failure returns no usable cursor and no clean coverage.
        if (pages > 1) store(prefix + id, state);
      }
      if (Date.now() >= state.expires_at) return incomplete(pattern, "traversal_state_expired");
      const start = index * request.page_size, end = Math.min(start + request.page_size, state.result.match_count);
      let offset = 0;
      const files = [];
      for (const entry of state.result.files) {
        const selected = entry.lines.slice(Math.max(0, start - offset), Math.max(0, end - offset));
        offset += entry.lines.length;
        if (!selected.length) continue;
        files.push({ ...entry, lines: selected, count: selected.length,
          pattern_counts: entry.pattern_counts.map(item => ({ ...item,
            occurrences: selected.reduce((sum, line) => sum + (line.pattern_counts.find(p => p.id === item.id)?.occurrences ?? 0), 0),
            matching_lines: selected.filter(line => line.pattern_counts.some(p => p.id === item.id)).length })) });
      }
      const more = end < state.result.match_count;
      const pattern_counts = state.result.pattern_counts.map(item => ({ ...item,
        occurrences: files.reduce((sum, file) => sum + file.pattern_counts.find(p => p.id === item.id).occurrences, 0),
        matching_lines: files.reduce((sum, file) => sum + file.pattern_counts.find(p => p.id === item.id).matching_lines, 0),
        matching_files: files.filter(file => file.pattern_counts.some(p => p.id === item.id && p.matching_lines > 0)).length,
        complete: !more, lower_bound: more }));
      return { ...state.result, files, file_count: files.length, match_count: end - start,
        pattern_counts, counts: { occurrences: pattern_counts.reduce((sum, p) => sum + p.occurrences, 0),
          matching_lines: end - start, matching_files: files.length, complete: !more, lower_bound: more },
        complete: !more && state.result.complete, search_complete: !more,
        coverage: { ...state.result.coverage, scope: currentScope, complete: !more, lower_bound: more, consistent: true,
          result_limit_hit: more, reasons: more ? ["page_limit"] : [] },
        continuation: more ? state.cursors[index + 1] : null,
        pagination: { consistent: true, start, end, total_matching_lines: state.result.match_count,
          expires_at: state.expires_at, source_count: state.sources.length,
          consistency: "guarded_observations_not_atomic_snapshot" } };
    } catch (cause) {
      // Preserve owner-issued live handles and exact-capture recovery on failure.
      if (cause?.evidence?.scope) currentScope = { ...cause.evidence.scope,
        limits: { ...currentScope.limits } };
      return { ...incomplete(pattern, "traversal_unavailable", cause?.message ?? String(cause)),
        ...(cause?.evidence ? { evidence: cause.evidence } : {}),
        ...(cause?.tool_failure ? { tool_failure: cause.tool_failure } : {}) };
    }
  };
};
