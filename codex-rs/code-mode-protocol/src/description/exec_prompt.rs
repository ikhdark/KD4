const DEFERRED_NESTED_TOOLS_GUIDANCE: &str =
    "Some deferred nested tools may be omitted from this description.";
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Nested tool schemas are discovered lazily at runtime. When `tool_search` is advertised, use it to activate tools that are not yet listed."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript; input JS, not JSON/Markdown; no Node/filesystem/network.
- Nested tools live on the global `tools` object: `await tools.exec_command({ cmd: "..." })`, `await tools.apply_patch(patchText)` when declared below. Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`; `console.log(...)` aliases `text(...)`. Only `ALL_TOOL_NAMES` entries are callable.
- Edit with `apply_patch`: nested when registered, otherwise direct (`*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- Returns printed `text(...)` values; on failure or no output, the host also retains bounded nested-tool results.
- Reuse current schemas, CLI usage, and results. Resolve missing/stale tool schemas before calling; consult CLI `--help` only for uncertain arguments/subcommands.
- Nested tools: use a present schema; else `resolve_tool(name)` when the name is known, or inspect `ALL_TOOL_NAMES`. Never scan/filter/stringify/print `ALL_TOOLS`.
- Do not rediscover known paths. Read/list known locations directly; otherwise search narrowly within that path. Reuse current applicable `AGENTS.md`; retrieve missing scopes or invalidated content.
- Start useful work in the initial exec. Batch independent known reads/probes with `Promise.allSettled`; inspect every result. Find unknown paths first; sequence dependent calls. Keep status and file outputs distinct; independent calls may share one exec.
- Prefer a purpose-built tool over shell; consolidate related read-only probes in one call. Never spawn a subprocess merely to re-filter a result already returned.
- Nested calls: hard 60s default deadline. After a timeout, resume a returned live session/cell ID. Do not rerun while the original is live or its effects are uncertain. Retry only if it never started, stopped and is safe to repeat, or the tool permits retry.
- A cell runs to completion within its initial 10s budget and yields only on `yield_control()`, new user input, or when that budget expires. Keep long commands in the same awaited evaluation; call `yield_control()` only for a new model decision.
- Run required validation after the final relevant edit. Parallelize only tool-permitted commands with independent build locks, output paths, and services. Propagate sequential failures with `&&` or exit-code checks; never mask them with `|| true`. Complete requested work and checks, or report failures/blockers. Follow plans while they match the current request.
- Do not repeat unchanged deterministic failures. Change route/state or report the blocker; resume live operations through documented wait interfaces.
- Keep evidence bounded and complete: relevant ranges for large files; whole files when small or required. Use retained-artifact selectors after truncation.
- Output defaults to the 10000-token hard cap. Set the smallest useful budget with first-line `// @exec: {"max_output_tokens": 2000}`. Nested-call deadlines use the documented `{ timeout_ms }` option.
- When evaluation ends, unawaited work is discarded.

Helpers:
- Values/media include `{ type: "image" }` / `{ type: "audio" }` blocks.
- `notify(value): Promise<void>` queues an extra model-visible message without yielding the cell; prefer `text(...)`.
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
    if !direct_only_tool_names.is_empty() {
        let names = direct_only_tool_names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(format!(
            "Direct-only tools omitted from `ALL_TOOLS`: {names}. Call these through their direct model tool interface, not through `exec`."
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
            "Direct-only tools omitted from `ALL_TOOLS`: `apply_patch`. Call these through their direct model tool interface, not through `exec`."
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
                    "Complete requested work and checks, or report failures/blockers.",
                    "Follow plans while they match the current request.",
                    "Do not repeat unchanged deterministic failures.",
                    "resume live operations through documented wait interfaces.",
                    "whole files when small or required.",
                    "Reuse current schemas, CLI usage, and results.",
                    "consult CLI `--help` only for uncertain arguments/subcommands.",
                    "Reuse current applicable `AGENTS.md`; retrieve missing scopes or invalidated content.",
                    "After a timeout, resume a returned live session/cell ID.",
                    "Do not rerun while the original is live or its effects are uncertain.",
                    "Retry only if it never started, stopped and is safe to repeat, or the tool permits retry.",
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
