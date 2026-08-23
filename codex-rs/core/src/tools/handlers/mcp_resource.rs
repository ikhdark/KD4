use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpServerCollection;
use codex_mcp::McpServerCollectionError;
use codex_protocol::items::McpToolCallError;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::CanonicalToolResult;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputProjectionJsonPointer;
use codex_tools::ToolOutputProjectionMetadata;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use rmcp::model::ListResourceTemplatesResult;
use rmcp::model::ListResourcesResult;
use rmcp::model::ReadResourceResult;
use rmcp::model::Resource;
use rmcp::model::ResourceContents;
use rmcp::model::ResourceTemplate;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use codex_protocol::protocol::McpInvocation;

const MCP_RESOURCE_CALL_CANCELLED_MESSAGE: &str = "MCP resource call cancelled";
const MAX_PREDETERMINED_MCP_RESOURCE_POINTERS: usize = 64;

struct McpResourceToolOutput {
    visible: FunctionToolOutput,
    canonical: Value,
}

impl ToolOutput for McpResourceToolOutput {
    fn log_preview(&self) -> String {
        self.visible.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.visible.success_for_logging()
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        self.visible.outcome_for_logging()
    }

    fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        self.visible.outcome_context()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        let mut metadata = self.visible.projection_metadata()?;
        metadata.merge_essential_from_json(&self.canonical);
        if let Some(visible) = metadata
            .spillable_text
            .first()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
        {
            metadata.predetermined_json_pointers =
                mcp_resource_recovery_json_pointers(&visible, &self.canonical);
        }
        Some(metadata)
    }

    fn requires_canonical_artifact(&self) -> bool {
        true
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(CanonicalToolResult::json(self.canonical.clone()))
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.visible.to_response_item(call_id, payload)
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<Value> {
        self.visible.post_tool_use_response(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.visible.code_mode_result(payload)
    }
}

fn mcp_resource_recovery_json_pointers(
    visible: &Value,
    canonical: &Value,
) -> Vec<ToolOutputProjectionJsonPointer> {
    let mut selectors = Vec::new();
    for key in ["resources", "resourceTemplates", "errors"] {
        append_omitted_array_entry_pointers(&mut selectors, visible, canonical, key);
    }
    append_omitted_content_pointers(&mut selectors, visible, canonical);
    selectors
}

fn append_omitted_array_entry_pointers(
    selectors: &mut Vec<ToolOutputProjectionJsonPointer>,
    visible: &Value,
    canonical: &Value,
    key: &str,
) {
    let Some(canonical_values) = canonical.get(key).and_then(Value::as_array) else {
        return;
    };
    let visible_values = visible
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut matched_visible = vec![false; visible_values.len()];
    for (index, canonical_value) in canonical_values.iter().enumerate() {
        if let Some(visible_index) =
            visible_values
                .iter()
                .enumerate()
                .position(|(visible_index, visible_value)| {
                    !matched_visible[visible_index] && visible_value == canonical_value
                })
        {
            matched_visible[visible_index] = true;
            continue;
        }
        push_mcp_resource_pointer(
            selectors,
            format!("mcp-resource:{key}:{index}"),
            format!("/{key}/{index}"),
        );
    }
}

fn append_omitted_content_pointers(
    selectors: &mut Vec<ToolOutputProjectionJsonPointer>,
    visible: &Value,
    canonical: &Value,
) {
    let Some(canonical_contents) = canonical.get("contents").and_then(Value::as_array) else {
        return;
    };
    let visible_contents = visible
        .get("contents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (index, canonical_content) in canonical_contents.iter().enumerate() {
        let Some(visible_content) = visible_contents.get(index) else {
            push_mcp_resource_pointer(
                selectors,
                format!("mcp-resource:contents:{index}"),
                format!("/contents/{index}"),
            );
            continue;
        };
        if visible_content == canonical_content {
            continue;
        }
        let field = if canonical_content.get("blob").is_some()
            && visible_content.get("blob") != canonical_content.get("blob")
        {
            Some("blob")
        } else if canonical_content.get("text").is_some()
            && visible_content.get("text") != canonical_content.get("text")
        {
            Some("text")
        } else {
            None
        };
        let (id, pointer) = field.map_or_else(
            || {
                (
                    format!("mcp-resource:contents:{index}"),
                    format!("/contents/{index}"),
                )
            },
            |field| {
                (
                    format!("mcp-resource:contents:{index}:{field}"),
                    format!("/contents/{index}/{field}"),
                )
            },
        );
        push_mcp_resource_pointer(selectors, id, pointer);
    }
}

fn push_mcp_resource_pointer(
    selectors: &mut Vec<ToolOutputProjectionJsonPointer>,
    id: String,
    pointer: String,
) {
    if selectors.len() < MAX_PREDETERMINED_MCP_RESOURCE_POINTERS {
        selectors.push(ToolOutputProjectionJsonPointer { id, pointer });
    }
}

mod list_mcp_resource_templates;
mod list_mcp_resources;
mod read_mcp_resource;

pub use list_mcp_resource_templates::ListMcpResourceTemplatesHandler;
pub use list_mcp_resources::ListMcpResourcesHandler;
pub use read_mcp_resource::ReadMcpResourceHandler;

fn model_can_access_mcp_server(turn: &TurnContext, server: &str) -> bool {
    turn.config.orchestrator_mcp_enabled || server != CODEX_APPS_MCP_SERVER_NAME
}

fn ensure_model_can_access_mcp_server(
    turn: &TurnContext,
    server: &str,
) -> Result<(), FunctionCallError> {
    if model_can_access_mcp_server(turn, server) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "MCP server '{server}' is disabled by `orchestrator.mcp.enabled`"
        )))
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListResourcesArgs {
    /// Lists all resources from all servers if not specified.
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

impl ListResourcesArgs {
    fn normalize(self) -> Result<Self, FunctionCallError> {
        Ok(Self {
            server: normalize_optional_selector("server", self.server)?,
            cursor: validate_optional_opaque_selector("cursor", self.cursor)?,
        })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListResourceTemplatesArgs {
    /// Lists all resource templates from all servers if not specified.
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

impl ListResourceTemplatesArgs {
    fn normalize(self) -> Result<Self, FunctionCallError> {
        Ok(Self {
            server: normalize_optional_selector("server", self.server)?,
            cursor: validate_optional_opaque_selector("cursor", self.cursor)?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ReadResourceArgs {
    server: String,
    uri: String,
}

#[derive(Debug, Serialize)]
struct ResourceWithServer {
    server: String,
    #[serde(flatten)]
    resource: Resource,
}

impl ResourceWithServer {
    fn new(server: String, resource: Resource) -> Self {
        Self { server, resource }
    }
}

#[derive(Debug, Serialize)]
struct ResourceTemplateWithServer {
    server: String,
    #[serde(flatten)]
    template: ResourceTemplate,
}

impl ResourceTemplateWithServer {
    fn new(server: String, template: ResourceTemplate) -> Self {
        Self { server, template }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResourcesPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    resources: Vec<ResourceWithServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    next_cursors: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    remaining_servers: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    truncated: bool,
    #[serde(skip_serializing_if = "is_zero")]
    omitted_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<McpResourceServerError>,
    #[serde(skip_serializing_if = "is_zero")]
    omitted_error_count: usize,
}

impl ListResourcesPayload {
    fn from_single_server(
        server: String,
        result: ListResourcesResult,
        truncation_policy: TruncationPolicy,
    ) -> Result<Self, FunctionCallError> {
        let total_resources = result.resources.len();
        let mut payload = Self {
            server: Some(server.clone()),
            resources: Vec::new(),
            next_cursor: result.next_cursor,
            next_cursors: BTreeMap::new(),
            remaining_servers: Vec::new(),
            truncated: total_resources > 0,
            omitted_count: total_resources,
            errors: Vec::new(),
            omitted_error_count: 0,
        };

        ensure_payload_metadata_fits(&payload, truncation_policy)?;
        for resource in result.resources {
            payload
                .resources
                .push(ResourceWithServer::new(server.clone(), resource));
            payload.omitted_count -= 1;
            payload.truncated = payload.omitted_count > 0;
            if !serialized_payload_fits(&payload, truncation_policy)? {
                payload.resources.pop();
                payload.omitted_count += 1;
                payload.truncated = true;
                break;
            }
        }

        Ok(payload)
    }

    fn from_all_servers(
        collection: McpServerCollection<ListResourcesResult>,
        truncation_policy: TruncationPolicy,
    ) -> Result<Self, FunctionCallError> {
        if collection.results.is_empty() && !collection.errors.is_empty() {
            return Err(all_servers_failed("list MCP resources", &collection.errors));
        }

        let mut entries: Vec<(String, ListResourcesResult)> =
            collection.results.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let total_resources = entries.iter().map(|(_, page)| page.resources.len()).sum();
        let mut errors: Vec<McpResourceServerError> = collection
            .errors
            .into_iter()
            .map(McpResourceServerError::from)
            .collect();
        errors.sort_by(|a, b| a.server.cmp(&b.server));

        let mut payload = Self {
            server: None,
            resources: Vec::new(),
            next_cursor: None,
            next_cursors: BTreeMap::new(),
            remaining_servers: entries.iter().map(|(server, _)| server.clone()).collect(),
            truncated: !entries.is_empty() || !errors.is_empty(),
            omitted_count: total_resources,
            errors: Vec::new(),
            omitted_error_count: errors.len(),
        };
        ensure_payload_metadata_fits(&payload, truncation_policy)?;
        let mut estimated_size = serialized_candidate_cost(&payload, truncation_policy)?;
        let serialized_budget = conservative_serialized_budget(truncation_policy);

        for error in errors {
            let candidate_cost =
                serialized_candidate_cost(&error, truncation_policy)?.saturating_add(16);
            if estimated_size.saturating_add(candidate_cost) > serialized_budget {
                break;
            }
            estimated_size = estimated_size.saturating_add(candidate_cost);
            payload.omitted_error_count -= 1;
            payload.errors.push(error);
        }
        for (server, page) in entries {
            let Ok(remaining_index) = payload.remaining_servers.binary_search(&server) else {
                continue;
            };
            let page_resource_count = page.resources.len();
            let resources: Vec<_> = page
                .resources
                .into_iter()
                .map(|resource| ResourceWithServer::new(server.clone(), resource))
                .collect();
            let mut candidate_cost =
                serialized_candidate_cost(&resources, truncation_policy)?.saturating_add(16);
            if let Some(next_cursor) = page.next_cursor.as_ref() {
                candidate_cost = candidate_cost
                    .saturating_add(serialized_candidate_cost(
                        &(&server, next_cursor),
                        truncation_policy,
                    )?)
                    .saturating_add(16);
            }
            if estimated_size.saturating_add(candidate_cost) > serialized_budget {
                continue;
            }

            estimated_size = estimated_size.saturating_add(candidate_cost);
            payload.remaining_servers.remove(remaining_index);
            payload.omitted_count -= page_resource_count;
            payload.resources.extend(resources);
            if let Some(next_cursor) = page.next_cursor {
                payload.next_cursors.insert(server, next_cursor);
            }
        }
        payload.truncated = payload.omitted_count > 0
            || payload.omitted_error_count > 0
            || !payload.remaining_servers.is_empty();
        ensure_payload_metadata_fits(&payload, truncation_policy)?;
        Ok(payload)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpResourceServerError {
    server: String,
    message: String,
}

impl From<McpServerCollectionError> for McpResourceServerError {
    fn from(error: McpServerCollectionError) -> Self {
        Self {
            server: error.server,
            message: error.message,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResourceTemplatesPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    resource_templates: Vec<ResourceTemplateWithServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    next_cursors: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    remaining_servers: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    truncated: bool,
    #[serde(skip_serializing_if = "is_zero")]
    omitted_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<McpResourceServerError>,
    #[serde(skip_serializing_if = "is_zero")]
    omitted_error_count: usize,
}

impl ListResourceTemplatesPayload {
    fn from_single_server(
        server: String,
        result: ListResourceTemplatesResult,
        truncation_policy: TruncationPolicy,
    ) -> Result<Self, FunctionCallError> {
        let total_templates = result.resource_templates.len();
        let mut payload = Self {
            server: Some(server.clone()),
            resource_templates: Vec::new(),
            next_cursor: result.next_cursor,
            next_cursors: BTreeMap::new(),
            remaining_servers: Vec::new(),
            truncated: total_templates > 0,
            omitted_count: total_templates,
            errors: Vec::new(),
            omitted_error_count: 0,
        };
        ensure_payload_metadata_fits(&payload, truncation_policy)?;
        for template in result.resource_templates {
            payload
                .resource_templates
                .push(ResourceTemplateWithServer::new(server.clone(), template));
            payload.omitted_count -= 1;
            payload.truncated = payload.omitted_count > 0;
            if !serialized_payload_fits(&payload, truncation_policy)? {
                payload.resource_templates.pop();
                payload.omitted_count += 1;
                payload.truncated = true;
                break;
            }
        }
        Ok(payload)
    }

    fn from_all_servers(
        collection: McpServerCollection<ListResourceTemplatesResult>,
        truncation_policy: TruncationPolicy,
    ) -> Result<Self, FunctionCallError> {
        if collection.results.is_empty() && !collection.errors.is_empty() {
            return Err(all_servers_failed(
                "list MCP resource templates",
                &collection.errors,
            ));
        }

        let mut entries: Vec<(String, ListResourceTemplatesResult)> =
            collection.results.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let total_templates = entries
            .iter()
            .map(|(_, page)| page.resource_templates.len())
            .sum();
        let mut errors: Vec<McpResourceServerError> = collection
            .errors
            .into_iter()
            .map(McpResourceServerError::from)
            .collect();
        errors.sort_by(|a, b| a.server.cmp(&b.server));

        let mut payload = Self {
            server: None,
            resource_templates: Vec::new(),
            next_cursor: None,
            next_cursors: BTreeMap::new(),
            remaining_servers: entries.iter().map(|(server, _)| server.clone()).collect(),
            truncated: !entries.is_empty() || !errors.is_empty(),
            omitted_count: total_templates,
            errors: Vec::new(),
            omitted_error_count: errors.len(),
        };
        ensure_payload_metadata_fits(&payload, truncation_policy)?;
        let mut estimated_size = serialized_candidate_cost(&payload, truncation_policy)?;
        let serialized_budget = conservative_serialized_budget(truncation_policy);
        for error in errors {
            let candidate_cost =
                serialized_candidate_cost(&error, truncation_policy)?.saturating_add(16);
            if estimated_size.saturating_add(candidate_cost) > serialized_budget {
                break;
            }
            estimated_size = estimated_size.saturating_add(candidate_cost);
            payload.omitted_error_count -= 1;
            payload.errors.push(error);
        }

        for (server, page) in entries {
            let Ok(remaining_index) = payload.remaining_servers.binary_search(&server) else {
                continue;
            };
            let page_template_count = page.resource_templates.len();
            let templates: Vec<_> = page
                .resource_templates
                .into_iter()
                .map(|template| ResourceTemplateWithServer::new(server.clone(), template))
                .collect();
            let mut candidate_cost =
                serialized_candidate_cost(&templates, truncation_policy)?.saturating_add(16);
            if let Some(next_cursor) = page.next_cursor.as_ref() {
                candidate_cost = candidate_cost
                    .saturating_add(serialized_candidate_cost(
                        &(&server, next_cursor),
                        truncation_policy,
                    )?)
                    .saturating_add(16);
            }
            if estimated_size.saturating_add(candidate_cost) > serialized_budget {
                continue;
            }

            estimated_size = estimated_size.saturating_add(candidate_cost);
            payload.remaining_servers.remove(remaining_index);
            payload.omitted_count -= page_template_count;
            payload.resource_templates.extend(templates);
            if let Some(next_cursor) = page.next_cursor {
                payload.next_cursors.insert(server, next_cursor);
            }
        }

        payload.truncated = payload.omitted_count > 0
            || payload.omitted_error_count > 0
            || !payload.remaining_servers.is_empty();
        ensure_payload_metadata_fits(&payload, truncation_policy)?;
        Ok(payload)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadResourcePayload {
    server: String,
    uri: String,
    contents: Vec<BoundedResourceContents>,
    #[serde(skip_serializing_if = "is_false")]
    truncated: bool,
    #[serde(skip_serializing_if = "is_zero")]
    omitted_count: usize,
}

impl ReadResourcePayload {
    fn new(
        server: String,
        uri: String,
        result: ReadResourceResult,
        truncation_policy: TruncationPolicy,
    ) -> Result<Self, FunctionCallError> {
        let total_contents = result.contents.len();
        let mut payload = Self {
            server,
            uri,
            contents: Vec::new(),
            truncated: total_contents > 0,
            omitted_count: total_contents,
        };
        ensure_payload_metadata_fits(&payload, truncation_policy)?;
        let mut content_was_bounded = false;

        for content in result.contents {
            payload.omitted_count -= 1;
            match content {
                ResourceContents::TextResourceContents {
                    uri,
                    mime_type,
                    text,
                    meta,
                } => {
                    let full = ResourceContents::TextResourceContents {
                        uri: uri.clone(),
                        mime_type: mime_type.clone(),
                        text: text.clone(),
                        meta: meta.clone(),
                    };
                    payload
                        .contents
                        .push(BoundedResourceContents::Complete(full));
                    if serialized_payload_fits(&payload, truncation_policy)? {
                        continue;
                    }
                    payload.contents.pop();

                    let bounded = fit_text_resource_content(
                        &mut payload,
                        uri,
                        mime_type,
                        text,
                        meta,
                        truncation_policy,
                    )?;
                    if let Some(content) = bounded {
                        payload
                            .contents
                            .push(BoundedResourceContents::Complete(content));
                        content_was_bounded = true;
                    } else {
                        payload.omitted_count += 1;
                    }
                    break;
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    meta,
                } => {
                    let full = ResourceContents::BlobResourceContents {
                        uri: uri.clone(),
                        mime_type: mime_type.clone(),
                        blob,
                        meta: meta.clone(),
                    };
                    payload
                        .contents
                        .push(BoundedResourceContents::Complete(full));
                    if serialized_payload_fits(&payload, truncation_policy)? {
                        continue;
                    }
                    payload.contents.pop();
                    payload.contents.push(BoundedResourceContents::OmittedBlob(
                        OmittedBlobResourceContents {
                            uri,
                            mime_type,
                            omitted: true,
                            reason: "blob content exceeded the MCP resource output budget",
                            meta,
                        },
                    ));
                    if serialized_payload_fits(&payload, truncation_policy)? {
                        content_was_bounded = true;
                    } else {
                        payload.contents.pop();
                        payload.omitted_count += 1;
                    }
                    break;
                }
            }
        }

        payload.truncated = content_was_bounded || payload.omitted_count > 0;
        Ok(payload)
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum BoundedResourceContents {
    Complete(ResourceContents),
    OmittedBlob(OmittedBlobResourceContents),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OmittedBlobResourceContents {
    uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
    omitted: bool,
    reason: &'static str,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<rmcp::model::Meta>,
}

fn fit_text_resource_content(
    payload: &mut ReadResourcePayload,
    uri: String,
    mime_type: Option<String>,
    text: String,
    meta: Option<rmcp::model::Meta>,
    truncation_policy: TruncationPolicy,
) -> Result<Option<ResourceContents>, FunctionCallError> {
    let output_policy = truncation_policy * 1.2;
    let mut low = 0;
    let mut high = truncation_policy_limit(output_policy);
    let mut best = None;

    while low <= high {
        let limit = low + (high - low) / 2;
        let bounded_text = truncate_text(&text, truncation_policy_with_limit(output_policy, limit));
        let candidate = ResourceContents::TextResourceContents {
            uri: uri.clone(),
            mime_type: mime_type.clone(),
            text: bounded_text,
            meta: meta.clone(),
        };
        payload
            .contents
            .push(BoundedResourceContents::Complete(candidate.clone()));
        let fits = serialized_payload_fits(payload, truncation_policy)?;
        payload.contents.pop();

        if fits {
            best = Some(candidate);
            if limit == truncation_policy_limit(output_policy) {
                break;
            }
            low = limit + 1;
        } else if limit == 0 {
            break;
        } else {
            high = limit - 1;
        }
    }

    Ok(best)
}

fn truncation_policy_limit(policy: TruncationPolicy) -> usize {
    match policy {
        TruncationPolicy::Bytes(limit) | TruncationPolicy::Tokens(limit) => limit,
    }
}

fn truncation_policy_with_limit(policy: TruncationPolicy, limit: usize) -> TruncationPolicy {
    match policy {
        TruncationPolicy::Bytes(_) => TruncationPolicy::Bytes(limit),
        TruncationPolicy::Tokens(_) => TruncationPolicy::Tokens(limit),
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn ensure_payload_metadata_fits<T>(
    payload: &T,
    truncation_policy: TruncationPolicy,
) -> Result<(), FunctionCallError>
where
    T: Serialize,
{
    if serialized_payload_fits(payload, truncation_policy)? {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(
            "MCP resource response metadata exceeds the output budget; narrow the request to one server or resource"
                .to_string(),
        ))
    }
}

fn all_servers_failed(action: &str, errors: &[McpServerCollectionError]) -> FunctionCallError {
    let mut errors = errors.to_vec();
    errors.sort_by(|a, b| a.server.cmp(&b.server));
    let mut details: Vec<String> = errors
        .iter()
        .take(3)
        .map(|error| format!("{}: {}", error.server, error.message))
        .collect();
    if errors.len() > details.len() {
        details.push(format!(
            "{} additional server(s) failed",
            errors.len() - details.len()
        ));
    }
    FunctionCallError::RespondToModel(format!(
        "Failed to {action} from every selected server: {}",
        details.join("; ")
    ))
}

fn call_tool_result_from_content(content: &str, success: Option<bool>) -> CallToolResult {
    CallToolResult {
        content: vec![serde_json::json!({"type": "text", "text": content})],
        structured_content: None,
        is_error: success.map(|value| !value),
        meta: None,
    }
}

async fn emit_tool_call_begin(
    session: &Arc<Session>,
    turn: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
) {
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(Value::Null),
        connector_id: None,
        mcp_app_resource_uri: None,
        link_id: None,
        app_name: None,
        template_id: None,
        action_name: None,
        plugin_id: None,
        status: McpToolCallStatus::InProgress,
        result: None,
        error: None,
        duration: None,
    });
    session.emit_turn_item_started(turn, &item).await;
}

async fn emit_tool_call_end(
    session: &Arc<Session>,
    turn: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    duration: Duration,
    result: Result<CallToolResult, String>,
) {
    let (status, result, error) = match result {
        Ok(result) if result.is_error.unwrap_or(false) => {
            (McpToolCallStatus::Failed, Some(result), None)
        }
        Ok(result) => (McpToolCallStatus::Completed, Some(result), None),
        Err(message) => (
            McpToolCallStatus::Failed,
            None,
            Some(McpToolCallError { message }),
        ),
    };
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(Value::Null),
        connector_id: None,
        mcp_app_resource_uri: None,
        link_id: None,
        app_name: None,
        template_id: None,
        action_name: None,
        plugin_id: None,
        status,
        result,
        error,
        duration: Some(duration),
    });
    session.emit_turn_item_completed(turn, item).await;
}

async fn execute_resource_call<F>(
    session: &Arc<Session>,
    turn: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    cancellation_token: CancellationToken,
    operation: F,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError>
where
    F: Future<Output = Result<McpResourceToolOutput, FunctionCallError>>,
{
    emit_tool_call_begin(session, turn, call_id, invocation.clone()).await;
    let start = Instant::now();
    tokio::pin!(operation);
    let result = tokio::select! {
        biased;
        result = &mut operation => result,
        _ = cancellation_token.cancelled() => Err(FunctionCallError::RespondToModel(
            MCP_RESOURCE_CALL_CANCELLED_MESSAGE.to_string(),
        )),
    };

    let terminal_result = match result.as_ref() {
        Ok(output) => {
            let content = function_call_output_content_items_to_text(&output.visible.body)
                .unwrap_or_default();
            Ok(call_tool_result_from_content(
                &content,
                output.visible.success,
            ))
        }
        Err(err) => Err(err.to_string()),
    };
    emit_tool_call_end(
        session,
        turn,
        call_id,
        invocation,
        start.elapsed(),
        terminal_result,
    )
    .await;

    result.map(boxed_tool_output)
}

fn normalize_optional_string(input: Option<String>) -> Option<String> {
    input.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn normalize_required_string(field: &str, value: String) -> Result<String, FunctionCallError> {
    match normalize_optional_string(Some(value)) {
        Some(normalized) => Ok(normalized),
        None => Err(FunctionCallError::RespondToModel(format!(
            "{field} must be provided"
        ))),
    }
}

fn normalize_optional_selector(
    field: &str,
    value: Option<String>,
) -> Result<Option<String>, FunctionCallError> {
    value
        .map(|value| {
            normalize_optional_string(Some(value)).ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "{field} must not be blank; omit it to use the default behavior"
                ))
            })
        })
        .transpose()
}

fn validate_optional_opaque_selector(
    field: &str,
    value: Option<String>,
) -> Result<Option<String>, FunctionCallError> {
    value
        .map(|value| {
            if value.trim().is_empty() {
                Err(FunctionCallError::RespondToModel(format!(
                    "{field} must not be blank; omit it to use the default behavior"
                )))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn serialize_function_output<T>(
    payload: T,
    canonical: Value,
    truncation_policy: TruncationPolicy,
) -> Result<McpResourceToolOutput, FunctionCallError>
where
    T: Serialize,
{
    let content = serialize_resource_payload(&payload)?;
    if truncate_text(&content, truncation_policy * 1.2) != content {
        return Err(FunctionCallError::RespondToModel(
            "MCP resource response exceeds the output budget; narrow the request".to_string(),
        ));
    }

    Ok(McpResourceToolOutput {
        visible: FunctionToolOutput::from_text(content, Some(true)),
        canonical,
    })
}

fn canonical_single_list_resources(
    server: &str,
    result: &ListResourcesResult,
) -> Result<Value, FunctionCallError> {
    let resources = result
        .resources
        .iter()
        .map(|resource| serialize_value_with_server(server, resource))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({
        "server": server,
        "resources": resources,
        "nextCursor": result.next_cursor,
    }))
}

fn canonical_all_list_resources(
    collection: &McpServerCollection<ListResourcesResult>,
) -> Result<Value, FunctionCallError> {
    let mut entries = collection.results.iter().collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut resources = Vec::new();
    let mut next_cursors = BTreeMap::new();
    for (server, page) in entries {
        for resource in &page.resources {
            resources.push(serialize_value_with_server(server, resource)?);
        }
        if let Some(cursor) = &page.next_cursor {
            next_cursors.insert(server.clone(), cursor.clone());
        }
    }
    let mut errors = collection
        .errors
        .iter()
        .map(|error| McpResourceServerError {
            server: error.server.clone(),
            message: error.message.clone(),
        })
        .collect::<Vec<_>>();
    errors.sort_by(|a, b| a.server.cmp(&b.server));
    Ok(serde_json::json!({
        "resources": resources,
        "nextCursors": next_cursors,
        "errors": errors,
    }))
}

fn canonical_single_list_resource_templates(
    server: &str,
    result: &ListResourceTemplatesResult,
) -> Result<Value, FunctionCallError> {
    let resource_templates = result
        .resource_templates
        .iter()
        .map(|template| serialize_value_with_server(server, template))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({
        "server": server,
        "resourceTemplates": resource_templates,
        "nextCursor": result.next_cursor,
    }))
}

fn canonical_all_list_resource_templates(
    collection: &McpServerCollection<ListResourceTemplatesResult>,
) -> Result<Value, FunctionCallError> {
    let mut entries = collection.results.iter().collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut resource_templates = Vec::new();
    let mut next_cursors = BTreeMap::new();
    for (server, page) in entries {
        for template in &page.resource_templates {
            resource_templates.push(serialize_value_with_server(server, template)?);
        }
        if let Some(cursor) = &page.next_cursor {
            next_cursors.insert(server.clone(), cursor.clone());
        }
    }
    let mut errors = collection
        .errors
        .iter()
        .map(|error| McpResourceServerError {
            server: error.server.clone(),
            message: error.message.clone(),
        })
        .collect::<Vec<_>>();
    errors.sort_by(|a, b| a.server.cmp(&b.server));
    Ok(serde_json::json!({
        "resourceTemplates": resource_templates,
        "nextCursors": next_cursors,
        "errors": errors,
    }))
}

fn canonical_read_resource(
    server: &str,
    uri: &str,
    result: &ReadResourceResult,
) -> Result<Value, FunctionCallError> {
    let contents = serde_json::to_value(&result.contents).map_err(resource_serialization_error)?;
    Ok(serde_json::json!({
        "server": server,
        "uri": uri,
        "contents": contents,
    }))
}

fn serialize_value_with_server<T: Serialize>(
    server: &str,
    value: &T,
) -> Result<Value, FunctionCallError> {
    let mut value = serde_json::to_value(value).map_err(resource_serialization_error)?;
    let Value::Object(object) = &mut value else {
        return Err(FunctionCallError::RespondToModel(
            "failed to serialize MCP resource response: expected an object".to_string(),
        ));
    };
    object.insert("server".to_string(), Value::String(server.to_string()));
    Ok(value)
}

fn resource_serialization_error(err: serde_json::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("failed to serialize MCP resource response: {err}"))
}

fn serialized_payload_fits<T>(
    payload: &T,
    truncation_policy: TruncationPolicy,
) -> Result<bool, FunctionCallError>
where
    T: Serialize,
{
    let content = serialize_resource_payload(payload)?;
    Ok(truncate_text(&content, truncation_policy * 1.2) == content)
}

fn conservative_serialized_budget(truncation_policy: TruncationPolicy) -> usize {
    match truncation_policy * 1.2 {
        TruncationPolicy::Bytes(bytes) | TruncationPolicy::Tokens(bytes) => bytes,
    }
}

fn serialized_candidate_cost<T>(
    candidate: &T,
    truncation_policy: TruncationPolicy,
) -> Result<usize, FunctionCallError>
where
    T: Serialize,
{
    let content = serialize_resource_payload(candidate)?;
    Ok(match truncation_policy {
        TruncationPolicy::Bytes(_) => content.len(),
        TruncationPolicy::Tokens(_) => approx_token_count(&content),
    })
}

fn serialize_resource_payload<T>(payload: &T) -> Result<String, FunctionCallError>
where
    T: Serialize,
{
    serde_json::to_string(payload).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize MCP resource response: {err}"
        ))
    })
}

fn parse_arguments(raw_args: &str) -> Result<Option<Value>, FunctionCallError> {
    if raw_args.trim().is_empty() {
        Ok(None)
    } else {
        let value: Value = serde_json::from_str(raw_args).map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
        })?;
        if value.is_null() {
            Ok(None)
        } else {
            Ok(Some(value))
        }
    }
}

fn parse_args<T>(arguments: Option<Value>) -> Result<T, FunctionCallError>
where
    T: DeserializeOwned,
{
    match arguments {
        Some(value) => serde_json::from_value(value).map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
        }),
        None => Err(FunctionCallError::RespondToModel(
            "failed to parse function arguments: expected value".to_string(),
        )),
    }
}

fn parse_args_with_default<T>(arguments: Option<Value>) -> Result<T, FunctionCallError>
where
    T: DeserializeOwned + Default,
{
    match arguments {
        Some(value) => parse_args(Some(value)),
        None => Ok(T::default()),
    }
}

#[cfg(test)]
#[path = "mcp_resource_tests.rs"]
mod tests;
