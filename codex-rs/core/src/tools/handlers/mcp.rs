use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use crate::FunctionCallError;
use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::mcp_tool_call::HandledMcpToolCall;
use crate::mcp_tool_call::handle_mcp_tool_call;
use crate::tools::context::McpToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::flat_tool_name;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolTelemetryTags;
use codex_mcp::LEGACY_MCP_TOOL_NAME_PREFIX;
use codex_mcp::MCP_TOOL_NAME_DELIMITER;
use codex_mcp::ToolInfo;
use codex_protocol::mcp::CallToolResult;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use codex_tools::can_request_original_image_detail;
use codex_tools::mcp_tool_to_responses_api_tool;
use codex_tools::schema_search_text;
use serde_json::Map;
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredJobStatus {
    pub(crate) job_id: String,
    pub(crate) tool_name: String,
    pub(crate) started: Instant,
}

#[derive(Default)]
struct McpDeferredJobsInner {
    next_id: u64,
    running: HashMap<String, DeferredJobStatus>,
    results: HashMap<String, watch::Sender<Option<Arc<HandledMcpToolCall>>>>,
}

/// Session-scoped in-flight calls sharing the original typed MCP result.
#[derive(Default)]
pub(crate) struct McpDeferredJobs {
    inner: std::sync::Mutex<McpDeferredJobsInner>,
}

impl McpDeferredJobs {
    /// Include the turn-scoped call ID: distinct requested actions may have identical
    /// arguments and must not be silently merged.
    pub(crate) fn job_key(tool_info: &ToolInfo, arguments: &str, call_id: &str) -> String {
        let canonical = serde_json::from_str::<Value>(arguments)
            .map(|value| canonical_json(&value))
            .unwrap_or_else(|_| arguments.trim().to_string());
        serde_json::json!([
            tool_info.server_name,
            tool_info.tool.name,
            call_id,
            canonical
        ])
        .to_string()
    }
}

/// JSON text with object keys sorted at every level, independent of the
/// serializer's key-order feature, so equal arguments always share a key.
fn canonical_json(value: &Value) -> String {
    codex_config::schema::canonicalize(value).to_string()
}

impl McpDeferredJobs {

    fn reserve(
        &self,
        key: String,
        tool_name: String,
        started: Instant,
    ) -> (
        DeferredJobStatus,
        bool,
        watch::Receiver<Option<Arc<HandledMcpToolCall>>>,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = inner.results.get(&key) {
            return (inner.running[&key].clone(), false, sender.subscribe());
        }
        inner.next_id += 1;
        let status = DeferredJobStatus {
            job_id: format!("mcp-job-{}", inner.next_id),
            tool_name,
            started,
        };
        let (sender, receiver) = watch::channel(None);
        inner.results.insert(key.clone(), sender);
        inner.running.insert(key, status.clone());
        (status, true, receiver)
    }

    /// Join an identical in-flight call before dispatch. Keep the actual typed
    /// result on the original tool future; code mode owns yielding and cancellation.
    async fn execute<F>(
        self: &Arc<Self>,
        key: String,
        tool_name: String,
        input: Value,
        cancellation: CancellationToken,
        call: F,
    ) -> Arc<HandledMcpToolCall>
    where
        F: Future<Output = HandledMcpToolCall>,
    {
        let (_status, owner, mut receiver) = self.reserve(key.clone(), tool_name, Instant::now());
        if owner {
            let lease = McpCallLease {
                jobs: Arc::clone(self),
                key,
                input,
            };
            let result = Arc::new(call.await);
            {
                let inner = self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(sender) = inner.results.get(&lease.key) {
                    sender.send_replace(Some(Arc::clone(&result)));
                }
            }
            drop(lease);
            return result;
        }
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            tokio::select! {
                _ = cancellation.cancelled() => return cancelled_mcp_result(input),
                changed = receiver.changed() => {
                    if changed.is_err() { return cancelled_mcp_result(input); }
                }
            }
        }
    }
}

/// Reuses session extension data to share overlapping calls before dispatch.
pub(crate) fn session_deferred_jobs(
    session: &crate::session::session::Session,
) -> std::sync::Arc<McpDeferredJobs> {
    session
        .session_extension_data()
        .get_or_init(McpDeferredJobs::default)
}

