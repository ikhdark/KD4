const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in contracts may follow; external and omitted contracts remain lazy. Omission is not deactivation: resolve known exact names locally; calls still pass current authorization."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no browser/Node globals or filesystem/network.
- Nested tools: const r = await tools.exec_command({cmd:"..."}); text(r.output); console.log aliases text.
- Edit via apply_patch: nested if registered, otherwise direct (*** Begin Patch envelope); never pipe patches through shell.
- exec_command waits; one still running after the default wait returns a live `session_id` (see await_command). r.output is display text; r.exit_code is status; the host reports failed commands itself. For exact parsing use `r.stdout`, present when `r.streams_complete` is true. Before parsing, concatenating, or indexing a result, verify success and the advertised representation, and finish its live handle or required continuations. A display string is not exact completed data.
- Await every call: unawaited work is discarded, and an uncaught rejection ends the cell. Earlier effects survive failure; recover receipts, never replay them. Use `Promise.allSettled` for independent fallible calls. Batch independent reads and searches as one `text(await Promise.allSettled([tools.read_file({path:"a.rs"}), tools.exec_command({cmd:"rg -n x src"})]))`. Run builds and other commands that may write outside such batches; they hold the workspace exclusively.
- Nested deadlines: { timeout_ms } overrides the host default. Expiry cancels the nested call and may return only an error, without a live handle. After expiry, check the outcome before retrying uncertain effects.
- Native failures expose `tool_failure` (also on caught exceptions): inspect `code`, `effects`, `retry_safe`, `safe_next_actions` and `live_operations` together. Codes distinguish stale input, permission denial, retryable failure, cancellation and uncertain effects; none grants permission to retry. Resume an existing operation using its owner-issued handle rather than launching a duplicate; cancellation is not rollback. Legacy/unclassified diagnostics remain non-retry-safe.
- {{delivery_guidance}}
- Output defaults to 10000 tokens; first-line // @exec: {"max_output_tokens": 40000} raises it (ceiling 40000). A truncated result names the `read_tool_output` call that returns the rest. Oversized: result_retained:true, outcome.recovery (possibly pending). Recover required recovery/canonical_recovery or outcome.recovery ranges with `await tools.read_tool_output(recipe.arguments)` in-cell before returning; consume pages; never rerun producers. For bulk JSON/metrics, aggregate records incrementally and print selected fields, not `text(recovered)` or entire batches. Never stringify an entire corpus merely to count bytes or slice its output: the allocation can fail before the limit is applied. Required full semantic reads still need the relevant text, not just a stored-byte receipt.
- JS bindings reset per exec; store(key,value)/load(key) retain JSON-serializable data, not arbitrary native result objects. Store a compact plain projection; represent bigint fields as decimal strings when needed. Prefer text(result) for native display: JSON.stringify(result) can throw on bigint. Load large JSON once per computation and compare stable keys, not object references across loads. Keep one working body, retiring obsolete copies before storing replacements. Summarize already-consumed machine data without reprinting it. Keep compact state/provenance; deleteStored(key) drops duplicates. Failed quota cells discard staged stores; check listKeys before resuming. listKeys({after?,limit?}): 1-64, default 32; continue next_after. First-line // @exec: {"persist":true} preserves completed values across host restart.
- Deferred tools: ALL_TOOL_NAMES/ALL_TOOLS or tool_search; resolve_tool(name) returns the callable with its full contract. Invoke that returned function (const fn = resolve_tool(name); await fn(args)) or the exact declared tools namespace/member; discovery names are not interchangeable with JavaScript property paths. Resolve known followups in-cell; never infer arguments from names. Follow namespace_instructions_ref and operation rules, or resolve the full contract.

