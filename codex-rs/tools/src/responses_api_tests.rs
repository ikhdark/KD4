use super::LoadableToolSpec;
use super::ResponsesApiNamespace;
use super::ResponsesApiNamespaceTool;
use super::ResponsesApiTool;
use super::dynamic_tool_to_responses_api_tool;
use super::mcp_tool_to_deferred_responses_api_tool;
use super::tool_definition_to_responses_api_tool;
use crate::JsonSchema;
use crate::ToolDefinition;
use crate::ToolName;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn tool_definition_to_responses_api_tool_omits_false_defer_loading() {
    let tool = tool_definition_to_responses_api_tool(ToolDefinition {
        name: "lookup_order".to_string(),
        description: "Look up an order".to_string(),
        input_schema: JsonSchema::object(
            BTreeMap::from([(
                "order_id".to_string(),
                JsonSchema::string(/*description*/ None),
            )]),
            Some(vec!["order_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(json!({"type": "object"})),
        defer_loading: false,
    });
    assert_eq!(
        tool,
        ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::string(/*description*/ None),
                )]),
                Some(vec!["order_id".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(json!({"type": "object"})),
        }
    );
    assert_eq!(
        serde_json::to_value(tool).unwrap(),
        json!({
            "name":"lookup_order", "description":"Look up an order", "strict":false,
            "parameters":{"type":"object", "properties":{"order_id":{"type":"string"}}, "required":["order_id"], "additionalProperties":false}
        })
    );
}

#[test]
fn dynamic_tool_to_responses_api_tool_preserves_defer_loading() {
    let tool = DynamicToolFunctionSpec {
        name: "lookup_order".to_string(),
        description: "Look up an order".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "order_id": {"type": "string"}
            },
            "required": ["order_id"],
            "additionalProperties": false,
        }),
        defer_loading: true,
    };

    assert_eq!(
        dynamic_tool_to_responses_api_tool(&tool).expect("convert dynamic tool"),
        ResponsesApiTool {
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
                Some(false.into())
            ),
            output_schema: None,
        }
    );
}

#[test]
fn mcp_tool_to_deferred_responses_api_tool_sets_defer_loading() {
    let tool = rmcp::model::Tool::new(
        "lookup_order",
        "Look up an order",
        std::sync::Arc::new(rmcp::model::object(json!({
            "type": "object",
            "properties": {
                "order_id": {"type": "string"}
            },
            "required": ["order_id"],
            "additionalProperties": false,
        }))),
    );

    assert_eq!(
        mcp_tool_to_deferred_responses_api_tool(
            &ToolName::namespaced("mcp__codex_apps__", "lookup_order"),
            &tool,
        )
        .expect("convert deferred tool"),
        ResponsesApiTool {
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
                Some(false.into())
            ),
            output_schema: None,
        }
    );
}

#[test]
fn loadable_tool_spec_namespace_serializes_with_deferred_child_tools() {
    let namespace = LoadableToolSpec::Namespace(ResponsesApiNamespace {
        name: "mcp__codex_apps__calendar".to_string(),
        description: "Plan events".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: "create_event".to_string(),
            description: "Create a calendar event.".to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::object(
                Default::default(),
                /*required*/ None,
                /*additional_properties*/ None,
            ),
            output_schema: None,
        })],
    });

    let value = serde_json::to_value(namespace).expect("serialize namespace");

    assert_eq!(
        value,
        json!({
            "type": "namespace",
            "name": "mcp__codex_apps__calendar",
            "description": "Plan events",
            "tools": [
                {
                    "type": "function",
                    "name": "create_event",
                    "description": "Create a calendar event.",
                    "strict": false,
                    "defer_loading": true,
                    "parameters": {
                        "type": "object",
                        "properties": {}
                    }
                }
            ]
        })
    );
}

#[test]
fn coalescing_preserves_first_occurrence_and_child_order() {
    let tool = |name: &str| ResponsesApiTool {
        name: name.to_string(),
        description: name.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::default(),
        output_schema: None,
    };
    let namespace = |name: &str, description: &str, names: &[&str]| {
        LoadableToolSpec::Namespace(ResponsesApiNamespace {
            name: name.to_string(),
            description: description.to_string(),
            tools: names
                .iter()
                .map(|name| ResponsesApiNamespaceTool::Function(tool(name)))
                .collect(),
        })
    };
    assert_eq!(
        super::coalesce_loadable_tool_specs([
            LoadableToolSpec::Function(tool("plain1")),
            namespace("a", "first", &["a1"]),
            LoadableToolSpec::Function(tool("plain2")),
            namespace("b", "second", &["b1"]),
            namespace("a", "later", &["a2", "a3"]),
            namespace("b", "later", &["b2"]),
        ]),
        vec![
            LoadableToolSpec::Function(tool("plain1")),
            namespace("a", "first", &["a1", "a2", "a3"]),
            LoadableToolSpec::Function(tool("plain2")),
            namespace("b", "second", &["b1", "b2"]),
        ]
    );
}
