use codex_protocol::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use super::schema_ts::render_json_schema_to_typescript;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeModeToolKind {
    Function,
    Freeform,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub tool_name: ToolName,
    pub description: String,
    pub kind: CodeModeToolKind,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
}

pub fn is_code_mode_nested_tool(tool_name: &str) -> bool {
    tool_name != crate::PUBLIC_TOOL_NAME && tool_name != crate::WAIT_TOOL_NAME
}

pub fn normalize_code_mode_identifier(tool_key: &str) -> String {
    let mut identifier = String::new();

    for (index, ch) in tool_key.chars().enumerate() {
        let is_valid = if index == 0 {
            ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
        } else {
            ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
        };

        if is_valid {
            identifier.push(ch);
        } else {
            identifier.push('_');
        }
    }

    if identifier.is_empty() {
        "_".to_string()
    } else {
        identifier
    }
}

pub fn augment_tool_definition(mut definition: ToolDefinition) -> ToolDefinition {
    if is_code_mode_nested_tool(&definition.name) {
        definition.description = render_code_mode_sample_for_definition(&definition);
    }
    definition
}

pub fn enabled_tool_metadata(definition: &ToolDefinition) -> EnabledToolMetadata {
    EnabledToolMetadata {
        tool_name: definition.tool_name.clone(),
        global_name: normalize_code_mode_identifier(&definition.name),
        description: definition.description.clone(),
        kind: definition.kind,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EnabledToolMetadata {
    pub tool_name: ToolName,
    pub global_name: String,
    pub description: String,
    pub kind: CodeModeToolKind,
}

pub fn render_code_mode_sample(
    description: &str,
    tool_name: &str,
    input_name: &str,
    input_type: String,
    output_type: String,
) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_code_mode_tool_declaration(tool_name, input_name, input_type, output_type)
    );
    format!("{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```")
}

fn render_code_mode_sample_for_definition(definition: &ToolDefinition) -> String {
    let description = definition.description.trim().to_string();
    let input_name = match definition.kind {
        CodeModeToolKind::Function => "args",
        CodeModeToolKind::Freeform => "input",
    };
    let input_type = match definition.kind {
        CodeModeToolKind::Function => definition
            .input_schema
            .as_ref()
            .map(render_json_schema_to_typescript)
            .unwrap_or_else(|| "unknown".to_string()),
        CodeModeToolKind::Freeform => "string".to_string(),
    };
    let output_type = if let Some(structured_content_schema) =
        mcp_structured_content_schema(definition.output_schema.as_ref())
    {
        let structured_content_type = render_json_schema_to_typescript(structured_content_schema);
        if structured_content_type == "unknown" {
            "CallToolResult".to_string()
        } else {
            format!("CallToolResult<{structured_content_type}>")
        }
    } else {
        definition
            .output_schema
            .as_ref()
            .map(render_json_schema_to_typescript)
            .unwrap_or_else(|| "unknown".to_string())
    };
    if definition.name == "tool_search" {
        let declaration = format!(
            "type CodeModeToolSearchResult = {output_type};\ndeclare const tools: {{ {} }};",
            render_code_mode_tool_declaration(
                &definition.name,
                input_name,
                input_type,
                "CodeModeToolSearchResult".to_string(),
            )
        );
        return format!("{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```");
    }
    render_code_mode_sample(
        &description,
        &definition.name,
        input_name,
        input_type,
        output_type,
    )
}

fn render_code_mode_tool_declaration(
    tool_name: &str,
    input_name: &str,
    input_type: String,
    output_type: String,
) -> String {
    let tool_name = normalize_code_mode_identifier(tool_name);
    format!(
        "{tool_name}({input_name}: {input_type}, options?: {{ timeout_ms?: number }}): Promise<{output_type}>;"
    )
}

fn mcp_structured_content_schema(output_schema: Option<&JsonValue>) -> Option<&JsonValue> {
    let output_schema = output_schema?;
    // Registration, rather than coincidental property names, establishes this
    // envelope and the independent reference root of its embedded payload schema.
    // Ordinary output schemas use the general renderer with their enclosing root.
    if output_schema.get(crate::MCP_RESULT_SCHEMA_MARKER) != Some(&JsonValue::Bool(true)) {
        return None;
    }
    output_schema.get("properties")?.get("structuredContent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn envelope_selection_requires_a_boolean_registration_marker_and_payload() {
        let mut envelope = json!({
            "type": "object", "properties": {
                "content": {"type": "array", "items": {"type": "object"}},
                "isError": {"type": "boolean"}, "_meta": {"type": "object"},
                "structuredContent": {"type": "string"}
            }
        });
        assert_eq!(mcp_structured_content_schema(None), None);
        assert_eq!(mcp_structured_content_schema(Some(&envelope)), None);
        for marker in [json!(false), json!("true"), json!(1), JsonValue::Null] {
            envelope["x-codex-mcp-result"] = marker;
            assert_eq!(mcp_structured_content_schema(Some(&envelope)), None);
        }
        envelope["x-codex-mcp-result"] = json!(true);
        assert_eq!(
            mcp_structured_content_schema(Some(&envelope)),
            Some(&json!({"type": "string"}))
        );
        envelope["properties"]["structuredContent"] = json!(false);
        assert_eq!(
            mcp_structured_content_schema(Some(&envelope)),
            Some(&json!(false))
        );
        envelope["properties"]
            .as_object_mut()
            .unwrap()
            .remove("structuredContent");
        assert_eq!(mcp_structured_content_schema(Some(&envelope)), None);
    }

    #[test]
    fn metadata_reference_root_changes_only_for_a_registered_embedded_schema() {
        for (marker, expected_output) in [
            (false, "{ receipt: string; structuredContent: boolean; }"),
            (true, "CallToolResult<string>"),
        ] {
            // The same definition name deliberately has incompatible types in
            // the envelope and independent server schema, exposing wrong roots.
            let schema = json!({
                "x-codex-mcp-result": marker,
                "$defs": {"Value": {"type": "boolean"}},
                "type": "object", "required": ["receipt", "structuredContent"],
                "properties": {
                    "receipt": {"type": "string"},
                    "structuredContent": {
                        "$defs": {"Value": {"type": "string"}}, "$ref": "#/$defs/Value"
                    }
                }
            });
            let definition = ToolDefinition {
                name: "answer".to_string(),
                tool_name: ToolName::plain("answer"),
                description: "Answer.".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                output_schema: Some(schema.clone()),
            };
            // Exercise serialization and the metadata boundary used by discovery.
            let decoded =
                serde_json::from_value(serde_json::to_value(definition).unwrap()).unwrap();
            let augmented = augment_tool_definition(decoded);
            assert_eq!(augmented.output_schema, Some(schema));
            assert_eq!(
                enabled_tool_metadata(&augmented),
                EnabledToolMetadata {
                    tool_name: ToolName::plain("answer"),
                    global_name: "answer".to_string(),
                    kind: CodeModeToolKind::Function,
                    description: format!(
                        "Answer.\n\nexec tool declaration:\n```ts\ndeclare const tools: {{ answer(args: unknown, options?: {{ timeout_ms?: number }}): Promise<{expected_output}>; }};\n```"
                    ),
                }
            );
        }
    }
}
