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
    /// Immutable rendered contract, shared by per-cell timeout overlays.
    pub description: std::sync::Arc<str>,
    pub kind: CodeModeToolKind,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
    /// Default for this tool only; an explicit per-call timeout takes precedence.
    /// Zero delegates deadline ownership to the tool (host policy only).
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
        definition.description = render_code_mode_sample_for_definition(&definition).into();
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
    pub description: std::sync::Arc<str>,
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
        render_code_mode_tool_declaration(tool_name, input_name, input_type, output_type, false)
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
    let mut description = definition.description.trim().to_string();
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
    // Lazy resolution already returns this authoritative description. Retain
    // JSON only for projections that lost structure; ordinary tools do not pay
    // for a second schema tree in every cell. Output recovery can page this text.
    for (label, schema, incomplete) in [
        ("input_schema", definition.input_schema.as_ref(), input_fragments.incomplete),
        ("output_schema", definition.output_schema.as_ref(), output_fragments.incomplete),
    ] {
        if let Some(schema) = schema
            && incomplete
        {
            description.push_str(&format!(
                "\n\nAuthoritative {label} (TypeScript projection incomplete):\n```json\n{schema}\n```"
            ));
        }
    }
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
                false,
            )
        );
        return format!("{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```");
    }
    let declaration = format!(
        "{preamble}declare const tools: {{ {} }};",
        render_code_mode_tool_declaration(&definition.name, input_name, input_type, output_type, false)
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
    for (index, (definition, (input_type, mut output_type))) in definitions.iter().zip(types).enumerate() {
        if fragments[index * 2].incomplete || fragments[index * 2 + 1].incomplete
        {
            output.push_str(&format!(
                "// Use resolve_tool({}) for its authoritative JSON schema; do not infer arguments from unknown.\n",
                JsonValue::String(definition.name.clone())
            ));
        }
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
            definitions.len() > 1,
        ));
    }
    if declarations.len() > 1 {
        output.push_str("type ToolCallOptions = { timeout_ms?: number };\n");
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
    shared_options: bool,
) -> String {
    let tool_name = normalize_code_mode_identifier(tool_name);
    let options_type = if shared_options {
        "ToolCallOptions"
    } else {
        "{ timeout_ms?: number }"
    };
    format!(
        "{tool_name}({input_name}: {input_type}, options?: {options_type}): Promise<{output_type}>;"
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
    fn incomplete_projections_retain_selective_authoritative_contracts() {
        let large = json!({"type":"object", "properties": {
            "payload": {"type":"string", "description": format!("{}Use seconds, only after approval.", "界".repeat(50_000))}
        }});
        let small = json!({"type":"object", "description":"Root prerequisite", "properties": {"count":{"type":"integer"}}});
        for (input, output, label) in [
            (large.clone(), small.clone(), "input_schema"),
            (small, large.clone(), "output_schema"),
        ] {
            let definition = ToolDefinition {
                name: "sample".into(), tool_name: ToolName::plain("sample"),
                description: "Operation".into(), kind: CodeModeToolKind::Function,
                input_schema: Some(input), output_schema: Some(output), default_timeout_ms: None,
            };
            let bundle = render_code_mode_tool_bundle(std::slice::from_ref(&definition));
            assert!(bundle.contains("resolve_tool(\"sample\")"));
            assert!(!bundle.contains("Authoritative"));
            assert!(bundle.contains("Root prerequisite"));
            let augmented = augment_tool_definition(definition);
            assert_eq!(augmented.description.matches("Authoritative ").count(), 1);
            let marker = format!("Authoritative {label} (TypeScript projection incomplete):\n```json\n");
            let json = augmented.description.split_once(&marker).unwrap().1.split_once("\n```").unwrap().0;
            assert_eq!(serde_json::from_str::<JsonValue>(json).unwrap(), large);
        }
    }

    #[test]
    fn verified10_guidance_survives_refs_items_and_branches() {
        let schema = json!({
            "type": "object", "properties": {
                "duration": {"$ref": "#/$defs/minutes"},
                "items": {"type": "array", "items": {"type": "string", "description": "Use canonical recipient IDs"}},
                "mode": {"oneOf": [
                    {"const": "archive", "description": "Archive only after confirmation"},
                    {"const": "read", "description": "Read without marking seen"}
                ]}
            },
            "$defs": {"minutes": {"type": "number", "description": "duration in minutes"}}
        });
        let definition = ToolDefinition {
            name: "guidance".into(), tool_name: ToolName::plain("guidance"),
            description: "".into(), kind: CodeModeToolKind::Function,
            input_schema: Some(schema), output_schema: None, default_timeout_ms: None,
        };
        let rendered = augment_tool_definition(definition.clone());
        let bundle = render_code_mode_tool_bundle(&[definition]);
        for text in [rendered.description.as_ref(), &bundle] {
            for guidance in ["duration in minutes", "Use canonical recipient IDs", "Archive only after confirmation", "Read without marking seen"] {
                assert_eq!(text.matches(guidance).count(), 1, "{text}");
            }
        }
        let duplicate = json!({"$ref":"#/$defs/value", "description":"minutes", "$defs":{"value":{"type":"number", "description":"minutes"}}});
        assert_eq!(super::super::schema_ts::render_json_schema_to_typescript(&duplicate).matches("minutes").count(), 1);
        let property = json!({"type":"object", "properties":{"duration":duplicate}, "$defs":{"value":{"type":"number", "description":"minutes"}}});
        assert_eq!(super::super::schema_ts::render_json_schema_to_typescript(&property).matches("minutes").count(), 1);
    }

    #[test]
    fn verified10_incomplete_projections_return_authoritative_contracts() {
        for schema in [
            json!({"$ref":"https://example.invalid/schema"}),
            json!({"$ref":"#"}),
            json!({"$ref":"#/$defs/missing"}),
            json!({"type":"object", "not":{"required":["unsafe"]}}),
            json!({"type":"object", "patternProperties":{"^x":{"type":"string"}}}),
            // JSON Schema validation constraints must remain recoverable even
            // when TypeScript can describe only the surrounding structure.
            json!({"type":"array", "uniqueItems":true}),
            json!({"type":"array", "contains":{"const":42}}),
            json!({"type":"array", "contains":{"type":"number"}, "minContains":2}),
            json!({"type":"array", "contains":{"type":"number"}, "maxContains":3}),
            json!({"type":"array", "prefixItems":[{"type":"string"}], "unevaluatedItems":false}),
            json!({"type":"object", "minProperties":1}),
            json!({"type":"object", "maxProperties":2}),
            json!({"type":"object", "propertyNames":{"pattern":"^[a-z]+$"}}),
            json!({"type":"object", "unevaluatedProperties":false}),
            json!({"type":"object", "dependentRequired":{"credit_card":["billing_address"]}}),
            json!({"type":"object", "dependentSchemas":{"credit_card":{"required":["billing_address"]}}}),
            json!({"type":"object", "dependencies":{"credit_card":["billing_address"]}}),
            json!({"type":"object", "properties":{"ids":{"type":"array", "uniqueItems":true}}}),
        ] {
            for output in [false, true] {
                let definition = ToolDefinition {
                    name: "incomplete".into(), tool_name: ToolName::plain("incomplete"),
                    description: "".into(), kind: CodeModeToolKind::Function,
                    input_schema: (!output).then(|| schema.clone()),
                    output_schema: output.then(|| schema.clone()), default_timeout_ms: None,
                };
                assert!(render_code_mode_tool_bundle(std::slice::from_ref(&definition)).contains("resolve_tool"));
                let rendered = augment_tool_definition(definition);
                let label = if output { "output_schema" } else { "input_schema" };
                let marker = format!("Authoritative {label} (TypeScript projection incomplete):\n```json\n");
                let recovered = rendered.description.split_once(&marker).unwrap().1.split_once("\n```").unwrap().0;
                assert_eq!(serde_json::from_str::<JsonValue>(recovered).unwrap(), schema);
            }
        }
    }

    #[test]
    fn bundle_shares_numeric_bounds_and_call_options_without_widening_inputs() {
        let definitions = (0..12).map(|index| ToolDefinition {
            name: format!("sample_{index}"),
            tool_name: ToolName::plain(format!("sample_{index}")),
            description: "".into(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {
                    "marker": {"const": index},
                    "offset": {"type": "integer", "minimum": 0, "maximum": 9007199254740991_u64},
                    "line": {"type": "integer", "minimum": 1, "maximum": 9007199254740991_u64}
                },
                "required": ["offset", "line"],
                "additionalProperties": false
            })),
            output_schema: None,
            default_timeout_ms: None,
        }).collect::<Vec<_>>();
        let bundle = render_code_mode_tool_bundle(&definitions);
        assert_eq!(bundle.matches("minimum: 0").count(), 1);
        assert_eq!(bundle.matches("minimum: 1").count(), 1);
        assert_eq!(bundle.matches("timeout_ms?: number").count(), 1);
        assert_eq!(bundle.matches("options?: ToolCallOptions").count(), 12);
        assert_eq!(bundle.matches("Promise<unknown>").count(), 12);

        let mut with_literal = definitions;
        with_literal[0].input_schema = Some(json!({"const": "options?: { timeout_ms?: number }"}));
        let bundle = render_code_mode_tool_bundle(&with_literal);
        assert!(bundle.contains(r#"args: "options?: { timeout_ms?: number }""#));
    }

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
            description: format!("Call {name}.").into(),
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
        for (name, alias) in [
            ("read_file", "CodeModeSelector"),
            ("read_output", "CodeModeSelector"),
            ("another_selector", "CodeModeSelector2"),
        ] {
            assert!(
                rendered.contains(&format!(
                    "{name}(args: {alias}, options?: ToolCallOptions): Promise<{alias}>;"
                )),
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
                description: "Answer.".into(),
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
                    ).into(),
                }
            );
        }
    }
}
