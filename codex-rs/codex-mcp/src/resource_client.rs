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
        let contents = result
            .contents
            .into_iter()
            .map(resource_content_from_rmcp)
            .collect::<Result<Vec<_>>>()?;
        Ok(McpResourceReadResult { contents })
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
