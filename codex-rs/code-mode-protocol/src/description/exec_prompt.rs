const DEFERRED_NESTED_TOOLS_GUIDANCE: &str = r#"Some deferred nested tools may be omitted from this description. They are still available on the global `tools` object and listed in `ALL_TOOLS`.
To find one, filter `ALL_TOOLS` by `name` and `description`."#;
const LAZY_NESTED_TOOL_SCHEMA_GUIDANCE: &str = r#"Nested tool schemas are discovered lazily at runtime. Find a tool in `ALL_TOOLS` by `name` or `description`, then inspect that entry's exact description before calling it. When `tool_search` is advertised, use it to activate tools that are not yet listed."#;
pub(crate) const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript in a fresh V8 isolate. Input is JS, not JSON/Markdown; Node, filesystem, network, and console are unavailable.
- Nested tools are normalized `tools` methods (e.g. `await tools.exec_command(...)`) with documented I/O.
- Only tools listed in `ALL_TOOLS` are callable inside `exec`; direct-only tools stay outside.
- For a named symbol/config key, query its exact token and direct consumers first; search repo/project names only if unresolved, never in the same batch.
- Nested tool operations have a hard 60s default deadline; expiry cancels the operation. Only resume an observation poll when the tool returned a session or cell ID, and never duplicate a timed-out operation. Honor tool contracts. Await `notify` per settlement; use `allSettled`, never bare `Promise.all`. Sequence dependencies.
- Eight sampling passes per turn is an efficiency target, not a completion/validation cap. Required routing, safety, contract, test, or validation evidence overrides it for dependent or independent work.
- After deterministic failure, including a session poll, do not repeat the unchanged call; change route or relevant state.
- Keep evidence bounded: read relevant config/session tables or line ranges, never whole files; after truncation use a retained-artifact selector. Keep long commands in the same awaited evaluation; call `yield_control()` only for a new model decision.
- Optional first line: `// @exec: {"max_output_tokens": 10000}`. `max_output_tokens` defaults to 10000. Pass a nested tool's documented `{ timeout_ms }` option when it needs a longer bound.
- When evaluation ends, unawaited work is discarded.

Global helpers:
- `exit()` ends successfully.
- `text(value: string | number | boolean | undefined | null)` appends text, JSON-stringifying non-strings when possible.
- `image(imageUrlOrItem: string | { image_url: string; detail?: "auto" | "low" | "high" | "original" | null } | { type: "image"; data: string; mimeType: string; _meta?: Record<string, unknown> }, detail?: "auto" | "low" | "high" | "original" | null)` appends a base64 image or MCP image; explicit detail wins.
- `audio(audioUrlOrItem: string | { audio_url: string } | { type: "audio"; data: string; mimeType: string })` appends a base64 `data:` audio URL or one MCP audio block.
- `generatedImage(result: { image_url: string; output_hint?: string })` appends generated output; HTTP(S) is unsupported.
- `store(key: string, value: any)` and `load(key: string)` persist serializable session values.
- `notify(value: string | number | boolean | undefined | null): Promise<void>` resolves after delivery; await it.
- `setTimeout(callback: () => void, delayMs?: number)` schedules work; await timers to keep exec alive. `clearTimeout(timeoutId?: number)` cancels one.
- `ALL_TOOLS` lists `{ name, description }`; `yield_control()` emits output while execution continues."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- Use `wait` only after `exec` returns `Script running with cell ID ...`.
- `cell_id` identifies the running `exec` cell to resume.
- `max_tokens` limits how much new output this wait call returns. Model projections default to a coherent packet of up to 10000 tokens; a lower requested value is honored and every result remains bounded by the model hard limit.
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
