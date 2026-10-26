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
    /// Explicit producer-owned shared guidance, never inferred from prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_description: Option<std::sync::Arc<str>>,
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

/// Stable historical dispatch alias. Keep it callable even when a catalog
/// presents a shorter collision-checked name to the model.
pub fn legacy_code_mode_identifier(tool_name: &ToolName) -> String {
    use sha2::Digest;
    let raw = tool_name.namespace.as_deref().map_or_else(
        || tool_name.name.clone(), |namespace| format!("{namespace}__{}", tool_name.name),
    );
    let normalized = normalize_code_mode_identifier(&raw);
    if tool_name.namespace.is_none() && normalized == raw { return raw; }
    #[expect(clippy::expect_used, reason = "ToolName serializes only String and Option<String>; retain historical exact identity bytes")]
    let identity = serde_json::to_vec(tool_name).expect("tool identity serializes");
    format!("{normalized}__{}", hex::encode(sha2::Sha256::digest(identity)))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EnabledToolMetadata {
    pub tool_name: ToolName,
    pub global_name: String,
    #[serde(skip)]
    pub default_timeout_ms: Option<u64>,
    pub description: std::sync::Arc<str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace_description: Option<std::sync::Arc<str>>,
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
    if let Some(guidance) = definition.namespace_description.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        description = format!("{guidance}\n\n{description}");
    }
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
        (
            "input_schema",
            definition.input_schema.as_ref(),
            input_fragments.incomplete,
        ),
        (
            "output_schema",
            definition.output_schema.as_ref(),
            output_fragments.incomplete,
        ),
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
        render_code_mode_tool_declaration(
            &definition.name,
            input_name,
            input_type,
            output_type,
            false
        )
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
    let mut shared = std::collections::BTreeSet::new();
    for definition in definitions {
        let guidance = definition.namespace_description.as_deref().map(str::trim).filter(|text| !text.is_empty());
        if let Some(guidance) = guidance
            && shared.insert((definition.tool_name.namespace.as_deref(), guidance))
        {
            output.push_str(&format!("### Namespace {} instructions\n{guidance}\n\n", definition.tool_name.namespace.as_deref().unwrap_or("shared")));
        }
        let local = guidance.and_then(|guidance| definition.description.strip_prefix(guidance))
            .and_then(|rest| rest.strip_prefix("\n\n")).unwrap_or(&definition.description);
        if !local.trim().is_empty() {
            output.push_str(&format!(
                "### {}\n{}\n\n",
                definition.name,
                local.trim()
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
    for (index, (definition, (input_type, mut output_type))) in
        definitions.iter().zip(types).enumerate()
    {
        if fragments[index * 2].incomplete || fragments[index * 2 + 1].incomplete {
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