fn cancelled_mcp_result(input: Value) -> Arc<HandledMcpToolCall> {
    Arc::new(HandledMcpToolCall {
        result: CallToolResult::from_error_text("MCP call interrupted; remote completion status is unknown. Check its outcome before retrying.".to_string()),
        tool_input: input,
    })
}

struct McpCallLease {
    jobs: Arc<McpDeferredJobs>,
    key: String,
    input: Value,
}

impl Drop for McpCallLease {
    fn drop(&mut self) {
        let mut inner = self
            .jobs
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = inner.results.remove(&self.key) {
            let unfinished = sender.borrow().is_none();
            if unfinished {
                sender.send_replace(Some(cancelled_mcp_result(self.input.clone())));
            }
        }
        inner.running.remove(&self.key);
    }
}

pub struct McpHandler {
    tool_info: ToolInfo,
    tool_name: ToolName,
    spec: Arc<codex_tools::LoadableToolSpec>,
    input_validator: jsonschema::Validator,
    canonical_input_schema: Value,
}

impl McpHandler {
    pub fn new(tool_info: ToolInfo, use_namespace: bool) -> Result<Self, serde_json::Error> {
        let tool_name = if use_namespace {
            tool_info.canonical_tool_name()
        } else {
            // The MCP normalization owner bounds the combined identity,
            // including this delimiter, to the provider's 64-byte limit.
            let name = format!(
                "{}{}{}",
                tool_info.callable_namespace, MCP_TOOL_NAME_DELIMITER, tool_info.callable_name
            );
            if name.len() > 64 {
                return Err(serde::de::Error::custom(
                    "flat MCP tool name exceeds 64 bytes",
                ));
            }
            ToolName::plain(name)
        };
        let spec = Arc::new(create_tool_spec(&tool_info, &tool_name)?);
        let mut schema = Value::Object(tool_info.tool.input_schema.as_ref().clone());
        // Match the existing MCP compatibility rule for omitted/null properties.
        if schema.get("properties").is_some_and(Value::is_null) {
            schema["properties"] = serde_json::json!({});
        }
        let input_validator = jsonschema::validator_for(&schema).map_err(|error| {
            <serde_json::Error as serde::de::Error>::custom(format!(
                "invalid MCP tool input schema: {error}"
            ))
        })?;
        Ok(Self {
            tool_info,
            tool_name,
            spec,
            input_validator,
            canonical_input_schema: schema,
        })
    }

    fn hook_tool_name(&self) -> HookToolName {
        HookToolName::new(ensure_mcp_prefix(&join_tool_name(
            &self.tool_info.canonical_tool_name(),
        )))
    }

    pub(crate) fn external_mutation_intent(&self) -> ExternalMutationIntent {
        // Server annotations are advisory, not host-established read-only authority.
        ExternalMutationIntent::MayMutate
    }
}
fn join_tool_name(tool_name: &ToolName) -> String {
    match tool_name.namespace.as_deref() {
        Some(namespace) => {
            let namespace = namespace.trim_end_matches('_');
            let name = tool_name.name.trim_start_matches('_');
            format!("{namespace}{MCP_TOOL_NAME_DELIMITER}{name}")
        }
        None => tool_name.name.clone(),
    }
}

fn ensure_mcp_prefix(name: &str) -> String {
    if name.starts_with(LEGACY_MCP_TOOL_NAME_PREFIX) {
        name.to_string()
    } else {
        format!("{LEGACY_MCP_TOOL_NAME_PREFIX}{name}")
    }
}

impl ToolExecutor<ToolInvocation> for McpHandler {
    fn tool_name(&self) -> ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> ToolSpec {
        self.spec.as_ref().clone().into()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // Only explicit host configuration can authorize parallel MCP calls.
        self.tool_info.supports_parallel_tool_calls
    }

    fn search_info_for_registered_spec(
        &self,
        registered_spec: &ToolSpec,
    ) -> Option<ToolSearchInfo> {
        let source_name = self
            .tool_info
            .connector_name
            .as_deref()
            .map(str::trim)
            .filter(|connector_name| !connector_name.is_empty())
            .unwrap_or_else(|| self.tool_info.server_name.trim());
        let source_info = (!source_name.is_empty()).then(|| ToolSearchSourceInfo {
            name: source_name.to_string(),
            description: self
                .tool_info
                .namespace_description
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_string),
        });

