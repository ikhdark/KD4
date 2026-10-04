const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in tool contracts may be included below; external and omitted contracts remain lazy. When `tool_search` is advertised, use it to activate tools that are not yet listed. Use `resolve_tool("tool_search")` if needed. Resolve missing receipt contracts and execute those arguments in the same cell."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no Node/filesystem/network.
- Nested tools live on the global `tools` object: `await tools.exec_command({cmd:"..."})`, `await tools.apply_patch(patchText)` if registered. Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`; `console.log(...)` aliases `text(...)`. Only `ALL_TOOL_NAMES` entries are callable.
- Edit with `apply_patch`: nested when registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- `text(...)` emits values; on failure/no output, the host retains up to two nested-tool results, each capped at 1024 bytes, and reports omissions. Emit needed results explicitly.
- Reuse current schemas and results; resolve missing/stale schemas before calls. Eager output types may be omitted; resolution retains the full contract when required. For source paths, batch `tools.read_file({path})` calls; use selectors for multiple ranges in one file. Store results, check every result and `file_complete`/`complete`. Emit within the combined display budget; never substitute shell paging or omit unread required content.
- Nested tools: use a present schema; filter `ALL_TOOL_NAMES` or `ALL_TOOLS` locally; use `resolve_tool(name)` to obtain a missing schema and callable with `.name`/`.description`. Search only if local discovery fails.
- Await `Promise.allSettled` for independent known calls in the same exec and inspect every result. Do not split a known independent batch into one-call execs. Use `await notify(...)` in a handler for useful early results; still await the batch. At evaluation end, unawaited work is discarded. Sequence dependent calls only after checking prerequisite results. Do mechanical checks, such as exit codes, in JS and continue in the same exec. Compute paths and necessary existence checks within the operation that consumes them, not a separate exec.
- Use `run_graph` for bounded fan-out (default 4), rather than starting an unbounded array of calls. Pipeline each consumer with only its actual producer IDs in `deps`; do not put an all-results barrier between independent branches. Known discovery -> fetch -> completeness check chains belong in one graph, with model interpretation only when it changes scope or arguments. Keep evidence and continuation handles in node values; aggregate in definition order, not callback completion order.
- For command output, prefer `text(result.output)` and inspect `result.exit_code` and `result.error`; live-session and recovery controls are preserved by the host. For machine-readable commands, parse `result.stdout` only when `streams_complete` is true; `output` is a display projection and may combine diagnostics or omit bytes. Exact stdout/stderr are available to scripts but omitted when printing an unchanged whole result; explicitly emit only the stream data needed. Use `text(result)` only when the complete object is needed.
- Nested calls use a host-configured default deadline; override it with the documented `{ timeout_ms }` option when needed. Expiry cancels the nested call and may return only an error, without a live handle. Resume only an actually returned live session/cell ID; otherwise check the outcome before retrying uncertain effects. Retry only if unstarted, safely repeatable after stopping, or tool-approved.
- `text(...)` buffers output while awaited work continues in the same awaited evaluation; `yield_control()` is only for a new model decision. Input and host deadlines can yield.
- If the complete final answer is computable, use first-line `// @exec: {"deliver": true}` and emit only that answer with `text(...)`, after awaiting and checking all work. Success finishes the turn with exact text/supported images, without another model call. Match any required JSON schema; the host validates it. Never deliver intermediate evidence or unfinished work. Errors, incomplete work, invalid JSON, unsafe media, output loss/overflow, unsettled/failed siblings, conflicting intents, or new input fall back to the model; output never implies delivery. Intent survives empty yields for later wait in this turn; visible partial output, intervening input, failures, or schema changes invalidate it.
- An exec cell and a command process have separate lifecycles. A resolved `exec_command` call may still return a running command session. Resume a running cell with `wait(cell_id)`; resume a returned command session with `write_stdin(session_id)`. When no new model decision is needed, continue that session within the current evaluation. Completion of the cell does not establish completion of every process it started. Command lifecycle and recovery metadata survive text-only output and zero-token text budgets.
- For a quiet running command, use `write_stdin({session_id, wait_for_output:true})` to await output or exit without periodic model handoffs. If it yields only routine progress, await the next observation in this same cell. Stop for terminal state, pending_deferred_completions, new input, cancellation, no progress requiring diagnosis, or a genuine decision. Never restart the producer. For overlap, use a short initial command wait, retain its live handle, perform the independent work, then drain that same handle and check the terminal exit code. Use the ordinary awaited default when no independent work remains.
- Propagate failures with `&&` or exit-code checks; never mask them with `|| true`.
- For known required missing ranges/continuations, recover them with bounded calls in the same exec before returning to the model. Retain full results; emit needed evidence and completeness/continuation controls. Reduce logs/inventories before printing: use deterministic counts, bounded ranked records, and hashes plus exact selectors into retained evidence, not raw call/message dumps. Reuse an existing report projection instead of rescanning its producer. A compact digest does not replace required source content. Byte fragments are not JSON records: join contiguous ranges in source order, verify coverage, then parse; never parse subdivisions separately. Stop on no progress, cancellation, or a new decision; do not drain unrelated output or exceed the combined output budget.
- Output defaults to 10000 tokens, with a 10000-token hard cap. Override with first-line `// @exec: {"max_output_tokens": 10000}`. Nested exec_command/write_stdin display results carry at most 8000 output tokens, bounded by the cell hard cap. Omit per-call `max_output_tokens` unless deliberately requesting a smaller display; the host enforces the combined emitted-output cap. Do not divide the cell budget into smaller per-call caps. Use `store` to retain results and emit only the evidence needed for the next decision. Read whole useful regions; after truncation, select only missing evidence from the retained artifact. If recovery stops at its budget, follow its unconsumed selector rather than repeating the original range; preserve completion and continuation metadata when filtering recovered output.

