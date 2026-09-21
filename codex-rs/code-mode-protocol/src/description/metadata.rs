use codex_protocol::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use super::schema_ts::SharedFragments;
use super::schema_ts::hoist_shared_fragments;
use super::schema_ts::render_json_schema_to_typescript_recording;

const MCP_RESULT_ENVELOPE: &str = "type CallToolResult<T = unknown> = { content: Array<Record<string, unknown>>; structuredContent?: T; isError?: boolean; _meta?: Record<string, unknown>; };\n";

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
    /// Default for this tool only; an explicit per-call timeout takes precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout_ms: Option<u64>,
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
        default_timeout_ms: definition.default_timeout_ms,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EnabledToolMetadata {
    pub tool_name: ToolName,
    pub global_name: String,
    #[serde(skip)]
    pub default_timeout_ms: Option<u64>,
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

struct RenderedToolTypes {
    input_type: String,
    output_type: String,
    input_fragments: SharedFragments,
    output_fragments: SharedFragments,
}

fn render_tool_types(definition: &ToolDefinition) -> RenderedToolTypes {
    let mut input_fragments = SharedFragments::default();
    let input_type = match definition.kind {
        CodeModeToolKind::Function => definition
            .input_schema
            .as_ref()
            .map(|schema| {
                let (rendered, fragments) = render_json_schema_to_typescript_recording(schema);
                input_fragments = fragments;
                rendered
            })
            .unwrap_or_else(|| "unknown".to_string()),
        CodeModeToolKind::Freeform => "string".to_string(),
    };
    let mut output_fragments = SharedFragments::default();
    let output_type = if let Some(structured_content_schema) =
        mcp_structured_content_schema(definition.output_schema.as_ref())
    {
        let (structured_content_type, fragments) =
            render_json_schema_to_typescript_recording(structured_content_schema);
        output_fragments = fragments;
        if structured_content_type == "unknown" {
            "CallToolResult".to_string()
        } else {
            format!("CallToolResult<{structured_content_type}>")
        }
    } else {
        definition
            .output_schema
            .as_ref()
            .map(|schema| {
                let (rendered, fragments) = render_json_schema_to_typescript_recording(schema);
                output_fragments = fragments;
                rendered
            })
            .unwrap_or_else(|| "unknown".to_string())
    };
    RenderedToolTypes {
        input_type,
        output_type,
        input_fragments,
        output_fragments,
    }
}

fn render_code_mode_sample_for_definition(definition: &ToolDefinition) -> String {
    let description = definition.description.trim();
    let input_name = match definition.kind {
        CodeModeToolKind::Function => "args",
        CodeModeToolKind::Freeform => "input",
    };
    let RenderedToolTypes {
        mut input_type,
        mut output_type,
        input_fragments,
        output_fragments,
    } = render_tool_types(definition);
    // One shape reached from several properties, or from both the argument and
    // result schemas, is described once and referenced by name afterwards.
    let aliases = hoist_shared_fragments(
        &[&input_fragments, &output_fragments],
        &mut [&mut input_type, &mut output_type],
    );
    let mut preamble = String::new();
    if mcp_structured_content_schema(definition.output_schema.as_ref()).is_some() {
        preamble.push_str(MCP_RESULT_ENVELOPE);
    }
    for alias in &aliases {
        preamble.push_str(alias);
        preamble.push('\n');
    }
    if definition.name == "tool_search" {
        let declaration = format!(
            "{preamble}type CodeModeToolSearchResult = {output_type};\ndeclare const tools: {{ {} }};",
            render_code_mode_tool_declaration(
                &definition.name,
                input_name,
                input_type,
                "CodeModeToolSearchResult".to_string(),
            )
        );
        return format!("{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```");
    }
    let declaration = format!(
        "{preamble}declare const tools: {{ {} }};",
        render_code_mode_tool_declaration(&definition.name, input_name, input_type, output_type)
    );
    format!("{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```")
}

/// Render one model-visible contract bundle. Shared shapes are named once across
/// tools as well as within each tool, and all aliases use the same name scope.
/// Individual lazy descriptions remain self-contained through augment_tool_definition.
pub fn render_code_mode_tool_bundle(definitions: &[ToolDefinition]) -> String {
    let mut types = Vec::with_capacity(definitions.len());
    let mut fragments = Vec::with_capacity(definitions.len() * 2);
    for definition in definitions {
        let rendered = render_tool_types(definition);
        types.push((rendered.input_type, rendered.output_type));
        fragments.extend([rendered.input_fragments, rendered.output_fragments]);
    }
    let fragment_refs = fragments.iter().collect::<Vec<_>>();
    let mut type_refs = types
        .iter_mut()
        .flat_map(|(input, output)| [input, output])
        .collect::<Vec<_>>();
    let aliases = hoist_shared_fragments(&fragment_refs, &mut type_refs);
    let mut output = String::new();
    for definition in definitions {
        if !definition.description.trim().is_empty() {
            output.push_str(&format!(
                "### {}\n{}\n\n",
                definition.name,
                definition.description.trim()
            ));
        }
    }
    output.push_str("exec tool declarations:\n```ts\n");
    if definitions.iter().any(|definition| {
        mcp_structured_content_schema(definition.output_schema.as_ref()).is_some()
    }) {
        output.push_str(MCP_RESULT_ENVELOPE);
    }
    for alias in aliases {
        output.push_str(&alias);
        output.push('\n');
    }
    let mut declarations = Vec::with_capacity(definitions.len());
    for (definition, (input_type, mut output_type)) in definitions.iter().zip(types) {
        if definition.name == "tool_search" {
            output.push_str(&format!("type CodeModeToolSearchResult = {output_type};\n"));
            output_type = "CodeModeToolSearchResult".to_string();
        }
        let input_name = match definition.kind {
            CodeModeToolKind::Function => "args",
            CodeModeToolKind::Freeform => "input",
        };
        declarations.push(render_code_mode_tool_declaration(
            &definition.name,
            input_name,
            input_type,
            output_type,
        ));
    }
    output.push_str("declare const tools: {\n");
    for declaration in declarations {
        output.push_str(&declaration);
        output.push('\n');
    }
    output.push_str("};\n```");
    output
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

    fn referenced_tool(name: &str, marker: &str) -> ToolDefinition {
        let schema = json!({
            "$defs": {"selector": {
                "type": "object",
                "properties": {
                    "kind": {"const": marker},
                    "start": {"type": "integer", "description": "The exact start coordinate of the selected source range."},
                    "end": {"type": "integer", "description": "The exact end coordinate of the selected source range."}
                },
                "required": ["kind", "start", "end"], "additionalProperties": false
            }},
            "$ref": "#/$defs/selector"
        });
        ToolDefinition {
            name: name.to_string(),
            tool_name: ToolName::plain(name),
            description: format!("Call {name}."),
            kind: CodeModeToolKind::Function,
            input_schema: Some(schema.clone()),
            output_schema: Some(schema),
            default_timeout_ms: None,
        }
    }

    #[test]
    fn bundled_contracts_share_shapes_across_tools_without_alias_collisions() {
        let definitions = [
            referenced_tool("read_file", "file"),
            referenced_tool("read_output", "file"),
            referenced_tool("another_selector", "different"),
        ];
        let rendered = render_code_mode_tool_bundle(&definitions);
        assert_eq!(rendered.matches("declare const tools").count(), 1);
        assert_eq!(rendered.matches("type CodeModeSelector = ").count(), 1);
        assert_eq!(rendered.matches("type CodeModeSelector2 = ").count(), 1);
        assert_eq!(rendered.matches("kind: \"file\";").count(), 1);
        assert_eq!(rendered.matches("kind: \"different\";").count(), 1);
        for name in ["read_file", "read_output", "another_selector"] {
            assert!(
                rendered.contains(&format!("{name}(args: CodeModeSelector")),
                "{rendered}"
            );
        }
        // Both argument and result must use the same alias for each independent
        // schema root; different roots sharing a $defs name cannot be conflated.
        let declarations = rendered.split("declare const tools").nth(1).unwrap();
        for declaration in declarations.lines().filter(|line| line.contains("(args:")) {
            let argument = declaration
                .split("args: ")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap();
            assert!(
                declaration.contains(&format!("Promise<{argument}>")),
                "{declaration}"
            );
        }
        let separate_bytes: usize = definitions
            .iter()
            .cloned()
            .map(augment_tool_definition)
            .map(|tool| tool.description.len())
            .sum();
        assert!(
            rendered.len() < separate_bytes,
            "bundling must reduce the actual emitted contract"
        );
    }

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
                default_timeout_ms: None,
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
                    default_timeout_ms: None,
                    kind: CodeModeToolKind::Function,
                    description: format!(
                        "Answer.\n\nexec tool declaration:\n```ts\n{envelope}declare const tools: {{ answer(args: unknown, options?: {{ timeout_ms?: number }}): Promise<{expected_output}>; }};\n```",
                        envelope = if marker { MCP_RESULT_ENVELOPE } else { "" },
                    ),
                }
            );
        }
    }
}
