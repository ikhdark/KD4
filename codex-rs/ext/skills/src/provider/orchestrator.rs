use std::time::Duration;

use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::mcp::Resource;
use codex_protocol::mcp::ResourceContent;
use url::Url;

use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillProviderError;
use crate::catalog::SkillProviderErrorKind;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::provider::SkillListQuery;
use crate::provider::SkillProvider;
use crate::provider::SkillProviderFuture;
use crate::provider::SkillReadRequest;

const ORCHESTRATOR_SKILL_MIME_TYPE: &str = "mcp/skill";
const ORCHESTRATOR_SKILL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const ORCHESTRATOR_SKILL_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESOURCE_PAGES: usize = 10;
const MAX_ORCHESTRATOR_SKILLS: usize = 100;
const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_QUALIFIED_SKILL_NAME_CHARS: usize = 128;
const MAX_SKILL_PACKAGE_URI_CHARS: usize = 1_024;
const MAX_SKILL_RESOURCE_URI_CHARS: usize = 2_048;
const MAX_SKILL_RESOURCE_CONTENT_BYTES: usize = 1024 * 1024;

/// Discovers and reads skills owned by the orchestrator.
///
/// The provider uses session-scoped resources without exposing the transport or
/// resource server to callers that configure the skills extension.
#[derive(Clone, Debug, Default)]
pub struct OrchestratorSkillProvider;

impl OrchestratorSkillProvider {
    pub fn new() -> Self {
        Self
    }
}

impl SkillProvider for OrchestratorSkillProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            let Some(client) = query.mcp_resources else {
                return Ok(SkillCatalog::default());
            };
            if !client.has_server(CODEX_APPS_MCP_SERVER_NAME).await {
                return Ok(SkillCatalog::default());
            }

            Ok(discover(query.continuation, |cursor| {
                client.list_resources(CODEX_APPS_MCP_SERVER_NAME, cursor)
            })
            .await)
        })
    }

    fn read(&self, request: SkillReadRequest) -> SkillProviderFuture<'_, SkillReadResult> {
        Box::pin(async move {
            if request.authority
                != SkillAuthority::new(SkillSourceKind::Orchestrator, CODEX_APPS_MCP_SERVER_NAME)
            {
                return Err(SkillProviderError::new(format!(
                    "orchestrator skill provider cannot read authority {}",
                    request.authority.id
                ))
                .with_kind(SkillProviderErrorKind::InvalidResource));
            }
            if !resource_belongs_to_package(&request.package.0, request.resource.as_str()) {
                return Err(SkillProviderError::new(
                    "orchestrator skill resource does not match its package",
                )
                .with_kind(SkillProviderErrorKind::InvalidResource));
            }

            let Some(client) = request.mcp_resources.as_ref() else {
                return Err(SkillProviderError::new(
                    "session MCP resource client is not configured",
                ));
            };
            let result = tokio::time::timeout(
                ORCHESTRATOR_SKILL_READ_TIMEOUT,
                client.read_resource(CODEX_APPS_MCP_SERVER_NAME, request.resource.as_str()),
            )
            .await
            .map_err(|_| {
                SkillProviderError::new(format!(
                    "orchestrator skill read timed out after {ORCHESTRATOR_SKILL_READ_TIMEOUT:?}"
                ))
                .with_kind(SkillProviderErrorKind::Timeout)
            })?
            .map_err(|err| {
                SkillProviderError::new(format!(
                    "failed to read orchestrator skill resource {}: {err:#}",
                    request.resource.as_str()
                ))
                .with_kind(SkillProviderErrorKind::Transport)
            })?;
            let contents = result
                .contents
                .into_iter()
                .find_map(|contents| match contents {
                    ResourceContent::Text { uri, text, .. } if uri == request.resource.as_str() => {
                        Some(text)
                    }
                    ResourceContent::Text { .. } | ResourceContent::Blob { .. } => None,
                });
            let Some(contents) = contents else {
                return Err(SkillProviderError::new(format!(
                    "orchestrator skill resource {} did not return matching text contents",
                    request.resource.as_str()
                ))
                .with_kind(SkillProviderErrorKind::InvalidResponse));
            };
            if contents.len() > MAX_SKILL_RESOURCE_CONTENT_BYTES {
                return Err(SkillProviderError::new(format!(
                    "orchestrator skill resource {} exceeds the {MAX_SKILL_RESOURCE_CONTENT_BYTES}-byte read limit",
                    request.resource.as_str()
                )).with_kind(SkillProviderErrorKind::OversizedContent));
            }

            Ok(SkillReadResult {
                resource: request.resource,
                contents,
            })
        })
    }
}

