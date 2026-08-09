use std::sync::Arc;
use std::sync::Weak;

use anyhow::Context;
use anyhow::Result;
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
#[derive(Clone, Debug, PartialEq)]
pub struct McpResourceReadResult {
    /// Text or blob content returned for the requested resource.
    pub contents: Vec<ResourceContent>,
}

struct McpResourceLease {
    _generation: Arc<dyn Send + Sync>,
    manager: Arc<McpConnectionManager>,
}

type McpResourceLeaseProvider = dyn Fn() -> Option<McpResourceLease> + Send + Sync;

/// Session-scoped access to MCP resources through the currently installed runtime generation.
#[derive(Clone)]
pub struct McpResourceClient {
    lease: Arc<McpResourceLeaseProvider>,
}

/// Opaque identity for the manager currently used by an MCP resource client.
#[derive(Clone)]
pub struct McpResourceClientCacheKey(Option<Weak<McpConnectionManager>>);

impl PartialEq for McpResourceClientCacheKey {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(left), Some(right)) => left.ptr_eq(right),
            (None, None) => true,
            _ => false,
        }
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
    /// Creates a resource client backed by leases on the session's MCP runtime generation.
    pub fn new<L, F>(lease: F) -> Self
    where
        L: Send + Sync + 'static,
        F: Fn() -> Option<(Arc<L>, Arc<McpConnectionManager>)> + Send + Sync + 'static,
    {
        Self {
            lease: Arc::new(move || {
                lease().map(|(generation, manager)| McpResourceLease {
                    _generation: generation,
                    manager,
                })
            }),
        }
    }

    /// Returns an identity that changes whenever the published manager changes.
    pub fn cache_key(&self) -> McpResourceClientCacheKey {
        McpResourceClientCacheKey((self.lease)().map(|lease| Arc::downgrade(&lease.manager)))
    }

    /// Returns whether the current manager contains the named server.
    ///
    /// This does not wait for server startup or imply that startup succeeded.
    pub async fn has_server(&self, server: &str) -> bool {
        (self.lease)().is_some_and(|lease| lease.manager.contains_server(server))
    }

    /// Lists one resource page from the named server.
    pub async fn list_resources(
        &self,
        server: &str,
        cursor: Option<String>,
    ) -> Result<McpResourcePage> {
        let params =
            cursor.map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let lease = (self.lease)().context("MCP runtime is not installed")?;
        let result = lease.manager.list_resources(server, params).await?;
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
        let lease = (self.lease)().context("MCP runtime is not installed")?;
        let result = lease
            .manager
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_runtime_is_reported_without_panicking() {
        let client = McpResourceClient::new(|| None::<(Arc<()>, Arc<McpConnectionManager>)>);

        assert!(client.cache_key() == client.cache_key());
        assert!(!client.has_server("not-installed").await);
        assert!(client.list_resources("not-installed", None).await.is_err());
        assert!(
            client
                .read_resource("not-installed", "test://resource")
                .await
                .is_err()
        );
    }
}

fn resource_from_rmcp(resource: rmcp::model::Resource) -> Result<Resource> {
    let value = serde_json::to_value(resource).context("failed to serialize MCP resource")?;
    Resource::from_mcp_value(value).context("failed to convert MCP resource")
}

fn resource_content_from_rmcp(content: rmcp::model::ResourceContents) -> Result<ResourceContent> {
    let value =
        serde_json::to_value(content).context("failed to serialize MCP resource content")?;
    serde_json::from_value(value).context("failed to convert MCP resource content")
}