        let search_text = build_mcp_search_text(&self.tool_info, registered_spec);
        // Registration may override the callable contract; only share an unchanged definition.
        let unchanged = match (registered_spec, self.spec.as_ref()) {
            (ToolSpec::Namespace(left), codex_tools::LoadableToolSpec::Namespace(right)) => {
                left == right
            }
            (ToolSpec::Function(left), codex_tools::LoadableToolSpec::Function(right)) => {
                left == right
            }
            _ => false,
        };
        let mut info = if unchanged {
            ToolSearchInfo::from_shared_spec(search_text, Arc::clone(&self.spec), source_info)
        } else {
            ToolSearchInfo::from_spec(search_text, registered_spec, source_info)?
        };
        // Titles describe this operation. They must survive the retrieval /
        // activation boundary without becoming duplicated model-visible prose.
        let unchanged_callable = unchanged
            || match (registered_spec, self.spec.as_ref()) {
                (ToolSpec::Namespace(left), codex_tools::LoadableToolSpec::Namespace(right)) => {
                    left.name == right.name && left.tools == right.tools
                }
                _ => false,
            };
        if unchanged_callable {
            info.entry.callable_title = self
                .tool_info
                .tool
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_owned);
        }
        Some(info)
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl McpHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            call_id,
            payload,
            source,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let payload = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "mcp handler received unsupported payload".to_string(),
                ));
            }
        };

        // This is after hook rewrites and before reserving/dispatching remote work.
        // The advertised schema may be simplified; never validate against it alone.
        let tool_input_value = serde_json::from_str::<Value>(&payload).map_err(|error| {
            FunctionCallError::RespondToModel(format!("invalid MCP arguments: {error}"))
        })?;
        if !tool_input_value.is_object() {
            return Err(FunctionCallError::RespondToModel(
                "MCP arguments must be a JSON object".into(),
            ));
        }
        let errors = self
            .input_validator
            .iter_errors(&tool_input_value)
            .take(4)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(FunctionCallError::RespondToModel(format!(
                "tool `{}` arguments violate its full input schema: {}",
                self.tool_name(),
                errors.join("; ")
            )));
        }
        let started = Instant::now();
        let jobs = session_deferred_jobs(&session);
        let logical_call_id = format!("{}/{call_id}", turn.sub_id);
        let job_key = McpDeferredJobs::job_key(&self.tool_info, &payload, &logical_call_id);
        // TODO(sayan): Use StepContext for MCP file arguments when MCP follows dynamic environments.
        let call = {
            let session = Arc::clone(&session);
            let step_context = Arc::clone(&step_context);
            let tool_info = self.tool_info.clone();
            let call_id = call_id.clone();
            let cancellation_token = cancellation_token.clone();
            async move {
                handle_mcp_tool_call(
                    session,
                    &step_context,
                    call_id,
                    &tool_info,
                    payload,
                    cancellation_token,
                )
                .await
            }
        };
        let result = jobs
            .execute(
                job_key,
                self.hook_tool_name().name().to_string(),
                tool_input_value,
                cancellation_token,
                call,
            )
            .await;
        let output = McpToolOutput::new(
            result.result.clone(),
            result.tool_input.clone(),
            started.elapsed(),
            can_request_original_image_detail(&turn.model_info),
            turn.model_info.truncation_policy.into(),
        );
        let output = if matches!(
            source,
            crate::tools::context::ToolCallSource::CodeMode { .. }
        ) {
            output
        } else {
            output
                .retain_reduced_result(
                    &turn.config.codex_home,
                    &session.thread_id.to_string(),
                    Some((session.as_ref(), &call_id)),
                )
                .await
        };
        Ok(boxed_tool_output(output))
    }
}

