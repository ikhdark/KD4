use super::augment_tool_spec_for_code_mode;
use super::tool_spec_to_code_mode_tool_definition;
use crate::AdditionalProperties;
use crate::FreeformTool;
use crate::FreeformToolFormat;
use crate::JsonSchema;
use crate::ResponsesApiTool;
use crate::ToolName;
use crate::ToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn augment_tool_spec_for_code_mode_augments_function_tools() {
    assert_eq!(
        augment_tool_spec_for_code_mode(ToolSpec::Function(ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["order_id".to_string()]),
                Some(AdditionalProperties::Boolean(false))
            ),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": {"type": "boolean"}
                },
                "required": ["ok"],
            })),
        })),
        ToolSpec::Function(ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: r#"Look up an order

exec tool declaration:
```ts
declare const tools: { lookup_order(args: { order_id: string; }, options?: { timeout_ms?: number }): Promise<{ ok: boolean; }>; };
```"#
                .to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["order_id".to_string()]),
                Some(AdditionalProperties::Boolean(false))
            ),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": {"type": "boolean"}
                },
                "required": ["ok"],
            })),
        })
    );
}

#[test]
fn augment_tool_spec_for_code_mode_preserves_exec_tool_description() {
    assert_eq!(
        augment_tool_spec_for_code_mode(ToolSpec::Freeform(FreeformTool {
            name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
            description: "Run code".to_string(),
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: \"exec\"".to_string(),
            },
        })),
        ToolSpec::Freeform(FreeformTool {
            name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
            description: "Run code".to_string(),
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: \"exec\"".to_string(),
            },
        })
    );
}

#[test]
fn tool_spec_to_code_mode_tool_definition_returns_augmented_nested_tools() {
    let spec = ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description: "Apply a patch".to_string(),
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: "start: \"patch\"".to_string(),
        },
    });

    assert_eq!(
        tool_spec_to_code_mode_tool_definition(&spec),
        Some(codex_code_mode::ToolDefinition {
            name: "apply_patch".to_string(),
            tool_name: ToolName::plain("apply_patch"),
            description: r#"Apply a patch

exec tool declaration:
```ts
declare const tools: { apply_patch(input: string, options?: { timeout_ms?: number }): Promise<unknown>; };
```"#
                .to_string(),
            kind: codex_code_mode::CodeModeToolKind::Freeform,
            input_schema: None,
            output_schema: None,
        })
    );
}

#[test]
fn tool_search_code_mode_declaration_matches_structured_result_contract() {
    let definition = tool_spec_to_code_mode_tool_definition(&ToolSpec::ToolSearch {
        execution: "client".to_string(),
        description: "Search deferred tools.".to_string(),
        parameters: JsonSchema::object(
            BTreeMap::from([(
                "query".to_string(),
                JsonSchema::string(/*description*/ None),
            )]),
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
    })
    .expect("tool_search code-mode definition");

    assert_eq!(definition.name, "tool_search");
    assert!(definition.description.contains("Inspect `status`"));
    assert!(definition.description.contains("`aborted`"));
    assert!(
        definition
            .description
            .contains("type CodeModeToolSearchResult =")
    );
    assert!(
        definition
            .description
            .contains("status: \"completed\" | \"incomplete\" | \"aborted\";")
    );
    assert!(definition.description.contains("execution: \"client\";"));
    assert!(definition.description.contains("tools: unknown[];"));
    assert!(
        definition
            .description
            .contains("omitted_result_count: number | null;")
    );
    assert!(
        definition
            .description
            .contains("Promise<CodeModeToolSearchResult>")
    );
    assert_eq!(
        definition.output_schema,
        Some(super::code_mode_tool_search_output_schema())
    );
}

#[test]
fn tool_spec_to_code_mode_tool_definition_still_skips_web_search() {
    assert_eq!(
        tool_spec_to_code_mode_tool_definition(&ToolSpec::WebSearch {
            external_web_access: None,
            indexed_web_access: None,
            filters: None,
            user_location: None,
            search_context_size: None,
            search_content_types: None,
        }),
        None,
    );
}
