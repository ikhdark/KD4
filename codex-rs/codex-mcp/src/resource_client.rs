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
            .collect();
        Ok(McpResourceReadResult { contents })
    }
}

fn resource_from_rmcp(resource: rmcp::model::Resource) -> Result<Resource> {
    let rmcp::model::Annotated { raw, annotations } = resource;
    let rmcp::model::RawResource {
        uri,
        name,
        title,
        description,
        mime_type,
        size,
        icons,
        meta,
    } = raw;

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
        size: size.map(i64::from),
        title,
        uri,
        icons,
        meta: meta.map(|meta| serde_json::Value::Object(meta.0)),
    })
}

fn resource_content_from_rmcp(content: rmcp::model::ResourceContents) -> ResourceContent {
    match content {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rmcp::model::Annotated;
    use rmcp::model::Annotations;
    use rmcp::model::Icon;
    use rmcp::model::Meta;
    use rmcp::model::RawResource;
    use rmcp::model::ResourceContents;
    use rmcp::model::Role;

    #[test]
    fn resource_conversion_preserves_typed_fields_and_opaque_metadata() {
        let mut annotations = Annotations::default();
        annotations.audience = Some(vec![Role::User]);
        annotations.priority = Some(0.75);

        let mut meta = Meta::new();
        meta.insert("source".to_string(), serde_json::json!("calendar"));
        let raw = RawResource::new("resource://calendar/today", "today")
            .with_title("Today's calendar")
            .with_description("Calendar entries for today")
            .with_mime_type("application/json")
            .with_size(42)
            .with_icons(vec![Icon::new("https://example.com/calendar.png")])
            .with_meta(meta);

        assert_eq!(
            resource_from_rmcp(Annotated::new(raw, Some(annotations))).expect("convert resource"),
            Resource {
                annotations: Some(serde_json::json!({
                    "audience": ["user"],
                    "priority": 0.75,
                })),
                description: Some("Calendar entries for today".to_string()),
                mime_type: Some("application/json".to_string()),
                name: "today".to_string(),
                size: Some(42),
                title: Some("Today's calendar".to_string()),
                uri: "resource://calendar/today".to_string(),
                icons: Some(vec![serde_json::json!({
                    "src": "https://example.com/calendar.png",
                })]),
                meta: Some(serde_json::json!({"source": "calendar"})),
            }
        );
    }

    #[test]
    fn resource_content_conversion_moves_text_blob_and_metadata_directly() {
        let mut text_meta = Meta::new();
        text_meta.insert("page".to_string(), serde_json::json!(1));
        let text = ResourceContents::TextResourceContents {
            uri: "resource://docs/readme".to_string(),
            mime_type: Some("text/markdown".to_string()),
            text: "# Readme".to_string(),
            meta: Some(text_meta),
        };

        let mut blob_meta = Meta::new();
        blob_meta.insert("encoding".to_string(), serde_json::json!("base64"));
        let blob = ResourceContents::BlobResourceContents {
            uri: "resource://images/logo".to_string(),
            mime_type: Some("image/png".to_string()),
            blob: "iVBORw0KGgo=".to_string(),
            meta: Some(blob_meta),
        };

        assert_eq!(
            resource_content_from_rmcp(text),
            ResourceContent::Text {
                uri: "resource://docs/readme".to_string(),
                mime_type: Some("text/markdown".to_string()),
                text: "# Readme".to_string(),
                meta: Some(serde_json::json!({"page": 1})),
            }
        );
        assert_eq!(
            resource_content_from_rmcp(blob),
            ResourceContent::Blob {
                uri: "resource://images/logo".to_string(),
                mime_type: Some("image/png".to_string()),
                blob: "iVBORw0KGgo=".to_string(),
                meta: Some(serde_json::json!({"encoding": "base64"})),
            }
        );
    }

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
