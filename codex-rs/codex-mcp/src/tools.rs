//! MCP tool metadata, filtering, schema shaping, and name normalization.
//!
//! Raw MCP tool identities must be preserved for protocol calls, while
//! model-visible tool names must be sanitized, deduplicated, and kept within API
//! limits. This module owns that translation as well as the shared [`ToolInfo`]
//! type and helpers that adjust tool schemas before exposing them to the model.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_config::McpServerConfig;
use codex_protocol::ToolName;
use codex_utils_string::sha1_hex;
use rmcp::model::Tool;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value as JsonValue;
use tracing::warn;

use crate::mcp::LEGACY_MCP_TOOL_NAME_PREFIX;
use crate::mcp::MCP_TOOL_NAME_DELIMITER;
use crate::mcp::sanitize_responses_api_tool_name;

pub(crate) const MCP_TOOLS_CACHE_WRITE_DURATION_METRIC: &str =
    "codex.mcp.tools.cache_write.duration_ms";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Raw MCP server name used for routing the tool call.
    pub server_name: String,
    /// Whether calls routed to this server may run in parallel.
    #[serde(default)]
    pub supports_parallel_tool_calls: bool,
    /// MCP server origin used for telemetry and diagnostics, when known.
    #[serde(default)]
    pub server_origin: Option<String>,
    /// Model-visible tool name used in Responses API tool declarations.
    #[serde(rename = "tool_name", alias = "callable_name")]
    pub callable_name: String,
    /// Model-visible namespace used for deferred tool loading.
    #[serde(rename = "tool_namespace", alias = "callable_namespace")]
    pub callable_namespace: String,
    /// Model-visible namespace description.
    // Keep the old serialized field name readable for cached ToolInfo values.
    #[serde(default, alias = "connector_description")]
    pub namespace_description: Option<String>,
    /// Raw MCP tool definition; `tool.name` is sent back to the MCP server.
    pub tool: Tool,
    pub connector_id: Option<String>,
    pub connector_name: Option<String>,
    #[serde(default)]
    pub plugin_display_names: Vec<String>,
}

impl ToolInfo {
    pub fn canonical_tool_name(&self) -> ToolName {
        ToolName::namespaced(self.callable_namespace.clone(), self.callable_name.clone())
    }
}

