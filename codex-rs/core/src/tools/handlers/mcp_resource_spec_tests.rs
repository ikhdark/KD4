use super::*;
use codex_tools::JsonSchema;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn list_mcp_resources_tool_matches_expected_spec() {
    assert_eq!(
        create_list_mcp_resources_tool(),
        ToolSpec::Function(ResponsesApiTool {
            name: "list_mcp_resources".to_string(),
            description: "Lists resources provided by MCP servers, such as files, database schemas, or application-specific information. Use a known relevant resource for the requested source and scope; resource listing is not a prerequisite for public web research or tool discovery. If an aggregate response includes `remainingServers`, those servers were omitted from the inline preview. Recover their retained entries with read_tool_output using the supplied artifact before listing them again. Use each server's cursor only to fetch pages not yet collected.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                    (
                        "server".to_string(),
                        JsonSchema::string(Some(
                                "MCP server name. Omit to list resources from every configured server."
                                    .to_string(),
                            ),),
                    ),
                    (
                        "cursor".to_string(),
                        JsonSchema::string(Some(
                                "Opaque cursor for an explicit single-server page. Omit to collect all bounded pages from the selected server or servers. A cursor requires `server`."
                                    .to_string(),
                            ),),
                    ),
                ]), /*required*/ None, Some(false.into())),
            output_schema: None,
        })
    );
}

#[test]
fn list_mcp_resource_templates_tool_matches_expected_spec() {
    assert_eq!(
        create_list_mcp_resource_templates_tool(),
        ToolSpec::Function(ResponsesApiTool {
            name: "list_mcp_resource_templates".to_string(),
            description: "Lists resource templates provided by MCP servers, such as parameterized files, database schemas, or application-specific information. Use a known relevant template for the requested source and scope; template listing is not a prerequisite for public web research or tool discovery. If an aggregate response includes `remainingServers`, those servers were omitted from the inline preview. Recover their retained entries with read_tool_output using the supplied artifact before listing them again. Use each server's cursor only to fetch pages not yet collected.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                    (
                        "server".to_string(),
                        JsonSchema::string(Some(
                                "MCP server name. Omit to list resource templates from every configured server."
                                    .to_string(),
                            ),),
                    ),
                    (
                        "cursor".to_string(),
                        JsonSchema::string(Some(
                                "Opaque cursor for an explicit single-server page. Omit to collect all bounded pages from the selected server or servers. A cursor requires `server`."
                                    .to_string(),
                            ),),
                    ),
                ]), /*required*/ None, Some(false.into())),
            output_schema: None,
        })
    );
}

#[test]
fn read_mcp_resource_tool_matches_expected_spec() {
    assert_eq!(
        create_read_mcp_resource_tool(),
        ToolSpec::Function(ResponsesApiTool {
            name: "read_mcp_resource".to_string(),
            description:
                "Read a specific authorized MCP resource directly when its server and URI are known from the user or retained evidence. List resources or templates only when discovery is necessary; a known template must be instantiated with every required variable."
                    .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                    (
                        "server".to_string(),
                        JsonSchema::string(Some(
                                "MCP server name exactly as configured, supplied by the user, retained evidence, or resource discovery. Listing is not required for a known identity."
                                    .to_string(),
                            ),),
                    ),
                    (
                        "uri".to_string(),
                        JsonSchema::string(Some(
                                "Resource URI supplied by the user, retained evidence, or resource discovery. For a known uriTemplate, fill every required variable to produce a concrete URI."
                                    .to_string(),
                            ),),
                    ),
                ]), Some(vec!["server".to_string(), "uri".to_string()]), Some(false.into())),
            output_schema: None,
        })
    );
}
