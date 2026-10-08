use crate::JsonSchema;
use crate::LoadableToolSpec;
use crate::ResponsesApiNamespaceTool;
use crate::ResponsesApiTool;
use crate::ToolSearchSourceInfo;
use crate::ToolSpec;
use crate::code_mode_name_for_tool_name;
use crate::default_namespace_description;
use std::sync::Arc;

#[derive(Clone, PartialEq)]
pub struct ToolSearchEntry {
    pub search_text: String,
    /// Operation-specific provider title, not connector or namespace prose.
    pub callable_title: Option<String>,
    pub tool_names: Vec<String>,
    pub output: Arc<LoadableToolSpec>,
    /// Shared MCP definitions are normalized only after selection; owned entries already are.
    pub normalize_on_selection: bool,
}

impl ToolSearchEntry {
    /// Positive evidence for one namespace member, without copying its schema.
    pub fn callable_function_search_text(tool: &ResponsesApiTool) -> String {
        let mut parts = String::new();
        append_function_search_text(tool, &mut parts, true);
        parts
    }

    /// Callable evidence excludes connector and namespace boilerplate. Shared
    /// metadata remains in search_text for discovery, not action activation.
    pub fn callable_search_text(&self) -> String {
        let mut parts = String::new();
        if let Some(title) = &self.callable_title {
            append_activation_description(title, &mut parts);
        }
        match self.output.as_ref() {
            LoadableToolSpec::Function(tool) => append_function_search_text(tool, &mut parts, true),
            LoadableToolSpec::Namespace(namespace) => {
                for tool in &namespace.tools {
                    let ResponsesApiNamespaceTool::Function(tool) = tool;
                    append_function_search_text(tool, &mut parts, true);
                }
            }
        }
        parts
    }

    pub fn to_loadable_spec(&self) -> LoadableToolSpec {
        self.normalize_output(self.output.as_ref().clone())
    }

    /// Normalize a selected callable without cloning unrelated namespace children.
    pub fn normalize_output(&self, mut output: LoadableToolSpec) -> LoadableToolSpec {
        if self.normalize_on_selection {
            match &mut output {
                LoadableToolSpec::Function(tool) => {
                    tool.defer_loading = Some(true);
                    tool.output_schema = None;
                }
                LoadableToolSpec::Namespace(namespace) => {
                    if namespace.description.trim().is_empty() {
                        namespace.description = default_namespace_description(&namespace.name);
                    }
                    for tool in &mut namespace.tools {
                        let ResponsesApiNamespaceTool::Function(tool) = tool;
                        tool.defer_loading = Some(true);
                        tool.output_schema = None;
                    }
                }
            }
        }
        output
    }
}

#[derive(Clone, PartialEq)]
pub struct ToolSearchInfo {
    pub entry: ToolSearchEntry,
    pub source_info: Option<ToolSearchSourceInfo>,
}

impl ToolSearchInfo {
    pub fn from_shared_spec(
        mut search_text: String,
        output: Arc<LoadableToolSpec>,
        source_info: Option<ToolSearchSourceInfo>,
    ) -> Self {
        let tool_names = output.callable_tool_names().into_iter().map(|name| name.name).collect();
        match output.as_ref() {
            LoadableToolSpec::Function(tool) => append_output_search_text(tool, &mut search_text),
            LoadableToolSpec::Namespace(namespace) => {
                for ResponsesApiNamespaceTool::Function(tool) in &namespace.tools {
                    append_output_search_text(tool, &mut search_text);
                }
            }
        }
        Self {
            entry: ToolSearchEntry { search_text, callable_title: None, tool_names, output, normalize_on_selection: true },
            source_info,
        }
    }

    pub fn from_tool_spec(
        spec: &ToolSpec,
        source_info: Option<ToolSearchSourceInfo>,
    ) -> Option<Self> {
        let search_text = default_tool_search_text(spec);
        Self::from_spec(search_text, spec, source_info)
    }

    pub fn from_spec(
        mut search_text: String,
        spec: &ToolSpec,
        source_info: Option<ToolSearchSourceInfo>,
    ) -> Option<Self> {
        let tool_names = tool_names(spec);
        let output = match spec {
            ToolSpec::Function(tool) => {
                append_output_search_text(tool, &mut search_text);
                LoadableToolSpec::Function(search_function(tool))
            }
            ToolSpec::Namespace(namespace) => {
                for ResponsesApiNamespaceTool::Function(tool) in &namespace.tools {
                    append_output_search_text(tool, &mut search_text);
                }
                LoadableToolSpec::Namespace(crate::ResponsesApiNamespace {
                    name: namespace.name.clone(),
                    description: if namespace.description.trim().is_empty() {
                        default_namespace_description(&namespace.name)
                    } else {
                        namespace.description.clone()
                    },
                    tools: namespace
                        .tools
                        .iter()
                        .map(|tool| {
                            let ResponsesApiNamespaceTool::Function(tool) = tool;
                            ResponsesApiNamespaceTool::Function(search_function(tool))
                        })
                        .collect(),
                })
            }
            ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } | ToolSpec::Freeform(_) => {
                return None;
            }
        };

