# Projecting evidence in Code Mode

Use these opt-in recipes when the consumer needs a known subset of the result. They do not change tool permissions, retention, or tool dispatch. Keep the complete source available. Compare the cost of the **whole interaction**: omitting a useful range can cost more if it forces another model request.

- **Diagnostic rows:** after checking that the input and all selections are complete, filter for the requested status **before** printing. Return all matching identities and diagnostics, including distinct failures. An empty array means no matches only for a complete, successful read. Preserve failures and recovery receipts instead of treating them as empty.
  For a complete uniform native structured page, Code Mode may print `value_columns` once and positional `rows[].value` arrays. Pair each cell with the corresponding JSON pointer; keep the original row numbers. Nulls and wide integers retain their exact representation. Missing fields, incomplete coverage, and parser errors remain in the ordinary shape. Native JavaScript and direct results still use keyed row values.
- **Schemas:** resolve a known tool by exact name; inspect its description or a detail section only when needed. Keep other tools discoverable. Do not enumerate and print the whole catalog to use one tool.
  The eager nested `read_file` and `read_tool_output` schemas name specialized selector kinds without repeating their detailed fields. Use `resolve_tool(name).section("arguments")` for those fields. Ordinary selector forms and any advertised direct schemas keep their full contracts.
- **Overlapping reads:** for two known, overlapping ranges of the same source revision, request their union in one selection. Retain the origin and bounds so both original slices can be reconstructed. Do not merge across files, revisions or gaps that were not requested.
- **Unchanged results:** reuse the existing request-local reference when its owner is present. Another marker or a duplicate cache is not needed.

- **Progress after resume:** use the native plan as the durable owner. When only current progress is needed, project the latest revision, stable step IDs, statuses and unresolved obligations rather than printing every intermediate plan. Retain the original requirement pins and evidence; a display summary is not completion authority. Guard a subsequent status change with the recovered revision and step identity. Do not create a second checkpoint file solely to save input tokens.
  In [model request projection](../model-client/src/client_common/repeated_bodies.rs), a direct plan receipt can show `current_plan.steps_from` instead of repeating each `step`. Copy only the step text from the referenced earlier receipt in `step_ids` order; use the current statuses, revision and obligations. If no authenticated literal owner is included, step text remains inline. Native tool results and stored plans are unchanged.

For independent literal queries with the same scope and options, pass the
needles as one `search_files` array and read the per-pattern IDs and counts
rather than the union count. Push a known file-family constraint into `glob`
when the request is actually limited to that family. A filtered search does
not establish absence outside that scope. Use `run_graph` targets for
explicitly optional branches, not required reads or validation.

## A decision-local failure receipt

For a caught, owner-confirmed pre-execution rejection with no effects and no
live owner, project the field diagnostics and recovery actions. Keep the
original native exception available to the cell when the full diagnostic is
needed. For every other failure, handle the original receipt and its live
operations; a compact message is not proof that a retry is safe.

<!-- failureDecision -->
```js
function failureDecision(failure) {
  if (failure?.execution_state !== "not_started" || failure.effects !== "none" ||
      !Array.isArray(failure.live_operations) || failure.live_operations.length)
    return failure;
  const {code, execution_state, effects, retry_safe, field_errors,
    safe_next_actions, live_operations} = failure;
  return {code, execution_state, effects, retry_safe, field_errors,
    safe_next_actions, live_operations};
}
```

## A smaller eager tool catalog for read-first tasks

For an explicitly read-first Code Mode Only workload, set
`features.code_mode.read_task_eager_only = true`. File and retained-output
contracts stay eager. Other built-in contracts are discoverable by name and
remain callable through `resolve_tool(name)`; direct tools and authorization
are unchanged. Leave the option unset for general-purpose work. It does not
select a task automatically or restrict what the agent is allowed to do.

## One source binding for a set of exact lines

Printing an unmodified native `results` array of at least two complete line
selections from one retained file can show the source identity on the enclosing
object rather than on every `[source …]` body label. Match each label to the
enclosing `results` entry by its `body` index; the label still owns its selector
and exact byte range. This is a display format only. A standalone row, mixed
selection, or modified array does not use this shared format. For exact recovery,
keep the native receipt or the display's own recovery arguments.

