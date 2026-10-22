use crate::JsonSchema;
use crate::ToolDefinition;
use crate::ToolName;
use crate::parse_dynamic_tool;
use crate::parse_mcp_tool;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FreeformTool {
    pub name: String,
    pub description: String,
    pub format: FreeformToolFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FreeformToolFormat {
    pub r#type: String,
    pub syntax: String,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponsesApiTool {
    pub name: String,
    pub description: String,
    /// TODO: Validation. When strict is set to true, the JSON schema,
    /// `required` and `additional_properties` must be present. All fields in
    /// `properties` must be present in `required`.
    pub strict: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    pub parameters: JsonSchema,
    #[serde(skip)]
    pub output_schema: Option<crate::ToolOutputSchema>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum LoadableToolSpec {
    #[allow(dead_code)]
    #[serde(rename = "function")]
    Function(ResponsesApiTool),
    #[serde(rename = "namespace")]
    Namespace(ResponsesApiNamespace),
}

impl LoadableToolSpec {
    /// Normalize persisted discovery output, including legacy flat functions.
    /// Unknown shapes must stay raw rather than become a complete name receipt.
    pub fn callable_names_from_value(value: &serde_json::Value) -> Option<Vec<String>> {
        let name = |value: &serde_json::Value| {
            value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("namespace") {
            let namespace = name(value)?;
            return value
                .get("tools")?
                .as_array()?
                .iter()
                .map(|tool| {
                    (tool.get("type").and_then(serde_json::Value::as_str) == Some("function"))
                        .then_some(())?;
                    Some(format!("{namespace}.{}", name(tool)?))
                })
                .collect();
        }
        if value.get("type").is_some_and(|kind| kind != "function") {
            return None;
        }
        let function = value.get("function").unwrap_or(value);
        let name = name(function)?;
        Some(vec![match value
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .filter(|namespace| !namespace.is_empty())
        {
            Some(namespace) => format!("{namespace}.{name}"),
            None => name,
        }])
    }

    /// Returns every callable name declared by this loadable specification.
    pub fn callable_tool_names(&self) -> Vec<ToolName> {
        match self {
            Self::Function(tool) => vec![ToolName::plain(tool.name.clone())],
            Self::Namespace(namespace) => namespace
                .tools
                .iter()
                .map(|tool| match tool {
                    ResponsesApiNamespaceTool::Function(tool) => {
                        ToolName::namespaced(namespace.name.clone(), tool.name.clone())
                    }
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponsesApiNamespace {
    pub name: String,
    pub description: String,
    pub tools: Vec<ResponsesApiNamespaceTool>,
}

pub fn default_namespace_description(namespace_name: &str) -> String {
    format!("Tools in the {namespace_name} namespace.")
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponsesApiNamespaceTool {
    #[serde(rename = "function")]
    Function(ResponsesApiTool),
}

pub fn dynamic_tool_to_responses_api_tool(
    tool: &DynamicToolFunctionSpec,
) -> Result<ResponsesApiTool, serde_json::Error> {
    Ok(tool_definition_to_responses_api_tool(parse_dynamic_tool(
        tool,
    )?))
}

pub fn mcp_tool_to_responses_api_tool(
    tool_name: &ToolName,
    tool: &rmcp::model::Tool,
) -> Result<ResponsesApiTool, serde_json::Error> {
    Ok(tool_definition_to_responses_api_tool(
        parse_mcp_tool(tool)?.renamed(tool_name.name.clone()),
    ))
}

pub fn tool_definition_to_responses_api_tool(tool_definition: ToolDefinition) -> ResponsesApiTool {
    ResponsesApiTool {
        name: tool_definition.name,
        description: tool_definition.description,
        strict: false,
        defer_loading: tool_definition.defer_loading.then_some(true),
        parameters: tool_definition.input_schema,
        output_schema: tool_definition.output_schema,
    }
}

#[cfg(test)]
#[path = "responses_api_tests.rs"]
mod tests;
