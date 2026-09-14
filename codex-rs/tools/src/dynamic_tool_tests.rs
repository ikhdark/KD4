use super::parse_dynamic_tool;
use crate::JsonSchema;
use crate::ToolDefinition;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn parse_dynamic_tool_sanitizes_input_schema() {
    let tool = DynamicToolFunctionSpec {
        name: "lookup_ticket".to_string(),
        description: "Fetch a ticket".to_string(),
        input_schema: serde_json::json!({
            "properties": {
                "id": {
                    "description": "Ticket identifier"
                }
            }
        }),
        defer_loading: false,
    };

    assert_eq!(
        parse_dynamic_tool(&tool).expect("parse dynamic tool"),
        ToolDefinition {
            name: "lookup_ticket".to_string(),
            description: "Fetch a ticket".to_string(),
            input_schema: JsonSchema::object(
                BTreeMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        description: Some("Ticket identifier".to_string()),
                        ..Default::default()
                    },
                )]),
                /*required*/ None,
                /*additional_properties*/ None
            ),
            output_schema: None,
            defer_loading: false,
        }
    );
}

#[test]
fn parse_dynamic_tool_preserves_defer_loading() {
    let tool = DynamicToolFunctionSpec {
        name: "lookup_ticket".to_string(),
        description: "Fetch a ticket".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
        defer_loading: true,
    };

    assert_eq!(
        parse_dynamic_tool(&tool).expect("parse dynamic tool"),
        ToolDefinition {
            name: "lookup_ticket".to_string(),
            description: "Fetch a ticket".to_string(),
            input_schema: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ None
            ),
            output_schema: None,
            defer_loading: true,
        }
    );
}

#[test]
fn dynamic_tool_registration_preserves_argument_constraints_and_rejects_false_schema() {
    let mut tool = DynamicToolFunctionSpec {
        name: "choose".to_string(),
        description: "Choose a mode".to_string(),
        defer_loading: false,
        input_schema: serde_json::json!({"type":"object", "properties":{
            "mode":{"enum":[1,"auto"], "description":"Choose a mode"},
            "code":{"type":"string", "pattern":"^[0-9]{6}$", "minLength":6, "maxLength":6},
            "values":{"type":"array"}, "anything":true
        }}),
    };
    assert_eq!(
        serde_json::to_value(
            crate::dynamic_tool_to_responses_api_tool(&tool)
                .unwrap()
                .parameters
        )
        .unwrap(),
        serde_json::json!({"type":"object", "properties":{
            "mode":{"enum":[1,"auto"], "description":"Choose a mode"},
            "code":{"type":"string", "pattern":"^[0-9]{6}$", "minLength":6, "maxLength":6},
            "values":{"type":"array", "items":{}}, "anything":{}
        }})
    );
    tool.input_schema = serde_json::json!({"type":"object", "properties":{"blocked":false}});
    assert!(crate::dynamic_tool_to_responses_api_tool(&tool).is_err());
    let error =
        super::validate_dynamic_tools(&[codex_protocol::dynamic_tools::DynamicToolSpec::Function(
            tool,
        )])
        .unwrap_err();
    assert!(error.contains("dynamic tool input schema is not supported for choose"));
}