When several rows come from one complete native line selection, keep the path,
environment and source digest once, and attach each exact line by number. Do
not merge across different snapshots, paths or representations. Keep the
native receipt when offsets, artifact recovery or other metadata may be
needed. A null projection means this recipe cannot prove a complete span;
inspect the native receipt instead.

<!-- bindSourceLines -->
```js
function bindSourceLines(read, index = 0) {
  const row = read?.results?.[index];
  const {kind, start, end} = row?.selector ?? {};
  if (!read?.complete || !row?.complete || row.status !== "ok" || kind !== "lines" ||
      !read.path || !read.environment_id || !/^[a-f0-9]{64}$/.test(read.source_sha256) ||
      !Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 1 || end < start ||
      typeof row.text !== "string")
    return null;
  const lines = row.text.match(/[^\n]*\n|[^\n]+$/g) ?? [];
  if (lines.length !== end - start + 1) return null;
  return {
    source: {path: read.path, environment_id: read.environment_id, sha256: read.source_sha256},
    rows: lines.map((line, i) => [start + i, line]),
  };
}
```

## Compact line reads in a prebound workflow

When a caller already provides a Code Mode cell preamble and the task only
needs one current local file's line range, the preamble can bind a short,
task-local action. Advertise just `text(await r(path, start, end))` to the
model for that action. Do not add this alias to the global tool contract or
have the model redefine it for every read; setup and discovery would change
the cost comparison. Bounds are one-based and inclusive.

<!-- compactLocalLines -->
```js
async function r(path, start, end) {
  if (typeof path !== "string" || !path || !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(end) || start < 1 || end < start)
    throw new TypeError("expected a file path and positive inclusive line bounds");
  const result = await tools.read_file({path, selectors:[{kind:"lines", start, end}]});
  const row = result.results?.[0];
  if (!result.complete || result.results?.length !== 1 || !row?.complete ||
      row.status !== "ok" || typeof row.text !== "string" ||
      row.selector?.kind !== "lines" || row.selector.start !== start || row.selector.end !== end ||
      (row.text.match(/[^\n]*\n|[^\n]+$/g) ?? []).length !== end - start + 1)
    throw Object.assign(new Error("exact line selection unavailable"), {evidence: result});
  return row.text;
}
```

This is a text-only action. Use the ordinary keyed tool call for a revision,
another environment, hashes, mixed selectors, or receipt and byte recovery.
Native access checks and read failures still apply.

## Replay known windows of one retained snapshot

When several consumers need windows from the **same authenticated artifact**,
pass the known selectors in one call. `preserve_line_windows` reconstructs
each original window in caller order, including duplicates, even when the
native owner merges or sorts selectors:

```js
const {line_windows} = await select_output({
  artifact_id: owner.artifact_id,
  source_thread_id: owner.source_thread_id,
  selectors: [{kind: "lines", start: 10, end: 18}, {kind: "lines", start: 15, end: 24}],
}, {
  preserve_line_windows: true,
  expected_sha256: owner.canonical_sha256,
  expected_bytes: owner.canonical_bytes,
});
```

Use the retained producer receipt as `owner`; do not resolve a live path
to fill in a missing identity. The opt-in requires a complete authenticated
selection; the full result also has the unmodified native `initial` receipt.
On an incomplete read, follow the native recovery evidence, never treat
missing windows as empty or restart the producer. For known sparse or
overlapping windows, fetch the exact batch before considering a whole-body
read. Compute with the returned windows in the cell and print only what
downstream work needs; keep the artifact and range locators for recovery.

### Reuse a complete batch across downstream stages

If two stages need the same immutable evidence, share the completed selection
rather than asking each stage to retrieve it again. This also keeps a known
invariant read outside repeated work. Bind expected identity once from the
producer, then read new or changed windows separately.

