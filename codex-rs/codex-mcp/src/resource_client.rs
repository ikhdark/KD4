use std::sync::Arc;
use std::sync::Weak;

use anyhow::Context;
use anyhow::Result;
use arc_swap::ArcSwap;
use codex_protocol::mcp::Resource;
use codex_protocol::mcp::ResourceContent;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ReadResourceRequestParams;

use crate::McpConnectionManager;

/// One page of resources returned by an MCP server.
#[derive(Clone, Debug, PartialEq)]
pub struct McpResourcePage {
    /// Resources advertised on this page.
    pub resources: Vec<Resource>,
    /// Opaque cursor to supply when requesting the next page.
    pub next_cursor: Option<String>,
}

/// Contents returned after reading one MCP resource.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct McpResourceReadResult {
    /// Text or blob content returned for the requested resource.
    pub contents: Vec<ResourceContent>,
}

/// Session-scoped access to MCP resources through the currently installed manager.
///
/// The client retains the manager's shared publication handle rather than a manager
/// snapshot, so calls automatically use replacements installed during startup and refresh.
#[derive(Clone)]
pub struct McpResourceClient {
    manager: Arc<ArcSwap<McpConnectionManager>>,
}

/// Opaque identity for the manager currently used by an MCP resource client.
#[derive(Clone)]
pub struct McpResourceClientCacheKey(Weak<McpConnectionManager>);

impl PartialEq for McpResourceClientCacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
}

impl Eq for McpResourceClientCacheKey {}

impl std::fmt::Debug for McpResourceClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpResourceClient")
            .finish_non_exhaustive()
    }
}

impl McpResourceClient {
    /// Creates a resource client backed by the session's replaceable MCP manager.
    pub fn new(manager: Arc<ArcSwap<McpConnectionManager>>) -> Self {
        Self { manager }
    }

    /// Returns an identity that changes whenever the published manager changes.
    pub fn cache_key(&self) -> McpResourceClientCacheKey {
        McpResourceClientCacheKey(Arc::downgrade(&self.manager.load_full()))
    }

    /// Returns whether the current manager contains the named server.
    ///
    /// This does not wait for server startup or imply that startup succeeded.
    pub async fn has_server(&self, server: &str) -> bool {
        self.manager.load_full().contains_server(server)
    }

    /// Lists one resource page from the named server.
    pub async fn list_resources(
        &self,
        server: &str,
        cursor: Option<String>,
    ) -> Result<McpResourcePage> {
        let params =
            cursor.map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let result = self
            .manager
            .load_full()
            .list_resources(server, params)
            .await?;
        let resources = result
            .resources
            .into_iter()
            .map(resource_from_rmcp)
            .collect::<Result<Vec<_>>>()?;
        Ok(McpResourcePage {
            resources,
            next_cursor: result.next_cursor,
        })
    }

    /// Reads one resource from the named server.
    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<McpResourceReadResult> {
        let result = self
            .manager
            .load_full()
            .read_resource(server, ReadResourceRequestParams::new(uri.to_string()))
            .await?;
        resource_read_result_from_rmcp(result)
    }
}

fn resource_from_rmcp(resource: rmcp::model::Resource) -> Result<Resource> {
    let value = serde_json::to_value(resource).context("failed to serialize MCP resource")?;
    Resource::from_mcp_value(value).context("failed to convert MCP resource")
}

pub fn resource_content_from_rmcp(
    content: rmcp::model::ResourceContents,
) -> Result<ResourceContent> {
    Ok(match content {
        rmcp::model::ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            meta,
        } => ResourceContent::Text {
            uri,
            mime_type,
            text,
            meta: meta.map(|meta| serde_json::Value::Object(meta.0)),
        },
        rmcp::model::ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            meta,
        } => ResourceContent::Blob {
            uri,
            mime_type,
            blob,
            meta: meta.map(|meta| serde_json::Value::Object(meta.0)),
        },
    })
}

/// Converts an rmcp read result into the canonical Codex resource result without JSON bridging.
pub fn resource_read_result_from_rmcp(
    result: rmcp::model::ReadResourceResult,
) -> Result<McpResourceReadResult> {
    let contents = result
        .contents
        .into_iter()
        .map(resource_content_from_rmcp)
        .collect::<Result<Vec<_>>>()?;
    Ok(McpResourceReadResult { contents })
}

/// Serializes the typed resource result for the retained `Value` compatibility API.
pub fn resource_read_result_to_value(
    result: McpResourceReadResult,
) -> std::result::Result<serde_json::Value, serde_json::Error> {
    serde_json::to_value(result)
}

#[cfg(test)]
mod tests {
    use codex_protocol::mcp::ResourceContent;
    use pretty_assertions::assert_eq;
    use rmcp::model::Meta;
    use rmcp::model::ReadResourceResult;
    use rmcp::model::ResourceContents;
    use serde_json::json;

    use super::McpResourceReadResult;
    use super::resource_read_result_from_rmcp;
    use super::resource_read_result_to_value;

    #[test]
    fn typed_resource_read_preserves_legacy_value_wrapper_shape() {
        let text_meta = Meta(serde_json::Map::from_iter([
            ("nested".to_string(), json!({ "kind": "text" })),
            ("count".to_string(), json!(1)),
        ]));
        let blob_meta = Meta(serde_json::Map::from_iter([
            ("nested".to_string(), json!({ "kind": "blob" })),
            ("count".to_string(), json!(2)),
        ]));
        let rmcp_result = ReadResourceResult::new(vec![
            ResourceContents::TextResourceContents {
                uri: "file:///text".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: "hello".to_string(),
                meta: Some(text_meta),
            },
            ResourceContents::BlobResourceContents {
                uri: "file:///blob".to_string(),
                mime_type: None,
                blob: "aGVsbG8=".to_string(),
                meta: Some(blob_meta),
            },
        ]);
        let legacy_value = serde_json::to_value(&rmcp_result).unwrap();

        let typed = resource_read_result_from_rmcp(rmcp_result).unwrap();

        assert_eq!(
            typed,
            McpResourceReadResult {
                contents: vec![
                    ResourceContent::Text {
                        uri: "file:///text".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: "hello".to_string(),
                        meta: Some(json!({
                            "nested": { "kind": "text" },
                            "count": 1
                        })),
                    },
                    ResourceContent::Blob {
                        uri: "file:///blob".to_string(),
                        mime_type: None,
                        blob: "aGVsbG8=".to_string(),
                        meta: Some(json!({
                            "nested": { "kind": "blob" },
                            "count": 2
                        })),
                    },
                ],
            }
        );
        assert_eq!(resource_read_result_to_value(typed).unwrap(), legacy_value);
    }
}
