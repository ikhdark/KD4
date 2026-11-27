(() => {
  // Bounded workflows over the existing admitted dispatch and delivery paths.
  const jsonPointer = (value) => typeof value === "string" &&
    (value === "" || value.startsWith("/")) && !/~(?:[^01]|$)/.test(value);
  const jsonScalar = (value) => value === null || typeof value === "string" ||
    typeof value === "boolean" || Number.isSafeInteger(value);
  const readJsonValue = async (path, pointer) => {
    if (typeof path !== "string" || !path || !jsonPointer(pointer))
      throw new TypeError("expected a file path and an RFC 6901 JSON pointer");
    const result = await tools.read_file({ path, selectors: [{ kind: "json_pointer", pointer }] });
    const row = result.results?.[0];
    if (result.complete !== true || result.results?.length !== 1 ||
        row?.status !== "ok" || row.complete !== true || !Object.hasOwn(row, "value"))
      throw Object.assign(new Error("one complete JSON selection is required"), { evidence: result });
    return { result, value: row.value };
  };

  const patchPath = p => typeof p === "string" && p.length > 0 &&
    p.trim() === p && !/[\r\n\0]/.test(p);
  const patchHash = h => typeof h === "string" && /^[a-f0-9]{64}$/.test(h);
  const patchLimit = 64;
  const apply = body => tools.apply_patch("*** Begin Patch\n" + body + "*** End Patch");
  const exactTextAdd = (path, content) => {
    if (!content.isWellFormed()) throw new TypeError("expected well-formed UTF-8 text");
    return "*** Add File: " + path + "\n@@ codex-text " + JSON.stringify(content) + "\n";
  };

  Object.defineProperty(globalThis, "apply_literal_edits", {
    value: async function apply_literal_edits(from, to, files) {
      if (typeof from !== "string" || !from || typeof to !== "string" ||
          !Array.isArray(files) || !files.length || files.length > patchLimit)
        throw new TypeError("expected a nonempty search literal, replacement text and 1–64 file selections");
      const search = JSON.stringify(from), replacement = JSON.stringify(to);
      const body = Array.from(files, file => {
        const { path, source_sha256, count = 1 } = file ?? {};
        if (!patchPath(path) || !patchHash(source_sha256) || !Number.isSafeInteger(count) || count < 0)
          throw new TypeError("expected a patch-safe path, full source hash and nonnegative count");
        return "*** Update File: " + path + "\n@@ codex-replace " + count +
          " sha256:" + source_sha256 + "\n-" + search + "\n+" + replacement + "\n";
      }).join("");
      return apply(body);
    },
  });

  Object.defineProperty(globalThis, "apply_patch_plan", {
    value: async function apply_patch_plan({ update = [], replace = [], add = [], guard = [] }) {
      if (![update, replace, add, guard].every(Array.isArray) || (!update.length && !replace.length && !add.length) ||
          update.length + replace.length + add.length + guard.length > patchLimit)
        throw new TypeError("expected 1–64 total edit and guard rows, including an edit");
      const tuple = (row, size) => {
        if (!Array.isArray(row) || row.length !== size || !patchPath(row[0]))
          throw new TypeError("expected a patch-safe path and exactly " + size + " tuple fields");
        return row;
      };
      const plus = (lines, allowEmpty) => {
        if (!Array.isArray(lines) || (!allowEmpty && !lines.length))
          throw new TypeError("expected replacement lines");
        return Array.from(lines, line => {
          if (typeof line !== "string" || /[\r\n\0]/.test(line))
            throw new TypeError("expected newline-free text lines");
          return "+" + line + "\n";
        }).join("");
      };
      const guards = Array.from(guard, row => {
        const [path, hash] = tuple(row, 2);
        if (!patchHash(hash)) throw new TypeError("expected a full dependency source hash");
        return "*** Guard File: " + path + "\n*** Expect: sha256:" + hash + "\n";
      });
      let previousPath, previousHash;
      const updates = Array.from(update, row => {
        const [path, hash, start, end, lines] = tuple(row, 5);
        if (!patchHash(hash) || !Number.isSafeInteger(start) || !Number.isSafeInteger(end) ||
            start < 1 || end < start)
          throw new TypeError("expected a full source hash and an inclusive one-based range");
        // Adjacent ranges from one snapshot must reach the native parser as one file hunk.
        // A new hash (or a different file) retains the caller's ordered verification boundary.
        const header = path === previousPath && hash === previousHash ? "" : "*** Update File: " + path + "\n";
        previousPath = path;
        previousHash = hash;
        return header + "@@ codex-range " + start + ":" + end + " sha256:" + hash + "\n" + plus(lines, false);
      });
      let literalPath, literalHash;
      const replacements = Array.from(replace, row => {
        const [path, hash, from, to, count = 1] = tuple(row, row?.length === 5 ? 5 : 4);
        if (!patchHash(hash) || typeof from !== "string" || !from || typeof to !== "string" ||
            !Number.isSafeInteger(count) || count < 0)
          throw new TypeError("expected a full source hash, nonempty search text, replacement and nonnegative count");
        const header = path === literalPath && hash === literalHash ? "" : "*** Update File: " + path + "\n";
        literalPath = path;
        literalHash = hash;
        return header + "@@ codex-replace " + count + " sha256:" + hash +
          "\n-" + JSON.stringify(from) + "\n+" + JSON.stringify(to) + "\n";
      });
      const additions = Array.from(add, row => {
        const [path, content] = tuple(row, 2);
        return typeof content === "string"
          ? exactTextAdd(path, content)
          : "*** Add File: " + path + "\n" + plus(content, true);
      });
      return apply([...guards, ...updates, ...replacements, ...additions].join(""));
    },
  });

  // A live byte-range read has a revision and file size, not a snapshot SHA.
  // Ask past EOF for the empty file: the native selector itself must be nonempty.
  const smallBytes = expected => {
    const limit = 64 * 1024;
    if (typeof expected === "string" && expected.length <= limit && expected.isWellFormed())
      expected = new TextEncoder().encode(expected);
    if (!(expected instanceof Uint8Array) || expected.length > limit)
      throw new TypeError("expected at most 64 KiB of UTF-8 text or bytes");
    return expected;
  };
  const verifyBytes = async (path, bytes) => {
    // Capture the expectation before yielding; callers may mutate a Uint8Array.
    const size = bytes.length, end = Math.max(1, size);
    const hex = Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("");
    const r = await tools.read_file({ path, encoding: "hex", selectors: [{kind:"bytes", start:0, end}] });
    const row = r.results?.[0];
    if (r.read_mode !== "byte_ranges" || r.selector_coordinate_space !== "source_bytes" ||
        r.complete !== true || r.file_bytes !== size || r.results?.length !== 1 ||
        row?.status !== "ok" || row.complete !== true || row.canonical_range?.start !== 0 ||
        row.canonical_range?.end !== size || row.exact_bytes !== size || row.data_hex !== hex)
      throw Object.assign(new Error("complete file bytes did not match"), { evidence: r });
    return { path: r.path, environment_id: r.environment_id, bytes: size, file_revision: r.file_revision,
      ...(patchHash(r.readback_sha256) ? { readback_sha256: r.readback_sha256 } : {}) };
  };
  // A matched native absence probe observes the exact directory entry. A
  // timeout, cancellation or error is unknown, never a proof of absence.
  Object.defineProperty(globalThis, "verify_paths_absent", {
    value: async function verify_paths_absent(paths, { timeout_ms = 1000 } = {}) {
      if (!Array.isArray(paths) || paths.length < 1 || paths.length > 32 ||
          !Array.from(paths).every(p => typeof p === "string" && p && !p.includes("\0")) ||
          !Number.isSafeInteger(timeout_ms) || timeout_ms < 1 || timeout_ms > 300000)
        throw new TypeError("expected 1–32 exact local paths and a bounded observation timeout");
      const names = Array.from(paths);
      const observed = await Promise.allSettled(names.map(async path => {
        const result = await tools.inspect_files({operation: "wait",
          arguments: {path, condition: "absent", timeout_ms}});
        if (result.path !== path || result.status !== "matched")
          throw Object.assign(new Error("absence not established for " + path + ": " + result.status), { evidence: result });
        return result;
      }));
      const failed = observed.flatMap((r, i) => r.status === "rejected" ? [i] : []);
      if (failed.length)
        throw Object.assign(new Error("absence not established: " + failed.map(i => names[i]).join(", ")),
          { evidence: observed });
      return observed.map(r => r.value);
    },
  });
  Object.defineProperty(globalThis, "verify_file_bytes", {
    value: async (path, expected) => verifyBytes(path, smallBytes(expected)),
  });
  Object.defineProperty(globalThis, "create_verified_text", {
    value: async function create_verified_text(path, content) {
      if (!patchPath(path) || typeof content !== "string" || !content.isWellFormed())
        throw new TypeError("expected a patch-safe path and well-formed UTF-8 text");
      const bytes = smallBytes(content);
      const patch = await apply(exactTextAdd(path, content));
      if (patch.success !== true || patch.changes_exact !== true || patch.outcome !== "committed")
        throw Object.assign(new Error("create was not confirmed; do not retry"), { evidence: patch });
      try {
        return { patch, readback: await verifyBytes(path, bytes) };
      } catch (cause) {
        throw Object.assign(new Error("create committed; readback did not verify it", { cause }),
          { evidence: { patch, readback: cause.evidence ?? cause } });
      }
    },
  });

  Object.defineProperty(globalThis, "create_verified_texts", {
    value: async function create_verified_texts(entries) {
      if (!Array.isArray(entries) || !entries.length || entries.length > 32)
        throw new TypeError("expected 1–32 [path, text] pairs");
      const seen = new Set();
      let total = 0;
      const files = Array.from(entries, row => {
        if (!Array.isArray(row) || row.length !== 2)
          throw new TypeError("expected [path, text]");
        const [path, content] = row;
        if (!patchPath(path) || seen.has(path) || typeof content !== "string")
          throw new TypeError("expected distinct patch paths and UTF-8 strings");
        const bytes = smallBytes(content);
        total += bytes.length;
        if (total > 64 * 1024) throw new RangeError("batch exceeds 64 KiB");
        seen.add(path);
        return { path, bytes, hunk: exactTextAdd(path, content) };
      });
      const patch = await apply(files.map(file => file.hunk).join(""));
      if (patch.success !== true || patch.changes_exact !== true || patch.outcome !== "committed")
        throw Object.assign(new Error("batch create was not confirmed; do not retry"), { evidence: patch });
      const settled = await Promise.allSettled(files.map(file => verifyBytes(file.path, file.bytes)));
      const readbacks = settled.map((result, i) => result.status === "fulfilled"
        ? { path: files[i].path, status: "verified", readback: result.value }
        : { path: files[i].path, status: "unverified", error: result.reason?.message ?? String(result.reason),
            evidence: result.reason?.evidence ?? result.reason?.tool_failure ?? result.reason });
      if (readbacks.some(row => row.status !== "verified"))
        throw Object.assign(new Error("batch committed; readbacks incomplete"), { evidence: { patch, readbacks } });
      return { patch, readbacks };
    },
  });

  // Never substitute a combined display, a decoded pipe or the last poll's suffix.
  Object.defineProperty(globalThis, "save_command_streams", {
    value: async function save_command_streams(r, stdoutPath, stderrPath) {
      if (r?.execution_state !== "exited" || r.exit_code !== 0 || r.session_id != null ||
          r.error != null || r.tool_failure != null || r.process_exited !== true ||
          r.output_drained !== true || r.streams_complete !== true || r.output_encoding != null ||
          r.stream_chunks_complete !== true || r.stream_ranges?.length !== 2)
        throw new Error("expected a successful terminal receipt with complete separate UTF-8 streams");
      const entries = [[stdoutPath, r.stdout], [stderrPath, r.stderr]];
      for (const [i, [, content]] of entries.entries()) {
        const range = r.stream_ranges[i];
        if (typeof content !== "string" || range?.start !== 0 || range.end !== smallBytes(content).length)
          throw new Error("stream text does not cover its original byte range");
      }
      return globalThis.create_verified_texts(entries);
    },
  });

  Object.defineProperty(globalThis, "ensure_json_value", {
    value: async function ensure_json_value({ path, pointer, value }) {
      if (!jsonScalar(value) || typeof path !== "string" || path.trim() !== path ||
          /[\r\n\0]/.test(path))
        throw new TypeError("expected a patch-safe path and a JSON scalar (numbers must be safe integers)");
      const before = await readJsonValue(path, pointer);
      const type = (scalar) => scalar === null ? "null" : typeof scalar;
      if (!jsonScalar(before.value) || type(before.value) !== type(value))
        throw new TypeError("replacement must match the existing JSON scalar type");
      const source_sha256 = before.result.source_sha256;
      if (typeof source_sha256 !== "string" || !/^[a-f0-9]{64}$/.test(source_sha256))
        throw Object.assign(new Error("no current source identity"), { evidence: before.result });
      if (before.value === value) return { status: "unchanged", value, source_sha256 };

      const operation = JSON.stringify([{ op: "replace", path: pointer, value }]);
      const patch = await tools.apply_patch("*** Begin Patch\n*** Update File: " + path +
        "\n@@ codex-json sha256:" + source_sha256 + "\n+" + operation + "\n*** End Patch");
      if (patch.success !== true || patch.changes_exact !== true || patch.outcome !== "committed")
        throw Object.assign(new Error("guarded JSON update was not confirmed"), { evidence: patch });
      // Never retry the completed write, even when readback fails or disagrees.
      try {
        const after = await readJsonValue(path, pointer);
        if (after.value !== value) throw Object.assign(new Error("JSON value changed after update"), { evidence: after.result });
        return { status: "verified", value: after.value, source_sha256: after.result.source_sha256 };
      } catch (cause) {
        throw Object.assign(new Error("guarded JSON update committed; readback did not verify it", { cause }),
          { evidence: { patch, readback: cause.evidence } });
      }
    },
  });
  Object.defineProperty(globalThis, "emit_file_report", {
    value: async function emit_file_report({ path, pointer = "", fields, format = "json" }) {
      if (typeof path !== "string" || !path || !jsonPointer(pointer) || !Array.isArray(fields) ||
          fields.length < 1 || fields.length > 64 ||
          fields.some(field => !field || typeof field.name !== "string" || !jsonPointer(field.pointer)))
        throw new TypeError("expected a file, a JSON pointer, and 1–64 named fields with relative JSON pointers");
      // Ask the native index for each exact lexical address. Selecting only the
      // parent object could collapse duplicate keys before template extraction.
      const addresses = fields.map(field => pointer + field.pointer);
      const selectors = [...new Set(addresses)].map(pointer => ({ kind: "json_pointer", pointer }));
      const result = await tools.read_file({ path, selectors });
      const rows = result.results;
      if (result.complete !== true || rows?.length !== selectors.length ||
          rows.some(row => row.status !== "ok" || row.complete !== true || !Object.hasOwn(row, "value")))
        throw Object.assign(new Error("every report field requires a complete JSON selection"), { evidence: result });
      const indexes = new Map(rows.map((row, index) => [row.selector?.pointer, index]));
      if (addresses.some(address => !indexes.has(address)))
        throw Object.assign(new Error("report selection identity mismatch"), { evidence: result });
      const report = extract_result(result, {
        fields: fields.map(({ name }, index) => ({ name, pointer: `/results/${indexes.get(addresses[index])}/value` })),
        format,
      });
      text(report.text);
      return report.provenance;
    },
  });
})();
