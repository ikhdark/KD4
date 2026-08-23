use codex_protocol::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

use crate::PUBLIC_TOOL_NAME;

const MAX_JS_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
const DEFERRED_NESTED_TOOLS_GUIDANCE: &str = r#"Some deferred nested tools may be omitted from this description. They are still available on the global `tools` object and listed in `ALL_TOOLS`.
To find one, filter `ALL_TOOLS` by `name` and `description`."#;
const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript in a fresh V8 isolate. Input is JS, not JSON/Markdown; Node, filesystem, network, and console are unavailable.
- Nested tools are normalized `tools` methods (e.g. `await tools.exec_command(...)`) with documented I/O.
- Only tools listed in `ALL_TOOLS` are callable inside `exec`; direct-only tools stay outside.
- For a named symbol/config key, query its exact token and direct consumers first; search repo/project names only if unresolved, never in the same batch.
- Nested tool operations have a hard 60s default deadline; expiry cancels the operation. Only resume an observation poll when the tool returned a session or cell ID, and never duplicate a timed-out operation. Honor tool contracts. Await `notify` per settlement; use `allSettled`, never bare `Promise.all`. Sequence dependencies.
- Eight sampling passes per turn is an efficiency target, not a completion/validation cap. Required routing, safety, contract, test, or validation evidence overrides it for dependent or independent work.
- After deterministic failure, including a session poll, do not repeat the unchanged call; change route or relevant state.
- Keep evidence bounded: read relevant config/session tables or line ranges, never whole files; after truncation use a retained-artifact selector. Keep long commands in the same awaited evaluation; call `yield_control()` only for a new model decision.
- Optional first line: `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 10000}`. `yield_time_ms` is compatibility-only and does not extend nested-tool deadlines; owner waits wake on output or terminal state, never unchanged cells. Pass a nested tool's documented `{ timeout_ms }` option when it needs a longer bound. `max_output_tokens` defaults to 10000.
- When evaluation ends, unawaited work is discarded.

Global helpers:
- `exit()` ends successfully.
- `text(value: string | number | boolean | undefined | null)` appends text, JSON-stringifying non-strings when possible.
- `image(imageUrlOrItem: string | { image_url: string; detail?: "auto" | "low" | "high" | "original" | null } | { type: "image"; data: string; mimeType: string; _meta?: Record<string, unknown> }, detail?: "auto" | "low" | "high" | "original" | null)` appends a base64 image or MCP image; explicit detail wins.
- `audio(audioUrlOrItem: string | { audio_url: string } | { type: "audio"; data: string; mimeType: string })` appends a base64 `data:` audio URL or one MCP audio block.
- `generatedImage(result: { image_url: string; output_hint?: string })` appends generated output; HTTP(S) is unsupported.
- `store(key: string, value: any)` and `load(key: string)` persist serializable session values.
- `notify(value: string | number | boolean | undefined | null): Promise<void>` resolves after delivery; await it.
- `setTimeout(callback: () => void, delayMs?: number)` schedules work; await timers to keep exec alive. `clearTimeout(timeoutId?: number)` cancels one.
- `ALL_TOOLS` lists `{ name, description }`; `yield_control()` emits output while execution continues."#;
const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- Use `wait` only after `exec` returns `Script running with cell ID ...`.
- `cell_id` identifies the running `exec` cell to resume.
- `yield_time_ms` is retained for compatibility. Owner-held observations wake directly on output or terminal state; unchanged cells remain host-internal and do not cause a model-visible yield or a new model generation.
- `max_tokens` limits how much new output this wait call returns. Model projections default to a coherent packet of up to 10000 tokens; a lower requested value is honored and every result remains bounded by the model hard limit.
- `terminate: true` stops the running cell; false or omitted waits for output.
- `wait` returns only meaningful new output or state changes since the last model-visible result, or the final completion or termination result for that cell.
- New user steering or mailbox input interrupts a held wait without terminating a still-valid cell.
- If the cell has already finished, `wait` returns the completed result and closes the cell."#;
// Based off of https://modelcontextprotocol.io/specification/draft/schema#calltoolresult
const MCP_TYPESCRIPT_PREAMBLE: &str = r#"type ContentBlock = {
  type: "text" | "image" | "audio" | "resource_link" | "resource";
  [key: string]: unknown;
};
type CallToolResult<TStructured = { [key: string]: unknown }> = {
  _meta?: Record<string, unknown>;
  content: ContentBlock[];
  isError?: boolean;
  structuredContent?: TStructured;
  [key: string]: unknown;
};"#;

