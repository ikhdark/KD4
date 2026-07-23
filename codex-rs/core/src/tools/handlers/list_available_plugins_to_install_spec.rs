use codex_tools::JsonSchema;
use codex_tools::LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME;
use codex_tools::REQUEST_PLUGIN_INSTALL_TOOL_NAME;
use codex_tools::ResponsesApiTool;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolSpec;
use serde_json::json;

fn list_available_plugins_to_install_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "tools": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "description": { "type": ["string", "null"] },
                        "tool_type": {
                            "type": "string",
                            "enum": ["plugin", "connector"]
                        },
                        "has_skills": { "type": "boolean" },
                        "mcp_server_names": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "app_connector_ids": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "required": [
                        "id",
                        "name",
                        "description",
                        "tool_type",
                        "has_skills",
                        "mcp_server_names",
                        "app_connector_ids"
                    ],
                    "additionalProperties": false
                }
            }
        },
        "required": ["tools"],
        "additionalProperties": false
    })
}

pub(crate) fn create_list_available_plugins_to_install_tool() -> ToolSpec {
    let description = format!(
        "# List plugin/connector install candidates\n\nUse this tool only when both are true:\n- The user explicitly asks to use a specific plugin or connector that is not already available in the current context or active `tools` list.\n- `{TOOL_SEARCH_TOOL_NAME}` is not available, or it has already been called and did not find or make the requested tool callable.\n\nReturns known plugins and connectors that can be passed to `{REQUEST_PLUGIN_INSTALL_TOOL_NAME}`. When both a plugin and a connector match, prefer the plugin; use the connector only when its corresponding plugin is already installed.\n"
    );

    ToolSpec::Function(ResponsesApiTool {
        name: LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME.to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(Default::default(), Some(Vec::new()), Some(false.into())),
        output_schema: Some(list_available_plugins_to_install_output_schema()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn create_list_available_plugins_to_install_tool_uses_expected_wire_shape() {
        let expected_output_schema = json!({
            "type": "object",
            "properties": {
                "tools": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "name": { "type": "string" },
                            "description": { "type": ["string", "null"] },
                            "tool_type": {
                                "type": "string",
                                "enum": ["plugin", "connector"]
                            },
                            "has_skills": { "type": "boolean" },
                            "mcp_server_names": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "app_connector_ids": {
                                "type": "array",
                                "items": { "type": "string" }
                            }
                        },
                        "required": [
                            "id",
                            "name",
                            "description",
                            "tool_type",
                            "has_skills",
                            "mcp_server_names",
                            "app_connector_ids"
                        ],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tools"],
            "additionalProperties": false
        });

        assert_eq!(
            create_list_available_plugins_to_install_tool(),
            ToolSpec::Function(ResponsesApiTool {
                name: "list_available_plugins_to_install".to_string(),
                description: "# List plugin/connector install candidates\n\nUse this tool only when both are true:\n- The user explicitly asks to use a specific plugin or connector that is not already available in the current context or active `tools` list.\n- `tool_search` is not available, or it has already been called and did not find or make the requested tool callable.\n\nReturns known plugins and connectors that can be passed to `request_plugin_install`. When both a plugin and a connector match, prefer the plugin; use the connector only when its corresponding plugin is already installed.\n".to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    Default::default(),
                    Some(Vec::new()),
                    Some(false.into()),
                ),
                output_schema: Some(expected_output_schema),
            })
        );
    }

    #[test]
    fn code_mode_declaration_exposes_structured_tools_result() {
        let ToolSpec::Function(tool) = codex_tools::augment_tool_spec_for_code_mode(
            create_list_available_plugins_to_install_tool(),
        ) else {
            panic!("expected function tool");
        };

        assert!(tool.description.contains("Promise<{ tools: Array<"));
        assert!(!tool.description.contains("Promise<unknown>"));
    }
}
