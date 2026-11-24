function makeSourceRetrieval(search) {
  const maxPatterns = 32;
  const allowedOptions = new Set(["path", "glob", "hidden", "no_ignore", "max_per_file", "force_fresh"]);

  function patterns(values, label) {
    if (!Array.isArray(values) || values.length > maxPatterns ||
        values.some(value => typeof value !== "string" || !value.length)) {
      throw new TypeError(label + " must contain at most 32 nonempty literal strings");
    }
    return [...new Set(values)].map((pattern, index) => ({ id: label + index, pattern }));
  }

  function hits(result, entries) {
    const byId = new Map(entries.map(entry => [entry.id, entry.pattern]));
    return result.files.flatMap(file => file.lines.flatMap(line =>
      line.pattern_counts.flatMap(({ id, occurrences }) =>
        byId.has(id) ? [{ path: file.path, line: line.line, literal: byId.get(id), occurrences }] : [])));
  }

  return async function retrieve_source({ exact, related = [], relax = false }, options = {}) {
    if (!options || typeof options !== "object" || Array.isArray(options) ||
        Object.keys(options).some(key => !allowedOptions.has(key))) {
      throw new TypeError("retrieve_source accepts only literal-search scope and per-file limit options");
    }
    const exactPatterns = patterns(exact, "exact");
    let relatedPatterns = patterns(related, "related");
    if (!exactPatterns.length || typeof relax !== "boolean" ||
        (relax && (exactPatterns.length !== 1 || relatedPatterns.length !== 0))) {
      throw new TypeError("supply at least one exact literal; relaxation requires one exact literal and no related terms");
    }
    if (relax) {
      // Only relax a plain identifier phrase. Do not reinterpret punctuation,
      // case, codes, or a single identifier as a different query.
      const phrase = exactPatterns[0].pattern;
      const terms = phrase.split(/[ \t]+/);
      if (terms.length >= 2 && terms.length <= 8 &&
          terms.every(term => /^[A-Za-z_][A-Za-z_0-9]{2,}$/.test(term))) {
        const unique = [...new Set(terms)];
        if (unique.length >= 2) relatedPatterns = patterns(unique, "related");
      }
    }

    const searchOptions = { ...options, fixed: true, lines: true };
    const combined = await search([...exactPatterns, ...relatedPatterns], searchOptions);
    const failedScope = combined.coverage?.read_errors?.present === true &&
      combined.coverage?.result_limit_hit === false;
    // Related matches can consume the shared per-file line cap before an exact
    // match. Repeat only the exact patterns instead of certifying a false miss.
    // Relaxation does not requery a known read failure or treat it as a miss.
    const exactFallback = relatedPatterns.length && !combined.search_complete &&
      !(relax && failedScope) ? await search(exactPatterns, searchOptions) : null;
    const exactReceipt = exactFallback ?? combined;
    const exactMatches = hits(exactReceipt, exactPatterns);
    const relatedMatches = hits(combined, relatedPatterns);
    const selection = !relax ? "mixed" : exactMatches.length ? "exact" :
      !exactReceipt.search_complete ? "incomplete" : relatedMatches.length ? "relaxed" :
        combined.search_complete ? "none" : "incomplete";
    return {
      combined,
      exact_fallback: exactFallback,
      exact_matches: exactMatches,
      exact_complete: exactReceipt.search_complete === true,
      related_matches: !relax || selection === "relaxed" ? relatedMatches : [],
      selection,
    };
  };
}

Object.defineProperty(globalThis, "retrieve_source", {
  value: makeSourceRetrieval(globalThis.search_files),
});
