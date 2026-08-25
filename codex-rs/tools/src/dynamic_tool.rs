use crate::ToolDefinition;
use crate::parse_tool_input_schema;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use std::collections::HashSet;

pub fn validate_dynamic_tools(tools: &[DynamicToolSpec]) -> Result<(), String> {
    const DYNAMIC_TOOL_NAME_MAX_LEN: usize = 128;
    const DYNAMIC_TOOL_NAMESPACE_MAX_LEN: usize = 64;
    const DYNAMIC_TOOL_NAMESPACE_DESCRIPTION_MAX_LEN: usize = 1024;
    const DYNAMIC_TOOL_IDENTIFIER_PATTERN: &str = "^[a-zA-Z0-9_-]+$";
    const RESERVED_RESPONSES_NAMESPACES: &[&str] = &[
        "api_tool",
        "browser",
        "computer",
        "collaboration",
        "container",
        "file_search",
        "functions",
        "image_gen",
        "multi_tool_use",
        "python",
        "python_user_visible",
        "submodel_delegator",
        "terminal",
        "tool_search",
        "web",
    ];

    fn escape_identifier_for_error(value: &str) -> String {
        value.escape_default().to_string()
    }

    fn validate_dynamic_tool_identifier(
        value: &str,
        label: &str,
        max_len: usize,
    ) -> Result<(), String> {
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(format!(
                "{label} must match {DYNAMIC_TOOL_IDENTIFIER_PATTERN} to match Responses API: {}",
                escape_identifier_for_error(value),
            ));
        }
        if value.chars().count() > max_len {
            return Err(format!(
                "{label} must be at most {max_len} characters to match Responses API: {}",
                escape_identifier_for_error(value),
            ));
        }
        Ok(())
    }

    fn validate_dynamic_tool<'a>(
        tool: &'a DynamicToolFunctionSpec,
        namespace: Option<&str>,
        seen: &mut HashSet<&'a str>,
    ) -> Result<(), String> {
        let name = tool.name.trim();
        if name.is_empty() {
            return Err("dynamic tool name must not be empty".to_string());
        }
        if name != tool.name {
            return Err(format!(
                "dynamic tool name has leading/trailing whitespace: {}",
                escape_identifier_for_error(&tool.name),
            ));
        }
        validate_dynamic_tool_identifier(name, "dynamic tool name", DYNAMIC_TOOL_NAME_MAX_LEN)?;
        if name == "mcp" || name.starts_with("mcp__") {
            return Err(format!("dynamic tool name is reserved: {name}"));
        }
        if !seen.insert(name) {
            if let Some(namespace) = namespace {
                return Err(format!(
                    "duplicate dynamic tool name in namespace {namespace}: {name}"
                ));
            }
            return Err(format!("duplicate dynamic tool name: {name}"));
        }
        if tool.defer_loading && namespace.is_none() {
            return Err(format!(
                "deferred dynamic tool must include a namespace: {name}"
            ));
        }

        if let Err(err) = parse_tool_input_schema(&tool.input_schema) {
            return Err(format!(
                "dynamic tool input schema is not supported for {name}: {err}"
            ));
        }
        Ok(())
    }

    let mut seen_tools = HashSet::new();
    let mut seen_namespaces = HashSet::new();
    for spec in tools {
        match spec {
            DynamicToolSpec::Function(tool) => {
                validate_dynamic_tool(tool, /*namespace*/ None, &mut seen_tools)?;
            }
            DynamicToolSpec::Namespace(namespace) => {
                let name = namespace.name.trim();
                if name.is_empty() {
                    return Err("dynamic tool namespace must not be empty".to_string());
                }
                if name != namespace.name {
                    return Err(format!(
                        "dynamic tool namespace has leading/trailing whitespace: {}",
                        escape_identifier_for_error(&namespace.name),
                    ));
                }
                validate_dynamic_tool_identifier(
                    name,
                    "dynamic tool namespace",
                    DYNAMIC_TOOL_NAMESPACE_MAX_LEN,
                )?;
                if namespace.description.chars().count()
                    > DYNAMIC_TOOL_NAMESPACE_DESCRIPTION_MAX_LEN
                {
                    return Err(format!(
                        "dynamic tool namespace description must be at most {DYNAMIC_TOOL_NAMESPACE_DESCRIPTION_MAX_LEN} characters"
                    ));
                }
                if name == "mcp" || name.starts_with("mcp__") {
                    return Err(format!("dynamic tool namespace is reserved: {name}"));
                }
                if RESERVED_RESPONSES_NAMESPACES.contains(&name) {
                    return Err(format!(
                        "dynamic tool namespace collides with a reserved Responses API namespace: {name}",
                    ));
                }
                if !seen_namespaces.insert(name) {
                    return Err(format!("duplicate dynamic tool namespace: {name}"));
                }
                if namespace.tools.is_empty() {
                    return Err(format!(
                        "dynamic tool namespace must contain at least one tool: {name}"
                    ));
                }
                let mut seen_namespace_tools = HashSet::new();
                for tool in &namespace.tools {
                    let DynamicToolNamespaceTool::Function(tool) = tool;
                    validate_dynamic_tool(tool, Some(name), &mut seen_namespace_tools)?;
                }
            }
        }
    }
    Ok(())
}

pub fn parse_dynamic_tool(
    tool: &DynamicToolFunctionSpec,
) -> Result<ToolDefinition, serde_json::Error> {
    Ok(ToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: parse_tool_input_schema(&tool.input_schema)?,
        output_schema: None,
        defer_loading: tool.defer_loading,
    })
}

#[cfg(test)]
#[path = "dynamic_tool_tests.rs"]
mod tests;
