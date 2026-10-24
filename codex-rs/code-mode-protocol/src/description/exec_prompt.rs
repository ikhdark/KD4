const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in contracts may follow; external and omitted contracts remain lazy. Omission is not deactivation: resolve known exact names locally; calls still pass current authorization."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no browser/Node globals or filesystem/network.
- Nested tools: const r = await tools.exec_command({cmd:"..."}); text(r.output); console.log aliases text.
- Edit via apply_patch: nested if registered, otherwise direct (*** Begin Patch envelope); never pipe patches through shell.
- exec_command waits; one still running after the default wait returns a live `session_id` (see await_command). r.output is display text; r.exit_code is status; the host reports failed commands itself. For exact parsing use `r.stdout`, present when `r.streams_complete` is true.
- Await every call: unawaited work is discarded, and an uncaught rejection ends the cell. Earlier effects survive failure; recover receipts, never replay them. Use `Promise.allSettled` for independent fallible calls. Batch independent reads and searches as one `text(await Promise.allSettled([tools.read_file({path:"a.rs"}), tools.exec_command({cmd:"rg -n x src"})]))`. Run builds and other commands that may write outside such batches; they hold the workspace exclusively.
- Nested deadlines: { timeout_ms } overrides the host default. Expiry cancels the nested call and may return only an error, without a live handle. After expiry, check the outcome before retrying uncertain effects.
- First-line // @exec: {"deliver": true} makes printed text the final answer, without a model call, when everything in the cell succeeded. For a settled answer, await closing work (including tools.update_plan), then print only that answer in this cell. Plan completion alone is not task completion. Delivery appears at cell end; use a normal final for long reports.
- Output defaults to 10000 tokens; first-line // @exec: {"max_output_tokens": 40000} raises it (ceiling 40000). A truncated result names the `read_tool_output` call that returns the rest. Oversized: result_retained:true, outcome.recovery (possibly pending). Recover required recovery/canonical_recovery or outcome.recovery ranges with `await tools.read_tool_output(recipe.arguments)` in-cell before returning; consume pages; bound printing; never rerun producers.
- JS bindings reset per exec; store(key,value)/load(key) retain JSON. Keep one body plus compact state/provenance; deleteStored(key) drops duplicates. Failed quota cells discard staged stores; check listKeys before resuming. listKeys({after?,limit?}): 1-64, default 32; continue next_after. First-line // @exec: {"persist":true} preserves completed values across host restart.
- Deferred tools: ALL_TOOL_NAMES/ALL_TOOLS or tool_search; resolve_tool(name) retains full contracts. Resolve known followups in-cell; never infer arguments from names. Follow namespace_instructions_ref and operation rules, or resolve the full contract.