pub const CODE_MODE_PRAGMA_PREFIX: &str = "// @exec:";

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNamespaceDescription {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CodeModeExecPragma {
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedExecSource {
    pub code: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
}

pub fn parse_exec_source(input: &str) -> Result<ParsedExecSource, String> {
    if input.trim().is_empty() {
        return Err(
            "exec expects raw JavaScript source text (non-empty). Provide JS only, optionally with first-line `// @exec: {\"yield_time_ms\": 10000, \"max_output_tokens\": 10000}`.".to_string(),
        );
    }

    let mut args = ParsedExecSource {
        code: input.to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
    };

    let mut lines = input.splitn(2, '\n');
    let first_line = lines.next().unwrap_or_default();
    let rest = lines.next().unwrap_or_default();
    let trimmed = first_line.trim_start();
    let Some(pragma) = trimmed.strip_prefix(CODE_MODE_PRAGMA_PREFIX) else {
        return Ok(args);
    };

    if rest.trim().is_empty() {
        return Err(
            "exec pragma must be followed by JavaScript source on subsequent lines".to_string(),
        );
    }

    let directive = pragma.trim();
    if directive.is_empty() {
        return Err(
            "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
                .to_string(),
        );
    }

    let value: serde_json::Value = serde_json::from_str(directive).map_err(|err| {
        format!(
            "exec pragma must be valid JSON with supported fields `yield_time_ms` and `max_output_tokens`: {err}"
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
            .to_string()
    })?;
    for key in object.keys() {
        match key.as_str() {
            "yield_time_ms" | "max_output_tokens" => {}
            _ => {
                return Err(format!(
                    "exec pragma only supports `yield_time_ms` and `max_output_tokens`; got `{key}`"
                ));
            }
        }
    }

    let pragma: CodeModeExecPragma = serde_json::from_value(value).map_err(|err| {
        format!(
            "exec pragma fields `yield_time_ms` and `max_output_tokens` must be non-negative safe integers: {err}"
        )
    })?;
    if pragma
        .yield_time_ms
        .is_some_and(|yield_time_ms| yield_time_ms > MAX_JS_SAFE_INTEGER)
    {
        return Err(
            "exec pragma field `yield_time_ms` must be a non-negative safe integer".to_string(),
        );
    }
    if pragma.max_output_tokens.is_some_and(|max_output_tokens| {
        u64::try_from(max_output_tokens)
            .map(|max_output_tokens| max_output_tokens > MAX_JS_SAFE_INTEGER)
            .unwrap_or(true)
    }) {
        return Err(
            "exec pragma field `max_output_tokens` must be a non-negative safe integer".to_string(),
        );
    }

    args.code = rest.to_string();
    args.yield_time_ms = pragma.yield_time_ms;
    args.max_output_tokens = pragma.max_output_tokens;
    Ok(args)
}

pub fn is_code_mode_nested_tool(tool_name: &str) -> bool {
    tool_name != crate::PUBLIC_TOOL_NAME && tool_name != crate::WAIT_TOOL_NAME
}

pub fn build_exec_tool_description(
    enabled_tools: &[ToolDefinition],
    deferred_tools: &[ToolDefinition],
    namespace_descriptions: &BTreeMap<String, ToolNamespaceDescription>,
    code_mode_only: bool,
) -> String {
    build_exec_tool_description_with_direct_only_tools(
        enabled_tools,
        deferred_tools,
        namespace_descriptions,
        code_mode_only,
        &[],
    )
}

pub fn build_exec_tool_description_with_direct_only_tools(
    enabled_tools: &[ToolDefinition],
    deferred_tools: &[ToolDefinition],
    namespace_descriptions: &BTreeMap<String, ToolNamespaceDescription>,
    code_mode_only: bool,
    direct_only_tool_names: &[String],
) -> String {
    let mut sections = Vec::new();
    sections.push(EXEC_DESCRIPTION_TEMPLATE.to_string());
    if !direct_only_tool_names.is_empty() {
        let names = direct_only_tool_names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        sections.push(format!(
            "Direct-only tools omitted from `ALL_TOOLS`: {names}. Call these through their direct model tool interface, not through `exec`."
        ));
    }
    if !deferred_tools.is_empty() {
        sections.push(DEFERRED_NESTED_TOOLS_GUIDANCE.to_string());
    }
    if !code_mode_only {
        return sections.join("\n\n");
    }

    let has_mcp_tools = enabled_tools
        .iter()
        .any(|tool| mcp_structured_content_schema(tool.output_schema.as_ref()).is_some());
    if has_mcp_tools {
        sections.push(format!(
            "Shared MCP Types:\n```ts\n{MCP_TYPESCRIPT_PREAMBLE}\n```"
        ));
    }

    if !enabled_tools.is_empty() {
        let mut current_namespace: Option<&str> = None;
        let mut nested_tool_sections = Vec::with_capacity(enabled_tools.len());

        for tool in enabled_tools {
            let name = tool.name.as_str();
            let nested_description = render_compact_code_mode_sample_for_definition(tool);
            let namespace_description = tool
                .tool_name
                .namespace
                .as_ref()
                .and_then(|namespace| namespace_descriptions.get(namespace));
            let next_namespace = namespace_description
                .map(|namespace_description| namespace_description.name.as_str());
            if next_namespace != current_namespace {
                if let Some(namespace_description) = namespace_description {
                    let namespace_description_text = namespace_description.description.trim();
                    if !namespace_description_text.is_empty() {
                        nested_tool_sections.push(format!(
                            "## {}\n{namespace_description_text}",
                            namespace_description.name
                        ));
                    }
                }
                current_namespace = next_namespace;
            }

            let global_name = normalize_code_mode_identifier(name);
            let nested_description = nested_description.trim();
            if nested_description.is_empty() {
                nested_tool_sections.push(render_tool_heading(&global_name, name));
            } else {
                nested_tool_sections.push(format!(
                    "{}\n{nested_description}",
                    render_tool_heading(&global_name, name)
                ));
            }
        }

        let nested_tool_reference = nested_tool_sections.join("\n\n");
        sections.push(nested_tool_reference);
    }

    sections.join("\n\n")
}

pub fn build_wait_tool_description() -> &'static str {
    WAIT_DESCRIPTION_TEMPLATE
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
    if definition.name != PUBLIC_TOOL_NAME {
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
    render_code_mode_sample_for_definition_with_schema_comments(definition, true)
}

fn render_compact_code_mode_sample_for_definition(definition: &ToolDefinition) -> String {
    render_code_mode_sample_for_definition_with_schema_comments(definition, false)
}

fn render_code_mode_sample_for_definition_with_schema_comments(
    definition: &ToolDefinition,
    include_schema_comments: bool,
) -> String {
    let description = definition.description.trim().to_string();
    let input_name = match definition.kind {
        CodeModeToolKind::Function => "args",
        CodeModeToolKind::Freeform => "input",
    };
    let input_type = match definition.kind {
        CodeModeToolKind::Function => definition
            .input_schema
            .as_ref()
            .map(|schema| render_tool_schema(schema, include_schema_comments))
            .unwrap_or_else(|| "unknown".to_string()),
        CodeModeToolKind::Freeform => "string".to_string(),
    };
    let output_type = if let Some(structured_content_schema) =
        mcp_structured_content_schema(definition.output_schema.as_ref())
    {
        let structured_content_type =
            render_tool_schema(structured_content_schema, include_schema_comments);
        if structured_content_type == "unknown" {
            "CallToolResult".to_string()
        } else {
            format!("CallToolResult<{structured_content_type}>")
        }
    } else {
        definition
            .output_schema
            .as_ref()
            .map(|schema| render_tool_schema(schema, include_schema_comments))
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

fn render_tool_schema(schema: &JsonValue, include_schema_comments: bool) -> String {
    if include_schema_comments {
        return render_json_schema_to_typescript(schema);
    }
    render_json_schema_to_typescript(&schema_without_descriptions(schema))
}

fn schema_without_descriptions(schema: &JsonValue) -> JsonValue {
    match schema {
        JsonValue::Object(map) => JsonValue::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "description")
                .map(|(key, value)| {
                    let value = if matches!(
                        key.as_str(),
                        "properties"
                            | "patternProperties"
                            | "$defs"
                            | "definitions"
                            | "dependentSchemas"
                    ) {
                        match value {
                            JsonValue::Object(named_schemas) => JsonValue::Object(
                                named_schemas
                                    .iter()
                                    .map(|(name, schema)| {
                                        (name.clone(), schema_without_descriptions(schema))
                                    })
                                    .collect(),
                            ),
                            value => schema_without_descriptions(value),
                        }
                    } else {
                        schema_without_descriptions(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        JsonValue::Array(values) => {
            JsonValue::Array(values.iter().map(schema_without_descriptions).collect())
        }
        value => value.clone(),
    }
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

fn render_tool_heading(global_name: &str, raw_name: &str) -> String {
    if global_name == raw_name {
        format!("### `{global_name}`")
    } else {
        format!("### `{global_name}` (`{raw_name}`)")
    }
}

pub fn render_json_schema_to_typescript(schema: &JsonValue) -> String {
    render_json_schema_to_typescript_inner(schema)
}

fn mcp_structured_content_schema(output_schema: Option<&JsonValue>) -> Option<&JsonValue> {
    let output_schema = output_schema?;
    let properties = output_schema
        .get("properties")
        .and_then(JsonValue::as_object)?;
    let content_schema = properties.get("content").and_then(JsonValue::as_object)?;
    if content_schema.get("type").and_then(JsonValue::as_str) != Some("array") {
        return None;
    }

    if content_schema
        .get("items")
        .and_then(JsonValue::as_object)
        .is_none_or(|items| items.get("type").and_then(JsonValue::as_str) != Some("object"))
    {
        return None;
    }

    if properties
        .get("isError")
        .and_then(JsonValue::as_object)
        .is_none_or(|schema| schema.get("type").and_then(JsonValue::as_str) != Some("boolean"))
    {
        return None;
    }

    if properties
        .get("_meta")
        .and_then(JsonValue::as_object)
        .is_none_or(|schema| schema.get("type").and_then(JsonValue::as_str) != Some("object"))
    {
        return None;
    }

    Some(
        properties
            .get("structuredContent")
            .unwrap_or(&JsonValue::Bool(true)),
    )
}

fn render_json_schema_to_typescript_inner(schema: &JsonValue) -> String {
    match schema {
        JsonValue::Bool(true) => "unknown".to_string(),
        JsonValue::Bool(false) => "never".to_string(),
        JsonValue::Object(map) => {
            if let Some(value) = map.get("const") {
                return render_json_schema_literal(value);
            }

            if let Some(values) = map.get("enum").and_then(JsonValue::as_array) {
                let rendered = values
                    .iter()
                    .map(render_json_schema_literal)
                    .collect::<Vec<_>>();
                if !rendered.is_empty() {
                    return rendered.join(" | ");
                }
            }

            for key in ["anyOf", "oneOf"] {
                if let Some(variants) = map.get(key).and_then(JsonValue::as_array) {
                    let rendered = variants
                        .iter()
                        .map(render_json_schema_to_typescript_inner)
                        .collect::<Vec<_>>();
                    if !rendered.is_empty() {
                        return rendered.join(" | ");
                    }
                }
            }

            if let Some(variants) = map.get("allOf").and_then(JsonValue::as_array) {
                let rendered = variants
                    .iter()
                    .map(render_json_schema_to_typescript_inner)
                    .collect::<Vec<_>>();
                if !rendered.is_empty() {
                    return rendered.join(" & ");
                }
            }

            if let Some(schema_type) = map.get("type") {
                if let Some(types) = schema_type.as_array() {
                    let rendered = types
                        .iter()
                        .filter_map(JsonValue::as_str)
                        .map(|schema_type| render_json_schema_type_keyword(map, schema_type))
                        .collect::<Vec<_>>();
                    if !rendered.is_empty() {
                        return rendered.join(" | ");
                    }
                }

                if let Some(schema_type) = schema_type.as_str() {
                    return render_json_schema_type_keyword(map, schema_type);
                }
            }

            if map.contains_key("properties")
                || map.contains_key("additionalProperties")
                || map.contains_key("required")
            {
                return render_json_schema_object(map);
            }

            if map.contains_key("items") || map.contains_key("prefixItems") {
                return render_json_schema_array(map);
            }

            "unknown".to_string()
        }
        _ => "unknown".to_string(),
    }
}

fn render_json_schema_type_keyword(
    map: &serde_json::Map<String, JsonValue>,
    schema_type: &str,
) -> String {
    match schema_type {
        "string" => "string".to_string(),
        "number" | "integer" => "number".to_string(),
        "boolean" => "boolean".to_string(),
        "null" => "null".to_string(),
        "array" => render_json_schema_array(map),
        "object" => render_json_schema_object(map),
        _ => "unknown".to_string(),
    }
}

fn render_json_schema_array(map: &serde_json::Map<String, JsonValue>) -> String {
    if let Some(items) = map.get("items") {
        let item_type = render_json_schema_to_typescript_inner(items);
        return format!("Array<{item_type}>");
    }

    if let Some(items) = map.get("prefixItems").and_then(JsonValue::as_array) {
        let item_types = items
            .iter()
            .map(render_json_schema_to_typescript_inner)
            .collect::<Vec<_>>();
        if !item_types.is_empty() {
            return format!("[{}]", item_types.join(", "));
        }
    }

    "unknown[]".to_string()
}

fn append_additional_properties_line(
    lines: &mut Vec<String>,
    map: &serde_json::Map<String, JsonValue>,
    properties: &serde_json::Map<String, JsonValue>,
    line_prefix: &str,
) {
    if let Some(additional_properties) = map.get("additionalProperties") {
        let property_type = match additional_properties {
            JsonValue::Bool(true) => Some("unknown".to_string()),
            JsonValue::Bool(false) => None,
            value => Some(render_json_schema_to_typescript_inner(value)),
        };

        if let Some(property_type) = property_type {
            lines.push(format!("{line_prefix}[key: string]: {property_type};"));
        }
    } else if properties.is_empty() {
        lines.push(format!("{line_prefix}[key: string]: unknown;"));
    }
}

fn has_property_description(value: &JsonValue) -> bool {
    value
        .get("description")
        .and_then(JsonValue::as_str)
        .is_some_and(|description| !description.is_empty())
}

fn render_json_schema_object_property(name: &str, value: &JsonValue, required: &[&str]) -> String {
    let optional = if required.iter().any(|required_name| required_name == &name) {
        ""
    } else {
        "?"
    };
    let property_name = render_json_schema_property_name(name);
    let property_type = render_json_schema_to_typescript_inner(value);
    format!("{property_name}{optional}: {property_type};")
}

fn render_json_schema_object(map: &serde_json::Map<String, JsonValue>) -> String {
    let required = map
        .get("required")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let properties = map
        .get("properties")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();

    let mut sorted_properties = properties.iter().collect::<Vec<_>>();
    sorted_properties.sort_unstable_by_key(|(name_a, _)| *name_a);
    if sorted_properties
        .iter()
        .any(|(_, value)| has_property_description(value))
    {
        let mut lines = vec!["{".to_string()];
        for (name, value) in sorted_properties {
            if let Some(description) = value.get("description").and_then(JsonValue::as_str) {
                for description_line in description
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    lines.push(format!("  // {description_line}"));
                }
            }

            lines.push(format!(
                "  {}",
                render_json_schema_object_property(name, value, &required)
            ));
        }

        append_additional_properties_line(&mut lines, map, &properties, "  ");
        lines.push("}".to_string());
        return lines.join("\n");
    }

    let mut lines = sorted_properties
        .into_iter()
        .map(|(name, value)| render_json_schema_object_property(name, value, &required))
        .collect::<Vec<_>>();

    append_additional_properties_line(&mut lines, map, &properties, "");

    if lines.is_empty() {
        return "{}".to_string();
    }

    format!("{{ {} }}", lines.join(" "))
}

fn render_json_schema_property_name(name: &str) -> String {
    if normalize_code_mode_identifier(name) == name {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| format!("\"{}\"", name.replace('"', "\\\"")))
    }
}

fn render_json_schema_literal(value: &JsonValue) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::CodeModeToolKind;
    use super::EXEC_DESCRIPTION_TEMPLATE;
    use super::ParsedExecSource;
    use super::ToolDefinition;
    use super::ToolNamespaceDescription;
    use super::augment_tool_definition;
    use super::build_exec_tool_description;
    use super::build_wait_tool_description;
    use super::normalize_code_mode_identifier;
    use super::parse_exec_source;
    use codex_protocol::ToolName;
    use pretty_assertions::assert_eq;
    use serde_json::Value as JsonValue;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn mcp_call_tool_result_schema(structured_content_schema: JsonValue) -> JsonValue {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "array",
                    "items": {
                        "type": "object"
                    }
                },
                "structuredContent": structured_content_schema,
                "isError": { "type": "boolean" },
                "_meta": { "type": "object" }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    #[test]
    fn parse_exec_source_without_pragma() {
        assert_eq!(
            parse_exec_source("text('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn parse_exec_source_with_pragma() {
        assert_eq!(
            parse_exec_source("// @exec: {\"yield_time_ms\": 10}\ntext('hi')").unwrap(),
            ParsedExecSource {
                code: "text('hi')".to_string(),
                yield_time_ms: Some(10),
                max_output_tokens: None,
            }
        );
    }

    #[test]
    fn normalize_identifier_rewrites_invalid_characters() {
        assert_eq!(
            "mcp__ologs__get_profile",
            normalize_code_mode_identifier("mcp__ologs__get_profile")
        );
        assert_eq!(
            "hidden_dynamic_tool",
            normalize_code_mode_identifier("hidden-dynamic-tool")
        );
    }

    #[test]
    fn augment_tool_definition_appends_typed_declaration() {
        let definition = ToolDefinition {
            name: "hidden_dynamic_tool".to_string(),
            tool_name: ToolName::plain("hidden_dynamic_tool"),
            description: "Test tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"]
            })),
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains("declare const tools"));
        assert!(
            description.contains(
                "hidden_dynamic_tool(args: { city: string; }, options?: { timeout_ms?: number }): Promise<{ ok: boolean; }>;"
            )
        );
    }

    #[test]
    fn augment_tool_definition_includes_property_descriptions_as_comments() {
        let definition = ToolDefinition {
            name: "weather_tool".to_string(),
            tool_name: ToolName::plain("weather_tool"),
            description: "Weather tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {
                    "weather": {
                        "type": "array",
                        "description": "look up weather for a given list of locations",
                        "items": {
                            "type": "object",
                            "properties": {
                                "location": { "type": "string" }
                            },
                            "required": ["location"]
                        }
                    }
                },
                "required": ["weather"]
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "forecast": {
                        "type": "string",
                        "description": "human readable weather forecast"
                    }
                },
                "required": ["forecast"]
            })),
        };

        let description = augment_tool_definition(definition).description;
        assert!(description.contains(
            r#"weather_tool(args: {
  // look up weather for a given list of locations
  weather: Array<{ location: string; }>;
}, options?: { timeout_ms?: number }): Promise<{
  // human readable weather forecast
  forecast: string;
}>;"#
        ));
    }

    #[test]
    fn code_mode_only_description_includes_nested_tools() {
        let description = build_exec_tool_description(
            &[ToolDefinition {
                name: "foo".to_string(),
                tool_name: ToolName::plain("foo"),
                description: "bar".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                output_schema: None,
            }],
            &[],
            &BTreeMap::new(),
            /*code_mode_only*/ true,
        );
        assert!(description.contains(
            "### `foo`
bar"
        ));
        assert!(!description.contains("do not attempt to use any other tools directly"));
    }

    #[test]
    fn exec_description_mentions_timeout_helpers() {
        let description =
            build_exec_tool_description(&[], &[], &BTreeMap::new(), /*code_mode_only*/ false);
        assert!(description.contains("`setTimeout(callback: () => void, delayMs?: number)`"));
        assert!(description.contains("`clearTimeout(timeoutId?: number)`"));
    }

    #[test]
    fn rollout_workflow_guardrails_require_precise_bounded_discovery() {
        let description =
            build_exec_tool_description(&[], &[], &BTreeMap::new(), /*code_mode_only*/ false);

        assert!(description.contains("raw JavaScript"));
        assert!(description.contains("await tools.exec_command"));
        assert!(description.contains("yield_time_ms"));
        assert!(description.contains("max_output_tokens"));
        assert!(description.contains("type: \"image\""));
        assert!(description.contains("type: \"audio\""));
        assert!(description.contains("unawaited work is discarded"));
        assert!(description.contains("named symbol/config key"));
        assert!(description.contains("its exact token and direct consumers"));
        assert!(description.contains("search repo/project names only if unresolved"));
        assert!(description.contains("never in the same batch"));
        assert!(description.contains("hard 60s default deadline"));
        assert!(description.contains("expiry cancels the operation"));
        assert!(description.contains("Only resume an observation poll"));
        assert!(description.contains("returned a session or cell ID"));
        assert!(description.contains("never duplicate a timed-out operation"));
        assert!(!description.contains("never an operation"));
        assert!(description.contains("Honor tool contracts"));
        assert!(description.contains("Await `notify` per settlement"));
        assert!(description.contains("use `allSettled`"));
        assert!(description.contains("resolves after delivery; await it"));
        assert!(description.contains("never bare `Promise.all`"));
        assert!(description.contains("Eight sampling passes per turn"));
        assert!(description.contains("efficiency target, not a completion/validation cap"));
        assert!(
            description
                .contains("Required routing, safety, contract, test, or validation evidence")
        );
        assert!(description.contains("dependent or independent work"));
        assert!(description.contains("same awaited evaluation"));
        assert!(description.contains("read relevant config/session tables or line ranges"));
        assert!(description.contains("never whole files"));
        assert!(description.contains("after truncation use a retained-artifact selector"));
        assert!(!description.contains("including six small calls"));
        assert!(!description.contains("group 2-5 independent discovery calls"));
        assert!(
            description.contains("Only tools listed in `ALL_TOOLS` are callable inside `exec`")
        );
        assert!(description.contains("including a session poll"));
        assert!(description.contains("change route or relevant state"));
        assert!(description.contains("Sequence dependencies"));
        assert!(description.contains("Keep evidence bounded"));
        assert!(!description.contains("Shared MCP Types:"));
        assert!(!description.contains("type ImageContent ="));
        assert!(!description.contains("Model projections are capped"));
        const HISTORICAL_COMMON_EXEC_DESCRIPTION_BYTES: usize = 3_337;
        assert!(
            EXEC_DESCRIPTION_TEMPLATE.len() * 100 <= HISTORICAL_COMMON_EXEC_DESCRIPTION_BYTES * 92,
            "information-gain-aware batching guidance takes priority over marginal descriptor savings"
        );
    }

    #[test]
    fn default_manifest_preserves_tool_contract_and_omits_schema_comments() {
        let mandatory_tail = "mandatory safety and citation rules";
        let tool_description = format!("{}\n{mandatory_tail}", "d".repeat(1_250));
        let description = build_exec_tool_description(
            &[ToolDefinition {
                name: "sample_tool".to_string(),
                tool_name: ToolName::plain("sample_tool"),
                description: tool_description.clone(),
                kind: CodeModeToolKind::Function,
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "description": "schema-only property guidance"
                        },
                        "path": {
                            "type": "string",
                            "description": "schema-only guidance"
                        }
                    },
                    "required": ["description", "path"],
                    "additionalProperties": false
                })),
                output_schema: None,
            }],
            &[],
            &BTreeMap::new(),
            /*code_mode_only*/ true,
        );

        let section = description
            .split_once("### `sample_tool`\n")
            .expect("nested tool section")
            .1;
        let inline_description = section
            .split_once("\n\nexec tool declaration:")
            .expect("nested tool declaration")
            .0;
        assert_eq!(inline_description, tool_description);
        assert!(description.contains("description: string;"));
        assert!(!description.contains("schema-only guidance"));
        assert!(!description.contains("schema-only property guidance"));
        assert!(!description.contains("// schema-only guidance"));
    }

    #[test]
    fn code_mode_only_description_groups_namespace_instructions_once() {
        let namespace_descriptions = BTreeMap::from([(
            "mcp__sample__".to_string(),
            ToolNamespaceDescription {
                name: "mcp__sample".to_string(),
                description: "Shared namespace guidance.".to_string(),
            },
        )]);
        let description = build_exec_tool_description(
            &[
                ToolDefinition {
                    name: "mcp__sample__alpha".to_string(),
                    tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
                    description: "First tool".to_string(),
                    kind: CodeModeToolKind::Function,
                    input_schema: Some(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })),
                    output_schema: Some(mcp_call_tool_result_schema(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }))),
                },
                ToolDefinition {
                    name: "mcp__sample__beta".to_string(),
                    tool_name: ToolName::namespaced("mcp__sample__", "beta"),
                    description: "Second tool".to_string(),
                    kind: CodeModeToolKind::Function,
                    input_schema: Some(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    })),
                    output_schema: Some(mcp_call_tool_result_schema(json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }))),
                },
            ],
            &[],
            &namespace_descriptions,
            /*code_mode_only*/ true,
        );
        assert_eq!(description.matches("## mcp__sample").count(), 1);
        assert!(description.contains("## mcp__sample\nShared namespace guidance."));
        assert!(description.contains(
            "declare const tools: { mcp__sample__alpha(args: {}, options?: { timeout_ms?: number }): Promise<CallToolResult<{}>>; };"
        ));
        assert!(description.contains(
            "declare const tools: { mcp__sample__beta(args: {}, options?: { timeout_ms?: number }): Promise<CallToolResult<{}>>; };"
        ));
    }

    #[test]
    fn code_mode_only_description_omits_empty_namespace_sections() {
        let namespace_descriptions = BTreeMap::from([(
            "mcp__sample__".to_string(),
            ToolNamespaceDescription {
                name: "mcp__sample".to_string(),
                description: String::new(),
            },
        )]);
        let description = build_exec_tool_description(
            &[ToolDefinition {
                name: "mcp__sample__alpha".to_string(),
                tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
                description: "First tool".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })),
                output_schema: Some(mcp_call_tool_result_schema(json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }))),
            }],
            &[],
            &namespace_descriptions,
            /*code_mode_only*/ true,
        );

        assert!(!description.contains("## mcp__sample"));
        assert!(description.contains("### `mcp__sample__alpha`"));
    }

    #[test]
    fn code_mode_only_description_renders_shared_mcp_types_once() {
        let first_tool = augment_tool_definition(ToolDefinition {
            name: "mcp__sample__alpha".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
            description: "First tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "array",
                        "items": {
                            "type": "object"
                        }
                    },
                    "structuredContent": {
                        "type": "object",
                        "properties": {
                            "echo": { "type": "string" }
                        },
                        "required": ["echo"],
                        "additionalProperties": false
                    },
                    "isError": { "type": "boolean" },
                    "_meta": { "type": "object" }
                },
                "required": ["content"],
                "additionalProperties": false
            })),
        });
        let second_tool = augment_tool_definition(ToolDefinition {
            name: "mcp__sample__beta".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "beta"),
            description: "Second tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "array",
                        "items": {
                            "type": "object"
                        }
                    },
                    "structuredContent": {
                        "type": "object",
                        "properties": {
                            "count": { "type": "integer" }
                        },
                        "required": ["count"],
                        "additionalProperties": false
                    },
                    "isError": { "type": "boolean" },
                    "_meta": { "type": "object" }
                },
                "required": ["content"],
                "additionalProperties": false
            })),
        });

        let description = build_exec_tool_description(
            &[
                ToolDefinition {
                    name: first_tool.name,
                    tool_name: first_tool.tool_name,
                    description: "First tool".to_string(),
                    kind: first_tool.kind,
                    input_schema: first_tool.input_schema,
                    output_schema: first_tool.output_schema,
                },
                ToolDefinition {
                    name: second_tool.name,
                    tool_name: second_tool.tool_name,
                    description: "Second tool".to_string(),
                    kind: second_tool.kind,
                    input_schema: second_tool.input_schema,
                    output_schema: second_tool.output_schema,
                },
            ],
            &[],
            &BTreeMap::new(),
            /*code_mode_only*/ true,
        );

        assert_eq!(
            description
                .matches("type CallToolResult<TStructured = { [key: string]: unknown }>")
                .count(),
            1
        );
        assert_eq!(description.matches("Shared MCP Types:").count(), 1);
    }

    #[test]
    fn code_mode_only_description_defers_shared_mcp_types_with_deferred_tools() {
        let deferred_tool = ToolDefinition {
            name: "mcp__sample__alpha".to_string(),
            tool_name: ToolName::namespaced("mcp__sample__", "alpha"),
            description: "Deferred tool".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            output_schema: Some(mcp_call_tool_result_schema(json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }))),
        };

        let description = build_exec_tool_description(
            &[],
            &[deferred_tool],
            &BTreeMap::new(),
            /*code_mode_only*/ true,
        );

        assert!(description.contains("Some deferred nested tools may be omitted"));
        assert!(!description.contains("Shared MCP Types:"));
        assert!(!description.contains("### `mcp__sample__alpha`"));
    }

    #[test]
    fn exec_description_mentions_deferred_nested_tools_when_available() {
        let description = build_exec_tool_description(
            &[],
            &[ToolDefinition {
                name: "deferred_tool".to_string(),
                tool_name: ToolName::plain("deferred_tool"),
                description: "Deferred tool".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                output_schema: None,
            }],
            &BTreeMap::new(),
            /*code_mode_only*/ false,
        );

        assert!(description.contains("Some deferred nested tools may be omitted"));
        assert!(description.contains("filter `ALL_TOOLS` by `name` and `description`"));
        assert!(!description.contains("do not print the full `ALL_TOOLS` array"));
    }

    #[test]
    fn yield_time_descriptions_define_internal_cadence_without_changing_fields() {
        let exec = build_exec_tool_description(&[], &[], &BTreeMap::new(), false);
        let wait = build_wait_tool_description();

        assert!(exec.contains("`yield_time_ms` is compatibility-only"));
        assert!(exec.contains("does not extend nested-tool deadlines"));
        assert!(exec.contains("documented `{ timeout_ms }` option"));
        assert!(exec.contains("wake on output or terminal state"));
        assert!(exec.contains("never unchanged cells"));
        assert!(wait.contains("`yield_time_ms` is retained for compatibility"));
        assert!(wait.contains("wake directly on output or terminal state"));
        assert!(wait.contains("unchanged cells remain host-internal"));
    }
}
