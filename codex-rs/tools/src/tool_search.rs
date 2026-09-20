use crate::JsonSchema;
use crate::LoadableToolSpec;
use crate::ResponsesApiNamespaceTool;
use crate::ResponsesApiTool;
use crate::ToolSearchSourceInfo;
use crate::ToolSpec;
use crate::code_mode_name_for_tool_name;
use crate::default_namespace_description;

#[derive(Clone, PartialEq)]
pub struct ToolSearchEntry {
    pub search_text: String,
    pub tool_names: Vec<String>,
    pub output: LoadableToolSpec,
}

#[derive(Clone, PartialEq)]
pub struct ToolSearchInfo {
    pub entry: ToolSearchEntry,
    pub source_info: Option<ToolSearchSourceInfo>,
}

impl ToolSearchInfo {
    pub fn from_tool_spec(
        spec: &ToolSpec,
        source_info: Option<ToolSearchSourceInfo>,
    ) -> Option<Self> {
        let search_text = default_tool_search_text(spec);
        Self::from_spec(search_text, spec, source_info)
    }

    pub fn from_spec(
        search_text: String,
        spec: &ToolSpec,
        source_info: Option<ToolSearchSourceInfo>,
    ) -> Option<Self> {
        let tool_names = tool_names(spec);
        let output = match spec {
            ToolSpec::Function(tool) => LoadableToolSpec::Function(search_function(tool)),
            ToolSpec::Namespace(namespace) => {
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
                tool_names,
                output,
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
        ToolSpec::Function(tool) => append_function_search_text(tool, &mut parts),
        ToolSpec::Namespace(namespace) => {
            push_search_part(&mut parts, &namespace.name);
            push_search_part(&mut parts, &namespace.description);
            for tool in &namespace.tools {
                let ResponsesApiNamespaceTool::Function(tool) = tool;
                push_search_part(
                    &mut parts,
                    &code_mode_name_for_tool_name(&crate::ToolName::namespaced(
                        namespace.name.clone(),
                        tool.name.clone(),
                    )),
                );
                append_function_search_text(tool, &mut parts);
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

fn append_function_search_text(tool: &ResponsesApiTool, parts: &mut String) {
    push_search_part(parts, &tool.name);
    push_search_part(parts, &tool.name.replace('_', " "));
    push_search_part(parts, &tool.description);
    append_schema_search_text(&tool.parameters, parts);
}

pub fn schema_search_text(schema: &JsonSchema) -> String {
    let mut parts = String::new();
    append_schema_search_text(schema, &mut parts);
    parts
}

fn append_schema_search_text(schema: &JsonSchema, parts: &mut String) {
    if let Some(schema_ref) = &schema.schema_ref {
        push_search_part(parts, schema_ref);
    }
    if let Some(description) = &schema.description {
        push_search_part(parts, description);
    }
    if let Some(required) = &schema.required {
        for name in required {
            push_search_part(parts, name);
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
            push_search_part(parts, name);
            append_schema_search_text(schema, parts);
        }
    }
    if let Some(items) = &schema.items {
        append_schema_search_text(items, parts);
    }
    if let Some(crate::AdditionalProperties::Schema(schema)) = &schema.additional_properties {
        append_schema_search_text(schema, parts);
    }
    for variants in [&schema.any_of, &schema.one_of, &schema.all_of]
        .into_iter()
        .flatten()
    {
        for variant in variants {
            append_schema_search_text(variant, parts);
        }
    }
    for definitions in [&schema.defs, &schema.definitions].into_iter().flatten() {
        for (name, schema) in definitions {
            push_search_part(parts, name);
            append_schema_search_text(schema, parts);
        }
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
