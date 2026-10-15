const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in contracts may follow; external and omitted contracts remain lazy. Omission is not deactivation: resolve known exact names locally; calls still pass current authorization."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no Node/filesystem/network.
- Common path: call known tools, leave timing defaults unset, and await completion. Simple chains need only awaits; use the optional helpers below when needed.
- Nested tools live on the global `tools` object: `await tools.exec_command({cmd:"..."})`, `await tools.apply_patch(patchText)` if registered. Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`; `console.log(...)` aliases `text(...)`. Only `ALL_TOOL_NAMES` entries are callable; a namespaced tool is also `tools.<namespace>.<name>(...)` when that alias is unambiguous.
- Edit with `apply_patch`: nested if registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- `text(...)` emits values. On failure/no output the host retains up to two nested-tool results, each capped at 1024 bytes, and reports omissions.
- Unsafe JSON integers use exact `bigint` in tools/store/load; display uses `{$bigint:"decimal"}`. `JSON.stringify` needs a bigint replacer.
- Discovery: use a present schema; otherwise filter `ALL_TOOL_NAMES`/`ALL_TOOLS` locally and `resolve_tool(name)` for a callable with `.name`/`.description` and its full contract, including omitted output types. If arguments are established, resolve and invoke in the same cell without reprinting the schema; otherwise emit the needed contract, not names alone. `resolve_tool(name, {diagnostic:true})` returns non-callable status and bounded candidates on a miss, never fuzzy execution. Use advertised `tool_search` only after local discovery fails; resolve needed `activated_omitted_tools` in that cell, without another search or handoff. Do not infer arguments from names or reuse contracts after a capability-change notice.
- Await all work: unawaited work is discarded. Uncaught nested-call rejection ends the cell before later statements run. For fallible calls, use `read_files` or `Promise.allSettled` to retain independent results and check every status; `notify(...)` can emit early results.
- For command output, prefer `text(result.output)`. The host itself reports the exit code and error of every failed command and preserves lifecycle/recovery controls, so do not print them; inspect `result.exit_code` only to branch inside the cell. Parse exact `result.stdout` only if `streams_complete`; display `output` may mix/omit bytes. Whole-result printing omits exact stdout/stderr: emit required streams explicitly.
- Nested calls use a host-configured default deadline; override it with the documented `{ timeout_ms }` option when needed. Expiry cancels the nested call and may return only an error, without a live handle. Resume only an actually returned live session/cell ID; check the outcome before retrying uncertain effects. Retry only if unstarted, safely repeatable after stopping, or tool-approved.
- `text(...)` buffers output while awaited work continues in the same awaited evaluation; `yield_control()` is only for a new model decision. Input and host deadlines can yield.
- First-line `// @exec: {"deliver": true}` requests direct delivery of the emitted answer after all work settles. Success delivers exact text/supported images without a model call; required JSON is host-validated. Close any final plan update in this cell, check its result, then emit only the ready answer. Intermediate/unfinished work, errors, invalid JSON, unsafe media, output loss/overflow, unsettled/failed siblings, conflicting intents or new input require model review. Only empty yields retain intent; partial output, input, failures or schema changes invalidate it.
- Command lifetime: await `exec_command`; overlap independent work in the same cell rather than returning early. A resolved `exec_command` may return a live process. Wait until done: use `await await_command(initial)` within its bounds, or `tools.write_stdin({session_id, incarnation, wait_for_output:true})` for output/exit. Continue validation through output bursts until exit or the bounded deadline; stop for pending_deferred_completions, input, cancellation or stalled progress. Use the direct `wait` tool only for a yielded cell. Cell completion does not prove process completion. Lifecycle/recovery metadata survive text-only and zero-token output.
- Output defaults to 10000 tokens; first-line `// @exec: {"max_output_tokens": 40000}` raises the budget (ceiling 40000). Each nested command result is bounded by the cell budget independently of earlier prints; explicit smaller per-call caps are honored. `store` a value only when a later cell will `load` it; after truncation, select only missing evidence from the retained artifact. Follow the unconsumed selector, not the original range; preserve completeness/continuations.

