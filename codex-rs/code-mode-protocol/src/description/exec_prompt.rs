const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in contracts may follow; external and omitted contracts remain lazy. Omission is not deactivation: resolve known exact names locally; calls still pass current authorization."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no browser/Node globals or filesystem/network.
- Nested tools live on the global `tools` object: `const r = await tools.exec_command({cmd:"..."}); text(r.output);`. `console.log(...)` aliases `text(...)`.
- Edit with `apply_patch`: nested if registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- `exec_command` waits for the command to finish; one still running after the default wait returns a live `session_id` (see `await_command`). `r.output` is the combined display text and `r.exit_code` the exit status; the host reports failed commands itself. For exact parsing (for example `JSON.parse`), use `r.stdout`, present when `r.streams_complete` is true.
- Await all work: unawaited work is discarded, and an uncaught rejection ends the cell. Earlier effects survive failure; recover receipts, never replay them. Use `Promise.allSettled` for independent fallible calls. Batch independent reads and searches as one `text(await Promise.allSettled([tools.read_file({path:"a.rs"}), tools.exec_command({cmd:"rg -n x src"})]))`: one failed read cannot skip siblings. Run builds and other commands that may write outside such batches: they hold the workspace exclusively, so queued siblings may not run.
- Nested calls use a host-configured default deadline; override it with the documented `{ timeout_ms }` option when needed. Expiry cancels the nested call and may return only an error, without a live handle. On expiry, check the outcome before retrying uncertain effects.
- First-line `// @exec: {"deliver": true}` makes all printed text the final answer, with no model call, when everything in the cell succeeded; close open plans first. It shows only at cell end, so put long reports in a normal final message.
- Output defaults to 10000 tokens per cell; first-line `// @exec: {"max_output_tokens": 40000}` raises it (ceiling 40000). A truncated result names the `read_tool_output` call that returns the rest.
- JS bindings reset per exec; `store(key, value)`/`load(key)` keep JSON values for the session. `listKeys({after?, limit?})` pages stored keys (`limit` 1-64, default 32; continue from `next_after`); `deleteStored(key)` removes one. First-line `// @exec: {"persist":true}` also keeps completed values across a host restart.
- To call a tool not described here, filter `ALL_TOOL_NAMES` or `ALL_TOOLS` locally, then use `resolve_tool(name)` and call it in the same cell; resolution retains the full contract when required. Do not infer arguments from a name.
- `ALL_TOOLS` may factor shared namespace rules into one entry's `namespace_instructions`; `namespace_instructions_ref` names that entry. Follow those rules or use `resolve_tool(name)`, which expands the complete shared and operation-specific contract, before calling a selected tool.

Optional helpers:
- `await read_files(paths, {full?, concurrency?})`: read 1–32 paths; returns `{path,status,value?,reason?}` per path. `value` contains `{initial,pages,file_complete}`. Default `full:false` keeps the initial selection; for already-required full reading, `full:true` recovers the retained snapshot within bounded limits. Check every status and `file_complete` before claiming full byte coverage, not semantic review.
- `await search_files(pattern, {path?, glob?, lines?, max_per_file?, ignore_case?, fixed?, hidden?, no_ignore?})`: ripgrep without a shell, returning `{files:[{path,count,lines?}], match_count, complete, errors?, hint?}` sorted by path; `lines` adds up to `max_per_file` (default 10) `{line,text}` per file; `hint` names existing paths near ones rg could not open.
- `await await_command(r)`: wait until a command that returned a live `session_id` exits (no-output notices do not end the wait); resolves to `{terminal, observations}` and throws on failure with that object on the error's `.evidence`.
- `await run_graph([{id, deps?, run: async (deps) => value, accept?: async (value) => boolean}], {concurrency?, targets?})`: cell-local DAG (1–256 nodes, concurrency 1–16, default 4) returning `{[id]: {status, value}}`. Without `accept`, command results must exit 0. A failed node skips its dependents; it throws with `.results` after all started work settles.
- `notify(value)` queues a model-visible message without yielding; `image(value, detail?)` emits an image from a `data:` URI, a `view_image` result or an MCP `{ type: "image" }` block.
- `setTimeout(callback: () => void, delayMs?: number)` returns an ID; `clearTimeout(timeoutId?: number)` cancels it. Await a promise resolved by the callback to wait."#;
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
                    "A truncated result names the `read_tool_output` call that returns the rest.",
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
