use super::*;
use codex_tools::LoadableToolSpec;
use codex_tools::ToolSearchSourceInfo;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn search_info_uses_mcp_tool_metadata_and_parameter_names() {
    let handler = McpHandler::new(tool_info()).expect("MCP tool spec should build");
    let search_info = handler.search_info().expect("MCP search info");
    assert!(Arc::ptr_eq(&handler.spec, &search_info.entry.output));

    assert_eq!(
        search_info.entry.search_text,
        "mcp__calendar___create_event _create_event createEvent codex-apps create event Create event Create a calendar event. Calendar Plan events. Calendar plugin attendees start_time start time"
    );
    assert_eq!(
        search_info.source_info,
        Some(ToolSearchSourceInfo {
            name: "Calendar".to_string(),
            description: Some("Plan events.".to_string()),
        })
    );
}

#[test]
fn search_info_uses_connector_name_for_output_namespace_description() {
    let mut tool_info = tool_info();
    tool_info.namespace_description = None;
    let handler = McpHandler::new(tool_info).expect("MCP tool spec should build");
    let search_info = handler.search_info().expect("MCP search info");

    let LoadableToolSpec::Namespace(namespace) = search_info.entry.to_loadable_spec() else {
        panic!("expected namespace search output");
    };
    assert_eq!(namespace.description, "Tools for working with Calendar.");
    assert_eq!(
        search_info.source_info,
        Some(ToolSearchSourceInfo {
            name: "Calendar".to_string(),
            description: None,
        })
    );
}

#[test]
fn registered_contract_override_is_not_replaced_by_shared_mcp_spec() {
    let handler = McpHandler::new(tool_info()).unwrap();
    let ToolSpec::Namespace(mut namespace) = handler.spec() else {
        panic!("expected namespace");
    };
    namespace.description = "Shortened source description".to_string();
    let metadata_only = handler.search_info_for_registered_spec(&ToolSpec::Namespace(namespace.clone())).unwrap();
    assert_eq!(metadata_only.entry.callable_title.as_deref(), Some("Create event"));
    let ResponsesApiNamespaceTool::Function(tool) = &mut namespace.tools[0];
    tool.name = "overridden".to_string();
    tool.parameters = codex_tools::JsonSchema::string(Some("Override input".to_string()));
    let mut expected_namespace = namespace.clone();
    let ResponsesApiNamespaceTool::Function(expected_tool) = &mut expected_namespace.tools[0];
    expected_tool.defer_loading = Some(true);
    expected_tool.output_schema = None;
    let registered = ToolSpec::Namespace(namespace);
    let info = handler.search_info_for_registered_spec(&registered).unwrap();
    assert_eq!(info.entry.callable_title, None);
    assert!(!Arc::ptr_eq(&handler.spec, &info.entry.output));
    assert_eq!(info.entry.tool_names, vec!["overridden"]);
    assert_eq!(
        info.entry.to_loadable_spec(),
        LoadableToolSpec::Namespace(expected_namespace)
    );
}

#[test]
fn search_info_indexes_nested_schema_branches_and_definitions() {
    let mut info = tool_info();
    info.tool.input_schema = Arc::new(rmcp::model::object(json!({
        "type": "object",
        "properties": {
            "payload": {
                "oneOf": [
                    { "$ref": "#/$defs/batchRequest" },
                    { "type": "string", "enum": ["single-event"] }
                ]
            }
        },
        "$defs": {
            "batchRequest": {
                "type": "object",
                "description": "Bulk calendar import",
                "required": ["calendar_ids"],
                "properties": {
                    "calendar_ids": { "type": "array", "items": { "type": "string" } }
                }
            }
        }
    })));

    let handler = McpHandler::new(info).expect("MCP tool spec should build");
    let search_text = handler
        .search_info()
        .expect("MCP search info")
        .entry
        .search_text;

    for expected in [
        "#/$defs/batchRequest",
        "single-event",
        "batchRequest",
        "Bulk calendar import",
        "calendar_ids",
    ] {
        assert!(
            search_text.contains(expected),
            "missing `{expected}` from `{search_text}`"
        );
    }
}

fn tool_info() -> ToolInfo {
    ToolInfo {
        server_name: "codex-apps".to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: "_create_event".to_string(),
        callable_namespace: "mcp__calendar__".to_string(),
        namespace_description: Some("Plan events.".to_string()),
        tool: rmcp::model::Tool::new(
            "createEvent",
            "Create a calendar event.",
            Arc::new(rmcp::model::object(json!({
                "type": "object",
                "properties": {
                    "start_time": { "type": "string" },
                    "attendees": { "type": "string" }
                },
                "additionalProperties": false
            }))),
        )
        .with_title("Create event"),
        connector_id: None,
        connector_name: Some("Calendar".to_string()),
        plugin_display_names: vec![" Calendar plugin ".to_string(), " ".to_string()],
    }
}