Optional helpers (not required workflow steps):
- `await read_files(paths, {concurrency?: number, full?: boolean})`: 1–32 exact paths, concurrency 4; exact duplicates share one batch observation. Returns ordered `{path,status,value?,reason?}` after all settle; check every result. Values keep `initial`, `pages`, `file_complete`, including successful siblings. full:false (default) reads once/path. full:true recovers contiguous bytes from the original snapshot, checking identity/coverage; stops on uncertainty, 64 recovery calls/file or 8 MiB aggregate scope. Recovery is not freshness or model-visible coverage; emit required source. Native read_file handles selectors and environments.
- `await await_command(initial, {max_observations?: number, max_wait_ms?: number, on_progress?: async result => boolean})`: polls a live command until exit without restarting it. Defaults: 256 observations (max 1024), 300000 ms (5000–300000). Resolves to `{terminal,observations}` only when the command exits 0 (or 1 from a search with no match); any other exit, a stall, pending input or an exhausted bound throws an Error whose `.evidence` is that object with the live handle, so use try/catch when you need a failing command's output. on_progress must return true to continue; notify shows progress, text buffers it.
- `await run_graph([{id, deps?: string[], step_id?: string, requires?: string[], estimated_ms?: number, resources?: {read?: string[], write?: string[]}, run: async (dependencies) => value, accept: async (value) => boolean}], {concurrency?: number, targets?: string[]})`: cell-local DAG; 1–256 nodes, concurrency 1–16 (default 4). step_id links existing plan steps. requires must name ALL_TOOL_NAMES; missing capabilities/deps and cycles fail preflight. accept checks status/exit codes. Failed deps skip descendants; all started work settles before throwing `.results` (fulfilled/rejected/skipped); acceptance errors retain producer values. Success: `{status:"fulfilled",value,step_id?}` in definition order. No retries/rollback/crash resume/agent authorization; normal admission/cancellation apply. `resources` are cell-local claims (shared reads, exclusive writes), not permissions; `targets` selects nodes plus prerequisites.
- Media: `image(value, detail?)` emits an image from a `data:` URI, `{image_url, detail?}` (a `view_image` result) or an MCP `{ type: "image" }` block; `{ type: "audio" }` blocks.
- `notify(value): Promise<void>` queues a model-visible message without yielding.
- JS bindings reset per exec; `store(key, value)`/`load(key)` keep JSON values for the session. `listKeys({after?,limit?})` pages sorted `keys` (`limit` 1-64, default 32; a larger limit is clamped, so follow `next_after`) with `entry_bytes` and whole-store `usage` against its limits; `deleteStored(key)` retires an obsolete entry. First-line `// @exec: {"persist":true}` persists bounded completed values/terminal receipts; opt in again after restart.
- `setTimeout(callback: () => void, delayMs?: number)` returns an ID; `clearTimeout(timeoutId?: number)` cancels it. Await a promise resolved by the callback to wait."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- `exec` buffers output while its awaited continuation runs. Use `wait` only after `exec` returns a genuinely live `Script running with cell ID ...` result, such as an explicit `yield_control()` or input interruption; a completed cell never needs `wait`.
- `max_tokens` limits how much new output this wait call returns. Model projections default to 10000 tokens; explicit requests are capped at 40000 tokens.
- `wait` buffers output until an explicit yield, input activity, final completion or termination, or 10 minutes without output or a nested-call completion. An idle handoff leaves the cell running; inspect before waiting again.
- New user steering, internal completions, or agent messages with `trigger_turn: true` interrupt a held wait without terminating a still-valid cell.
- If the cell has already finished, `wait` returns the completed result and closes the cell."#;

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
    fn compact_base_contract_keeps_its_budget_when_helpers_are_added() {
        let description = build_exec_tool_description(true, true, &[]);
        let helpers_start = description.find("- `await read_files(").unwrap();
        let helpers_end = description.find("- `await run_graph(").unwrap();
        let helpers_bytes = helpers_end - helpers_start;
        // The new workflow helpers get an explicit allowance, not permission
        // to expand the existing contract on every model request.
        assert!(helpers_bytes < 1_500);
        assert!(description.len() - helpers_bytes < 8_500);
        assert!(description.len() < 10_000);
    }

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
                    assert_eq!(
                        options,
                        vec![
                            (None, true, false),
                            (Some(40000), false, false),
                            (None, false, true)
                        ]
                    );
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
            assert!(description.contains("Edit with `apply_patch`: nested if registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper."));
        }
    }

    #[test]
    fn execution_contract_keeps_lifecycle_rules_without_general_workflow() {
        for code_mode_only in [false, true] {
            for has_deferred_tools in [false, true] {
                let description =
                    build_exec_tool_description(code_mode_only, has_deferred_tools, &[]);
                for required in [
                    "`resources` are cell-local claims (shared reads, exclusive writes), not permissions",
                    "buffers output while awaited work continues in the same awaited evaluation;",
                    "`yield_control()` is only for a new model decision.",
                    "unawaited work is discarded.",
                    "A resolved `exec_command` may return a live process",
                    "Cell completion does not prove process completion",
                    "Wait until done: use `await await_command(initial)`",
                    "Follow the unconsumed selector, not the original range; preserve completeness/continuations",
                    "Expiry cancels the nested call and may return only an error, without a live handle.",
                    "Resume only an actually returned live session/cell ID;",
                    "check the outcome before retrying uncertain effects",
                    "Retry only if unstarted, safely repeatable after stopping, or tool-approved.",
                    "acceptance errors retain producer values",
                    "full:true recovers contiguous bytes from the original snapshot, checking identity/coverage",
                    "Recovery is not freshness or model-visible coverage",
                    "64 recovery calls/file or 8 MiB aggregate scope",
                    "on_progress must return true to continue",
                    "all started work settles before throwing",
                ] {
                    assert!(
                        description.contains(required),
                        "missing guidance: {required}"
                    );
                }
                for retired in [
                    "Start expensive validation before independent review",
                    "Pipeline known discovery -> fetch -> completeness checks",
                    "prepare its conditional final answer in that cell",
                    "Reduce logs/inventories before printing",
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
