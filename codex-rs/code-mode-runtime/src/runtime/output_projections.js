(() => {
  "use strict";
  const sha256 = s => typeof s === "string" && /^[0-9a-f]{64}$/.test(s);
  const nativeText = r => r?.file_complete === true && r.results?.length === 1 &&
    typeof r.results[0].text === "string" && r.results[0].complete !== false && sha256(r.source_sha256)
    ? r.results[0].text : undefined;
  const output = (compact, full) =>
    JSON.stringify(compact).length < JSON.stringify(full).length ? compact : full;
  const encode = new TextEncoder();

  async function readSymbols(path, names, options = {}) {
    if (!Array.isArray(names) || names.length === 0 || names.length > 64 ||
        names.some(name => typeof name !== "string" || !name.length)) {
      throw new TypeError("Expected 1 to 64 named source items");
    }
    return tools.read_file({
      path, revision: options.revision, environment_id: options.environment_id,
      selectors: names.map(name => ({kind: "symbol", name})),
    });
  }

  // Base stays in cell storage; only the observation needs model delivery.
  async function refreshFile(path, base, environment_id) {
    if (/^(skill:|context:)/.test(path)) throw new TypeError("Expected a filesystem source path");
    const prior = base?.requestedPath === path && base.environment_id === (environment_id ?? null) &&
      typeof base.text === "string" && sha256(base.sha) ? base : undefined;
    const r = await tools.read_file({path, environment_id, if_source_hash: prior?.sha});
    if (prior && r.unchanged === true && r.path === prior.path) {
      return {observation: {kind: "unchanged", sha: prior.sha}, base: prior};
    }
    const text = nativeText(r);
    if (text === undefined || r.source_encoding || /\.html?$/i.test(r.path) ||
        encode.encode(text).length !== r.canonical_bytes) {
      return {observation: {kind: "unprojected", receipt: r}, base: null};
    }
    const next = {requestedPath: path, path: r.path, environment_id: environment_id ?? null,
      sha: r.source_sha256, text};
    const full = {kind: "full", sha: next.sha, text};
    if (!prior || prior.path !== next.path) return {observation: full, base: next};
    // Use code points so matching prefixes never split surrogate pairs.
    const a = Array.from(prior.text), b = Array.from(text);
    let start = 0, tail = 0;
    const shortest = Math.min(a.length, b.length);
    while (start < shortest && a[start] === b[start]) start++;
    while (tail < shortest - start && a[a.length-1-tail] === b[b.length-1-tail]) tail++;
    const delta = {kind: "delta", base: prior.sha, sha: next.sha,
      start, end: a.length-tail, insert: b.slice(start,b.length-tail).join("")};
    return {observation: output(delta, full), base: next};
  }

  function groupDiagnostics(r) {
    const original = nativeText(r);
    if (original === undefined || !r.artifact_id || r.retained_artifact_complete !== true)
      return {kind: "unprojected", receipt: r};
    const full = {kind: "full", text: original, artifact_id: r.artifact_id};
    const groups = new Map(), rows = original.split("\n");
    if (rows.at(-1) === "") rows.pop();
    for (const [i,line] of rows.entries()) {
      let row;
      try { row = JSON.parse(line); } catch { return full; }
      // An optional replay wrapper may vary only by attempt. Every other field,
      // including Cargo package/target identity in compiler-message rows, keys the group.
      const replay = row && Object.keys(row).length === 2 &&
        Number.isSafeInteger(row.attempt) && row.diagnostic?.$message_type === "diagnostic";
      if (!replay && row?.$message_type !== "diagnostic" && row?.reason !== "compiler-message")
        return full;
      const diag = replay ? row.diagnostic : row;
      const key = JSON.stringify([replay,diag]);
      if (!groups.has(key)) groups.set(key, {diag, occ: []});
      groups.get(key).occ.push(replay ? [i+1,row.attempt] : [i+1]);
    }
    return output({kind: "grouped", artifact_id: r.artifact_id, groups: [...groups.values()]}, full);
  }

  function cargoEdges(m, packageNameOrId) {
    if (!Array.isArray(m?.packages) || !Array.isArray(m.workspace_members)) {
      throw new TypeError("Expected complete Cargo metadata");
    }
    const ids = new Set(m.workspace_members);
    const members = m.packages.filter(p => ids.has(p.id));
    if (members.length !== ids.size || new Set(members.map(p=>p.id)).size !== ids.size ||
        members.some(p=>typeof p.manifest_path!=="string" || !Array.isArray(p.dependencies))) {
      throw new TypeError("Workspace metadata lacks member or dependency details");
    }
    const exact = members.find(p=>p.id===packageNameOrId);
    const matches = exact ? [exact] : members.filter(p=>p.name===packageNameOrId);
    if (matches.length !== 1) throw new TypeError("Expected a unique workspace package; use its Cargo ID");
    const selected = matches[0], path = p => p?.replaceAll("\\","/").replace(/\/$/,""),
      dir = p => path(p.manifest_path).replace(/\/[^/]+$/,"");
    // Match the path and name, not name alone (a registry crate may share it).
    return members.flatMap(p=>p.dependencies.filter(d=>p.id===selected.id ||
      (d.name===selected.name && typeof d.path==="string" && path(d.path)===dir(selected)))
      .map(d=>({from:p.name, id:p.id, name:d.name, alias:d.rename??null, kind:d.kind??null,
        target:d.target??null, path:d.path??null, source:d.source??null})));
  }

  function repeatRuns(r) {
    if (r?.exit_code !== 0 || r.session_id != null || r.error != null ||
        r.streams_complete !== true || r.retained_artifact_complete !== true ||
        !r.raw_output_artifact_id || typeof r.stdout !== "string" || r.stderr !== "") {
      return {kind: "unprojected", receipt: r};
    }
    const full = {kind:"full", text:r.stdout, artifact_id:r.raw_output_artifact_id};
    const runs = [];
    for (const line of r.stdout.match(/[^\n]*\n|[^\n]+$/g) ?? []) {
      if (runs.at(-1)?.[1] === line) runs.at(-1)[0]++;
      else runs.push([1,line]);
    }
    return output({kind:"runs", artifact_id:r.raw_output_artifact_id, runs},full);
  }

  function factorContracts(names) {
    if (!Array.isArray(names) || !names.length || names.length > 64 ||
        names.some(n=>typeof n!=="string" || !n.length)) throw new TypeError("Expected 1 to 64 exact tool names");
    const descriptions = names.map(n=>resolve_tool(n).description);
    const entries = descriptions.map(s=>s.split("\n")), counts = new Map();
    for (const row of entries) for (const line of row) counts.set(line,(counts.get(line)??0)+1);
    const table = [...counts].filter(([s,n])=>n>1 && s.length>=24).map(([s])=>s);
    const indices = new Map(table.map((s,i)=>[s,i]));
    return output({kind:"factored", names, table, rows:entries.map(row=>row.map(s=>indices.get(s)??s))},
      {kind:"full", names, descriptions});
  }

  function processDelta(receipt) {
    // No caching, polling, or launching; preserve the owner's identity and recovery.
    const {session_id, session_capabilities, output, stdout, stderr, streams_complete,
      stream_chunks_complete, raw_output_artifact_id, retained_artifact_complete,
      exit_code, error, execution_state, process_exited, output_reference, recovery, chunk_id,
      stream_ranges, current_chunk_display_complete, output_reduced} = receipt;
    if (error != null || (exit_code != null && exit_code !== 0) || receipt.status_only ||
        receipt.output_truncated || receipt.truncated || receipt.interaction_error || receipt.tool_failure) {
      return {kind:"unprojected", receipt};
    }
    return {kind:"delta", session_id, session_capabilities, output, stdout, stderr,
      streams_complete, stream_chunks_complete, raw_output_artifact_id, retained_artifact_complete,
      exit_code, execution_state, process_exited, output_reference, recovery, chunk_id, stream_ranges,
      current_chunk_display_complete, output_reduced};
  }

  // Pack opt-in, string-or-null columns in a single projected page. Keep the
  // owning selection and its continuation with the caller; this is presentation.
  function packStructuredRows(page, fields) {
    if (!Array.isArray(fields) || fields.length < 1 || fields.length > 64 ||
        new Set(fields).size !== fields.length ||
        fields.some(f => typeof f !== "string" || (f !== "" && !f.startsWith("/")) || /~(?:[^01]|$)/.test(f)))
      throw new TypeError("Expected 1 to 64 unique RFC 6901 field pointers");
    const plain = {page};
    if (page?.coverage_complete !== true || page.malformed_rows !== 0 ||
        !Array.isArray(page.rows) || page.rows.length === 0 || page.rows.length > 1000 ||
        page.rows.some(r => !Number.isSafeInteger(r?.row) || !r.value ||
          typeof r.value !== "object" || Array.isArray(r.value))) return plain;

    const columns = new Map();
    for (const field of fields) {
      const values = new Map();
      let strings = 0, usable = true;
      for (const {value} of page.rows) {
        if (!Object.hasOwn(value, field) || value[field] === null) continue;
        const v = value[field];
        if (typeof v !== "string") { usable = false; break; }
        strings++;
        if (!values.has(v)) values.set(v, values.size);
      }
      if (usable && values.size < strings) columns.set(field, values);
    }
    if (!columns.size) return plain;
    const dictionaries = Object.fromEntries([...columns].map(([f, v]) => [f, [...v.keys()]]));
    const rows = page.rows.map(r => ({...r, value:Object.fromEntries(Object.entries(r.value)
      .map(([f, v]) => [f, typeof v === "string" && columns.has(f) ? columns.get(f).get(v) : v]))}));
    const packed = {page:{...page, rows}, dictionaries};
    // Native structured JSON keeps wide integers as BigInt. Count their exact
    // JSON decimal bytes without altering the packet or rounding through Number.
    const bytes = packet => encode.encode(JSON.stringify(packet, (_key, value) =>
      typeof value === "bigint" ? JSON.rawJSON(value.toString()) : value)).length;
    return bytes(packed) < bytes(plain) ? packed : plain;
  }

  function unpackStructuredRows({page, dictionaries}) {
    if (dictionaries === undefined) return page;
    if (!dictionaries || typeof dictionaries !== "object" || Array.isArray(dictionaries) ||
        !Array.isArray(page?.rows) || page.rows.length > 1000 || Object.keys(dictionaries).length > 64 ||
        Object.values(dictionaries).some(values => !Array.isArray(values) ||
          values.length > 1000 || values.some(v => typeof v !== "string")))
      throw new TypeError("Invalid structured row dictionaries");
    const rows = page.rows.map(r => ({...r, value:Object.fromEntries(Object.entries(r.value)
      .map(([f, v]) => {
        if (!Object.hasOwn(dictionaries, f) || v === null) return [f, v];
        if (!Number.isSafeInteger(v) || v < 0 || !Object.hasOwn(dictionaries[f], v))
          throw new TypeError("Invalid structured row dictionary index");
        return [f, dictionaries[f][v]];
      }))}));
    return {...page, rows};
  }

  // Narrow policy for workflows that define all stderr or a nonzero exit as
  // unexpected. Never infer success from the quiet display string.
  function expectedCommand(receipt) {
    const ref = receipt?.output_reference;
    if (receipt?.quiet_success !== true || receipt.success !== true || receipt.exit_code !== 0 ||
        receipt.process_exited !== true || receipt.output_drained !== true || receipt.streams_complete !== true ||
        typeof receipt.stdout !== "string" || receipt.stderr !== "" || receipt.session_id != null ||
        receipt.error != null || receipt.tool_failure || receipt.raw_output_artifact_retention_limit_hit ||
        !ref?.capture_complete || ref.retrieval_availability !== "available" || ref.missing_bytes !== 0 ||
        typeof ref.artifact_id !== "string" || !ref.artifact_id ||
        !Number.isSafeInteger(ref.retained_bytes) || ref.retained_bytes < 1) return null;
    return {exit_code:0, artifact_id:ref.artifact_id, bytes:ref.retained_bytes};
  }

  return Object.freeze({readSymbols, refreshFile, groupDiagnostics, cargoEdges, repeatRuns,
    factorContracts, processDelta, packStructuredRows, unpackStructuredRows, expectedCommand});
})()
