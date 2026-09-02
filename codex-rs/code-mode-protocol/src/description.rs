//! Code-mode exec contract pipeline.
//!
//! ToolSpec registration builds the public exec prompt, [`parse_exec_source`]
//! parses the optional execution pragma, and runtime metadata exposes exact nested-tool
//! declarations. Schema rendering is isolated because it changes independently of both
//! the public prompt and the execution parser.

mod exec_prompt;
mod metadata;
mod pragma;
mod schema_ts;

pub use exec_prompt::build_exec_tool_description;
pub use exec_prompt::build_wait_tool_description;
pub use metadata::CodeModeToolKind;
pub use metadata::EnabledToolMetadata;
pub use metadata::ToolDefinition;
pub use metadata::augment_tool_definition;
pub use metadata::enabled_tool_metadata;
pub use metadata::is_code_mode_nested_tool;
pub use metadata::normalize_code_mode_identifier;
pub use metadata::render_code_mode_sample;
pub use pragma::CODE_MODE_PRAGMA_PREFIX;
pub use pragma::parse_exec_source;
pub use schema_ts::render_json_schema_to_typescript;

#[cfg(test)]
mod tests {
    use super::CodeModeToolKind;
    use super::ToolDefinition;
    use super::augment_tool_definition;
    use super::build_exec_tool_description;
    use super::build_wait_tool_description;
    use super::exec_prompt::EXEC_DESCRIPTION_TEMPLATE;
    use super::normalize_code_mode_identifier;
    use super::parse_exec_source;
    use super::pragma::ParsedExecSource;
    use codex_protocol::ToolName;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn parse_exec_source_without_pragma() {
        assert_eq!(
            parse_exec_source("text('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')",
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn parse_exec_source_with_pragma() {
        assert_eq!(
            parse_exec_source("// @exec: {\"yield_time_ms\": 10}\ntext('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')",
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn parse_exec_source_borrows_the_selected_source_slice() {
        let input = String::from("// @exec: {\"max_output_tokens\": 20}\ntext('borrowed')");
        let parsed = parse_exec_source(&input).expect("pragma should parse");
        let rest_offset = input.find("text('borrowed')").expect("source should exist");

        assert_eq!(parsed.code, "text('borrowed')");
        assert_eq!(parsed.code.as_ptr(), input[rest_offset..].as_ptr());
        assert_eq!(parsed.max_output_tokens, Some(20));
    }

    #[test]
    fn normalize_identifier_rewrites_invalid_characters() {
        assert_eq!(
            "mcp__ologs__get_profile",
            normalize_code_mode_identifier("mcp__ologs__get_profile")
        );
        assert_eq!(
            "hidden_dynamic_tool",
            normalize_code_mode_identifier("hidden-dynamic-tool")
        );
    }

    #[test]
    fn augment_tool_definition_appends_typed_declaration() {
        let definition = ToolDefinition {
            name: "hidden_dynamic_tool".to_string(),
            tool_name: ToolName::plain("hidden_dynamic_tool"),
            description: "Test tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"]
            })),
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains("declare const tools"));
        assert!(
            description.contains(
                "hidden_dynamic_tool(args: { city: string; }, options?: { timeout_ms?: number }): Promise<{ ok: boolean; }>;"
            )
        );
    }

    #[test]
    fn augment_tool_definition_includes_property_descriptions_as_comments() {
        let definition = ToolDefinition {
            name: "weather_tool".to_string(),
            tool_name: ToolName::plain("weather_tool"),
            description: "Weather tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {
                    "weather": {
                        "type": "array",
                        "description": "look up weather for a given list of locations",
                        "items": {
                            "type": "object",
                            "properties": {
                                "location": { "type": "string" }
                            },
                            "required": ["location"]
                        }
                    }
                },
                "required": ["weather"]
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "forecast": {
                        "type": "string",
                        "description": "human readable weather forecast"
                    }
                },
                "required": ["forecast"]
            })),
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains(
            r#"weather_tool(args: {
  // look up weather for a given list of locations
  weather: Array<{ location: string; }>;
}, options?: { timeout_ms?: number }): Promise<{
  // human readable weather forecast
  forecast: string;
}>;"#
        ));
    }

    #[test]
    fn code_mode_only_description_discovers_nested_tools_lazily() {
        let description = build_exec_tool_description(
            /*code_mode_only*/ true,
            /*has_deferred_tools*/ false,
            &[],
        );
        assert!(description.contains("Nested tool schemas are discovered lazily at runtime"));
        assert!(description.contains("When the exact tool name is known"));
        assert!(
            description.contains("inspect compact `ALL_TOOL_NAMES` only when the name is unknown")
        );
        assert!(description.contains("`resolve_tool(name)`"));
        assert!(description.contains("Never scan `ALL_TOOLS`"));
        assert!(
            description.len() < 4_000,
            "compact exec prompt unexpectedly expanded to {} bytes",
            description.len()
        );
    }

    #[test]
    fn exec_description_mentions_timeout_helpers() {
        let description = build_exec_tool_description(false, false, &[]);
        assert!(description.contains("`setTimeout(callback: () => void, delayMs?: number)`"));
        assert!(description.contains("`clearTimeout(timeoutId?: number)`"));
    }

    #[test]
    fn rollout_workflow_guardrails_require_precise_bounded_discovery() {
        let description = build_exec_tool_description(false, false, &[]);

        assert!(description.contains("raw JavaScript"));
        assert!(description.contains("Nested tools live on the global `tools` object"));
        assert!(description.contains("await tools.exec_command"));
        assert!(description.contains("await tools.apply_patch(patchText)"));
        assert!(
            description
                .contains("Bare `exec(...)` / `exec_command(...)` alias `tools.exec_command`")
        );
        assert!(description.contains("`console.log(...)` aliases `text(...)`"));
        assert!(description.contains("Only `ALL_TOOL_NAMES` entries are callable"));
        assert!(description.contains("never pipe a patch through a shell wrapper"));
        assert!(description.contains("host also retains bounded nested-tool results"));
        assert!(!description.contains("yield_time_ms"));
        assert!(description.contains("max_output_tokens"));
        assert!(description.contains("type: \"image\""));
        assert!(description.contains("type: \"audio\""));
        assert!(description.contains("unawaited work is discarded"));
        assert!(description.contains("Prefer a purpose-built tool over shell"));
        assert!(description.contains("consolidate related read-only probes"));
        assert!(description.contains("merely to re-filter a result already returned"));
        assert!(description.contains("first safe useful read or action in the initial exec"));
        assert!(description.contains("skip status-only sampling"));
        assert!(description.contains(
            "Batch the instructions, target source, tests, and status you already know you need into one exec"
        ));
        assert!(description.contains("project instructions already in context"));
        assert!(description.contains("loaded `AGENTS.md` contract"));
        assert!(
            description.contains("Reuse exact schemas, CLI usage, and results already in context")
        );
        assert!(description.contains("do not rediscover or guess arguments/subcommands"));
        assert!(description.contains("If absent or stale"));
        assert!(description.contains("inspect the exact schema or `--help` once before calling"));
        assert!(description.contains("Nested tools: use a present schema"));
        assert!(description.contains("`resolve_tool(name)` when the name is known"));
        assert!(description.contains("or inspect `ALL_TOOL_NAMES`"));
        assert!(description.contains("Never scan/filter/stringify/print `ALL_TOOLS`"));
        assert!(description.contains("Read or list known paths directly"));
        assert!(description.contains("do not substitute a search or second shell"));
        assert!(description.contains("hard 60s default deadline"));
        assert!(description.contains("Resume only a returned session/cell ID"));
        assert!(description.contains("never duplicate a timed-out operation"));
        assert!(description.contains("Honor tool contracts"));
        assert!(description.contains("with `Promise.allSettled`"));
        assert!(description.contains("never bare `Promise.all`"));
        assert!(description.contains("sequence true dependencies"));
        assert!(description.contains("initial 10s budget"));
        assert!(description.contains("same awaited evaluation"));
        assert!(description.contains("only for a new model decision"));
        assert!(description.contains("Run the required test as its own final command"));
        assert!(description.contains("never mask it with `|| true`"));
        assert!(description.contains("at most one combined diff/status check, then finish"));
        assert!(description.contains("unchanged evidence"));
        assert!(description.contains("synthesize, or stop"));
        assert!(description.contains("never repeat the same call/poll"));
        assert!(description.contains("change route/state"));
        assert!(description.contains("Keep evidence bounded"));
        assert!(description.contains("relevant tables/line ranges"));
        assert!(description.contains("never whole files"));
        assert!(description.contains("concise synthesis, not raw payloads"));
        assert!(description.contains("retained-artifact selectors after truncation"));
        assert!(description.contains("Output defaults to the 10000-token hard cap"));
        assert!(description.contains("smallest useful budget"));
        assert!(
            description.contains("queues an extra model-visible message without yielding the cell")
        );
        // Retired guidance that pushed the model into wait rounds or extra
        // sampling passes must stay out of the contract.
        assert!(!description.contains("per settlement"));
        assert!(!description.contains("resolves after delivery"));
        assert!(!description.contains("sampling passes"));
        assert!(!description.contains("Only `ALL_TOOLS` entries are callable inside `exec`"));
        assert!(!description.contains("Shared MCP Types:"));
        assert!(!description.contains("type ImageContent ="));
        assert!(!description.contains("Model projections are capped"));
        const COMPACT_EXEC_DESCRIPTION_BYTE_BUDGET: usize = 3_300;
        assert!(
            EXEC_DESCRIPTION_TEMPLATE.len() <= COMPACT_EXEC_DESCRIPTION_BYTE_BUDGET,
            "the exec contract must stay within {COMPACT_EXEC_DESCRIPTION_BYTE_BUDGET} bytes; got {}",
            EXEC_DESCRIPTION_TEMPLATE.len()
        );
    }

    #[test]
    fn code_mode_only_prompt_omits_contract_while_runtime_metadata_preserves_it() {
        let mandatory_tail = "mandatory safety and citation rules";
        let tool_description = format!("{}\n{mandatory_tail}", "d".repeat(1_250));
        let tool = ToolDefinition {
            name: "sample_tool".to_string(),
            tool_name: ToolName::plain("sample_tool"),
            description: tool_description,
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "schema-only property guidance"
                    },
                    "path": {
                        "type": "string",
                        "description": "schema-only guidance"
                    }
                },
                "required": ["description", "path"],
                "additionalProperties": false
            })),
            output_schema: None,
        };
        let description = build_exec_tool_description(true, false, &[]);

        assert!(!description.contains("sample_tool"));
        assert!(!description.contains(mandatory_tail));
        assert!(!description.contains("description: string;"));
        assert!(!description.contains("schema-only guidance"));
        assert!(!description.contains("schema-only property guidance"));
        assert!(!description.contains("// schema-only guidance"));

        let runtime_description = augment_tool_definition(tool).description;
        assert!(runtime_description.contains(mandatory_tail));
        assert!(runtime_description.contains("description: string;"));
        assert!(runtime_description.contains("// schema-only guidance"));
    }

    #[test]
    fn exec_description_mentions_deferred_nested_tools_when_available() {
        let description = build_exec_tool_description(false, true, &[]);

        assert!(description.contains("Some deferred nested tools may be omitted"));
        assert!(
            description.contains("inspect compact `ALL_TOOL_NAMES` only when the name is unknown")
        );
        assert!(description.contains("`resolve_tool(\"tool_name\")`"));
        assert!(description.contains("Never scan `ALL_TOOLS`"));
    }

    #[test]
    fn exec_description_lists_direct_only_tools_without_nested_contracts() {
        let description =
            build_exec_tool_description(false, false, &["request_user_input".to_string()]);

        assert!(
            description
                .contains("Direct-only tools omitted from `ALL_TOOLS`: `request_user_input`")
        );
        assert!(!description.contains("request_user_input(args:"));
    }

    #[test]
    fn yield_time_control_is_not_advertised_to_the_model() {
        let exec = build_exec_tool_description(false, false, &[]);
        let wait = build_wait_tool_description();

        assert!(!exec.contains("yield_time_ms"));
        assert!(exec.contains("documented `{ timeout_ms }` option"));
        assert!(!wait.contains("yield_time_ms"));
    }
}
