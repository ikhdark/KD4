use super::mcp_call_tool_result_output_schema;
use super::parse_mcp_tool;
use crate::JsonSchema;
use crate::ToolDefinition;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn mcp_tool(name: &str, description: &str, input_schema: serde_json::Value) -> rmcp::model::Tool {
    rmcp::model::Tool::new(
        name.to_string(),
        description.to_string(),
        std::sync::Arc::new(rmcp::model::object(input_schema)),
    )
}

#[test]
fn parse_mcp_tool_inserts_empty_properties() {
    for input in [
        serde_json::json!({"type":"object"}),
        serde_json::json!({"type":"object", "properties":null}),
    ] {
        let tool = mcp_tool("no_props", "No properties", input);

        assert_eq!(
            parse_mcp_tool(&tool).expect("parse MCP tool"),
            ToolDefinition {
                name: "no_props".to_string(),
                description: "No properties".to_string(),
                input_schema: JsonSchema::object(
                    BTreeMap::new(),
                    /*required*/ None,
                    /*additional_properties*/ None
                ),
                output_schema: Some(serde_json::json!({
                    "x-codex-mcp-result":true, "type":"object",
                    "properties":{
                        "content":{"type":"array", "items":{"type":"object"}},
                        "structuredContent":{}, "isError":{"type":"boolean"}, "_meta":{"type":"object"}
                    }, "required":["content"], "additionalProperties":false
                }).into()),
                defer_loading: false,
            }
        );
    }
}

#[test]
fn parse_mcp_tool_preserves_top_level_output_schema() {
    let mut tool = mcp_tool(
        "with_output",
        "Has output schema",
        serde_json::json!({
            "type": "object"
        }),
    );
    tool.output_schema = Some(std::sync::Arc::new(rmcp::model::object(
        serde_json::json!({
            "properties": {
                "result": {
                    "properties": {
                        "nested": {}
                    }
                }
            },
            "required": ["result"]
        }),
    )));

    assert_eq!(
        parse_mcp_tool(&tool).expect("parse MCP tool"),
        ToolDefinition {
            name: "with_output".to_string(),
            description: "Has output schema".to_string(),
            input_schema: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ None
            ),
            output_schema: Some(mcp_call_tool_result_output_schema(serde_json::json!({
                "properties": {
                    "result": {
                        "properties": {
                            "nested": {}
                        }
                    }
                },
                "required": ["result"]
            })).into()),
            defer_loading: false,
        }
    );
}

#[test]
fn parse_mcp_tool_preserves_output_schema_without_inferred_type() {
    let mut tool = mcp_tool(
        "with_enum_output",
        "Has enum output schema",
        serde_json::json!({
            "type": "object"
        }),
    );
    tool.output_schema = Some(std::sync::Arc::new(rmcp::model::object(
        serde_json::json!({
            "enum": ["ok", "error"]
        }),
    )));

    assert_eq!(
        parse_mcp_tool(&tool).expect("parse MCP tool"),
        ToolDefinition {
            name: "with_enum_output".to_string(),
            description: "Has enum output schema".to_string(),
            input_schema: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ None
            ),
            output_schema: Some(mcp_call_tool_result_output_schema(serde_json::json!({
                "enum": ["ok", "error"]
            })).into()),
            defer_loading: false,
        }
    );
}

#[test]
fn mcp_registration_preserves_payload_reference_root_in_code_mode() {
    let mut tool = mcp_tool(
        "answer",
        "Get answer",
        serde_json::json!({"type": "object"}),
    );
    tool.output_schema = Some(std::sync::Arc::new(rmcp::model::object(
        serde_json::json!({
            "$defs": {"Payload": {
                "type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"]
            }},
            "$ref": "#/$defs/Payload"
        }),
    )));
    let parsed = parse_mcp_tool(&tool).unwrap();
    assert_eq!(
        parsed.output_schema.as_ref().unwrap().to_value()["x-codex-mcp-result"],
        true
    );
    let specs = [crate::ToolSpec::Function(crate::ResponsesApiTool {
        name: parsed.name,
        description: parsed.description,
        strict: false,
        defer_loading: None,
        parameters: parsed.input_schema,
        output_schema: parsed.output_schema,
    })];
    let definitions = crate::collect_code_mode_tool_definitions(&specs);
    assert_eq!(definitions.len(), 1);
    assert!(
        definitions[0]
            .description
            .contains("Promise<CallToolResult<{ answer: string; }>>")
    );
    assert!(!definitions[0].description.contains("unresolved $ref"));
}