pub fn declared_openai_file_input_param_names(
    meta: Option<&Map<String, JsonValue>>,
) -> Vec<String> {
    let Some(meta) = meta else {
        return Vec::new();
    };

    meta.get(META_OPENAI_FILE_PARAMS)
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// A tool is allowed to be used if both are true:
/// 1. enabled is None (no allowlist is set) or the tool is explicitly enabled.
/// 2. The tool is not explicitly disabled.
#[derive(Default, Clone)]
pub(crate) struct ToolFilter {
    pub(crate) enabled: Option<HashSet<String>>,
    pub(crate) disabled: HashSet<String>,
}

impl ToolFilter {
    pub(crate) fn from_config(cfg: &McpServerConfig) -> Self {
        let enabled = cfg
            .enabled_tools
            .as_ref()
            .map(|tools| tools.iter().cloned().collect::<HashSet<_>>());
        let disabled = cfg
            .disabled_tools
            .as_ref()
            .map(|tools| tools.iter().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();

        Self { enabled, disabled }
    }

    pub(crate) fn allows(&self, tool_name: &str) -> bool {
        if let Some(enabled) = &self.enabled
            && !enabled.contains(tool_name)
        {
            return false;
        }

        !self.disabled.contains(tool_name)
    }
}

/// Returns the model-visible view of a tool while preserving the raw metadata used by execution.
/// Declared file parameters are presented as local file paths; execution later uploads those files
/// and replaces the paths with the uploaded-file objects expected by the app.
pub(crate) fn tool_with_model_visible_input_schema(tool: &Tool) -> Tool {
    let file_params = declared_openai_file_input_param_names(tool.meta.as_deref());
    if file_params.is_empty() {
        return tool.clone();
    }

    let mut tool = tool.clone();
    let mut input_schema = JsonValue::Object(tool.input_schema.as_ref().clone());
    rewrite_input_schema_for_local_file_paths(&mut input_schema, &file_params);
    if let JsonValue::Object(input_schema) = input_schema {
        tool.input_schema = Arc::new(input_schema);
    }
    tool
}

pub(crate) fn filter_tools(tools: Vec<ToolInfo>, filter: &ToolFilter) -> Vec<ToolInfo> {
    tools
        .into_iter()
        .filter(|tool| filter.allows(&tool.tool.name))
        .collect()
}

/// Returns MCP tools with model-visible names normalized.
///
/// Raw MCP server/tool names are kept on each [`ToolInfo`] for protocol calls, while
/// `callable_namespace` / `callable_name` are sanitized and, when necessary, hashed so
/// every model-visible name is unique and <= 64 bytes.
///
/// When `prefix_mcp_tool_names` is true, the historical `mcp__` namespace
/// prefix is added without restoring the old trailing `__` namespace suffix.
pub(crate) fn normalize_tools_for_model_with_prefix<I>(
    tools: I,
    prefix_mcp_tool_names: bool,
) -> Vec<ToolInfo>
where
    I: IntoIterator<Item = ToolInfo>,
{
    let mut raw = std::collections::BTreeMap::<String, Option<ToolInfo>>::new();
    for tool in tools {
        let identity = format!("{}\0{}\0{}\0{}\0{}", tool.server_name, tool.callable_namespace,
            tool.connector_id.as_deref().unwrap_or_default(), tool.callable_name, tool.tool.name);
        match raw.entry(identity) {
            std::collections::btree_map::Entry::Vacant(entry) => { entry.insert(Some(tool)); }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().as_ref().is_some_and(|previous| previous != &tool) {
                    entry.insert(None);
                }
            }
        }
    }
    let mut candidates = Vec::new();
    for (raw_tool_identity, tool) in raw {
        let Some(mut tool) = tool else {
            warn!(identity = ?raw_tool_identity, "quarantining conflicting MCP tool definitions");
            continue;
        };
        let raw_namespace_identity = format!("{}\0{}\0{}", tool.server_name,
            tool.callable_namespace, tool.connector_id.as_deref().unwrap_or_default());
        let sanitized_namespace = sanitize_responses_api_tool_name(&tool.callable_namespace);
        let mut namespace = callable_namespace_with_prefix(&sanitized_namespace, prefix_mcp_tool_names);
        // Hash lossy identities even in singleton catalogs. Adding another
        // connector must never change an existing callable name.
        if sanitized_namespace != tool.callable_namespace
            || tool.callable_namespace != tool.server_name
            || tool.connector_id.is_some()
        {
            namespace = append_namespace_hash_suffix(&namespace, &raw_namespace_identity);
        }
        let mut name = sanitize_responses_api_tool_name(&tool.callable_name);
        if name != tool.callable_name || tool.callable_name != tool.tool.name {
            name = append_hash_suffix(&name, &raw_tool_identity);
        }
        if namespace.len() + name.len() + MCP_TOOL_NAME_DELIMITER.len() > MAX_TOOL_NAME_LENGTH {
            (namespace, name) = fit_callable_parts_with_hash(
                &namespace, &name, &raw_tool_identity, MCP_TOOL_NAME_DELIMITER.len(),
            );
        }
        tool.callable_namespace = namespace;
        tool.callable_name = name;
        candidates.push(tool);
    }
    // A native identifier can deliberately equal a generated alias. Quarantine
    // the ambiguous identity instead of redirecting old code or reallocating it.
    let mut counts = HashMap::<ToolName, usize>::new();
    for tool in &candidates {
        *counts.entry(tool.canonical_tool_name()).or_default() += 1;
    }
    candidates.retain(|tool| {
        let valid = counts[&tool.canonical_tool_name()] == 1;
        if !valid { warn!(name = %tool.canonical_tool_name(), "quarantining ambiguous MCP callable name"); }
        valid
    });
    candidates
}

const MAX_TOOL_NAME_LENGTH: usize = 64;
const CALLABLE_NAME_HASH_LEN: usize = 12;
const META_OPENAI_FILE_PARAMS: &str = "openai/fileParams";

fn rewrite_input_schema_for_local_file_paths(input_schema: &mut JsonValue, file_params: &[String]) {
    let Some(properties) = input_schema
        .as_object_mut()
        .and_then(|schema| schema.get_mut("properties"))
        .and_then(JsonValue::as_object_mut)
    else {
        return;
    };

    for field_name in file_params {
        let Some(property_schema) = properties.get_mut(field_name) else {
            continue;
        };
        rewrite_input_property_schema_as_local_file_path(property_schema);
    }
}

fn rewrite_input_property_schema_as_local_file_path(schema: &mut JsonValue) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };

    let mut description = object
        .get("description")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let is_array = object.get("type").and_then(JsonValue::as_str) == Some("array")
        || object.get("items").is_some();
    let guidance = if is_array {
        "Paths to the files to upload, relative to the primary execution environment's working directory. Absolute paths and parent-directory (`..`) components are not allowed."
    } else {
        "Path to the file to upload, relative to the primary execution environment's working directory. Absolute paths and parent-directory (`..`) components are not allowed."
    };
    if description.is_empty() {
        description = guidance.to_string();
    } else if !description.contains(guidance) {
        description = format!("{description} {guidance}");
    }

    object.clear();
    object.insert("description".to_string(), JsonValue::String(description));
    if is_array {
        object.insert("type".to_string(), JsonValue::String("array".to_string()));
        object.insert("items".to_string(), serde_json::json!({ "type": "string" }));
    } else {
        object.insert("type".to_string(), JsonValue::String("string".to_string()));
    }
}