Helpers:
- `await run_graph([{id, deps?: string[], step_id?: string, requires?: string[], estimated_ms?: number, resources?: {read?: string[], write?: string[]}, run: async (dependencies) => value, accept: async (value) => boolean}], {concurrency?: number, targets?: string[]})`: cell-local DAG, 1–256 nodes, concurrency 1–16 (default 4). step_id links existing update_plan steps without changing status or proving completion. requires uses exact ALL_TOOL_NAMES; missing capabilities, unknown deps, and cycles fail before execution. Required accept predicates must check status/exit codes. Failed deps skip descendants; independent nodes finish before failure throws `.results` with all fulfilled/rejected/skipped selected nodes. Success maps selected IDs to `{status:"fulfilled", value, step_id?}` in definition order. No automatic retries, rollback, crash resume, or agent authorization. Use existing coordination tools for authorized agents, not another task ledger. Normal admission/cancellation apply. Prefer ordinary awaits for simple chains.
- Graph scheduling: `estimated_ms` is an optional finite duration from 0 to 86400000, based on observed comparable work, not invented timing. Ready nodes on the longest estimated downstream path start first; ties use definition order and omitted estimates are zero. Start expensive validation ahead of independent review when its inputs are final. `resources` are caller-declared exact keys, shared for reads and exclusive for writes, held through `accept`; use the same canonical key for aliases of a repository, file, or Cargo target directory. Claims are atomic and cell-graph-local, not permissions or cross-cell locks; ordinary tool admission still applies. Unknown effects require conservative dependencies. A command node must drain its live process handle before settling, otherwise its resource claim ends too early.
- `targets` selects only those node IDs and their transitive prerequisites before anything starts; omitted means all nodes. Results include only that closure, in definition order; excluded definitions are still preflight-validated. Include every required validation, evidence/recovery, and cleanup obligation in the closure. Use this only to omit explicitly optional work, never to hide failures or detach started work. Do not launch optional work merely to cancel it when the answer is ready.
- Media: `{ type: "image" }` / `{ type: "audio" }` blocks.
- `notify(value): Promise<void>` queues a model-visible message without yielding.
- JS bindings reset per exec; `store(key, value)`/`load(key)` keep JSON values. First-line `// @exec: {"persist":true}` restores/persists bounded completed values and terminal results/artifact refs for this chat. Opt in before storage and after host restart; observe saved cells, never resume live JS or rerun tools. Interrupted effects stay unknown; no power-loss guarantee.
- `setTimeout(callback: () => void, delayMs?: number)` returns an ID; `clearTimeout(timeoutId?: number)` cancels it. Await a promise resolved by the callback to wait."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- `exec` buffers output while its awaited continuation runs. Use `wait` only after `exec` returns a genuinely live `Script running with cell ID ...` result, such as an explicit `yield_control()` or input interruption; a completed cell never needs `wait`.
- `cell_id` identifies the running `exec` cell to resume.
- `max_tokens` limits how much new output this wait call returns. Model projections default to 10000 tokens; explicit requests are capped at 10000 tokens.
- `terminate: true` stops the running cell; false or omitted waits for output.
- `wait` buffers output until an explicit yield, input activity, or final completion or termination. Silence alone does not cause a model handoff.
- New user steering or mailbox input interrupts a held wait without terminating a still-valid cell.
- If the cell has already finished, `wait` returns the completed result and closes the cell. A retained explicit delivery intent can finish the turn only if the cell yielded no partial output and input and schema remain unchanged."#;