        Some(Self {
            entry: ToolSearchEntry {
                search_text,
                callable_title: None,
                tool_names,
                output: Arc::new(output),
                normalize_on_selection: false,
            },
            source_info,
        })
    }
}

// Search results own their callable input contract, but never need a copy of
// the runtime-only output schema.
fn search_function(tool: &ResponsesApiTool) -> ResponsesApiTool {
    ResponsesApiTool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        strict: tool.strict,
        defer_loading: Some(true),
        parameters: tool.parameters.clone(),
        output_schema: None,
    }
}

fn tool_names(spec: &ToolSpec) -> Vec<String> {
    spec.callable_tool_names()
        .into_iter()
        .map(|tool_name| tool_name.name)
        .collect()
}

fn default_tool_search_text(spec: &ToolSpec) -> String {
    let mut parts = String::new();

    match spec {
        ToolSpec::Function(tool) => append_function_search_text(tool, &mut parts, false),
        ToolSpec::Namespace(namespace) => {
            for tool in &namespace.tools {
                let ResponsesApiNamespaceTool::Function(tool) = tool;
                push_search_part(&mut parts, &namespace_member_search_text(
                    &namespace.name, &namespace.description, tool,
                ));
            }
        }
        ToolSpec::ToolSearch { description, .. } => {
            push_search_part(&mut parts, description);
        }
        ToolSpec::WebSearch { .. } => {
            push_search_part(&mut parts, "web search");
        }
        ToolSpec::Freeform(tool) => {
            push_search_part(&mut parts, &tool.name);
            push_search_part(&mut parts, &tool.description);
            push_search_part(&mut parts, &tool.format.syntax);
        }
    }

    parts
}

pub fn namespace_member_search_text(
    namespace: &str,
    description: &str,
    tool: &ResponsesApiTool,
) -> String {
    let mut parts = String::new();
    push_search_part(&mut parts, namespace);
    push_search_part(&mut parts, description);
    push_search_part(&mut parts, &code_mode_name_for_tool_name(&crate::ToolName::namespaced(
        namespace, tool.name.clone(),
    )));
    append_function_search_text(tool, &mut parts, false);
    parts
}

fn append_function_search_text(tool: &ResponsesApiTool, parts: &mut String, activation: bool) {
    push_search_part(parts, &tool.name);
    push_search_part(parts, &identifier_search_words(&tool.name));
    append_description(&tool.description, parts, activation);
    append_schema_search_text(&tool.parameters, parts, activation);
}

fn append_description(description: &str, parts: &mut String, activation: bool) {
    if activation {
        append_activation_description(description, parts);
    } else {
        push_search_part(parts, description);
    }
}

/// Explicit negative clauses remain searchable and visible in the authoritative
/// contract, but are not positive capability evidence. This is deliberately not
/// a general natural-language classifier: preserve preceding positive clauses
/// and constructions such as "not only", and resume at the next sentence/clause.
fn append_activation_description(description: &str, parts: &mut String) {
    // Most descriptions contain no exclusion/routing cue. Check byte prefixes
    // without allocating, lowercasing Unicode, or splitting those descriptions.
    if !description.as_bytes().windows(3).any(|word| {
        match word[0].to_ascii_lowercase() {
            b'c' => word.eq_ignore_ascii_case(b"can"),
            b'd' => word.eq_ignore_ascii_case(b"don") || word.eq_ignore_ascii_case(b"doe"),
            b'n' => word.eq_ignore_ascii_case(b"nev") || word.eq_ignore_ascii_case(b"not"),
            b'i' => word.eq_ignore_ascii_case(b"ins"),
            _ => false,
        }
    }) {
        push_search_part(parts, description);
        return;
    }
    for clause in description.split_inclusive(['.', ';', '\n']) {
        let mut offset = 0;
        let mut previous = ("", 0);
        let mut positive_start = 0;
        let mut cutoff = clause.len();
        let mut first = "";
        for piece in clause.split_inclusive(char::is_whitespace) {
            let word = piece.trim();
            if !word.is_empty() {
                if cutoff < clause.len() && word.eq_ignore_ascii_case("but") {
                    push_search_part(parts, &clause[positive_start..cutoff]);
                    positive_start = offset + piece.len();
                    cutoff = clause.len();
                    first = "";
                    previous = ("", 0);
                    offset += piece.len();
                    continue;
                }
                if first.is_empty() { first = word; }
                let negative = match word.as_bytes()[0].to_ascii_lowercase() {
                    b'c' => word.eq_ignore_ascii_case("cannot") || word.eq_ignore_ascii_case("can't"),
                    b'd' => word.eq_ignore_ascii_case("doesn't") || word.eq_ignore_ascii_case("don't"),
                    b'n' => word.eq_ignore_ascii_case("never"),
                    _ => false,
                };
                let pair = word.eq_ignore_ascii_case("not")
                    && (previous.0.eq_ignore_ascii_case("do") || previous.0.eq_ignore_ascii_case("does"));
                if cutoff == clause.len() && (negative || pair) {
                    cutoff = if pair { previous.1 } else { offset };
                }
                if (first.eq_ignore_ascii_case("to") || first.eq_ignore_ascii_case("use"))
                    && word.trim_end_matches(['.', ';']).eq_ignore_ascii_case("instead") {
                    cutoff = positive_start; break;
                }
                previous = (word, offset);
            }
            offset += piece.len();
        }
        push_search_part(parts, &clause[positive_start..cutoff]);
    }
}

