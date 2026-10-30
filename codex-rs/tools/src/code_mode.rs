use crate::ResponsesApiNamespaceTool;
use crate::ToolName;
use crate::ToolSpec;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ToolDefinition as CodeModeToolDefinition;

const CODE_MODE_TOOL_SEARCH_RESULT_GUIDANCE: &str = "In code mode, inspect `status` and `search_complete` before reusing a discovery result. `completed` is complete; `incomplete` remains retryable, including bounded matches or an unavailable catalog even with zero omitted contracts. `omitted_result_count` counts omitted contracts when known; `aborted` has a null count. Reuse complete results only for the same catalog version, query, limit, scope, and permissions. `direct_only_tools` overrides local resolution: these excluded names are activated for the next direct model interface, not this cell. Resolve returned or `activated_omitted_tools` names with `resolve_tool(name)`; repeat or refine discovery when enumeration is incomplete. A returned namespace tool outside `direct_only_tools` is callable as `resolve_tool(\"<namespace>.<name>\")`.";

pub fn code_mode_tool_search_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["completed", "incomplete", "aborted"]
            },
            "execution": { "const": "client" },
            "search_complete": { "type": "boolean" },
            "tools": { "type": "array" },
            "direct_only_tools": { "type": "array", "items": { "type": "string" } },
            "direct_only_guidance": { "type": "string" },
            "activated_omitted_tools": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Activated callable names whose schemas were omitted. Resolve these names without repeating the search."
            },
            "unactivated_matches": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Low-relevance names only, not activated. Refine the query or resolve an exact name."
            },
            "omitted_result_count": {
                "type": ["integer", "null"],
                "minimum": 0
            },
            "unmatched_identifiers": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Query identifiers that are not callable names; these did not restrict the search to a provider."
            },
            "resolution": {
                "type": "object",
                "properties": {
                    "helper": { "const": "resolve_tool" },
                    "argument": { "const": "exact activated_omitted_tools name" }
                },
                "required": ["helper", "argument"],
                "additionalProperties": false
            },
            "exact_name_ambiguity": {
                "type": "object",
                "properties": {
                    "match_count": { "type": "integer", "minimum": 2 },
                    "omitted_alternative_count": { "type": "integer", "minimum": 0 },
                    "qualified_alternatives": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["match_count", "omitted_alternative_count", "qualified_alternatives"],
                "additionalProperties": false
            }
        },
        "required": ["status", "execution", "tools", "omitted_result_count"],
        "additionalProperties": false
    })
}

/// Augment tool descriptions with code-mode-specific exec samples.
pub fn augment_tool_spec_for_code_mode(spec: ToolSpec) -> ToolSpec {
    let description = matches!(&spec, ToolSpec::Function(_) | ToolSpec::Freeform(_))
        .then(|| augmented_description_for_spec(&spec))
        .flatten();
    match spec {
        ToolSpec::Function(mut tool) => {
            let Some(description) = description else {
                return ToolSpec::Function(tool);
            };
            tool.description = description;
            ToolSpec::Function(tool)
        }
        ToolSpec::Freeform(mut tool) => {
            let Some(description) = description else {
                return ToolSpec::Freeform(tool);
            };
            tool.description = description;
            ToolSpec::Freeform(tool)
        }
        ToolSpec::Namespace(mut namespace) => {
            for tool in &mut namespace.tools {
                match tool {
                    ResponsesApiNamespaceTool::Function(tool) => {
                        let tool_name =
                            ToolName::namespaced(namespace.name.clone(), tool.name.clone());
                        let definition = CodeModeToolDefinition {
                            name: code_mode_name_for_tool_name(&tool_name),
                            tool_name,
                            description: tool.description.clone().into(),
                            kind: CodeModeToolKind::Function,
                            input_schema: serde_json::to_value(&tool.parameters).ok(),
                            namespace_description: None,
                            default_timeout_ms: None,
                            output_schema: tool
                                .output_schema
                                .as_ref()
                                .map(crate::ToolOutputSchema::to_value),
                        };
                        tool.description = codex_code_mode::augment_tool_definition(definition)
                            .description
                            .to_string();
                    }
                }
            }
            ToolSpec::Namespace(namespace)
        }
        other => other,
    }
}

/// Convert a supported nested tool spec into the code-mode runtime shape,
/// including the code-mode-specific description sample. Namespaces must contain
/// exactly one callable; use `collect_code_mode_tool_definitions` for catalogs.
pub fn tool_spec_to_code_mode_tool_definition(spec: &ToolSpec) -> Option<CodeModeToolDefinition> {
    let definition = code_mode_tool_definition_for_spec(spec)?;
    codex_code_mode::is_code_mode_nested_tool(&definition.name)
        .then(|| codex_code_mode::augment_tool_definition(definition))
}

