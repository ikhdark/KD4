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
pub use metadata::render_code_mode_tool_bundle;
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
            default_timeout_ms: None,
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
    fn repeated_shapes_are_declared_once_and_referenced_by_name() {
        // One selector shape reached from four properties, as the retained-output
        // tools do. Re-rendering it at every site is the projector's default.
        let selector = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "kind": {"const": "bytes"},
                        "start": {"type": "integer", "description": "Zero-based start byte."},
                        "end": {"type": "integer", "description": "Exclusive end byte."}
                    },
                    "required": ["kind", "start", "end"]
                },
                {
                    "type": "object",
                    "properties": {
                        "kind": {"const": "lines"},
                        "start": {"type": "integer", "description": "One-based first line."},
                        "end": {"type": "integer", "description": "Inclusive last line."}
                    },
                    "required": ["kind", "start", "end"]
                }
            ]
        });
        let definition = ToolDefinition {
            name: "read_retained".to_string(),
            tool_name: ToolName::plain("read_retained"),
            description: "Read retained output.".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "$defs": {"selector": selector},
                "type": "object",
                "properties": {"selectors": {"type": "array", "items": {"$ref": "#/$defs/selector"}}},
                "required": ["selectors"]
            })),
            default_timeout_ms: None,
            output_schema: Some(json!({
                "$defs": {"selector": selector},
                "type": "object",
                "properties": {
                    "selector": {"$ref": "#/$defs/selector"},
                    "continuation": {"$ref": "#/$defs/selector"},
                    "child_selectors": {"type": "array", "items": {"$ref": "#/$defs/selector"}}
                },
                "required": ["selector", "continuation", "child_selectors"]
            })),
        };

        let description = augment_tool_definition(definition).description;
        // The shape is declared once, under the name its reference supplies.
        assert_eq!(
            description.matches("type CodeModeSelector = ").count(),
            1,
            "the shared shape must be declared exactly once: {description}"
        );
        // Every use site refers to the alias rather than repeating the shape.
        assert_eq!(
            description.matches("Zero-based start byte.").count(),
            1,
            "the shape body must not be repeated at use sites: {description}"
        );
        for use_site in [
            "selectors: Array<CodeModeSelector>",
            "selector: CodeModeSelector;",
            "continuation: CodeModeSelector;",
            "child_selectors: Array<CodeModeSelector>",
        ] {
            assert!(
                description.contains(use_site),
                "missing `{use_site}` in: {description}"
            );
        }
        // The alias is declared before the declaration that consumes it.
        let alias_at = description
            .find("type CodeModeSelector = ")
            .expect("alias must be present");
        let declaration_at = description
            .find("declare const tools")
            .expect("declaration must be present");
        assert!(alias_at < declaration_at, "alias must precede its use");
    }

    #[test]
    fn single_use_shapes_are_not_given_names() {
        let definition = ToolDefinition {
            name: "single_use".to_string(),
            tool_name: ToolName::plain("single_use"),
            description: "One shape, one site.".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {"only": {
                    "type": "object",
                    "properties": {
                        "alpha": {"type": "string", "description": "A description long enough to pass the hoist threshold on its own."},
                        "beta": {"type": "string", "description": "A second description long enough to pass the hoist threshold."}
                    },
                    "required": ["alpha", "beta"]
                }},
                "required": ["only"]
            })),
            default_timeout_ms: None,
            output_schema: None,
        };

        let description = augment_tool_definition(definition).description;
        assert!(
            !description.contains("type CodeMode"),
            "a shape used once must stay inline: {description}"
        );
    }

    #[test]
    fn augment_tool_definition_preserves_intersection_grouping() {
        let schema = json!({
            "allOf": [
                {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": { "city": { "type": "string" } },
                            "required": ["city"]
                        },
                        {
                            "type": "object",
                            "properties": { "zip": { "type": "string" } },
                            "required": ["zip"]
                        }
                    ]
                },
                {
                    "type": "object",
                    "properties": { "country": { "type": "string" } },
                    "required": ["country"]
                }
            ]
        });
        let definition = ToolDefinition {
            name: "lookup".to_string(),
            tool_name: ToolName::plain("lookup"),
            description: "Look up an address.".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(schema.clone()),
            default_timeout_ms: None,
            output_schema: Some(schema),
        };

        // Country is required for both city and zip variants. Without grouping,
        // TypeScript's intersection precedence makes it optional for city.
        let address = "({ city: string; } | { zip: string; }) & ({ country: string; })";
        assert_eq!(
            augment_tool_definition(definition).description,
            format!(
                "Look up an address.\n\nexec tool declaration:\n```ts\ndeclare const tools: {{ lookup(args: {address}, options?: {{ timeout_ms?: number }}): Promise<{address}>; }};\n```"
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
            default_timeout_ms: None,
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
    fn code_mode_only_description_distinguishes_eager_and_lazy_contracts() {
        let description = build_exec_tool_description(
            /*code_mode_only*/ true,
            /*has_deferred_tools*/ false,
            &[],
        );
        assert!(description.contains("Stable built-in tool contracts may be included below"));
        assert!(description.contains("external and omitted contracts remain lazy"));
        assert!(description.contains("`resolve_tool(name)` to obtain a missing schema"));
        assert!(description.contains("filter `ALL_TOOL_NAMES` or `ALL_TOOLS` locally"));
        assert!(description.contains("Search only if local discovery fails"));
        assert_eq!(description.matches("`resolve_tool(name)`").count(), 1);
        assert!(description.contains(
            "When `tool_search` is advertised, use it to activate tools that are not yet listed."
        ));
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
        assert!(description.contains("returns an ID"));
        assert!(description.contains("Await a promise resolved by the callback to wait."));
        assert!(!description.contains("manage timers; await them"));
    }

    #[test]
    fn executor_contract_preserves_discovery_lifecycle_and_output_mechanics() {
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
        assert!(description.contains("host retains up to eight nested-tool results, each capped at 4096 bytes, and reports omissions"));
        assert!(!description.contains("yield_time_ms"));
        assert!(description.contains("max_output_tokens"));
        assert!(description.contains("type: \"image\""));
        assert!(description.contains("type: \"audio\""));
        assert!(description.contains("unawaited work is discarded"));
        assert!(description.contains("Choose tools whose scope, evidence, and cost fit the task"));
        assert!(description.contains("Process returned results in JavaScript"));
        assert!(description.contains("Await `Promise.allSettled` for independent known calls"));
        assert!(description.contains("Reuse current schemas and results"));
        assert!(description.contains("resolve missing/stale schemas before calls"));
        assert!(description.contains("Nested tools: use a present schema"));
        assert!(description.contains("`resolve_tool(name)` to obtain a missing schema"));
        assert!(description.contains("filter `ALL_TOOL_NAMES` or `ALL_TOOLS` locally"));
        assert!(!description.contains("Never scan/filter/stringify/print `ALL_TOOLS`"));
        assert!(!description.contains("do not substitute a search or second shell"));
        assert!(description.contains("host-configured default deadline"));
        assert!(description.contains(
            "Expiry cancels the nested call and may return only an error, without a live handle."
        ));
        assert!(description.contains("Resume only an actually returned live session/cell ID"));
        assert!(description.contains("check the outcome before retrying uncertain effects"));
        assert!(description.contains(
            "Retry only if unstarted, safely repeatable after stopping, or tool-approved"
        ));
        assert!(description.contains("inspect every result"));
        assert!(
            description
                .contains("Sequence dependent calls only after checking prerequisite results")
        );
        assert!(description.contains("buffers output while awaited work continues"));
        assert!(description.contains("same awaited evaluation"));
        assert!(description.contains("only for a new model decision"));
        assert!(description.contains(
            "Parallelize only when tools permit and build locks, outputs, and services are independent"
        ));
        assert!(description.contains("Propagate failures with `&&` or exit-code checks"));
        assert!(description.contains("never mask them with `|| true`"));
        assert!(!description.contains("never repeat the same call/poll"));
        assert!(!description.contains("never whole files"));
        assert!(description.contains("retained-artifact selectors after truncation"));
        assert_eq!(
            crate::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
            crate::MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
        );
        assert!(description.contains(&format!(
            "Output defaults to the {}-token hard cap",
            crate::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
        )));
        assert!(description.contains("the cell budget is separate"));
        assert!(description.contains(r#"first-line `// @exec: {"max_output_tokens": 10000}`"#));
        assert!(description.contains("queues a model-visible message without yielding"));
        // Retired guidance that pushed the model into wait rounds or extra
        // sampling passes must stay out of the contract.
        assert!(!description.contains("per settlement"));
        assert!(!description.contains("resolves after delivery"));
        assert!(!description.contains("sampling passes"));
        assert!(!description.contains("Only `ALL_TOOLS` entries are callable inside `exec`"));
        assert!(!description.contains("Shared MCP Types:"));
        assert!(!description.contains("type ImageContent ="));
        assert!(!description.contains("Model projections are capped"));
        const COMPACT_EXEC_DESCRIPTION_BYTE_BUDGET: usize = 3_600;
        assert!(
            description.len() <= COMPACT_EXEC_DESCRIPTION_BYTE_BUDGET,
            "the exec contract must stay within {COMPACT_EXEC_DESCRIPTION_BYTE_BUDGET} bytes; got {}",
            description.len()
        );
    }

    #[test]
    fn augment_tool_definition_preserves_long_descriptions_and_schema_guidance() {
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
            default_timeout_ms: None,
            output_schema: None,
        };
        let runtime_description = augment_tool_definition(tool).description;
        assert!(runtime_description.contains(mandatory_tail));
        assert!(runtime_description.contains("description: string;"));
        assert!(runtime_description.contains("// schema-only guidance"));
    }

    #[test]
    fn exec_description_mentions_deferred_nested_tools_when_available() {
        let description = build_exec_tool_description(false, true, &[]);

        assert!(description.contains("Some deferred nested tools may be omitted"));
        assert!(description.contains("`resolve_tool(name)` to obtain a missing schema"));
        assert!(description.contains("filter `ALL_TOOL_NAMES` or `ALL_TOOLS` locally"));
        assert!(!description.contains("Never scan/filter/stringify/print `ALL_TOOLS`"));
        assert_eq!(description.matches("`resolve_tool(name)`").count(), 1);
        assert!(!description.contains("Nested tool schemas are discovered lazily at runtime"));
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
    fn assembled_exec_descriptions_bound_boilerplate_without_limiting_tool_names() {
        let inventories = [
            Vec::new(),
            vec!["request_user_input".to_string()],
            (0..128)
                .map(|index| format!("direct_tool_{index}"))
                .collect(),
        ];
        for names in inventories {
            // Names, their backticks, and separators grow with the inventory.
            // The 4,000-byte budget covers all remaining assembled instructions.
            let list_bytes = names.iter().map(|name| name.len() + 2).sum::<usize>()
                + names.len().saturating_sub(1) * 2;
            for code_mode_only in [false, true] {
                for has_deferred_tools in [false, true] {
                    let description =
                        build_exec_tool_description(code_mode_only, has_deferred_tools, &names);
                    assert!(
                        description.len() < 4_000 + list_bytes,
                        "assembled exec prompt exceeded its boilerplate budget: {} bytes, \
                         {list_bytes} bytes of names, code_mode_only={code_mode_only}, \
                         has_deferred_tools={has_deferred_tools}",
                        description.len()
                    );
                    for name in &names {
                        assert!(description.contains(&format!("`{name}`")));
                    }
                }
            }
        }
    }

    #[test]
    fn yield_time_control_is_not_advertised_to_the_model() {
        let exec = build_exec_tool_description(false, false, &[]);
        let wait = build_wait_tool_description();

        assert!(!exec.contains("yield_time_ms"));
        assert!(exec.contains("documented `{ timeout_ms }` option"));
        assert!(!wait.contains("yield_time_ms"));
    }

    fn assert_input_declaration(schema: serde_json::Value, expected: &str) {
        let definition = ToolDefinition {
            name: "sample".to_string(),
            tool_name: ToolName::plain("sample"),
            description: "Sample tool.".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(schema),
            default_timeout_ms: None,
            output_schema: None,
        };
        assert_eq!(
            augment_tool_definition(definition).description,
            format!(
                "Sample tool.\n\nexec tool declaration:\n```ts\ndeclare const tools: {{ sample(args: {expected}, options?: {{ timeout_ms?: number }}): Promise<unknown>; }};\n```"
            )
        );
    }

    #[test]
    fn declarations_preserve_composition_siblings() {
        for keyword in ["allOf", "anyOf", "oneOf"] {
            assert_input_declaration(
                json!({
                    "type": "object", "properties": {"cmd": {"type": "string"}},
                    "required": ["cmd"],
                    (keyword): [{"properties": {"timeout": {"type": "number"}}}]
                }),
                &format!(
                    "({{ cmd: string; }}) & (string | number | boolean | null | unknown[] | {{ timeout?: number; }}){}",
                    if keyword == "oneOf" {
                        " /* oneOf: exactly one branch must match */"
                    } else {
                        ""
                    }
                ),
            );
        }
        assert_input_declaration(
            json!({
                "const": {"cmd": "run"}, "type": "object", "required": ["path"]
            }),
            r#"({ path: unknown; [key: string]: unknown; }) & ({"cmd":"run"})"#,
        );
        assert_input_declaration(
            json!({
                "enum": [{"cmd": "run"}], "type": "object", "required": ["path"]
            }),
            r#"({ path: unknown; [key: string]: unknown; }) & ({"cmd":"run"})"#,
        );
    }

    #[test]
    fn declarations_respect_explicit_reference_dialects() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"base": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}},
            "$ref": "#/$defs/base",
            "properties": {"path": {"type": "string"}}, "required": ["path"]
        });
        assert_input_declaration(
            schema.clone(),
            "(string | number | boolean | null | unknown[] | { path: string; }) & ({ cmd: string; })",
        );
        schema["$schema"] = json!("http://json-schema.org/draft-07/schema#");
        assert_input_declaration(schema, "{ cmd: string; }");
    }

    #[test]
    fn declarations_preserve_tuple_prefix_tail_and_length() {
        assert_input_declaration(
            json!({
                "type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}],
                "items": false, "minItems": 2
            }),
            "[string, number /* integer */] /* minItems: 2 */",
        );
        assert_input_declaration(
            json!({
                "type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}]
            }),
            "[(string)?, (number /* integer */)?, ...Array<unknown>]",
        );
        assert_input_declaration(
            json!({
                "type": "array", "prefixItems": [{"type": "string"}],
                "items": {"type": "boolean"}, "minItems": 2, "maxItems": 4
            }),
            "[string, ...Array<boolean>] /* minItems: 2; maxItems: 4 */",
        );
        assert_input_declaration(
            json!({
                "type": "array", "prefixItems": [{"type": "string"}], "items": false, "minItems": 2
            }),
            "never",
        );
        assert_input_declaration(
            json!({
                "type": "array", "items": [{"type": "string"}], "additionalItems": false, "minItems": 1
            }),
            "[string] /* minItems: 1 */",
        );
        assert_input_declaration(
            json!({
                "type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}], "maxItems": 1
            }),
            "[(string)?] /* maxItems: 1 */",
        );
    }

    #[test]
    fn declarations_preserve_required_only_keys_and_extra_key_rules() {
        assert_input_declaration(
            json!({"type": "object", "required": ["path"]}),
            "{ path: unknown; [key: string]: unknown; }",
        );
        assert_input_declaration(
            json!({"type": "object", "required": ["path"], "additionalProperties": false}),
            "never",
        );
        assert_input_declaration(
            json!({"type": "object", "required": ["path"], "additionalProperties": {"type": "string"}}),
            "{ path: string; [key: string]: unknown; /* additional keys: string */ }",
        );
        assert_input_declaration(
            json!({
                "type": "object", "properties": {"name": {"type": "string"}},
                "required": ["name"], "additionalProperties": {"type": "number"}
            }),
            "{ name: string; [key: string]: unknown; /* additional keys: number */ }",
        );
        assert_input_declaration(
            json!({
                "type": "object", "required": ["path"], "additionalProperties": false,
                "patternProperties": {"^path$": {"type": "string"}}
            }),
            "{ path: unknown; [key: string]: unknown; /* patternProperties not projected */ }",
        );
    }

    #[test]
    fn output_envelopes_preserve_root_refs_and_ordinary_fields() {
        let definition = ToolDefinition {
            name: "receipt".to_string(),
            tool_name: ToolName::plain("receipt"),
            description: "Receipt.".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            default_timeout_ms: None,
            output_schema: Some(json!({
                "$defs": {"Payload": {"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"]}},
                "type": "object",
                "properties": {
                    "content": {"type": "array", "items": {"type": "object"}},
                    "isError": {"type": "boolean"}, "_meta": {"type": "object"},
                    "structuredContent": {"$ref": "#/$defs/Payload"},
                    "receipt_id": {"type": "string"}
                }, "required": ["receipt_id", "structuredContent"]
            })),
        };
        let description = augment_tool_definition(definition).description;
        assert!(description.contains("receipt_id: string;"));
        assert!(description.contains("structuredContent: { answer: string; };"));
        assert!(!description.contains("CallToolResult"));
        assert!(!description.contains("unresolved $ref"));
    }

    #[test]
    fn pragma_data_errors_do_not_blame_an_unrelated_field() {
        let error = parse_exec_source("// @exec: {\"yield_time_ms\": -1}\ntext('hi')").unwrap_err();
        assert!(error.contains("invalid field value"));
        assert!(error.contains("-1"));
        assert!(!error.contains("max_output_tokens"));
        let error = parse_exec_source(
            "// @exec: {\"max_output_tokens\": 1, \"max_output_tokens\": 2}\ntext('hi')",
        )
        .unwrap_err();
        assert!(error.contains("duplicate field"));
        assert!(!error.contains("must be a non-negative safe integer"));
    }

    #[test]
    fn direct_only_patch_routing_and_output_pragma_are_usable() {
        let description = build_exec_tool_description(false, false, &["apply_patch".to_string()]);
        assert!(description.contains("nested when registered, otherwise direct"));
        assert!(description.contains("Direct-only tools omitted from `ALL_TOOLS`: `apply_patch`"));
        let directive = description
            .split("first-line `")
            .nth(1)
            .unwrap()
            .split('`')
            .next()
            .unwrap();
        assert_eq!(
            parse_exec_source(&format!("{directive}\ntext('hi')"))
                .unwrap()
                .max_output_tokens,
            Some(10000)
        );
    }

    #[test]
    fn declarations_bound_acyclic_reference_expansion_and_large_literals() {
        let mut defs = serde_json::Map::new();
        defs.insert("L0".to_string(), json!({"type": "string"}));
        for level in 1..=20 {
            let reference = format!("#/$defs/L{}", level - 1);
            defs.insert(
                format!("L{level}"),
                json!({"type": "object", "properties": {
                    "left": {"$ref": reference}, "right": {"$ref": reference}
                }}),
            );
        }
        let incomplete = "unknown /* schema projection incomplete: rendering limit reached */";
        assert_input_declaration(json!({"$defs": defs, "$ref": "#/$defs/L20"}), incomplete);
        assert_input_declaration(json!({"const": "x".repeat(200_000)}), incomplete);
        let mut deep = json!({"type": "string"});
        for _ in 0..70 {
            deep = json!({"type": "array", "items": deep});
        }
        assert_input_declaration(deep, incomplete);
        // Exhaustion must not poison subsequent independent renders.
        assert_input_declaration(json!({"type": "string"}), "string");
    }

    #[test]
    fn declarations_preserve_reviewed_schema_counterexamples() {
        for (schema, expected) in [
            (
                json!({"$defs": {"a b": {"type": "integer"}, "a%20b": {"type": "boolean"}}, "$ref": "#/$defs/a%20b"}),
                "number /* integer */",
            ),
            (
                json!({"type": "object", "additionalProperties": false}),
                "Record<string, never>",
            ),
            (
                json!({"const": "ok", "required": ["id"]}),
                "(string | number | boolean | null | unknown[] | { id: unknown; [key: string]: unknown; }) & (\"ok\")",
            ),
            (
                json!({"items": false}),
                "string | number | boolean | null | Array<never> | { [key: string]: unknown; }",
            ),
            (
                json!({"required": ["id"], "items": false}),
                "string | number | boolean | null | Array<never> | { id: unknown; [key: string]: unknown; }",
            ),
            (
                json!({"type": "string", "pattern": "^[a-z]+$", "minLength": 1, "maxLength": 5}),
                "string /* pattern: \"^[a-z]+$\"; minLength: 1; maxLength: 5 */",
            ),
            (
                json!({"const": "ok", "type": "string", "pattern": "*/\n"}),
                r#""ok" /* pattern: "* /\n" */"#,
            ),
            (
                json!({"oneOf": [{"type": "number"}, {"type": "number"}]}),
                "number | number /* oneOf: exactly one branch must match */",
            ),
            (
                json!({"type": "number", "not": {"const": 0}}),
                "number /* unprojected keyword: not */",
            ),
        ] {
            assert_input_declaration(schema, expected);
        }
    }

    #[test]
    fn declarations_do_not_resolve_fragments_across_nested_resource_boundaries() {
        let resource = json!({
            "$id": "https://example.invalid/inner", "$defs": {"Value": {"type": "integer"}},
            "$ref": "#/$defs/Value", "properties": {"value": {"$ref": "#/$defs/Value"}}
        });
        let incomplete =
            "unknown /* schema projection incomplete: nested $id resource not projected */";
        for reference in ["#/$defs/inner", "#/$defs/inner/properties/value"] {
            assert_input_declaration(
                json!({
                    "$defs": {"Value": {"type": "string"}, "inner": resource}, "$ref": reference
                }),
                incomplete,
            );
        }
        assert_input_declaration(
            json!({
                "$defs": {"Value": {"type": "string"}}, "type": "object",
                "properties": {"inner": resource}, "required": ["inner"]
            }),
            &format!("{{ inner: {incomplete}; }}"),
        );
    }

    #[test]
    fn declarations_bound_reference_work_and_programmatically_constructed_literals() {
        let incomplete = "unknown /* schema projection incomplete: rendering limit reached */";
        assert_input_declaration(
            json!({"$ref": format!("#/{}", "x".repeat(200_000))}),
            incomplete,
        );
        assert_input_declaration(
            json!({"type": "string", "pattern": "x".repeat(200_000)}),
            incomplete,
        );
        let mut literal = json!(0);
        for _ in 0..70 {
            literal = json!([literal]);
        }
        assert_input_declaration(json!({"const": literal}), incomplete);
        assert_input_declaration(json!({"const": vec![0; 1024]}), incomplete);
        assert_input_declaration(json!({"const": [1, {"ok": true}]}), r#"[1,{"ok":true}]"#);
    }

    #[test]
    fn augmentation_preserves_exec_and_wait_definitions() {
        for name in ["exec", "wait"] {
            let definition = ToolDefinition {
                name: name.to_string(),
                tool_name: ToolName::plain(name),
                description: "Direct tool.".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: Some(json!({"type": "string"})),
                default_timeout_ms: None,
                output_schema: None,
            };
            assert_eq!(augment_tool_definition(definition.clone()), definition);
        }
    }
}