fn append_output_search_text(tool: &ResponsesApiTool, parts: &mut String) {
    let Some(schema) = &tool.output_schema else { return; };
    let start = parts.len();
    for name in schema.search_field_names().take(64) {
        // No descriptions, literals, recursive refs, or unbounded field names.
        if name.len() > 256 { continue; }
        let words = identifier_search_words(name);
        if parts.len() - start + name.len() + words.len() + 2 > 2048 { break; }
        push_search_part(parts, name);
        if words != name { push_search_part(parts, &words); }
    }
}

/// Capability words supplement, but never replace, exact callable identities.
pub fn identifier_search_words(identifier: &str) -> String {
    let chars = identifier.chars().collect::<Vec<_>>();
    let mut words = String::new();
    for (index, ch) in chars.iter().copied().enumerate() {
        if !ch.is_alphanumeric() {
            if !words.is_empty() && !words.ends_with(' ') {
                words.push(' ');
            }
            continue;
        }
        let previous = index.checked_sub(1).map(|index| chars[index]);
        let boundary = ch.is_uppercase() && previous.is_some_and(|previous| {
            previous.is_lowercase() || previous.is_numeric()
                || (previous.is_uppercase() && chars.get(index + 1).is_some_and(|next| next.is_lowercase()))
        });
        if boundary && !words.ends_with(' ') {
            words.push(' ');
        }
        words.extend(ch.to_lowercase());
    }
    words.trim_end().to_string()
}

pub fn schema_search_text(schema: &JsonSchema) -> String {
    let mut parts = String::new();
    append_schema_search_text(schema, &mut parts, false);
    parts
}

fn append_schema_search_text(schema: &JsonSchema, parts: &mut String, activation: bool) {
    if let Some(schema_ref) = &schema.schema_ref {
        push_search_part(parts, schema_ref);
    }
    if let Some(description) = &schema.description {
        append_description(description, parts, activation);
    }
    if let Some(required) = &schema.required {
        for name in required {
            append_schema_identifier(parts, name);
        }
    }
    if let Some(values) = &schema.enum_values {
        for value in values {
            append_json_search_text(value, parts);
        }
    }
    for value in [
        schema.minimum.as_ref(),
        schema.maximum.as_ref(),
        schema.exclusive_minimum.as_ref(),
        schema.exclusive_maximum.as_ref(),
        schema.multiple_of.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        push_search_part(parts, &value.to_string());
    }
    if let Some(properties) = &schema.properties {
        for (name, schema) in properties {
            append_schema_identifier(parts, name);
            append_schema_search_text(schema, parts, activation);
        }
    }
    if let Some(items) = &schema.items {
        append_schema_search_text(items, parts, activation);
    }
    if let Some(crate::AdditionalProperties::Schema(schema)) = &schema.additional_properties {
        append_schema_search_text(schema, parts, activation);
    }
    for variants in [&schema.any_of, &schema.one_of, &schema.all_of]
        .into_iter()
        .flatten()
    {
        for variant in variants {
            append_schema_search_text(variant, parts, activation);
        }
    }
    for definitions in [&schema.defs, &schema.definitions].into_iter().flatten() {
        for (name, schema) in definitions {
            append_schema_identifier(parts, name);
            append_schema_search_text(schema, parts, activation);
        }
    }
}

fn append_schema_identifier(parts: &mut String, name: &str) {
    push_search_part(parts, name);
    // Ordinary lowercase names already tokenize correctly; avoid duplicate text.
    if name.bytes().any(|byte| byte == b'_' || byte.is_ascii_uppercase()) {
        let words = identifier_search_words(name);
        if words != name { push_search_part(parts, &words); }
    }
}

fn append_json_search_text(value: &serde_json::Value, parts: &mut String) {
    match value {
        serde_json::Value::Null => push_search_part(parts, "null"),
        serde_json::Value::Bool(value) => push_search_part(parts, &value.to_string()),
        serde_json::Value::Number(value) => push_search_part(parts, &value.to_string()),
        serde_json::Value::String(value) => push_search_part(parts, value),
        serde_json::Value::Array(values) => {
            for value in values {
                append_json_search_text(value, parts);
            }
        }
        serde_json::Value::Object(values) => {
            for (name, value) in values {
                push_search_part(parts, name);
                append_json_search_text(value, parts);
            }
        }
    }
}

fn push_search_part(parts: &mut String, part: &str) {
    let part = part.trim();
    if !part.is_empty() {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(part);
    }
}

#[cfg(test)]
#[path = "tool_search_tests.rs"]
mod tests;