Helpers (prefer over equivalent read/search/poll plumbing; preserve scope, budgets and failure evidence):
- `await read_files(paths, {full?, concurrency?})`: 1–32 paths; {path,status,value?,reason?} each; value={initial,pages,file_complete}. full defaults false; true recovers an already-required full snapshot. Independent per-file snapshots, not an atomic workspace snapshot. Check every status and file_complete; byte coverage is not semantic review.
- `await search_files(pattern, {path?, glob?, lines?, max_per_file?, ignore_case?, fixed?, hidden?, no_ignore?, hydrate?, enclosing?, context_lines?, max_hydrated_files?})`: ripgrep returning {files:[{path,count,lines?,source?,hydration?}],match_count,complete,errors?,hint?}. lines adds {line,text}, default max_per_file=10. hydrate:true also returns native read_file snapshots (whole-source source_sha256, deduplicated ranges, exact text and immutable recovery), with context_lines=3 (0–20); enclosing:true implies hydration and adds complete Rust/Python enclosing items when they fit. Reads at most max_hydrated_files=8 (1–32), 32 hits and 1 MiB selected payload per file; no automatic recovery/reread. Check search_complete, hydration.complete/truncated, per-file reasons and source selection/continuation metadata separately. matches_verified counts exact rg hit bytes verified at their UTF-8 offsets in that source snapshot, not whole-file stability since search. Unsupported structure, read failures and changed/unavailable hits remain explicit partial results; hint suggests existing paths near errors.
- `await await_command(r, {max_bytes?})`: pass the complete returned exec_command/write_stdin receipt as r, not a reconstruction from its handle; preserve its execution state, capabilities and any pending outcomes. If only an authorized live handle remains, use write_stdin with its incarnation/capabilities and pass the new receipt. Collects into {terminal,observations}; throws on command failure or a collection bound, preserving that object on the error's `.evidence`. Default wait/observation limits are 300 seconds/256; a collection limit is not process failure or termination. max_bytes is an integer from 65536 through 4194304 (default 4194304), not a display/token budget; omit it unless needed. It counts unique observations once, not the duplicated terminal alias in JSON/storage; serialized evidence can be larger. At the bound, retain/page .evidence; resume only a live retained handle and never restart completed work.
- `await run_graph([{id,deps?,run,accept?}], {concurrency?,targets?})`: 1–256 nodes; concurrency 1–16, default 4. Returns {[id]:{status,value,input_index}}; sort by input_index for original order including numeric IDs. Without accept, explicit `success:false` rejects a result, and commands require exit_code 0 with no interaction error; arrays check every entry. Use accept for domain-specific postconditions. On node failure, its dependents are skipped and it throws with `.results` after all started work settles.
- notify(value) queues a model-visible message without yielding. image(value,detail?) emits a data: URI, view_image result or MCP image block.
- setTimeout(callback,delayMs?) returns an ID; clearTimeout(id) cancels it. Await a callback-resolved promise to wait."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- `exec` buffers output while its awaited continuation runs. Use `wait` only after `exec` returns a genuinely live `Script running with cell ID ...` result, such as an explicit `yield_control()` or input interruption; a completed cell never needs `wait`.
- `max_tokens` sets the cell's output budget for this and later results; omit it to keep the current budget (initially the exec budget). Model projections default to 10000 tokens; explicit requests are capped at 40000 tokens.
- `wait` buffers output until an explicit yield, input activity, final completion or termination, or 10 minutes without output or a nested-call completion. An idle handoff leaves the cell running; inspect before waiting again.
- New user steering, internal completions, or agent messages with `trigger_turn: true` interrupt a held wait without terminating a still-valid cell.
- If the cell has already finished, `wait` returns the completed result and closes the cell."#;

pub fn build_exec_tool_description(
    code_mode_only: bool,
    direct_delivery_enabled: bool,
    has_deferred_tools: bool,
    direct_only_tool_names: &[String],
) -> String {
    let delivery_guidance = if direct_delivery_enabled {
        "First-line // @exec: {\"deliver\": true} makes printed text the final answer, without a model call, when everything in the cell succeeded. For a settled answer, await closing work (including tools.update_plan) and print only the answer in that same cell, not administrative receipts; avoid a separate generation just for plan closure or restatement. Plan completion alone is not task completion. Delivery appears at cell end; use a normal final for long reports."
    } else {
        "Final-answer delivery is unavailable in this session. `deliver: true` is rejected before the cell starts; omit it and return the answer in a normal final response after `exec` completes."
    };
    let mut description = EXEC_DESCRIPTION_TEMPLATE.replace("{{delivery_guidance}}", delivery_guidance);
    if !code_mode_only {
        description.push_str("\n\nFor a single call, prefer its direct interface when advertised. Use `exec` for orchestration or result processing.");
    }
    if !direct_only_tool_names.is_empty() {
        description.push_str("\n\nDirect-only tools omitted from `ALL_TOOLS`: ");
        for (index, name) in direct_only_tool_names.iter().enumerate() {
            if index > 0 {
                description.push_str(", ");
            }
            description.push('`');
            description.push_str(name);
            description.push('`');
        }
        description.push_str(". Call these through their direct model tool interface using the schema advertised there, not through `exec`.");
    }
    if code_mode_only {
        // The grammar is stable; callers can append eager built-in contracts
        // to this description. External inventory changes stay lazy.
        description.push_str("\n\n");
        description.push_str(LAZY_NESTED_TOOL_SCHEMA_GUIDANCE);
    } else if has_deferred_tools {
        description.push_str("\n\n");
        description.push_str(DEFERRED_NESTED_TOOLS_GUIDANCE);
    }

    description
}

pub fn build_wait_tool_description() -> &'static str {
    WAIT_DESCRIPTION_TEMPLATE
}