fn callable_namespace_with_prefix(namespace: &str, prefix_mcp_tool_names: bool) -> String {
    if !prefix_mcp_tool_names || namespace.starts_with(LEGACY_MCP_TOOL_NAME_PREFIX) {
        namespace.to_string()
    } else {
        format!("{LEGACY_MCP_TOOL_NAME_PREFIX}{namespace}")
    }
}

fn callable_name_hash_suffix(raw_identity: &str) -> String {
    let hash = sha1_hex(raw_identity.as_bytes());
    format!("_{}", &hash[..CALLABLE_NAME_HASH_LEN])
}

fn append_hash_suffix(value: &str, raw_identity: &str) -> String {
    format!("{value}{}", callable_name_hash_suffix(raw_identity))
}

fn append_namespace_hash_suffix(namespace: &str, raw_identity: &str) -> String {
    if let Some(namespace) = namespace.strip_suffix(MCP_TOOL_NAME_DELIMITER) {
        format!(
            "{}{}{}",
            namespace,
            callable_name_hash_suffix(raw_identity),
            MCP_TOOL_NAME_DELIMITER
        )
    } else {
        append_hash_suffix(namespace, raw_identity)
    }
}

fn truncate_name(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}

fn fit_callable_parts_with_hash(
    namespace: &str,
    tool_name: &str,
    raw_identity: &str,
    reserved_len: usize,
) -> (String, String) {
    let suffix = callable_name_hash_suffix(raw_identity);
    let max_tool_len = MAX_TOOL_NAME_LENGTH.saturating_sub(namespace.len() + reserved_len);
    if max_tool_len >= suffix.len() {
        let prefix_len = max_tool_len - suffix.len();
        return (
            namespace.to_string(),
            format!("{}{}", truncate_name(tool_name, prefix_len), suffix),
        );
    }

    let max_namespace_len = MAX_TOOL_NAME_LENGTH.saturating_sub(suffix.len() + reserved_len);
    (truncate_name(namespace, max_namespace_len), suffix)
}