Optional helpers:
- `await read_files(paths, {full?, concurrency?})`: 1–32 paths; {path,status,value?,reason?} each; value={initial,pages,file_complete}. full defaults false; true recovers an already-required full snapshot. Check every status and file_complete; byte coverage is not semantic review.
- `await search_files(pattern, {path?, glob?, lines?, max_per_file?, ignore_case?, fixed?, hidden?, no_ignore?})`: ripgrep returning {files:[{path,count,lines?}],match_count,complete,errors?,hint?}. lines adds {line,text}, default max_per_file=10; hint suggests existing paths near errors.
- `await await_command(r, {max_bytes?})`: drains a live command to {terminal,observations}; throws on failure with that object on the error's `.evidence`. At the evidence bound (default/max 4 MiB), retain/page .evidence and resume the exact handle; never restart.
- `await run_graph([{id,deps?,run,accept?}], {concurrency?,targets?})`: 1–256 nodes; concurrency 1–16, default 4. Returns {[id]:{status,value,input_index}}; sort by input_index for original order including numeric IDs. Without accept, command exit codes must be 0. On node failure, its dependents are skipped and it throws with `.results` after all started work settles.
- notify(value) queues a model-visible message without yielding. image(value,detail?) emits a data: URI, view_image result or MCP image block.
- setTimeout(callback,delayMs?) returns an ID; clearTimeout(id) cancels it. Await a callback-resolved promise to wait."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- `exec` buffers output while its awaited continuation runs. Use `wait` only after `exec` returns a genuinely live `Script running with cell ID ...` result, such as an explicit `yield_control()` or input interruption; a completed cell never needs `wait`.
- `max_tokens` sets the cell's output budget for this and later results; omit it to keep the current budget (initially the exec budget). Model projections default to 10000 tokens; explicit requests are capped at 40000 tokens.
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
        // Environment-backed examples are appended by the planner only when
        // the corresponding built-in is in the effective nested catalog.
        assert!(!description.contains("tools.list_files"));
        let helpers_start = description.find("- `await read_files(").unwrap();
        let helpers_end = description.find("- `await run_graph(").unwrap();
        let helpers_bytes = helpers_end - helpers_start;
        // The new workflow helpers get an explicit allowance, not permission
        // to expand the existing contract on every model request. On a fixed
        // task the earlier ~8 KB rules contract cost about 1.4x the reasoning
        // per turn of a lean one, so restated tool details stay out of it.
        assert!(helpers_bytes < 1_500);
        assert!(description.len() - helpers_bytes < 3_600);
        assert!(description.len() < 4_500);
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
                        .lines()
                        .filter_map(|line| line.split_once("// @exec:"))
                        .map(|(_, example)| {
                            let end = example.find('}').expect("complete pragma example");
                            format!("// @exec:{}", &example[..=end])
                        })
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
                        "Nested deadlines: { timeout_ms } overrides the host default."
                    ));
                }
            }
        }
    }

    #[test]
    fn wait_budget_guidance_matches_the_sticky_cell_budget() {
        // An explicit wait budget replaces the cell's budget for its later
        // results and nested reads, so per-call wording understates its reach.
        let wait = super::build_wait_tool_description();
        assert!(wait.contains("sets the cell's output budget for this and later results"));
        assert!(wait.contains("omit it to keep the current budget"));
        assert!(!wait.contains("this wait call"));
        assert!(wait.contains("default to 10000 tokens"));
        assert!(wait.contains("capped at 40000 tokens"));
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
            assert!(description.contains("Edit via apply_patch: nested if registered, otherwise direct (*** Begin Patch envelope); never pipe patches through shell."));
        }
    }

    #[test]
    fn execution_contract_keeps_lifecycle_rules_without_general_workflow() {
        for code_mode_only in [false, true] {
            for has_deferred_tools in [false, true] {
                let description =
                    build_exec_tool_description(code_mode_only, has_deferred_tools, &[]);
                for required in [
                    "unawaited work is discarded, and an uncaught rejection ends the cell.",
                    "Earlier effects survive failure; recover receipts, never replay them.",
                    "no browser/Node globals",
                    "Use `Promise.allSettled` for independent fallible calls.",
                    "Batch independent reads and searches as one `text(await Promise.allSettled([",
                    "Run builds and other commands that may write outside such batches",
                    "one still running after the default wait returns a live `session_id`",
                    "the host reports failed commands itself",
                    "use `r.stdout`, present when `r.streams_complete` is true.",
                    "Expiry cancels the nested call and may return only an error, without a live handle.",
                    "check the outcome before retrying uncertain effects",
                    "when everything in the cell succeeded",
                    "await closing work (including tools.update_plan), then print only that answer in this cell.",
                    "Plan completion alone is not task completion.",
                    "A truncated result names the `read_tool_output` call that returns the rest.",
                    "Recover required recovery/canonical_recovery or outcome.recovery ranges with `await tools.read_tool_output(recipe.arguments)` in-cell before returning;",
                    "JS bindings reset per exec;",
                    "throws on failure with that object on the error's `.evidence`",
                    "its dependents are skipped and it throws with `.results` after all started work settles",
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
