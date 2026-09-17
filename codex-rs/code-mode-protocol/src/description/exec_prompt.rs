const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Nested tool schemas are discovered lazily at runtime. When `tool_search` is advertised, use it to activate tools that are not yet listed."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript, not JSON/Markdown; no Node/filesystem/network.
- Nested tools live on the global `tools` object: `await tools.exec_command({cmd:"..."})`, `await tools.apply_patch(patchText)` if registered. Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`; `console.log(...)` aliases `text(...)`. Only `ALL_TOOL_NAMES` entries are callable.
- Edit with `apply_patch`: nested when registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- `text(...)` emits values; on failure/no output, the host retains up to eight nested-tool results, each capped at 4096 bytes, and reports omissions. Emit needed results explicitly.
- Reuse schemas, CLI usage, and results; resolve missing/stale schemas before calls. Use CLI `--help` only for uncertain arguments/subcommands.
- Nested tools: use a present schema; else `resolve_tool(name)` when the name is known, or inspect `ALL_TOOL_NAMES`. Never scan/filter/stringify/print `ALL_TOOLS`.
- Read/list known paths directly; search unknown locations narrowly. Reuse applicable `AGENTS.md`; refresh missing/invalidated scopes.
- Start useful work in the initial exec. Await `Promise.allSettled` for independent known reads/probes and inspect every result. Use `await notify(...)` in a handler for useful early results; still await the batch. At evaluation end, unawaited work is discarded. Finish discovery before dependent mutations. Separate status/file output; batch independent calls.
- Prefer a purpose-built tool over shell; consolidate related read-only probes. Never spawn subprocesses to re-filter returned results.
- Calls: hard 60s default deadline. After timeout, resume the returned live session/cell ID; never rerun live or uncertain effects. Retry only if unstarted, safely repeatable after stopping, or tool-approved.
- Cells yield on `yield_control()`, input, or their initial 10s budget. Keep long commands in the same awaited evaluation; yield explicitly only for a new model decision.
- Run required validation after the final relevant edit. Parallelize only when tools permit and build locks, outputs, and services are independent. Propagate failures with `&&` or exit-code checks; never mask them with `|| true`. Finish work/checks or report failures/blockers. Keep plans aligned with the request.
- For unchanged deterministic failures, change route/state or report blockers; resume live operations via documented waits.
- Read relevant ranges for large files; whole files when small or required. Read the complete enclosing unit before editing; refresh after intervening writes. Use retained-artifact selectors after truncation.
- Output defaults to the 10000-token hard cap. Set the smallest useful budget with first-line `// @exec: {"max_output_tokens": 2000}`. Nested-call deadlines use the documented `{ timeout_ms }` option.

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
    let mut sections = Vec::new();
    sections.push(EXEC_DESCRIPTION_TEMPLATE.to_string());
    if !code_mode_only {
        sections.push("For a single call, prefer its direct interface when advertised. Use `exec` for orchestration or result processing.".to_string());
    }
    if !direct_only_tool_names.is_empty() {
        let names = direct_only_tool_names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(format!(
            "Direct-only tools omitted from `ALL_TOOLS`: {names}. Call these through their direct model tool interface using the schema advertised there, not through `exec`."
        ));
    }
    if code_mode_only {
        // Keep the public `exec` schema invariant across nested-tool inventory
        // changes. Exact per-tool contracts remain available in the runtime's
        // augmented `ALL_TOOLS` entries and through tool search.
        sections.push(LAZY_NESTED_TOOL_SCHEMA_GUIDANCE.to_string());
    } else if has_deferred_tools {
        sections.push(DEFERRED_NESTED_TOOLS_GUIDANCE.to_string());
    }

    sections.join("\n\n")
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
                        "Nested-call deadlines use the documented `{ timeout_ms }` option."
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
    fn workflow_guidance_allows_blockers_waits_and_required_complete_reads() {
        for code_mode_only in [false, true] {
            for has_deferred_tools in [false, true] {
                let description =
                    build_exec_tool_description(code_mode_only, has_deferred_tools, &[]);
                for required in [
                    "Finish work/checks or report failures/blockers.",
                    "Keep plans aligned with the request.",
                    "For unchanged deterministic failures, change route/state or report blockers;",
                    "resume live operations via documented waits.",
                    "whole files when small or required.",
                    "Read the complete enclosing unit before editing; refresh after intervening writes.",
                    "Finish discovery before dependent mutations.",
                    "Await `Promise.allSettled`",
                    "Reuse schemas, CLI usage, and results;",
                    "Use CLI `--help` only for uncertain arguments/subcommands.",
                    "Reuse applicable `AGENTS.md`; refresh missing/invalidated scopes.",
                    "After timeout, resume the returned live session/cell ID;",
                    "never rerun live or uncertain effects.",
                    "Retry only if unstarted, safely repeatable after stopping, or tool-approved.",
                ] {
                    assert!(
                        description.contains(required),
                        "missing guidance: {required}"
                    );
                }
                for retired in [
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