impl CoreToolRuntime for McpHandler {
    fn code_mode_input_schema(&self) -> Option<Value> {
        Some(self.canonical_input_schema.clone())
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }

    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::NestedRuntime
    }

    fn telemetry_tags<'a>(
        &'a self,
        _invocation: &'a ToolInvocation,
    ) -> futures::future::BoxFuture<'a, ToolTelemetryTags> {
        let mut tags = vec![("mcp_server", self.tool_info.server_name.clone())];
        if let Some(origin) = self.tool_info.server_origin.clone() {
            tags.push(("mcp_server_origin", origin));
        }
        Box::pin(async move { tags })
    }

    fn pre_tool_use_hook_name(
        &self,
        _tool_name: &codex_tools::ToolName,
        payload: &ToolPayload,
    ) -> Option<HookToolName> {
        matches!(payload, ToolPayload::Function { .. }).then(|| self.hook_tool_name())
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PreToolUsePayload {
            tool_name: self.pre_tool_use_hook_name(&invocation.tool_name, &invocation.payload)?,
            tool_input: mcp_hook_tool_input(arguments),
        })
    }

    fn post_tool_use_hook_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        matches!(&invocation.payload, ToolPayload::Function { .. }).then(|| self.hook_tool_name())
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        invocation.payload = match invocation.payload {
            ToolPayload::Function { .. } => ToolPayload::Function {
                arguments: serde_json::to_string(&updated_input).map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to serialize rewritten MCP arguments: {err}"
                    ))
                })?,
            },
            payload => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "tool {} does not support hook input rewriting for payload {payload:?}",
                    self.tool_name()
                )));
            }
        };
        Ok(invocation)
    }
    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { .. } = &invocation.payload else {
            return None;
        };

        let tool_response =
            result.post_tool_use_response(&invocation.call_id, &invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: self.hook_tool_name(),
            tool_use_id: invocation.call_id.clone(),
            tool_input: result.post_tool_use_input(&invocation.payload)?,
            tool_response,
        })
    }
}

fn create_tool_spec(
    tool_info: &ToolInfo,
    tool_name: &ToolName,
) -> Result<codex_tools::LoadableToolSpec, serde_json::Error> {
    let tool = mcp_tool_to_responses_api_tool(tool_name, &tool_info.tool)?;
    if tool_name.namespace.is_none() {
        return Ok(codex_tools::LoadableToolSpec::Function(tool));
    }
    let description = tool_info
        .namespace_description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_string)
        .or_else(|| {
            tool_info
                .connector_name
                .as_deref()
                .map(str::trim)
                .filter(|connector_name| !connector_name.is_empty())
                .map(|connector_name| format!("Tools for working with {connector_name}."))
        })
        .unwrap_or_default();

    Ok(codex_tools::LoadableToolSpec::Namespace(
        ResponsesApiNamespace {
            name: tool_info.callable_namespace.clone(),
            description,
            tools: vec![ResponsesApiNamespaceTool::Function(tool)],
        },
    ))
}

fn mcp_hook_tool_input(raw_arguments: &str) -> Value {
    if raw_arguments.trim().is_empty() {
        return Value::Object(Map::new());
    }

    match crate::tools::handlers::parsed_function_argument_value(raw_arguments) {
        Some(Ok(value)) => value,
        Some(Err(_)) => Value::String(raw_arguments.to_string()),
        None => serde_json::from_str(raw_arguments)
            .unwrap_or_else(|_| Value::String(raw_arguments.to_string())),
    }
}

fn build_mcp_search_text(info: &ToolInfo, registered_spec: &ToolSpec) -> String {
    let tool_name = info.canonical_tool_name();
    let mut parts = vec![
        flat_tool_name(&tool_name).into_owned(),
        info.callable_name.clone(),
        info.tool.name.to_string(),
        info.server_name.clone(),
    ];
    for name in [&info.callable_name, info.tool.name.as_ref()] {
        let words = codex_tools::identifier_search_words(name);
        if !parts.contains(&words) {
            parts.push(words);
        }
    }
    if let Some(title) = info.tool.title.as_deref().map(str::trim)
        && !title.is_empty()
    {
        parts.push(title.to_string());
    }
    if let Some(description) = info.tool.description.as_deref().map(str::trim)
        && !description.is_empty()
    {
        parts.push(description.to_string());
    }
    if let Some(connector_name) = info.connector_name.as_deref().map(str::trim)
        && !connector_name.is_empty()
    {
        parts.push(connector_name.to_string());
    }
    if let Some(namespace_description) = info.namespace_description.as_deref().map(str::trim)
        && !namespace_description.is_empty()
    {
        parts.push(namespace_description.to_string());
    }
    parts.extend(
        info.plugin_display_names
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .filter(|display_name| !display_name.is_empty())
            .map(str::to_string),
    );
    match registered_spec {
        ToolSpec::Function(tool) => parts.push(schema_search_text(&tool.parameters)),
        ToolSpec::Namespace(namespace) => {
            for tool in &namespace.tools {
                let ResponsesApiNamespaceTool::Function(tool) = tool;
                parts.push(schema_search_text(&tool.parameters));
            }
        }
        ToolSpec::Freeform(_) | ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => {}
    }
    parts.retain(|part| !part.trim().is_empty());
    parts.join(" ")
}


