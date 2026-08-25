const DEFERRED_NESTED_TOOLS_GUIDANCE: &str = r#"Some deferred nested tools may be omitted from this description. When the exact tool name is known, call `resolve_tool("tool_name")`; inspect compact `ALL_TOOL_NAMES` only when the name is unknown. Never scan `ALL_TOOLS`."#;
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Nested tool schemas are discovered lazily at runtime. When the exact tool name is known, call `resolve_tool(name)`; inspect compact `ALL_TOOL_NAMES` only when the name is unknown. When `tool_search` is advertised, use it to activate tools that are not yet listed. Never scan `ALL_TOOLS`."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript; input JS, not JSON/Markdown; no Node/filesystem/network/console.
- Call `tools` methods, e.g. `await tools.exec_command(...)`. Only `ALL_TOOLS` entries are callable inside `exec`.
- Reuse exact schemas, CLI usage, and results already in context; do not rediscover or guess arguments/subcommands. If absent or stale, inspect the exact schema or `--help` once before calling.
- Before guessing an owner ID, use a compact ID-listing command.
- Nested tools: use a present schema; else `resolve_tool(name)` when the name is known, or inspect `ALL_TOOL_NAMES`. Never scan/filter/stringify/print `ALL_TOOLS`.
- Read or list known paths directly in the current shell; do not substitute a search or second shell.
- On Windows, use one statically parseable content read; omit redundant metadata probes.
- Start the first safe useful read or action in the initial exec; skip status-only sampling.
- Treat project instructions already in context as the loaded `AGENTS.md` contract; reread only if marked omitted, incomplete, or stale.
- For a named symbol/config key, query its exact token and direct consumers first; search repo/project names only if unresolved, never in the same batch.
- Repository-wide `rg`/`rg --files` requires a same-query narrower owner/subtree miss first. Start scoped with a positional directory. Never start with bare repo-root `rg --files -g ...`.
- Prefer a purpose-built tool over shell; consolidate related read-only probes in one call. Mutating or unproven workspace calls are serialized; independent proven-read-only workspace calls may run concurrently through the shared read gate. Never spawn a subprocess merely to re-filter a result already returned.
- Batch every known independent call in one exec and each fully known multi-file edit in one patch; await `notify` per settlement; use `allSettled`, never bare `Promise.all`. Sequence true dependencies.
- Nested calls: hard 60s default deadline. Resume only a returned session/cell ID; never duplicate a timed-out operation. Honor tool contracts.
- Target at most eight sampling passes; required routing, safety, contract, test, or validation evidence overrides this. A tool return alone needs no sampling pass.
- Keep long commands in the same awaited evaluation; call `yield_control()` only for a new model decision.
- On deterministic failure or unchanged evidence, change route/state, synthesize, or stop; never repeat the same call/poll.
- Keep evidence bounded: relevant tables/line ranges, never whole files; concise synthesis, not raw payloads; retained-artifact selectors after truncation.
- Output defaults to 4000 tokens; request the smallest useful budget within the 10000-token hard cap. Optional `max_output_tokens`; use the documented `{ timeout_ms }` option.
- When evaluation ends, unawaited work is discarded.

Helpers:
- Values/media include `{ type: "image" }` / `{ type: "audio" }` blocks.
- `notify(value): Promise<void>` resolves after delivery; await it.
- `setTimeout(callback: () => void, delayMs?: number)` / `clearTimeout(timeoutId?: number)` manage timers; await them.
- `ALL_TOOL_NAMES` lists; `resolve_tool(name)` resolves; `ALL_TOOLS` is legacy."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- Use `wait` only after `exec` returns `Script running with cell ID ...`.
- `cell_id` identifies the running `exec` cell to resume.
- `max_tokens` limits how much new output this wait call returns. Model projections default to 4000 tokens; an explicit request is honored up to the 10000-token hard cap.
- `terminate: true` stops the running cell; false or omitted waits for output.
- `wait` returns only meaningful new output or state changes since the last model-visible result, or the final completion or termination result for that cell.
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