async fn discover<F, Fut, E>(
    continuation: Option<crate::catalog::SkillDiscoveryContinuation>,
    mut list_page: F,
) -> SkillCatalog
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<codex_mcp::McpResourcePage, E>>,
{
    let discovery_deadline = tokio::time::Instant::now() + ORCHESTRATOR_SKILL_DISCOVERY_TIMEOUT;
    let mut catalog = SkillCatalog::default();
    let mut progress = continuation.unwrap_or_default();
    let mut skill_resources_seen = 0usize;
    let mut skipped_resources = 0usize;
    for page_index in 0..MAX_RESOURCE_PAGES {
        let page =
            match tokio::time::timeout_at(discovery_deadline, list_page(progress.cursor.clone()))
                .await
            {
                Ok(result) => result.map_err(|_| {
                    SkillProviderError::new(
                        "Orchestrator skill discovery is unavailable; continue listing to retry.",
                    )
                    .with_kind(SkillProviderErrorKind::Transport)
                }),
                Err(_) => Err(SkillProviderError::new(
                    "Orchestrator skill discovery timed out; continue listing to retry.",
                )
                .with_kind(SkillProviderErrorKind::Timeout)),
            };
        let result = match page {
            Ok(result) => result,
            Err(error) => {
                catalog.warnings.push(error.message);
                catalog.continuation = Some(progress);
                break;
            }
        };
        for (index, resource) in result
            .resources
            .iter()
            .enumerate()
            .skip(progress.resource_offset)
        {
            if resource.mime_type.as_deref() != Some(ORCHESTRATOR_SKILL_MIME_TYPE) {
                continue;
            }
            if skill_resources_seen == MAX_ORCHESTRATOR_SKILLS {
                progress.resource_offset = index;
                catalog.continuation = Some(progress.clone());
                break;
            }
            skill_resources_seen += 1;
            match catalog_entry_from_resource(resource) {
                Some(entry) => catalog.push_entry(entry),
                None => skipped_resources += 1,
            }
        }
        if catalog.continuation.is_some() {
            break;
        }
        let Some(next_cursor) = result.next_cursor else {
            break;
        };
        if !progress.seen_cursors.insert(next_cursor.clone()) {
            catalog.warnings.push("Orchestrator skill pagination repeated a cursor; discovery cannot advance on this provider response.".to_string());
            catalog.continuation = Some(progress);
            break;
        }
        progress.cursor = Some(next_cursor);
        progress.resource_offset = 0;
        if page_index + 1 == MAX_RESOURCE_PAGES || skill_resources_seen == MAX_ORCHESTRATOR_SKILLS {
            catalog.continuation = Some(progress);
            break;
        }
    }
    if catalog.continuation.is_some() {
        catalog.warnings.push("Orchestrator skill discovery is incomplete. Follow skills.list next_cursor to continue discovery.".to_string());
    }
    if skipped_resources > 0 {
        catalog.warnings.push(format!(
            "Skipped {skipped_resources} malformed orchestrator skill resources."
        ));
    }

    catalog
}