```js
async function consumeRetainedBatch(owner, selectors, laterSelectors) {
  const {artifact_id, source_thread_id, canonical_sha256, canonical_bytes} = owner;
  const options = {
    preserve_line_windows: true,
    expected_sha256: canonical_sha256,
    expected_bytes: canonical_bytes,
  };
  const read = requested => select_output({
    artifact_id, source_thread_id, selectors: requested,
  }, options);
  const shared = await read(selectors);
  const first = shared.line_windows.map(({start, end}) => [start, end]);
  const second = shared.line_windows.map(({text}) => text);
  const later = await read(laterSelectors);
  return {first, second, later: later.line_windows};
}
```

Reuse only within the workflow that already owns the completed selection.
This does not revalidate custody, refresh a live source, or authorize a
new read. Propagate refusals with the original evidence; do not cache them
or treat them as an empty selection. If the consumer requested a fresh
native receipt, call the owner instead of reusing the previous value.

## A sparse change to an already-consumed text window

This optional cell function projects one contiguous line splice. Use it only for two **complete selections** of the same logical window, when the consumer still has the exact earlier body. For each selection, check the native result and selector status/completeness; construct `{scope, sha, body}` from its identity and exact text. Bind `scope` to the environment, canonical path, representation and selector; `sha` is its full source revision hash. Pass the consumer's confirmed base hash as `expected`, **not** a hash inferred from the latest read. If the base was retired, truncated, or never seen, pass `null`. Retaining a value locally does not prove the consumer saw it.

<!-- projectLines -->
```js
function projectLines(base, expected, next) {
  if (!base || !expected || expected !== base.sha || base.scope !== next.scope)
    return next;
  const a = base.body.split("\n"), b = next.body.split("\n");
  let first = 0, tail = 0;
  while (first < a.length && first < b.length && a[first] === b[first]) first++;
  while (tail < a.length-first && tail < b.length-first &&
         a[a.length-1-tail] === b[b.length-1-tail]) tail++;
  const delta = {scope:next.scope, base:expected, sha:next.sha, first,
    remove:a.length-first-tail, insert:b.slice(first,b.length-tail)};
  return JSON.stringify(delta).length < JSON.stringify(next).length ? delta : next;
}
```

For a delta, split the verified base body on `"\n"`, replace `remove` elements starting at zero-based `first` with the elements of `insert`, then join with `"\n"`. Splitting this way preserves CR characters and a final empty line. Require matching scope and base revision before consuming a delta; otherwise request the complete current selection. A return with `body` is already complete. The size comparison uses serialized string length, not an exact token estimate. This is a display projection, not an edit or proof that any other range is unchanged.

## Compact table for complete JSON rows

When the consumer needs **every row and field**, an opt-in table can remove
repeated field names without discarding values. Use it on a complete array of
records from parsed JSON or a complete native selection, not on partial
results, errors, or live tool receipts. Keep the owning receipt and its exact
source identity separately. Unlike dictionary packing of repeated values,
this recipe saves space when the *field names* repeat, even if the values
are distinct.

<!-- projectRows -->
```js
function projectRows(rows) {
  if (!Array.isArray(rows) || rows.length < 2 || rows.length > 256) return rows;
  const isRow = r => r !== null && typeof r === "object" && !Array.isArray(r);
  if (!rows.every(isRow)) return rows;
  const schema = Object.keys(rows[0]);
  if (!schema.length || schema.length > 64 || !rows.every(r => {
    const keys = Object.keys(r);
    return keys.length === schema.length && keys.every((key, i) => key === schema[i]);
  })) return rows;
  const table = {schema, rows: rows.map(r => schema.map(key => r[key]))};
  const encoder = new TextEncoder();
  const size = value => encoder.encode(JSON.stringify(value)).length;
  return size(table) < size(rows) ? table : rows;
}
```

An array is unchanged. For a table, each row's values correspond to
`schema` in order; for example, reconstruct each object with
`Object.fromEntries(schema.map((key, i) => [key, row[i]]))`.
Rows with different keys or key order stay inline. Null and empty strings
are values, not missing fields. The function compares serialized UTF-8
bytes, not model tokens; use it only when the consumer knows the table format.
For a one-off cell, include the helper's own text when comparing the whole
interaction; a smaller payload does not necessarily make the request smaller.
