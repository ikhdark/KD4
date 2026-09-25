use crate::ToolDefinition;
use crate::ToolOutputSchema;
use crate::json_schema::parse_owned_tool_input_schema;
use serde_json::Value as JsonValue;
use serde_json::json;

#[tracing::instrument(level = "debug", skip(tool), fields(tool_name = %tool.name))]
pub fn parse_mcp_tool(tool: &rmcp::model::Tool) -> Result<ToolDefinition, serde_json::Error> {
    let mut serialized_input_schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());

    // OpenAI models mandate the "properties" field in the schema. Some MCP
    // servers omit it (or set it to null), so we insert an empty object to
    // match the behavior of the Agents SDK.
    if let serde_json::Value::Object(obj) = &mut serialized_input_schema
        && obj.get("properties").is_none_or(serde_json::Value::is_null)
    {
        obj.insert(
            "properties".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }

    let input_schema = parse_owned_tool_input_schema(serialized_input_schema)?;
    Ok(ToolDefinition {
        name: tool.name.to_string(),
        description: tool.description.clone().map(Into::into).unwrap_or_default(),
        input_schema,
        output_schema: Some(ToolOutputSchema::from_mcp_output_schema(
            tool.output_schema.clone(),
        )),
        defer_loading: false,
    })
}

pub fn mcp_call_tool_result_output_schema(structured_content_schema: JsonValue) -> JsonValue {
    let mut schema = json!({
        (codex_code_mode::MCP_RESULT_SCHEMA_MARKER): true,
        "type": "object",
        "properties": {
            "content": {
                "type": "array",
                "items": {
                    "type": "object"
                }
            },
            "structuredContent": null,
            "isError": {
                "type": "boolean"
            },
            "_meta": {
                "type": "object"
            }
        },
        "required": ["content"],
        "additionalProperties": false
    });
    // Move the payload without changing the envelope's property order or KD4 marker.
    schema["properties"]["structuredContent"] = structured_content_schema;
    schema
}

#[cfg(test)]
#[path = "mcp_tool_tests.rs"]
mod tests;