fn catalog_entry_from_resource(resource: &Resource) -> Option<SkillCatalogEntry> {
    let uri = validated_skill_uri(resource.uri.as_str(), MAX_SKILL_PACKAGE_URI_CHARS)?;
    let meta = resource.meta.as_ref()?.as_object()?;
    let skill_name = normalized_label(meta.get("skill_name")?.as_str()?, MAX_SKILL_NAME_CHARS)?;
    let name = if meta.get("source").and_then(|value| value.as_str()) == Some("user") {
        skill_name
    } else {
        let plugin_name =
            normalized_label(meta.get("plugin_name")?.as_str()?, MAX_SKILL_NAME_CHARS)?;
        let qualified_name = format!("{plugin_name}:{skill_name}");
        (qualified_name.len() <= MAX_QUALIFIED_SKILL_NAME_CHARS
            || qualified_name
                .chars()
                .nth(MAX_QUALIFIED_SKILL_NAME_CHARS)
                .is_none())
        .then_some(qualified_name)?
    };
    let description = normalized_description(resource.description.as_deref().unwrap_or_default())?;
    let main_prompt = main_prompt_uri(uri);

    Some(
        SkillCatalogEntry::new(
            SkillPackageId(uri.to_string()),
            SkillAuthority::new(SkillSourceKind::Orchestrator, CODEX_APPS_MCP_SERVER_NAME),
            name,
            description,
            SkillResourceId::new(main_prompt),
        )
        .with_display_path(uri),
    )
}

fn validated_skill_uri(uri: &str, max_chars: usize) -> Option<&str> {
    validated_skill_url(uri, max_chars).map(|_| uri)
}

fn validated_skill_url(uri: &str, max_chars: usize) -> Option<Url> {
    if uri.chars().nth(max_chars).is_some()
        || uri
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '<' | '>'))
    {
        return None;
    }

    let url = Url::parse(uri).ok()?;
    let path_is_valid = url.path_segments().is_some_and(|segments| {
        let segments = segments.collect::<Vec<_>>();
        !segments.is_empty() && segments.iter().all(|segment| !segment.is_empty())
    });
    (url.scheme() == "skill"
        && url.as_str() == uri
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && path_is_valid)
        .then_some(url)
}

fn resource_belongs_to_package(package: &str, resource: &str) -> bool {
    let Some(package) = validated_skill_url(package, MAX_SKILL_PACKAGE_URI_CHARS) else {
        return false;
    };
    let Some(resource) = validated_skill_url(resource, MAX_SKILL_RESOURCE_URI_CHARS) else {
        return false;
    };

    let Some(package_segments) = package.path_segments() else {
        return false;
    };
    let Some(resource_segments) = resource.path_segments() else {
        return false;
    };
    let package_segments = package_segments.collect::<Vec<_>>();
    let resource_segments = resource_segments.collect::<Vec<_>>();

    package.scheme() == resource.scheme()
        && package.host_str() == resource.host_str()
        && resource_segments.len() > package_segments.len()
        && resource_segments.starts_with(&package_segments)
}

fn normalized_label(value: &str, max_chars: usize) -> Option<String> {
    let value = normalized_single_line(value, max_chars)?;
    let invalid = value.is_empty() || value.chars().any(|ch| matches!(ch, '&' | '<' | '>'));
    (!invalid).then_some(value)
}

fn normalized_description(value: &str) -> Option<String> {
    // Bound normalization before allocating or escaping a potentially huge description.
    let mut output = String::new();
    let mut pending_space = false;
    let mut count = 0;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if ch.is_control() {
            return None;
        }
        let escaped = match ch {
            '&' => Some("&amp;"),
            '<' => Some("&lt;"),
            '>' => Some("&gt;"),
            _ => None,
        };
        let added = escaped.map_or(1, str::len) + usize::from(pending_space);
        if count + added > 1_021 {
            output.push_str("...");
            break;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        if let Some(escaped) = escaped {
            output.push_str(escaped);
        } else {
            output.push(ch);
        }
        count += added;
    }
    Some(output)
}

fn normalized_single_line(value: &str, max_chars: usize) -> Option<String> {
    let mut output = String::new();
    let mut pending_space = false;
    let mut count = 0;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if ch.is_control() {
            return None;
        }
        count += 1 + usize::from(pending_space);
        if count > max_chars {
            return None;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(ch);
    }
    Some(output)
}