pub fn collect_code_mode_tool_definitions<'a>(
    specs: impl IntoIterator<Item = &'a ToolSpec>,
) -> Vec<CodeModeToolDefinition> {
    collect_code_mode_tool_definitions_with_input_schemas(specs, &std::collections::HashMap::new())
}

pub fn collect_code_mode_tool_definitions_with_input_schemas<'a>(
    specs: impl IntoIterator<Item = &'a ToolSpec>,
    canonical_input_schemas: &std::collections::HashMap<ToolName, serde_json::Value>,
) -> Vec<CodeModeToolDefinition> {
    let mut tool_definitions = specs
        .into_iter()
        .flat_map(code_mode_tool_definitions_for_spec)
        .filter(|definition| codex_code_mode::is_code_mode_nested_tool(&definition.name))
        .collect::<Vec<_>>();
    tool_definitions.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.tool_name.cmp(&right.tool_name))
    });

    tool_definitions
        .into_iter()
        .map(|mut definition| {
            if let Some(schema) = canonical_input_schemas.get(&definition.tool_name) {
                definition.input_schema = Some(schema.clone());
            }
            codex_code_mode::augment_tool_definition(definition)
        })
        .collect()
}

fn augmented_description_for_spec(spec: &ToolSpec) -> Option<String> {
    code_mode_tool_definition_for_spec(spec)
        .map(codex_code_mode::augment_tool_definition)
        .map(|definition| definition.description.to_string())
}

pub fn code_mode_tool_definition_for_spec(spec: &ToolSpec) -> Option<CodeModeToolDefinition> {
    if matches!(spec, ToolSpec::Namespace(namespace) if namespace.tools.len() != 1) {
        return None;
    }
    code_mode_tool_definitions_for_spec(spec).into_iter().next()
}

fn code_mode_tool_definitions_for_spec(spec: &ToolSpec) -> Vec<CodeModeToolDefinition> {
    match spec {
        ToolSpec::Function(tool) => {
            let tool_name = ToolName::plain(tool.name.clone());
            let name = code_mode_name_for_tool_name(&tool_name);
            vec![CodeModeToolDefinition {
                tool_name,
                name,
                description: tool.description.clone().into(),
                kind: CodeModeToolKind::Function,
                input_schema: serde_json::to_value(&tool.parameters).ok(),
                namespace_description: None,
                default_timeout_ms: None,
                output_schema: tool
                    .output_schema
                    .as_ref()
                    .map(crate::ToolOutputSchema::to_value),
            }]
        }
        ToolSpec::Freeform(tool) => {
            let tool_name = ToolName::plain(tool.name.clone());
            let name = code_mode_name_for_tool_name(&tool_name);
            vec![CodeModeToolDefinition {
                tool_name,
                name,
                description: tool.description.clone().into(),
                kind: CodeModeToolKind::Freeform,
                input_schema: None,
                namespace_description: None,
                default_timeout_ms: None,
                output_schema: None,
            }]
        }
        ToolSpec::Namespace(namespace) => namespace
            .tools
            .iter()
            .zip(spec.callable_tool_names())
            .map(|(tool, tool_name)| match tool {
                ResponsesApiNamespaceTool::Function(tool) => CodeModeToolDefinition {
                    name: code_mode_name_for_tool_name(&tool_name),
                    tool_name,
                    description: tool.description.clone().into(),
                    namespace_description: (!namespace.description.trim().is_empty())
                        .then(|| namespace.description.trim().into()),
                    kind: CodeModeToolKind::Function,
                    input_schema: serde_json::to_value(&tool.parameters).ok(),
                    default_timeout_ms: None,
                    output_schema: tool
                        .output_schema
                        .as_ref()
                        .map(crate::ToolOutputSchema::to_value),
                },
            })
            .collect(),
        ToolSpec::ToolSearch {
            description,
            parameters,
            ..
        } => vec![CodeModeToolDefinition {
            tool_name: ToolName::plain("tool_search"),
            name: "tool_search".to_string(),
            description: format!(
                "{}\n\n{CODE_MODE_TOOL_SEARCH_RESULT_GUIDANCE}",
                description.trim()
            )
            .into(),
            kind: CodeModeToolKind::Function,
            input_schema: serde_json::to_value(parameters).ok(),
            namespace_description: None,
            default_timeout_ms: None,
            output_schema: Some(code_mode_tool_search_output_schema()),
        }],
        ToolSpec::WebSearch { .. } => Vec::new(),
    }
}

pub fn code_mode_name_for_tool_name(tool_name: &ToolName) -> String {
    codex_code_mode::legacy_code_mode_identifier(tool_name)
}
