// Opt-in adapters over native readers. No context or history is removed,
// no new authority is granted, and no producer or source is reopened on restore.
(() => {
  const hash = value => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
  const tool = name => {
    const call = resolve_tool(name);
    if (typeof call !== "function") throw new TypeError(`required tool unavailable: ${name}`);
    return call;
  };
  const fail = (message, evidence) => {
    const error = new Error(`read_context: ${message}`);
    error.evidence = evidence;
    throw error;
  };
  const complete = initial => initial?.complete === true &&
    Array.isArray(initial.results) && initial.results.length > 0 &&
    initial.results.every(row => row.status === "ok" && row.complete === true);
  const hasOwner = r => typeof r?.canonical_uri === "string" && typeof r.environment_id === "string";
  const sameOwner = (a, b) => hasOwner(a) && a.canonical_uri === b?.canonical_uri && a.environment_id === b.environment_id;
  const key = query => JSON.stringify([
    query.path, query.detail, query.environment_id ?? null, query.selectors ?? null,
  ]);

  Object.defineProperty(globalThis, "read_context", {
    value: async function read_context(path, {
      detail, selectors, preview_lines, environment_id, previous,
    } = {}) {
      if (typeof path !== "string" || !path || /^(?:context|skill):/.test(path))
        throw new TypeError("read_context requires a filesystem path");
      if (environment_id !== undefined && (typeof environment_id !== "string" || !environment_id))
        throw new TypeError("read_context requires a nonempty environment_id");
      if (!["ranges", "preview", "full", "defer"].includes(detail))
        throw new TypeError("read_context requires explicit detail: ranges, preview, full or defer");
      if (detail === "ranges") {
        if (!Array.isArray(selectors) || !selectors.length)
          throw new TypeError("ranges requires native read_file selectors");
        selectors = JSON.parse(JSON.stringify(selectors));
      } else if (selectors !== undefined) {
        throw new TypeError("selectors is only valid for ranges");
      }
      if (detail === "preview") {
        if (!Number.isSafeInteger(preview_lines) || preview_lines < 1)
          throw new TypeError("preview requires a positive preview_lines");
        selectors = [{ kind: "lines", start: 1, end: preview_lines }];
      } else if (preview_lines !== undefined) {
        throw new TypeError("preview_lines is only valid for preview");
      }

      const query = { path, detail, environment_id, selectors };
      const prior = previous?.initial;
      const reuse = detail !== "defer" && previous?.query && key(query) === key(previous.query) &&
        previous.selection_complete === true && prior?.unchanged !== true &&
        hash(prior?.source_sha256) && hasOwner(prior) &&
        (detail === "full" ? previous.file_complete === true : complete(prior));
      if (detail === "full") {
        const [row] = await read_files([path], {
          full: true, environment_id,
          ...(reuse && { refresh_hashes: Object.fromEntries([[path, prior.source_sha256]]) }),
        });
        if (row.status !== "fulfilled") throw row.reason;
        const result = row.value;
        if (result.unchanged && (!reuse || !sameOwner(prior, result.initial)))
          fail("freshness receipt did not identify the previous owner", result);
        return { ...result, query, selection_complete: result.file_complete === true };
      }

      const initial = await tool("read_file")({
        path, environment_id, selectors: detail === "defer" ? [{ kind: "bytes", start: 0, end: 1 }] : selectors,
        ...(reuse && { if_source_hash: prior.source_sha256 }),
      });
      if (initial?.unchanged === true) {
        if (!reuse || initial.source_sha256 !== prior.source_sha256 || !sameOwner(prior, initial) ||
            initial.complete !== false || initial.retained_bytes !== 0 ||
            initial.artifact_id || !Array.isArray(initial.results) || initial.results.length !== 0)
          fail("inconsistent freshness receipt", initial);
        return { query, initial, pages: [], selection_complete: false, unchanged: true };
      }

      if (detail === "defer") {
        const sha256 = initial?.canonical_sha256 ?? initial?.source_sha256;
        const bytes = initial?.canonical_bytes;
        if (!complete(initial) || initial.retained_artifact_complete !== true ||
            typeof initial.artifact_id !== "string" || !initial.artifact_id ||
            !hash(sha256) || !Number.isSafeInteger(bytes) || bytes < 1)
          fail("no complete native snapshot; use full or ranges", initial);
        return { artifact_id: initial.artifact_id, sha256, bytes };
      }
      return { query, initial, pages: [], selection_complete: complete(initial) };
    },
  });

  Object.defineProperty(globalThis, "search_context", {
    value: async function search_context(pattern, { owners = [], consumers = [], ...options } = {}) {
      if (!Array.isArray(owners) || !Array.isArray(consumers) || options.preferred_paths !== undefined)
        throw new TypeError("search_context takes owners and consumers instead of preferred_paths");
      const preferred_paths = [...new Set([...owners, ...consumers])];
      if (preferred_paths.length > 32 || preferred_paths.some(path => typeof path !== "string" || !path))
        throw new TypeError("search_context requires at most 32 distinct, nonempty result paths");
      if (preferred_paths.length && options.hydrate === false)
        throw new TypeError("search_context preferences require hydration");
      return search_files(pattern, { ...options, hydrate: options.hydrate ?? true,
        ...(preferred_paths.length && { preferred_paths }) });
    },
  });

  Object.defineProperty(globalThis, "restore_context", {
    value: async function restore_context({ artifact_id, sha256, bytes }, selectors) {
      if (!hash(sha256) || !Number.isSafeInteger(bytes) || bytes < 1)
        throw new TypeError("restore_context requires the complete deferred snapshot locator");
      const selected = await select_output({ artifact_id, selectors }, {
        expected_sha256: sha256, expected_bytes: bytes,
      });
      if (selected.initial.historical_authenticity !== "authenticated")
        fail("restoration did not return an authenticated snapshot", selected);
      return selected;
    },
  });

  Object.defineProperty(globalThis, "pack_context_rows", {
    value: function pack_context_rows(rows, columns) {
      if (!Array.isArray(rows) || !Array.isArray(columns) || !columns.length ||
          new Set(columns).size !== columns.length || columns.some(key => typeof key !== "string"))
        throw new TypeError("pack_context_rows requires rows and distinct string columns");
      if (!rows.length || rows.some(row => !row || Array.isArray(row) || typeof row !== "object" ||
          Object.keys(row).length !== columns.length ||
          columns.some(key => !Object.prototype.propertyIsEnumerable.call(row, key) || row[key] === undefined)))
        return rows;
      return { columns: [...columns], rows: rows.map(row => columns.map(key => row[key])) };
    },
  });
})();