fn main_prompt_uri(package_uri: &str) -> String {
    format!("{}/SKILL.md", package_uri.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(start: usize, count: usize, next: Option<&str>) -> codex_mcp::McpResourcePage {
        codex_mcp::McpResourcePage {
            resources: (start..start + count).map(|index| serde_json::from_value(serde_json::json!({
                "uri":format!("skill://plugin/skill-{index}"), "name":format!("skill-{index}"), "mimeType":"mcp/skill",
                "_meta":{"skill_name":format!("skill-{index}"),"source":"user"}
            })).expect("resource")).collect(),
            next_cursor: next.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn discovery_resumes_after_page_failure_and_mid_page_limit() {
        let mut calls = 0;
        let prefix = discover(None, |cursor| {
            calls += 1;
            std::future::ready(if cursor.is_none() {
                Ok(page(0, 1, Some("page-two")))
            } else {
                Err(())
            })
        })
        .await;
        assert_eq!(calls, 2);
        assert_eq!(prefix.entries.len(), 1);
        let resumed = discover(prefix.continuation, |cursor| {
            assert_eq!(cursor.as_deref(), Some("page-two"));
            std::future::ready(Ok::<_, ()>(page(1, 1, None)))
        })
        .await;
        assert_eq!(resumed.entries[0].name, "skill-1");
        assert!(resumed.continuation.is_none());

        let first = discover(None, |_| {
            std::future::ready(Ok::<_, ()>(page(0, 101, None)))
        })
        .await;
        assert_eq!(first.entries.len(), 100);
        let continuation = first.continuation.expect("remaining resource");
        assert_eq!(continuation.resource_offset, 100);
        let last = discover(Some(continuation), |_| {
            std::future::ready(Ok::<_, ()>(page(0, 101, None)))
        })
        .await;
        assert_eq!(last.entries.len(), 1);
        assert_eq!(last.entries[0].name, "skill-100");
        assert!(last.continuation.is_none());
    }

    #[tokio::test]
    async fn page_budget_and_duplicate_cursors_remain_bounded_and_recoverable() {
        let mut calls = 0;
        let first = discover(None, |_| {
            let index = calls;
            calls += 1;
            std::future::ready(Ok::<_, ()>(page(index, 1, Some(&calls.to_string()))))
        })
        .await;
        assert_eq!(calls, MAX_RESOURCE_PAGES);
        assert_eq!(first.entries.len(), MAX_RESOURCE_PAGES);
        let last = discover(first.continuation, |cursor| {
            assert_eq!(cursor.as_deref(), Some("10"));
            std::future::ready(Ok::<_, ()>(page(10, 1, None)))
        })
        .await;
        assert_eq!(last.entries[0].name, "skill-10");
        assert!(last.continuation.is_none());
        let mut calls = 0;
        let repeated = discover(None, |_| {
            calls += 1;
            std::future::ready(Ok::<_, ()>(page(0, 1, Some("same"))))
        })
        .await;
        assert_eq!(calls, 2);
        assert!(repeated.continuation.is_some());
        assert!(
            repeated
                .warnings
                .iter()
                .any(|warning| warning.contains("repeated a cursor"))
        );
    }

    #[test]
    fn resource_metadata_is_bounded_and_escaped_before_entering_catalog() {
        let mut resource: Resource = serde_json::from_value(serde_json::json!({
            "uri": "skill://plugin/demo", "name": "demo", "mimeType": "mcp/skill",
            "description": format!("  A & <B>\n{}", "🚀".repeat(100_000)),
            "_meta": {"skill_name": " demo ", "source": "user"}
        }))
        .expect("valid resource");
        let entry = catalog_entry_from_resource(&resource).expect("valid skill");
        assert_eq!(entry.name, "demo");
        assert_eq!(entry.main_prompt.as_str(), "skill://plugin/demo/SKILL.md");
        assert!(entry.description.starts_with("A &amp; &lt;B&gt; "));
        assert!(entry.description.ends_with("..."));
        assert!(entry.description.chars().count() <= 1_024);
        resource.meta =
            Some(serde_json::json!({"skill_name": "x".repeat(100_000), "source": "user"}));
        assert!(catalog_entry_from_resource(&resource).is_none());
    }
}
