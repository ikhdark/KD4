use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;

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

/// Resource identity for one connection, without retaining it or starting it.
#[derive(Clone)]
pub struct McpResourceServerCacheKey {
    pub(crate) connection: Weak<AtomicUsize>,
    pub(crate) generation: u64,
}

impl PartialEq for McpResourceServerCacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.connection.ptr_eq(&other.connection) && self.generation == other.generation
    }
}

impl Eq for McpResourceServerCacheKey {}

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

    pub fn server_cache_key(&self, server: &str) -> Option<McpResourceServerCacheKey> {
        (self.lease)().and_then(|lease| lease.manager.resource_cache_key(server))
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

pub(crate) fn resource_from_rmcp(resource: rmcp::model::Resource) -> Result<Resource> {
    let rmcp::model::Resource {
        uri,
        name,
        title,
        description,
        mime_type,
        size,
        icons,
        meta,
        annotations,
        ..
    } = resource;

    let annotations = annotations
        .map(serde_json::to_value)
        .transpose()
        .context("failed to convert MCP resource annotations")?;
    let icons = icons
        .map(|icons| {
            icons
                .into_iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()
        })
        .transpose()
        .context("failed to convert MCP resource icons")?;

    Ok(Resource {
        annotations,
        description,
        mime_type,
        name,
        size: size
            .map(i64::try_from)
            .transpose()
            .context("MCP resource size exceeds the supported range")?,
        title,
        uri,
        icons,
        meta: meta.map(|meta| serde_json::Value::Object(meta.0)),
    })
}

fn resource_content_from_rmcp(content: rmcp::model::ResourceContents) -> Result<ResourceContent> {
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
        _ => anyhow::bail!("unsupported MCP resource contents"),
    })
}

