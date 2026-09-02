const DEFERRED_NESTED_TOOLS_GUIDANCE: &str = r#"Some deferred nested tools may be omitted from this description. When the exact tool name is known, call `resolve_tool("tool_name")`; inspect compact `ALL_TOOL_NAMES` only when the name is unknown. Never scan `ALL_TOOLS`."#;
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Nested tool schemas are discovered lazily at runtime. When the exact tool name is known, call `resolve_tool(name)`; inspect compact `ALL_TOOL_NAMES` only when the name is unknown. When `tool_search` is advertised, use it to activate tools that are not yet listed. Never scan `ALL_TOOLS`."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript; input JS, not JSON/Markdown; no Node/filesystem/network.
- Nested tools live on the global `tools` object: `await tools.exec_command({ cmd: "..." })`, `await tools.apply_patch(patchText)` when declared below. Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`; `console.log(...)` aliases `text(...)`. Only `ALL_TOOL_NAMES` entries are callable.
- Edit files with `tools.apply_patch` (the `*** Begin Patch` envelope); never pipe a patch through a shell wrapper.
- A cell returns what it printed with `text(...)`; when the script fails or prints nothing, the host also retains bounded nested-tool results. Print the evidence you need.
- Reuse exact schemas, CLI usage, and results already in context; do not rediscover or guess arguments/subcommands. If absent or stale, inspect the exact schema or `--help` once before calling.
- Nested tools: use a present schema; else `resolve_tool(name)` when the name is known, or inspect `ALL_TOOL_NAMES`. Never scan/filter/stringify/print `ALL_TOOLS`.
- Read or list known paths directly; do not substitute a search or second shell. Treat project instructions already in context as the loaded `AGENTS.md` contract.
- Start the first safe useful read or action in the initial exec; skip status-only sampling. Batch the instructions, target source, tests, and status you already know you need into one exec with `Promise.allSettled`, never bare `Promise.all`; sequence true dependencies.
- Prefer a purpose-built tool over shell; consolidate related read-only probes in one call. Never spawn a subprocess merely to re-filter a result already returned.
- Nested calls: hard 60s default deadline. Resume only a returned session/cell ID; never duplicate a timed-out operation. Honor tool contracts.
- A cell runs to completion within its initial 10s budget and yields only on `yield_control()`, new user input, or when that budget expires. Keep long commands in the same awaited evaluation; call `yield_control()` only for a new model decision.
- Run the required test as its own final command or propagate its exit code; never mask it with `|| true`. Once it passes with no later edit, run at most one combined diff/status check, then finish.
- On deterministic failure or unchanged evidence, change route/state, synthesize, or stop; never repeat the same call/poll.
- Keep evidence bounded: relevant tables/line ranges, never whole files; concise synthesis, not raw payloads; retained-artifact selectors after truncation.
- Output defaults to the 10000-token hard cap; request the smallest useful budget when less output is sufficient. Optional `max_output_tokens`; use the documented `{ timeout_ms }` option.
- When evaluation ends, unawaited work is discarded.

Helpers:
- Values/media include `{ type: "image" }` / `{ type: "audio" }` blocks.
- `notify(value): Promise<void>` queues an extra model-visible message without yielding the cell; prefer `text(...)`.
- `setTimeout(callback: () => void, delayMs?: number)` / `clearTimeout(timeoutId?: number)` manage timers; await them.
- `ALL_TOOL_NAMES` lists; `resolve_tool(name)` resolves; `ALL_TOOLS` is legacy."#;
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
