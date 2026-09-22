const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Stable built-in tool contracts may be included below. Use those declarations directly; external and omitted contracts remain lazy. When `tool_search` is advertised, use it to activate tools that are not yet listed."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no Node/filesystem/network.
- Nested tools live on the global `tools` object: `await tools.exec_command({cmd:"..."})`, `await tools.apply_patch(patchText)` if registered. Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`; `console.log(...)` aliases `text(...)`. Only `ALL_TOOL_NAMES` entries are callable.
- Edit with `apply_patch`: nested when registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- `text(...)` emits values; on failure/no output, the host retains up to eight nested-tool results, each capped at 4096 bytes, and reports omissions. Emit needed results explicitly.
- Reuse current schemas and results; resolve missing/stale schemas before calls.
- Nested tools: use a present schema; else `resolve_tool(name)` when the name is known, or inspect `ALL_TOOL_NAMES`. Never scan/filter/stringify/print `ALL_TOOLS`.
- Await `Promise.allSettled` for independent known calls in the same exec and inspect every result. Do not split a known independent batch into one-call execs. Use `await notify(...)` in a handler for useful early results; still await the batch. At evaluation end, unawaited work is discarded. Sequence dependent calls only after checking prerequisite results.
- Choose tools whose scope, evidence, and cost fit the task. Process returned results in JavaScript rather than spawning subprocesses to re-filter them.
- Nested calls use a host-configured default deadline; override it with the documented `{ timeout_ms }` option when needed. Expiry cancels the nested call and may return only an error, without a live handle. Resume only an actually returned live session/cell ID; otherwise check the outcome before retrying uncertain effects. Retry only if unstarted, safely repeatable after stopping, or tool-approved.
- Cells yield on `yield_control()`, input, or their initial 10s budget. Keep already-planned calls and long commands in the same awaited evaluation; yield explicitly only for a new model decision or dependency resolution.
- An exec cell and a command process have separate lifecycles. A resolved `exec_command` call may still return a running command session. Resume a running cell with `wait(cell_id)`; resume a returned command session with `write_stdin(session_id)`. When no new model decision is needed, continue that session within the current evaluation. Completion of the cell does not establish completion of every process it started. Command lifecycle and recovery metadata survive text-only output and zero-token text budgets.
- Empty `write_stdin` polls: use default waits; avoid one-second loops.
- Parallelize only when tools permit and build locks, outputs, and services are independent. Propagate failures with `&&` or exit-code checks; never mask them with `|| true`.
- Output defaults to the 10000-token hard cap. Set the smallest useful budget with first-line `// @exec: {"max_output_tokens": 2000}`. This limits model-visible output separately from nested tools' output budgets. Select relevant results before emitting them; use retained-artifact selectors after truncation.

Helpers:
- Media: `{ type: "image" }` / `{ type: "audio" }` blocks.
- `notify(value): Promise<void>` queues a model-visible message without yielding.
- `setTimeout(callback: () => void, delayMs?: number)` returns an ID; `clearTimeout(timeoutId?: number)` cancels it. Await a promise resolved by the callback to wait."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- `exec` owns its initial 10s completion budget and internally drains ordinary empty observations. Use `wait` only after `exec` returns a genuinely live `Script running with cell ID ...` result, such as an explicit `yield_control()` or input interruption; a completed cell never needs `wait`.
- `cell_id` identifies the running `exec` cell to resume.
- `max_tokens` limits how much new output this wait call returns. Model projections default to the 10000-token hard cap; an explicit request can select a smaller budget.
- `terminate: true` stops the running cell; false or omitted waits for output.
- `wait` is host-held until meaningful new output, an explicit yield, input activity, or the final completion or termination result for that cell.
- New user steering or mailbox input interrupts a held wait without terminating a still-valid cell.
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
                    assert_eq!(directives.len(), 1);
                    let source = format!("{}\ntext('budget applied');", directives[0]);
                    let parsed = parse_exec_source(&source).unwrap();
                    assert_eq!(parsed.max_output_tokens, Some(2000));
                    assert_eq!(parsed.code, "text('budget applied');");
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
                    "Await `Promise.allSettled`",
                    "for independent known calls in the same exec and inspect every result.",
                    "Do not split a known independent batch into one-call execs.",
                    "Keep already-planned calls and long commands in the same awaited evaluation;",
                    "yield explicitly only for a new model decision or dependency resolution.",
                    "still await the batch. At evaluation end, unawaited work is discarded.",
                    "Parallelize only when tools permit and build locks, outputs, and services are independent.",
                    "Reuse current schemas and results;",
                    "A resolved `exec_command` call may still return a running command session.",
                    "continue that session within the current evaluation.",
                    "separately from nested tools' output budgets.",
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
