//! Types used when representing Model Context Protocol (MCP) values inside the
//! Codex protocol.
//!
//! We intentionally keep these types TS/JSON-schema friendly (via `ts-rs` and
//! `schemars`) so they can be embedded in Codex's own protocol structures.
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

const TOOL_CALL_THREAD_ID_META_KEY: &str = "threadId";
const TOOL_CALL_PROGRESS_TOKEN_META_KEY: &str = "progressToken";
const TOOL_CALL_PROGRESS_TOKEN_PREFIX: &str = "codex-mcp-progress:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallProgressToken {
    turn_id: String,
    item_id: String,
}

/// Adds the Codex thread identifier to object-valued MCP tool metadata.
///
/// Non-object metadata is preserved because MCP permits provider-specific
/// values that Codex must not rewrite.
pub fn with_tool_call_thread_id_meta(
    meta: Option<serde_json::Value>,
    thread_id: &str,
) -> Option<serde_json::Value> {
    match meta {
        Some(serde_json::Value::Object(mut map)) => {
            map.insert(
                TOOL_CALL_THREAD_ID_META_KEY.to_string(),
                serde_json::Value::String(thread_id.to_string()),
            );
            Some(serde_json::Value::Object(map))
        }
        None => Some(serde_json::json!({"threadId": thread_id})),
        other => other,
    }
}

/// Adds an opaque MCP progress token that can be correlated back to one Codex
/// turn item when a server echoes it in `notifications/progress`.
pub fn with_tool_call_progress_token_meta(
    meta: Option<serde_json::Value>,
    turn_id: &str,
    item_id: &str,
) -> Option<serde_json::Value> {
    let token = ToolCallProgressToken {
        turn_id: turn_id.to_string(),
        item_id: item_id.to_string(),
    };
    let Ok(token) = serde_json::to_string(&token) else {
        return meta;
    };
    match meta {
        Some(serde_json::Value::Object(mut map)) => {
            map.insert(
                TOOL_CALL_PROGRESS_TOKEN_META_KEY.to_string(),
                serde_json::Value::String(format!("{TOOL_CALL_PROGRESS_TOKEN_PREFIX}{token}")),
            );
            Some(serde_json::Value::Object(map))
        }
        None => Some(serde_json::json!({
            (TOOL_CALL_PROGRESS_TOKEN_META_KEY):
                format!("{TOOL_CALL_PROGRESS_TOKEN_PREFIX}{token}")
        })),
        other => other,
    }
}

/// Decodes a progress token previously created by
/// [`with_tool_call_progress_token_meta`]. Provider-owned tokens are ignored.
pub fn decode_tool_call_progress_token(token: &str) -> Option<(String, String)> {
    let encoded = token.strip_prefix(TOOL_CALL_PROGRESS_TOKEN_PREFIX)?;
    let token: ToolCallProgressToken = serde_json::from_str(encoded).ok()?;
    Some((token.turn_id, token.item_id))
}

/// ID of a request, which can be either a string or an integer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    #[ts(type = "number")]
    Integer(i64),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestId::String(s) => f.write_str(s),
            RequestId::Integer(i) => i.fmt(f),
        }
    }
}

/// Presentation metadata advertised by an initialized MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct McpServerInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
    pub description: Option<String>,
    pub icons: Option<Vec<serde_json::Value>>,
    pub website_url: Option<String>,
}

/// Definition for a tool the client can call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub annotations: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub icons: Option<Vec<serde_json::Value>>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub meta: Option<serde_json::Value>,
}

/// A known resource that the server is capable of reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub annotations: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mime_type: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[ts(type = "number")]
    pub size: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub title: Option<String>,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub icons: Option<Vec<serde_json::Value>>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub meta: Option<serde_json::Value>,
}

/// Contents returned when reading a resource from an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(untagged)]
pub enum ResourceContent {
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Text {
        /// The URI of this resource.
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        mime_type: Option<String>,
        text: String,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        meta: Option<serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Blob {
        /// The URI of this resource.
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        mime_type: Option<String>,
        blob: String,
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        meta: Option<serde_json::Value>,
    },
}

/// A template description for resources available on the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub annotations: Option<serde_json::Value>,
    pub uri_template: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mime_type: Option<String>,
}

/// The server's response to a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub structured_content: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub is_error: Option<bool>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub meta: Option<serde_json::Value>,
}

// === Adapter helpers ===
//
// These types and conversions intentionally live in `codex-protocol` so other crates can convert
// “wire-shaped” MCP JSON (typically coming from rmcp model structs serialized with serde) into our
// TS/JsonSchema-friendly protocol types without depending on `mcp-types`.

fn deserialize_lossy_opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Number>::deserialize(deserializer)? {
        Some(number) => {
            if let Some(v) = number.as_i64() {
                Ok(Some(v))
            } else if let Some(v) = number.as_u64() {
                Ok(i64::try_from(v).ok())
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolSerde {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "inputSchema", alias = "input_schema")]
    input_schema: serde_json::Value,
    #[serde(default, rename = "outputSchema", alias = "output_schema")]
    output_schema: Option<serde_json::Value>,
    #[serde(default)]
    annotations: Option<serde_json::Value>,
    #[serde(default)]
    icons: Option<Vec<serde_json::Value>>,
    #[serde(rename = "_meta", default)]
    meta: Option<serde_json::Value>,
}

impl From<ToolSerde> for Tool {
    fn from(value: ToolSerde) -> Self {
        let ToolSerde {
            name,
            title,
            description,
            input_schema,
            output_schema,
            annotations,
            icons,
            meta,
        } = value;
        Self {
            name,
            title,
            description,
            input_schema,
            output_schema,
            annotations,
            icons,
            meta,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceSerde {
    #[serde(default)]
    annotations: Option<serde_json::Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "mimeType", alias = "mime_type", default)]
    mime_type: Option<String>,
    name: String,
    #[serde(default, deserialize_with = "deserialize_lossy_opt_i64")]
    size: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    uri: String,
    #[serde(default)]
    icons: Option<Vec<serde_json::Value>>,
    #[serde(rename = "_meta", default)]
    meta: Option<serde_json::Value>,
}

impl From<ResourceSerde> for Resource {
    fn from(value: ResourceSerde) -> Self {
        let ResourceSerde {
            annotations,
            description,
            mime_type,
            name,
            size,
            title,
            uri,
            icons,
            meta,
        } = value;
        Self {
            annotations,
            description,
            mime_type,
            name,
            size,
            title,
            uri,
            icons,
            meta,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceTemplateSerde {
    #[serde(default)]
    annotations: Option<serde_json::Value>,
    #[serde(rename = "uriTemplate", alias = "uri_template")]
    uri_template: String,
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "mimeType", alias = "mime_type", default)]
    mime_type: Option<String>,
}

impl From<ResourceTemplateSerde> for ResourceTemplate {
    fn from(value: ResourceTemplateSerde) -> Self {
        let ResourceTemplateSerde {
            annotations,
            uri_template,
            name,
            title,
            description,
            mime_type,
        } = value;
        Self {
            annotations,
            uri_template,
            name,
            title,
            description,
            mime_type,
        }
    }
}

impl Tool {
    pub fn from_mcp_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        Ok(serde_json::from_value::<ToolSerde>(value)?.into())
    }
}

impl Resource {
    pub fn from_mcp_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        Ok(serde_json::from_value::<ResourceSerde>(value)?.into())
    }
}

impl ResourceTemplate {
    pub fn from_mcp_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        Ok(serde_json::from_value::<ResourceTemplateSerde>(value)?.into())
    }
}

