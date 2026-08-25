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
        assert!(description.contains("Find a tool in `ALL_TOOLS`"));
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
        assert!(description.contains("await tools.exec_command"));
        assert!(!description.contains("yield_time_ms"));
        assert!(description.contains("max_output_tokens"));
        assert!(description.contains("type: \"image\""));
        assert!(description.contains("type: \"audio\""));
        assert!(description.contains("unawaited work is discarded"));
        assert!(description.contains("named symbol/config key"));
        assert!(description.contains("its exact token and direct consumers"));
        assert!(description.contains("search repo/project names only if unresolved"));
        assert!(description.contains("never in the same batch"));
        assert!(description.contains("hard 60s default deadline"));
        assert!(description.contains("expiry cancels the operation"));
        assert!(description.contains("Only resume an observation poll"));
        assert!(description.contains("returned a session or cell ID"));
        assert!(description.contains("never duplicate a timed-out operation"));
        assert!(!description.contains("never an operation"));
        assert!(description.contains("Honor tool contracts"));
        assert!(description.contains("Await `notify` per settlement"));
        assert!(description.contains("use `allSettled`"));
        assert!(description.contains("resolves after delivery; await it"));
        assert!(description.contains("never bare `Promise.all`"));
        assert!(description.contains("Eight sampling passes per turn"));
        assert!(description.contains("efficiency target, not a completion/validation cap"));
        assert!(
            description
                .contains("Required routing, safety, contract, test, or validation evidence")
        );
        assert!(description.contains("dependent or independent work"));
        assert!(description.contains("same awaited evaluation"));
        assert!(description.contains("read relevant config/session tables or line ranges"));
        assert!(description.contains("never whole files"));
        assert!(description.contains("after truncation use a retained-artifact selector"));
        assert!(!description.contains("including six small calls"));
        assert!(!description.contains("group 2-5 independent discovery calls"));
        assert!(
            description.contains("Only tools listed in `ALL_TOOLS` are callable inside `exec`")
        );
        assert!(description.contains("including a session poll"));
        assert!(description.contains("change route or relevant state"));
        assert!(description.contains("Sequence dependencies"));
        assert!(description.contains("Keep evidence bounded"));
        assert!(!description.contains("Shared MCP Types:"));
        assert!(!description.contains("type ImageContent ="));
        assert!(!description.contains("Model projections are capped"));
        const HISTORICAL_COMMON_EXEC_DESCRIPTION_BYTES: usize = 3_337;
        assert!(
            EXEC_DESCRIPTION_TEMPLATE.len() * 100 <= HISTORICAL_COMMON_EXEC_DESCRIPTION_BYTES * 92,
            "information-gain-aware batching guidance takes priority over marginal descriptor savings"
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
        assert!(description.contains("filter `ALL_TOOLS` by `name` and `description`"));
        assert!(!description.contains("do not print the full `ALL_TOOLS` array"));
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