pub fn build_exec_tool_description(
    code_mode_only: bool,
    has_deferred_tools: bool,
    direct_only_tool_names: &[String],
) -> String {
    let mut description = String::from(EXEC_DESCRIPTION_TEMPLATE);
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

#[cfg(test)]
mod tests {
    use super::build_exec_tool_description;
    use crate::parse_exec_source;
    use pretty_assertions::assert_eq;

    #[test]
    fn every_prompt_variant_advertises_a_parseable_output_budget_directive() {
        for code_mode_only in [false, true] {
            for has_deferred_tools in [false, true] {
                for direct_names in [vec![], vec!["apply_patch".to_string()]] {
                    let description = build_exec_tool_description(
                        code_mode_only,
                        has_deferred_tools,
                        &direct_names,
                    );
                    assert_eq!(
                        description.contains("prefer its direct interface when advertised"),
                        !code_mode_only,
                    );
                    let directives = description
                        .split('`')
                        .filter(|part| part.starts_with("// @exec:"))
                        .collect::<Vec<_>>();
                    let mut options = Vec::new();
                    for directive in directives {
                        let source = format!("{directive}\ntext('budget applied');");
                        let parsed = parse_exec_source(&source).unwrap();
                        assert_eq!(parsed.code, "text('budget applied');");
                        options.push((parsed.max_output_tokens, parsed.deliver, parsed.persist));
                    }
                    assert_eq!(options, vec![(None, true, false), (Some(10000), false, false), (None, false, true)]);
                    assert!(description.contains(
                        "override it with the documented `{ timeout_ms }` option when needed."
                    ));
                }
            }
        }
    }

    #[test]
    fn direct_patch_inventory_keeps_the_direct_route_explicit() {
        let nested = build_exec_tool_description(false, false, &[]);
        assert!(!nested.contains("Direct-only tools omitted"));
        let direct = build_exec_tool_description(false, false, &["apply_patch".to_string()]);
        assert_eq!(
            direct.split("\n\n").last().unwrap(),
            "Direct-only tools omitted from `ALL_TOOLS`: `apply_patch`. Call these through their direct model tool interface using the schema advertised there, not through `exec`."
        );
        for description in [nested, direct] {
            assert!(description.contains("Edit with `apply_patch`: nested when registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper."));
        }
    }

    #[test]
    fn execution_contract_keeps_lifecycle_rules_without_general_workflow() {
        for code_mode_only in [false, true] {
            for has_deferred_tools in [false, true] {
                let description =
                    build_exec_tool_description(code_mode_only, has_deferred_tools, &[]);
                for required in [
                    "Sequence dependent calls only after checking prerequisite results.",
                    "Do mechanical checks, such as exit codes, in JS and continue in the same exec.",
                    "Compute paths and necessary existence checks within the operation that consumes them, not a separate exec.",
                    "Await `Promise.allSettled`",
                    "for independent known calls in the same exec and inspect every result.",
                    "Do not split a known independent batch into one-call execs.",
                    "Use `run_graph` for bounded fan-out (default 4)",
                    "Pipeline each consumer with only its actual producer IDs in `deps`",
                    "do not put an all-results barrier between independent branches",
                    "Known discovery -> fetch -> completeness check chains belong in one graph",
                    "Keep evidence and continuation handles in node values",
                    "ties use definition order and omitted estimates are zero",
                    "Start expensive validation ahead of independent review when its inputs are final",
                    "Claims are atomic and cell-graph-local, not permissions or cross-cell locks",
                    "must drain its live process handle before settling",
                    "Include every required validation, evidence/recovery, and cleanup obligation",
                    "excluded definitions are still preflight-validated",
                    "buffers output while awaited work continues in the same awaited evaluation;",
                    "`yield_control()` is only for a new model decision.",
                    "still await the batch. At evaluation end, unawaited work is discarded.",
                    "Reuse current schemas and results;",
                    "batch `tools.read_file({path})` calls",
                    "use selectors for multiple ranges in one file",
                    "check every result and `file_complete`/`complete`",
                    "or omit unread required content",
                    "retain its live handle, perform the independent work",
                    "drain that same handle and check the terminal exit code",
                    "Use the ordinary awaited default when no independent work remains.",
                    "A resolved `exec_command` call may still return a running command session.",
                    "continue that session within the current evaluation.",
                    "bounded by the cell hard cap.",
                    "Omit per-call `max_output_tokens` unless deliberately requesting a smaller display;",
                    "the host enforces the combined emitted-output cap.",
                    "Do not divide the cell budget into smaller per-call caps.",
                    "Use `store` to retain results",
                    "select only missing evidence from the retained artifact.",
                    "follow its unconsumed selector rather than repeating the original range;",
                    "preserve completion and continuation metadata when filtering recovered output.",
                    "recover them with bounded calls in the same exec before returning to the model.",
                    "Reduce logs/inventories before printing",
                    "hashes plus exact selectors into retained evidence",
                    "Reuse an existing report projection instead of rescanning its producer.",
                    "A compact digest does not replace required source content.",
                    "Stop on no progress, cancellation, or a new decision;",
                    "do not drain unrelated output or exceed the combined output budget.",
                    "host-configured default deadline",
                    "Expiry cancels the nested call and may return only an error, without a live handle.",
                    "Resume only an actually returned live session/cell ID;",
                    "check the outcome before retrying uncertain effects.",
                    "Retry only if unstarted, safely repeatable after stopping, or tool-approved.",
                ] {
                    assert!(
                        description.contains(required),
                        "missing guidance: {required}"
                    );
                }
                for retired in [
                    "Keep plans aligned",
                    "Run required validation",
                    "Read the complete enclosing unit",
                    "AGENTS.md",
                    "Prefer a purpose-built tool over shell",
                    "never whole files",
                    "never repeat the same call/poll",
                    "never duplicate a timed-out operation",
                    "request a larger cell budget",
                    "Budget the combined emitted output, not each nested call independently;",
                ] {
                    assert!(
                        !description.contains(retired),
                        "contradictory guidance: {retired}"
                    );
                }
            }
        }
    }
}
