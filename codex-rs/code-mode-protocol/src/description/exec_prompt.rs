const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in tool contracts may be included below; external and omitted contracts remain lazy. When `tool_search` is advertised, use it to activate tools that are not yet listed. Use `resolve_tool("tool_search")` if needed. Resolve missing receipt contracts and execute those arguments in the same cell."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no Node/filesystem/network.
- Nested tools live on the global `tools` object: `await tools.exec_command({cmd:"..."})`, `await tools.apply_patch(patchText)` if registered. Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`; `console.log(...)` aliases `text(...)`. Only `ALL_TOOL_NAMES` entries are callable.
- Edit with `apply_patch`: nested if registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- `text(...)` emits values. `text(format_tool_result(name, value))` explicitly reuses an enabled tool's display formatter for cloned/annotated/stored results; it does not certify provenance. On failure/no output the host retains up to two nested-tool results, each capped at 1024 bytes, and reports omissions. Emit needed evidence explicitly.
- Unsafe JSON integers use exact `bigint` in tools/store/load; display uses `{$bigint:"decimal"}`. `JSON.stringify` needs a bigint replacer. Error display is bounded; scripts keep original errors. A source label's `same_as_body` references the exact earlier body in that same display; source identity and observation order remain distinct.
- Eager output types may be omitted; resolution retains the full contract when required.
- Nested tools: use a present schema; filter `ALL_TOOL_NAMES` or `ALL_TOOLS` locally; use `resolve_tool(name)` to obtain a missing schema and callable with `.name`/`.description`. On a miss, `resolve_tool(name, {diagnostic:true})` returns a non-callable status and bounded canonical candidates, never fuzzy execution. Search only if local discovery fails.
- Await all work: unawaited work is discarded. `Promise.allSettled` retains independent failures; `notify(...)` can emit early results.
- For command output, prefer `text(result.output)` and inspect `result.exit_code` and `result.error`; the host preserves lifecycle/recovery controls. Parse exact `result.stdout` only if `streams_complete`; display `output` may mix/omit bytes. Whole-result printing omits exact stdout/stderr: emit required streams explicitly.
- Nested calls use a host-configured default deadline; override it with the documented `{ timeout_ms }` option when needed. Expiry cancels the nested call and may return only an error, without a live handle. Resume only an actually returned live session/cell ID; check the outcome before retrying uncertain effects. Retry only if unstarted, safely repeatable after stopping, or tool-approved.
- `text(...)` buffers output while awaited work continues in the same awaited evaluation; `yield_control()` is only for a new model decision. Input and host deadlines can yield.
- First-line `// @exec: {"deliver": true}` requests direct delivery of the emitted answer after all work settles. Success delivers exact text/supported images without a model call; required JSON is host-validated. Intermediate/unfinished work, errors, invalid JSON, unsafe media, output loss/overflow, unsettled/failed siblings, conflicting intents or new input require model review. Only empty yields retain intent; partial output, input, failures or schema changes invalidate it.
- A resolved `exec_command` may return a live process. Resume cells with `wait(cell_id)`, processes with `write_stdin(session_id)` in this evaluation unless a decision is needed. Cell completion does not prove process completion. Lifecycle/recovery metadata survive text-only and zero-token output.
- Wait until done: use `await await_command(initial)` to drain a live command through exit within its bounds. `write_stdin({session_id, incarnation, wait_for_output:true})` observes output/exit; validation polls continue through output bursts until exit or the bounded deadline. Preserve diagnostics before an empty exit packet. Stop for pending_deferred_completions, input, cancellation or stalled progress. Never restart the producer.
- Output defaults to 10000 tokens; first-line `// @exec: {"max_output_tokens": 40000}` raises the budget (ceiling 40000). Nested command display budgets scale with the cell's remaining output budget (buffered bytes plus envelope reserve); explicit smaller per-call caps are honored. Use `store` to retain results; after truncation, select only missing evidence from the retained artifact. Follow the unconsumed selector, not the original range; preserve completeness/continuations.

