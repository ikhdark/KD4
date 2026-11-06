const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in contracts may follow; external and omitted contracts remain lazy. Omission is not deactivation: resolve known exact names locally; calls still pass current authorization."#;
/// Full semantics behind the condensed exec description, read on demand through
/// `read_file(path="context:exec-reference")`.
pub const EXEC_REFERENCE: &str = include_str!("exec_reference.md");
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript as an ES module, not JSON/Markdown; top-level await is supported, top-level return is invalid (use conditional blocks or return inside a function). No browser/Node globals or filesystem/network.
- Nested tools: const r = await tools.exec_command({cmd:"..."}); text(r); console.log aliases text. Chain dependent steps in order in one cell: const p = await tools.apply_patch(patch); text(p); if (p.success) { text(await tools.exec_command({program:"python",args:["app.py"]})); text(await tools.read_file({path:"out.json"})); }
- {{delivery_guidance}}
- Paths resolve against the session cwd: use relative paths and omit workdir when working there.
- Edit via apply_patch: nested if registered, otherwise direct (*** Begin Patch envelope); never pipe patches through shell.
- exec_command waits for the command, so one await and text(r) normally finish it: r.output is display text and r.exit_code its status. A failed command does not throw, and the host reports the failure in the cell output even when unprinted, so a read or inspection after it needs no check; check r.exit_code only before a step that must not run after a failure. r.session_id appears only when a command outlives its wait (about 5 minutes unless you set yield_time_ms), so a routine launch needs no branch on it. To parse output, use stdout/stderr when streams_complete is true; a display string is not exact data. retained_output, when present, is the whole transcript, not another delta.
- Await every call: unawaited work is discarded, and an uncaught rejection ends the cell. Earlier effects survive failure; recover receipts, never replay them. Batch independent fallible calls with `Promise.allSettled`, e.g. independent reads/searches: `text(await Promise.allSettled([tools.read_file({path:"a.rs"}), tools.exec_command({cmd:"rg -n x src"})]))`. Run builds and other commands that may write outside such batches; they hold the workspace exclusively.
- A native failure exposes `tool_failure` (also on caught exceptions): read its `code`, `execution_state`, `retry_safe` and `safe_next_actions`. Only `not_started` proves nothing ran; resume through an owner-issued handle rather than launching a duplicate. { timeout_ms } overrides a nested call's deadline; expiry leaves the process running.
- Output defaults to 10000 tokens; first-line // @exec: {"max_output_tokens": 40000} raises it (ceiling 40000). A truncated result carries `recovery.arguments`, a ready `read_tool_output` call: for known records select their lines or matches directly from its artifact_id; take the whole omitted range only if the task needs it, in the same cell, and never rerun the producer. An oversized cell reports result_retained:true and outcome.recovery. For bulk JSON/metrics, print selected fields; never stringify an entire corpus to count or slice it.
- JS bindings reset per exec; store(key,value)/load(key) keep JSON-serializable values across cells, native results included. Integers beyond JS's safe range become BigInt, which JSON.stringify rejects; prefer text(result) for native display. deleteStored, listKeys and the // @exec: {"persist":true} pragma: context:exec-reference.
- Deferred tools: ALL_TOOL_NAMES/ALL_TOOLS or tool_search; resolve_tool(name) returns the callable, `.description` its core contract and `.section(name)` any detail section that core lists. Use the eager contracts below for ordinary calls; do not rediscover or print already-declared contracts. Invoke the returned function (const fn = resolve_tool(name); await fn(args)) or the exact declared tools namespace/member; discovery names are not interchangeable with JavaScript property paths. Resolve known followups in-cell; never infer arguments from names. Follow namespace_instructions_ref and operation rules.

Helpers (prefer over equivalent read/search/poll plumbing; full contracts and failure semantics: read_file({path:"context:exec-reference"})):
- `await read_files(paths, {full?, concurrency?, revision?, source_hashes?, environment_id?})`: 1–32 paths; Promise.allSettled rows {path,status,value|reason}; value={initial,pages,file_complete}, with text in initial.results/pages[].results. A rejected row or file_complete:false is a partial read.
- `await search_files(pattern, {path?, glob?, fixed?, ignore_case?, lines?, max_per_file?, count_only?, hydrate?, enclosing?, context_lines?, page_size?, cursor?, hidden?, no_ignore?})`: read-only native ripgrep when available. Returns {files:[{path,count,lines?,source?,hydration?}],match_count,complete,errors?,hint?}. A string pattern is a regex unless fixed:true; an array of 1–64 literals shares one scan. Check complete and coverage before treating a miss as absence. For known files, native read_file search selectors need no command permission.
- `await await_command(r, {max_bytes?})`: pass the complete exec_command/write_stdin receipt as r, not a reconstruction; a finished r returns at once. Collects into {terminal,observations,exit_code,output} (limits: 300 seconds, 256 observations, 4 MiB; max_bytes is not a display budget), where output joins every observation's display text; throws on command failure or a collection bound, keeping that object on `.evidence`, so a returned result needs no exit check. A collection limit is not process failure; resume only a live retained handle, never restart completed work.
- Also there: run_graph, extract_result, notify, image, setTimeout/clearTimeout."#;
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
        "Finish in the cell that does the work: once the remaining steps are known and the final answer, such as a short status or JSON, can be built from their results, start that cell with // @exec: {\"deliver\": true}, await the work and any check the answer depends on (including tools.update_plan), and print only the answer computed from those results. That text becomes the final answer without another model call, so do not stop just to look at results first. Failed, unsettled or truncated work, unresolved plan requirements, pending evidence recovery and changed input refuse delivery and return the cell's output, so you can continue normally: print no failure answer and write no else branch. Use a normal final for long reports or answers needing judgment."
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
