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
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_protocol::protocol::TruncationPolicy;
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
use crate::tools::context::boxed_tool_output;
use codex_protocol::protocol::McpInvocation;

const MCP_RESOURCE_CALL_CANCELLED_MESSAGE: &str = "MCP resource call cancelled";

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
        let resources: Vec<ResourceWithServer> = result
            .resources
            .into_iter()
            .map(|resource| ResourceWithServer::new(server.clone(), resource))
            .collect();
        let payload = Self {
            server: Some(server),
            resources,
            next_cursor: result.next_cursor,
            next_cursors: BTreeMap::new(),
            remaining_servers: Vec::new(),
            truncated: false,
            omitted_count: 0,
            errors: Vec::new(),
            omitted_error_count: 0,
        };
        if !serialized_payload_fits(&payload, truncation_policy)? {
            return Err(FunctionCallError::RespondToModel(
                "The MCP server returned a resource page that exceeds the output budget; the page cannot be shortened without skipping entries before its next cursor"
                    .to_string(),
            ));
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

        for error in errors {
            payload.omitted_error_count -= 1;
            payload.errors.push(error);
            if !serialized_payload_fits(&payload, truncation_policy)? {
                payload.errors.pop();
                payload.omitted_error_count += 1;
                break;
            }
        }
        for (server, page) in entries {
            let remaining_index = payload
                .remaining_servers
                .binary_search(&server)
                .expect("aggregate server originated from remaining_servers");
            payload.remaining_servers.remove(remaining_index);
            let resources_start = payload.resources.len();
            let page_resource_count = page.resources.len();
            payload.omitted_count -= page_resource_count;
            for resource in page.resources {
                payload
                    .resources
                    .push(ResourceWithServer::new(server.clone(), resource));
            }
            if let Some(next_cursor) = page.next_cursor {
                payload.next_cursors.insert(server.clone(), next_cursor);
            }

            if !serialized_payload_fits(&payload, truncation_policy)? {
                payload.resources.truncate(resources_start);
                payload.next_cursors.remove(&server);
                payload.remaining_servers.insert(remaining_index, server);
                payload.omitted_count += page_resource_count;
            }
        }
        payload.truncated = payload.omitted_count > 0
            || payload.omitted_error_count > 0
            || !payload.remaining_servers.is_empty();
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
        let resource_templates: Vec<ResourceTemplateWithServer> = result
            .resource_templates
            .into_iter()
            .map(|template| ResourceTemplateWithServer::new(server.clone(), template))
            .collect();
        let payload = Self {
            server: Some(server),
            resource_templates,
            next_cursor: result.next_cursor,
            next_cursors: BTreeMap::new(),
            remaining_servers: Vec::new(),
            truncated: false,
            omitted_count: 0,
            errors: Vec::new(),
            omitted_error_count: 0,
        };
        if !serialized_payload_fits(&payload, truncation_policy)? {
            return Err(FunctionCallError::RespondToModel(
                "The MCP server returned a resource-template page that exceeds the output budget; the page cannot be shortened without skipping entries before its next cursor"
                    .to_string(),
            ));
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
        for error in errors {
            payload.omitted_error_count -= 1;
            payload.errors.push(error);
            if !serialized_payload_fits(&payload, truncation_policy)? {
                payload.errors.pop();
                payload.omitted_error_count += 1;
                break;
            }
        }

        for (server, page) in entries {
            let remaining_index = payload
                .remaining_servers
                .binary_search(&server)
                .expect("aggregate server originated from remaining_servers");
            payload.remaining_servers.remove(remaining_index);
            let templates_start = payload.resource_templates.len();
            let page_template_count = page.resource_templates.len();
            payload.omitted_count -= page_template_count;
            for template in page.resource_templates {
                payload
                    .resource_templates
                    .push(ResourceTemplateWithServer::new(server.clone(), template));
            }
            if let Some(next_cursor) = page.next_cursor {
                payload.next_cursors.insert(server.clone(), next_cursor);
            }

            if !serialized_payload_fits(&payload, truncation_policy)? {
                payload.resource_templates.truncate(templates_start);
                payload.next_cursors.remove(&server);
                payload.remaining_servers.insert(remaining_index, server);
                payload.omitted_count += page_template_count;
            }
        }

        payload.truncated = payload.omitted_count > 0
            || payload.omitted_error_count > 0
            || !payload.remaining_servers.is_empty();
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

fn is_false(value: &bool) -> bool {
    !*value
}

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
    F: Future<Output = Result<FunctionToolOutput, FunctionCallError>>,
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
            let content =
                function_call_output_content_items_to_text(&output.body).unwrap_or_default();
            Ok(call_tool_result_from_content(&content, output.success))
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
    truncation_policy: TruncationPolicy,
) -> Result<FunctionToolOutput, FunctionCallError>
where
    T: Serialize,
{
    let content = serialize_resource_payload(&payload)?;
    if truncate_text(&content, truncation_policy * 1.2) != content {
        return Err(FunctionCallError::RespondToModel(
            "MCP resource response exceeds the output budget; narrow the request".to_string(),
        ));
    }

    Ok(FunctionToolOutput::from_text(content, Some(true)))
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