Helpers:
- `await read_files(paths, {concurrency?: number, full?: boolean})`: 1–32 exact paths, concurrency 4; exact duplicates share one batch observation. Returns ordered `{path,status,value?,reason?}` after all settle; store/check every result. Values keep `initial`, `pages`, `file_complete`, including successful siblings. full:false (default) reads once/path. full:true recovers contiguous bytes from the original snapshot, checking identity/coverage; stops on uncertainty, 64 recovery calls/file or 8 MiB aggregate scope. Recovery is not freshness or model-visible coverage; emit required source. Native read_file handles selectors/environments/force_fresh.
- `await read_status(paths, {environment_id?, query?})`: 1–32 paths; saved byte coverage, newest observations first (legacy recency may be unknown). query.source_sha256 selects an exact historical hash; returned offsets page remaining coverage. Not freshness or semantic-read proof.
- `await await_command(initial, {max_observations?: number, max_wait_ms?: number, on_progress?: async result => boolean})`: bounded empty polls; no restart. Defaults: 256 observations (max 1024), 300000 ms (5000–300000). Returns `{terminal,observations}` on success; errors retain `.evidence` and handle. Inspect before resuming after a stop. on_progress must return true to continue; notify shows progress, text buffers it. Check output and task completion separately.
- `await run_graph([{id, deps?: string[], step_id?: string, requires?: string[], estimated_ms?: number, resources?: {read?: string[], write?: string[]}, run: async (dependencies) => value, accept: async (value) => boolean}], {concurrency?: number, targets?: string[]})`: cell-local DAG; 1–256 nodes, concurrency 1–16 (default 4). step_id links existing plan steps, never status/proof. requires must name ALL_TOOL_NAMES; missing capabilities/deps and cycles fail preflight. accept checks status/exit codes. Failed deps skip descendants; all started work settles before throwing `.results` (fulfilled/rejected/skipped); acceptance errors retain producer values. Success: `{status:"fulfilled",value,step_id?}` in definition order. No retries/rollback/crash resume/agent authorization; normal admission/cancellation apply. Simple chains need only awaits.
- Graph scheduling: `estimated_ms` is finite 0–86400000. Longest downstream path first; ties use definition order, missing estimates zero. `resources`: canonical keys (including path aliases), shared reads/exclusive writes through accept. Claims are atomic/cell-local, not permissions/cross-cell locks; tool admission still applies. Unknown effects need conservative deps. Command nodes must drain live processes before settling/releasing claims.
- `targets`: select nodes plus prerequisites before dispatch (default all). Structure is validated globally; required capabilities only for selected nodes. Preflight errors include bounded `missing_capabilities` and `omitted_capabilities`; skipped nodes include immediate `blocked_by` IDs. Interrupted full reads may expose `.reason.evidence.recovery` for the last verified cursor; inspect before resuming, never infer freshness or retry an invalid page.
- Media: `{ type: "image" }` / `{ type: "audio" }` blocks.
- `notify(value): Promise<void>` queues a model-visible message without yielding.
- JS bindings reset per exec; `store(key, value)`/`load(key)` keep JSON values. `listKeys({after?,limit?})` lists sorted keys (default 32, max 64), with `next_after`; `deleteStored(key)` transactionally retires a key. First-line `// @exec: {"persist":true}` persists bounded completed values/terminal receipts. Late opt-in promotes quiescent memory only into a new snapshot; opt in again after restart. Observe saved cells; never rerun tools. Interrupted effects stay unknown; no power-loss guarantee.
- `setTimeout(callback: () => void, delayMs?: number)` returns an ID; `clearTimeout(timeoutId?: number)` cancels it. Await a promise resolved by the callback to wait."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- `exec` buffers output while its awaited continuation runs. Use `wait` only after `exec` returns a genuinely live `Script running with cell ID ...` result, such as an explicit `yield_control()` or input interruption; a completed cell never needs `wait`.
- `cell_id` identifies the running `exec` cell to resume.
- `max_tokens` limits how much new output this wait call returns. Model projections default to 10000 tokens; explicit requests are capped at 40000 tokens.
- `terminate: true` stops the running cell; false or omitted waits for output.
- `wait` buffers output until an explicit yield, input activity, final completion or termination, or 10 minutes without output or a nested-call completion. An idle handoff leaves the cell running; inspect before waiting again and do not restart uncertain work.
- New user steering, internal completions, or agent messages with `trigger_turn: true` interrupt a held wait without terminating a still-valid cell. Informational agent messages (`trigger_turn: false`) do not interrupt `exec` or `wait`.
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
                    assert_eq!(options, vec![(None, true, false), (Some(40000), false, false), (None, false, true)]);
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
                    "ties use definition order, missing estimates zero",
                    "Claims are atomic/cell-local, not permissions/cross-cell locks",
                    "Command nodes must drain live processes before settling/releasing claims",
                    "Structure is validated globally; required capabilities only for selected nodes",
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
