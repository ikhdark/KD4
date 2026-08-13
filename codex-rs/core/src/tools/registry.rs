use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::function_tool::FunctionCallError;
use crate::hook_runtime::PreToolUseHookResult;
use crate::hook_runtime::record_additional_contexts;
use crate::hook_runtime::run_post_tool_use_hooks;
use crate::hook_runtime::run_pre_tool_use_hooks;
use crate::memory_usage::emit_metric_for_tool_read;
use crate::sandbox_tags::permission_profile_policy_tag;
use crate::sandbox_tags::permission_profile_sandbox_tag;
use crate::session::turn_context::TurnContext;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
use crate::tools::command_output_artifact::attach_canonical_output_artifact;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::read_tool_output_selectors;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::flat_tool_name;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE;
use crate::tools::hook_names::HookToolName;
use crate::tools::lifecycle::notify_tool_finish;
use crate::tools::lifecycle::notify_tool_start;
use crate::tools::tool_dispatch_trace::ToolDispatchTrace;
use crate::tools::tool_dispatch_trace::mark_tool_handler_entry;
use crate::util::error_or_panic;
use codex_extension_api::ToolCallOutcome;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_rollout::state_db;
use codex_tools::CanonicalByteRange;
use codex_tools::CanonicalToolResult;
use codex_tools::ToolName;
use codex_tools::ToolOutputDiagnosticClass;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputProjectionFragment;
use codex_tools::ToolOutputProjectionFragmentKind;
use codex_tools::ToolOutputProjectionRange;
use codex_tools::ToolProjectionInclusion;
use codex_tools::ToolProjectionSection;
use codex_tools::ToolProjectionV1;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS;
use codex_utils_output_truncation::OutputDiagnosticClass;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::formatted_truncate_text_with_output_limit;
use codex_utils_output_truncation::resolve_projected_output_limits;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use futures::future::BoxFuture;
use serde_json::Value;
use tracing::instrument;

pub(crate) type ToolTelemetryTags = Vec<(&'static str, String)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolExecutionTiming {
    /// Time the core handler future as the actual tool invocation.
    Handler,
    /// A narrower runtime boundary inside the handler owns execution timing.
    NestedRuntime,
    /// The handler is an interactive wait rather than machine tool execution.
    Interactive,
}

pub use codex_tools::ToolExecutor;
pub use codex_tools::ToolExposure;

/// Typed runtime contract for locally executed tools.
///
/// Implementers provide the shared `ToolExecutor` behavior plus optional
/// core-owned metadata for hooks, telemetry, tool search, and argument diffs.
pub(crate) trait CoreToolRuntime: ToolExecutor<ToolInvocation> {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::Handler
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::ToolSearch { .. }
        )
    }

    /// Whether cancellation should let the handler finish teardown before the
    /// host returns an aborted tool response.
    fn waits_for_runtime_cancellation(&self) -> bool {
        false
    }

    fn telemetry_tags<'a>(
        &'a self,
        _invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, ToolTelemetryTags> {
        Box::pin(async { Vec::new() })
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PostToolUsePayload {
            tool_name: function_hook_tool_name(invocation),
            tool_use_id: result.post_tool_use_id(&invocation.call_id),
            tool_input: result
                .post_tool_use_input(&invocation.payload)
                .unwrap_or_else(|| function_hook_tool_input(arguments)),
            tool_response: result
                .post_tool_use_response(&invocation.call_id, &invocation.payload)
                .or_else(|| {
                    // Most function tools can expose their model-facing output
                    // as the hook response. Outputs with a more stable hook
                    // contract should override post_tool_use_response above.
                    let ResponseInputItem::FunctionCallOutput {
                        output: FunctionCallOutputPayload { body, .. },
                        ..
                    } = result.to_response_item(&invocation.call_id, &invocation.payload)
                    else {
                        return None;
                    };

                    serde_json::to_value(body).ok()
                })?,
        })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PreToolUsePayload {
            tool_name: function_hook_tool_name(invocation),
            tool_input: function_hook_tool_input(arguments),
        })
    }

    /// Rebuilds a tool invocation from hook-facing `tool_input`.
    ///
    /// Tools that opt into input-rewriting hooks should invert the same stable
    /// hook contract they expose from `pre_tool_use_payload`.
    fn with_updated_hook_input(
        &self,
        invocation: ToolInvocation,
        updated_input: Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { .. } = &invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported function tool payload".to_string(),
            ));
        };

        let arguments = serde_json::to_string(&updated_input).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to serialize rewritten {} arguments: {err}",
                flat_tool_name(&invocation.tool_name)
            ))
        })?;
        Ok(ToolInvocation {
            payload: ToolPayload::Function { arguments },
            ..invocation
        })
    }

    /// Creates an optional consumer for streamed tool argument diffs.
    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        None
    }
}

/// Consumes streamed argument diffs for a tool call and emits protocol events
/// derived from partial tool input.
pub(crate) trait ToolArgumentDiffConsumer: Send {
    /// Consume the next argument diff for a tool call.
    fn consume_diff(&mut self, turn: &TurnContext, call_id: String, diff: &str)
    -> Option<EventMsg>;

    /// Finish consuming argument diffs before the tool call completes.
    fn finish(&mut self) -> Result<Option<EventMsg>, FunctionCallError> {
        Ok(None)
    }
}

pub(crate) struct AnyToolResult {
    pub(crate) call_id: String,
    pub(crate) payload: ToolPayload,
    pub(crate) result: Box<dyn ToolOutput>,
    pub(crate) post_tool_use_payload: Option<PostToolUsePayload>,
    pub(crate) model_projection: Option<ModelToolProjection>,
}

pub(crate) struct ModelToolProjection {
    response: ResponseInputItem,
    candidate: crate::tool_history::ToolHistoryCandidate,
    projected_tokens: u64,
    canonical_bytes: u64,
    canonical_tokens: u64,
    model_bytes: u64,
    artifact_created: bool,
    projection_truncated: bool,
    omitted_sections: u64,
    deterministic_continuation_receipt: Option<TurnTimingDeterministicContinuationReceipt>,
}

impl AnyToolResult {
    pub(crate) fn success_for_logging(&self) -> bool {
        self.result.success_for_logging()
    }

    pub(crate) fn outcome_for_logging(&self) -> ToolOutputOutcome {
        self.result.outcome_for_logging()
    }

    pub(crate) fn sampling_request_signal(&self) -> Option<serde_json::Value> {
        self.result.sampling_request_signal()
    }

    pub(crate) fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        let mut receipts = self.result.deterministic_continuation_receipts();
        if let Some(receipt) = self
            .model_projection
            .as_ref()
            .and_then(|projection| projection.deterministic_continuation_receipt.clone())
        {
            receipts.push(receipt);
        }
        receipts
    }

    pub(crate) fn into_response(self) -> ResponseInputItem {
        if let Some(projection) = self.model_projection {
            return projection.response;
        }
        let Self {
            call_id,
            payload,
            result,
            ..
        } = self;
        result.to_response_item(&call_id, &payload)
    }

    pub(crate) fn code_mode_result(self) -> serde_json::Value {
        let Self {
            payload, result, ..
        } = self;
        result.code_mode_result(&payload)
    }
}

struct PostToolUseFeedbackOutput {
    original: Box<dyn ToolOutput>,
    model_visible: FunctionToolOutput,
}

impl ToolOutput for PostToolUseFeedbackOutput {
    fn log_preview(&self) -> String {
        self.original.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.original.success_for_logging()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        self.original.sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.original.deterministic_continuation_receipts()
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        self.model_visible.projection_metadata()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.model_visible.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.model_visible.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.model_visible.code_mode_result(payload)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreToolUsePayload {
    /// Hook-facing tool name model.
    ///
    /// The canonical name is serialized to hook stdin, while aliases are used
    /// only for matcher compatibility.
    pub(crate) tool_name: HookToolName,
    /// Tool-specific input exposed at `tool_input`.
    ///
    /// Shell-like tools use `{ "command": ... }`; MCP tools use their resolved
    /// JSON arguments.
    pub(crate) tool_input: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PostToolUsePayload {
    /// Hook-facing tool name model.
    ///
    /// The canonical name is serialized to hook stdin, while aliases are used
    /// only for matcher compatibility.
    pub(crate) tool_name: HookToolName,
    /// The originating tool-use id exposed at `tool_use_id`.
    pub(crate) tool_use_id: String,
    /// Tool-specific input exposed at `tool_input`.
    pub(crate) tool_input: Value,
    /// Tool result exposed at `tool_response`.
    pub(crate) tool_response: Value,
}

pub(crate) fn override_tool_exposure(
    handler: Arc<dyn CoreToolRuntime>,
    exposure: ToolExposure,
) -> Arc<dyn CoreToolRuntime> {
    if handler.exposure() == exposure {
        return handler;
    }

    Arc::new(ExposureOverride { handler, exposure })
}

struct ExposureOverride {
    handler: Arc<dyn CoreToolRuntime>,
    exposure: ToolExposure,
}

impl ToolExecutor<ToolInvocation> for ExposureOverride {
    fn tool_name(&self) -> ToolName {
        self.handler.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.handler.spec()
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.exposure != ToolExposure::Hidden && self.handler.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.handler.search_info()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        self.handler.handle(invocation)
    }
}

impl CoreToolRuntime for ExposureOverride {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        self.handler.tool_execution_timing()
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        self.handler.matches_kind(payload)
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        self.handler.waits_for_runtime_cancellation()
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        self.handler.pre_tool_use_payload(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        self.handler.post_tool_use_payload(invocation, result)
    }

    fn with_updated_hook_input(
        &self,
        invocation: ToolInvocation,
        updated_input: Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        self.handler
            .with_updated_hook_input(invocation, updated_input)
    }

    fn telemetry_tags<'a>(
        &'a self,
        invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, ToolTelemetryTags> {
        self.handler.telemetry_tags(invocation)
    }

    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.handler.create_diff_consumer()
    }
}

pub struct ToolRegistry {
    tools: HashMap<ToolName, Arc<dyn CoreToolRuntime>>,
}

impl ToolRegistry {
    fn new(tools: HashMap<ToolName, Arc<dyn CoreToolRuntime>>) -> Self {
        Self { tools }
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) fn from_tools(tools: impl IntoIterator<Item = Arc<dyn CoreToolRuntime>>) -> Self {
        let mut tools_by_name = HashMap::new();
        for tool in tools {
            let name = tool.tool_name();
            if tools_by_name.contains_key(&name) {
                error_or_panic(format!("tool {name} already registered"));
                continue;
            }
            tools_by_name.insert(name, tool);
        }
        Self::new(tools_by_name)
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self::new(HashMap::new())
    }

    #[cfg(test)]
    pub(crate) fn with_handler_for_test<T>(handler: Arc<T>) -> Self
    where
        T: CoreToolRuntime + 'static,
    {
        let name = handler.tool_name();
        Self::new(HashMap::from([(name, handler as Arc<dyn CoreToolRuntime>)]))
    }

    fn tool(&self, name: &ToolName) -> Option<Arc<dyn CoreToolRuntime>> {
        self.tools.get(name).map(Arc::clone)
    }

    #[cfg(test)]
    pub(crate) fn tool_names_for_test(&self) -> Vec<ToolName> {
        let mut names = self.tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    pub(crate) fn tool_exposure(&self, name: &ToolName) -> Option<ToolExposure> {
        self.tools.get(name).map(|tool| tool.exposure())
    }

    pub(crate) fn manifest_entries(&self) -> Vec<(ToolName, ToolExposure, ToolSpec)> {
        let mut entries = self
            .tools
            .iter()
            .map(|(name, tool)| (name.clone(), tool.exposure(), tool.spec()))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    pub(crate) fn create_diff_consumer(
        &self,
        name: &ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.tool(name)?.create_diff_consumer()
    }

    pub(crate) fn supports_parallel_tool_calls(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tool(name)?;
        Some(tool.supports_parallel_tool_calls())
    }

    pub(crate) fn waits_for_runtime_cancellation(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tool(name)?;
        Some(tool.waits_for_runtime_cancellation())
    }

    #[allow(dead_code)]
    pub(crate) async fn dispatch_any(
        &self,
        invocation: ToolInvocation,
    ) -> Result<AnyToolResult, FunctionCallError> {
        self.dispatch_any_with_terminal_outcome(invocation, /*terminal_outcome_reached*/ None)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "tool dispatch must keep active-turn accounting atomic"
    )]
    pub(crate) async fn dispatch_any_with_terminal_outcome(
        &self,
        mut invocation: ToolInvocation,
        terminal_outcome_reached: Option<Arc<AtomicBool>>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        let tool_name = invocation.tool_name.clone();
        let tool_name_flat = flat_tool_name(&tool_name);
        let call_id_owned = invocation.call_id.clone();
        let otel = invocation.turn.session_telemetry.clone();
        let base_tool_result_tags = [
            (
                "sandbox",
                permission_profile_sandbox_tag(
                    &invocation.turn.permission_profile,
                    invocation.turn.windows_sandbox_level,
                    invocation.turn.network.is_some(),
                ),
            ),
            (
                "sandbox_policy",
                permission_profile_policy_tag(
                    &invocation.turn.permission_profile,
                    #[allow(deprecated)]
                    invocation.turn.cwd.as_path(),
                ),
            ),
        ];

        {
            let mut active = invocation.session.active_turn.lock().await;
            if let Some(active_turn) = active.as_mut() {
                let mut turn_state = active_turn.turn_state.lock().await;
                turn_state.tool_calls = turn_state.tool_calls.saturating_add(1);
            }
        }
        let dispatch_trace = ToolDispatchTrace::start(&invocation);
        let tool = match self.tool(&tool_name) {
            Some(tool) => tool,
            None => {
                let message = unsupported_tool_call_message(&invocation.payload, &tool_name);
                let log_payload = invocation.payload.log_payload();
                otel.tool_result_with_tags(
                    tool_name_flat.as_ref(),
                    &call_id_owned,
                    log_payload.as_ref(),
                    Duration::ZERO,
                    /*success*/ false,
                    &message,
                    &base_tool_result_tags,
                    /*extra_trace_fields*/ &[],
                );
                let err = FunctionCallError::RespondToModel(message);
                dispatch_trace.record_failed(&err);
                return Err(err);
            }
        };

        let telemetry_tags = tool.telemetry_tags(&invocation).await;
        let mut tool_result_tags =
            Vec::with_capacity(base_tool_result_tags.len() + telemetry_tags.len());
        let mut extra_trace_fields = Vec::new();
        tool_result_tags.extend_from_slice(&base_tool_result_tags);
        for (key, value) in &telemetry_tags {
            if matches!(*key, "mcp_server" | "mcp_server_origin") {
                extra_trace_fields.push((*key, value.as_str()));
            } else {
                tool_result_tags.push((*key, value.as_str()));
            }
        }
        if !tool.matches_kind(&invocation.payload) {
            let message = format!("tool {tool_name} invoked with incompatible payload");
            let log_payload = invocation.payload.log_payload();
            otel.tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                /*success*/ false,
                &message,
                &tool_result_tags,
                &extra_trace_fields,
            );
            let err = FunctionCallError::Fatal(message);
            dispatch_trace.record_failed(&err);
            return Err(err);
        }

        notify_tool_start(&invocation).await;

        if let Some(pre_tool_use_payload) = tool.pre_tool_use_payload(&invocation) {
            match run_pre_tool_use_hooks(
                &invocation.session,
                &invocation.turn,
                invocation.call_id.clone(),
                &pre_tool_use_payload.tool_name,
                &pre_tool_use_payload.tool_input,
            )
            .await
            {
                PreToolUseHookResult::Blocked(message) => {
                    let err = FunctionCallError::RespondToModel(message);
                    dispatch_trace.record_failed(&err);
                    notify_tool_finish_if_unclaimed(
                        &invocation,
                        terminal_outcome_reached.as_deref(),
                        ToolCallOutcome::Blocked,
                    )
                    .await;
                    return Err(err);
                }
                PreToolUseHookResult::Continue {
                    updated_input: Some(updated_input),
                } => match tool.with_updated_hook_input(invocation.clone(), updated_input) {
                    Ok(updated_invocation) => {
                        invocation = updated_invocation;
                    }
                    Err(err) => {
                        dispatch_trace.record_failed(&err);
                        notify_tool_finish_if_unclaimed(
                            &invocation,
                            terminal_outcome_reached.as_deref(),
                            ToolCallOutcome::Failed {
                                handler_executed: false,
                            },
                        )
                        .await;
                        return Err(err);
                    }
                },
                PreToolUseHookResult::Continue {
                    updated_input: None,
                } => {}
            }
        }

        let response_cell = tokio::sync::Mutex::new(None);
        let invocation_for_tool = invocation.clone();
        let log_payload = invocation.payload.log_payload();

        let result = otel
            .log_tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                &tool_result_tags,
                &extra_trace_fields,
                || {
                    let tool = tool.clone();
                    let response_cell = &response_cell;
                    async move {
                        match handle_any_tool(tool.as_ref(), invocation_for_tool).await {
                            Ok(result) => {
                                let preview = result.result.log_preview();
                                let success = result.success_for_logging();
                                let mut guard = response_cell.lock().await;
                                *guard = Some(result);
                                Ok((preview, success))
                            }
                            Err(err) => Err(err),
                        }
                    }
                },
            )
            .await;
        let success = match &result {
            Ok((_, success)) => *success,
            Err(_) => false,
        };
        emit_metric_for_tool_read(&invocation, success);
        let post_tool_use_payload = if success {
            let guard = response_cell.lock().await;
            guard
                .as_ref()
                .and_then(|result| result.post_tool_use_payload.clone())
        } else {
            None
        };
        let post_tool_use_outcome = if let Some(post_tool_use_payload) = post_tool_use_payload {
            Some(
                run_post_tool_use_hooks(
                    &invocation.session,
                    &invocation.turn,
                    post_tool_use_payload.tool_use_id,
                    post_tool_use_payload.tool_name.name().to_string(),
                    post_tool_use_payload.tool_name.matcher_aliases().to_vec(),
                    post_tool_use_payload.tool_input,
                    post_tool_use_payload.tool_response,
                )
                .await,
            )
        } else {
            None
        };
        if let Some(outcome) = &post_tool_use_outcome {
            record_additional_contexts(
                &invocation.session,
                &invocation.turn,
                outcome.additional_contexts.clone(),
            )
            .await;
        }

        // A PostToolUse block rejects the result, not the already-completed tool execution.
        let lifecycle_outcome = match &result {
            Ok(_) => {
                let guard = response_cell.lock().await;
                match guard.as_ref() {
                    Some(result) => match result.outcome_for_logging() {
                        ToolOutputOutcome::Skipped => ToolCallOutcome::Skipped,
                        ToolOutputOutcome::Success => ToolCallOutcome::Completed { success: true },
                        ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => {
                            ToolCallOutcome::Completed { success: false }
                        }
                    },
                    None => ToolCallOutcome::Failed {
                        handler_executed: true,
                    },
                }
            }
            Err(_) => ToolCallOutcome::Failed {
                handler_executed: true,
            },
        };
        notify_tool_finish_if_unclaimed(
            &invocation,
            terminal_outcome_reached.as_deref(),
            lifecycle_outcome,
        )
        .await;

        match result {
            Ok(_) => {
                let mut guard = response_cell.lock().await;
                let mut result = guard.take().ok_or_else(|| {
                    FunctionCallError::Fatal("tool produced no output".to_string())
                })?;
                if let Some(outcome) = post_tool_use_outcome {
                    if outcome.should_block {
                        let message = outcome.feedback_message.unwrap_or_else(|| {
                            "PostToolUse hook blocked the tool result".to_string()
                        });
                        let err = FunctionCallError::RespondToModel(message);
                        dispatch_trace.record_failed(&err);
                        return Err(err);
                    }
                    if let Some(feedback_message) = outcome.feedback_message {
                        result.result = Box::new(PostToolUseFeedbackOutput {
                            original: result.result,
                            model_visible: FunctionToolOutput::from_text(
                                feedback_message,
                                /*success*/ None,
                            ),
                        });
                    }
                }
                let projection_input = prepare_model_projection(&invocation, &result);
                let model_projection = match projection_input {
                    Some(input) => project_model_output(input).await,
                    None => None,
                };
                if let Some(projection) = &model_projection {
                    invocation
                        .turn
                        .turn_timing_state
                        .record_tool_output_projection(projection.projected_tokens);
                    invocation
                        .turn
                        .turn_timing_state
                        .record_tool_output_projection_facts(
                            projection.canonical_bytes,
                            projection.canonical_tokens,
                            projection.model_bytes,
                            projection.projected_tokens,
                            projection.artifact_created,
                            projection.projection_truncated,
                            projection.omitted_sections,
                        );
                    invocation
                        .session
                        .register_tool_history_candidate(
                            invocation.turn.config.codex_home.as_path(),
                            projection.candidate.clone(),
                        )
                        .await;
                }
                if matches!(
                    flat_tool_name(&invocation.tool_name).as_ref(),
                    "locate_task" | "search_source" | "read_file_span" | "read_tool_output"
                ) {
                    let injected_tokens = model_projection.as_ref().map_or_else(
                        || {
                            let response = result
                                .result
                                .to_response_item(&result.call_id, &result.payload);
                            serde_json::to_string(&response)
                                .map_or(0, |text| approx_token_count(&text) as u64)
                        },
                        |projection| projection.projected_tokens,
                    );
                    invocation
                        .turn
                        .turn_timing_state
                        .record_discovery_result_cell(injected_tokens);
                }
                result.model_projection = model_projection;
                dispatch_trace.record_completed(
                    &invocation,
                    &result.call_id,
                    &result.payload,
                    result.result.as_ref(),
                );
                Ok(result)
            }
            Err(err) => {
                dispatch_trace.record_failed(&err);
                Err(err)
            }
        }
    }
}

async fn notify_tool_finish_if_unclaimed(
    invocation: &ToolInvocation,
    terminal_outcome_reached: Option<&AtomicBool>,
    outcome: ToolCallOutcome,
) -> bool {
    if terminal_outcome_reached.is_some_and(|reached| reached.swap(true, Ordering::AcqRel)) {
        return false;
    }

    notify_tool_finish(invocation, outcome).await;
    true
}

async fn handle_any_tool(
    tool: &dyn CoreToolRuntime,
    invocation: ToolInvocation,
) -> Result<AnyToolResult, FunctionCallError> {
    let _tool_execution_timing_guard =
        matches!(tool.tool_execution_timing(), ToolExecutionTiming::Handler)
            .then(|| invocation.turn.turn_timing_state.begin_tool_execution());
    mark_tool_handler_entry();
    let output = tool.handle(invocation.clone()).await?;
    if output.contains_external_context()
        && invocation.turn.config.memories.disable_on_external_context
    {
        state_db::mark_thread_memory_mode_polluted(
            invocation.session.services.state_db.as_deref(),
            invocation.session.thread_id,
            "tool_output",
        )
        .await;
    }
    let post_tool_use_payload =
        CoreToolRuntime::post_tool_use_payload(tool, &invocation, output.as_ref());
    Ok(AnyToolResult {
        call_id: invocation.call_id,
        payload: invocation.payload,
        result: output,
        post_tool_use_payload,
        model_projection: None,
    })
}

struct ModelProjectionInput {
    spillable_text: String,
    outcome: ToolOutputOutcome,
    essential_inline: Value,
    origin_call_id: String,
    selection_facts: ProjectionSelectionFacts,
    applied_token_limit: usize,
    projected_text: String,
    preserved_content: Vec<Value>,
    codex_home: std::path::PathBuf,
    thread_id: String,
    tool_name: String,
    canonical: CanonicalToolResult,
    original_output_sha256: String,
    original_output_tokens: u64,
    semantic_class: String,
    projection_eligible: bool,
    projection_truncated: bool,
    predetermined_ranges: Vec<ToolOutputProjectionRange>,
    original_response: ResponseInputItem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectionSelectionFacts {
    mode: &'static str,
    available_fragments: usize,
    selected_fragments: usize,
    exact_duplicates_removed: usize,
    selected_ids: Vec<String>,
    omitted_inline_ids: Vec<String>,
    partial_ids: Vec<String>,
}

fn prepare_model_projection(
    invocation: &ToolInvocation,
    result: &AnyToolResult,
) -> Option<ModelProjectionInput> {
    // Exact artifact reads are already bounded and must never recursively spill.
    if invocation.tool_name.name == "read_tool_output" {
        return None;
    }

    let metadata = result.result.projection_metadata()?;
    if metadata.spillable_text.is_empty() {
        return None;
    }
    let spillable_text = metadata.spillable_text.join("\n");
    let outcome = match metadata.outcome {
        ToolOutputOutcome::Success => OutputOutcome::Success,
        ToolOutputOutcome::Failure => OutputOutcome::Failure,
        ToolOutputOutcome::TimedOut => OutputOutcome::TimedOut,
        ToolOutputOutcome::Skipped => OutputOutcome::Skipped,
    };
    let diagnostic_class = match metadata.diagnostic_class {
        ToolOutputDiagnosticClass::Normal => OutputDiagnosticClass::Normal,
        ToolOutputDiagnosticClass::HighSignal => OutputDiagnosticClass::HighSignal,
    };
    let requested_limit = metadata.requested_limit.or_else(|| {
        skill_read_projection_limit(
            flat_tool_name(&invocation.tool_name).as_ref(),
            &spillable_text,
        )
    });
    let limits = resolve_projected_output_limits(
        requested_limit,
        outcome,
        diagnostic_class,
        DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
    );
    let generic_projection = formatted_truncate_text_with_output_limit(&spillable_text, limits);
    let projection_truncated = generic_projection.was_truncated;
    if !generic_projection.was_truncated
        && !requires_canonical_projection_artifact(
            invocation.tool_name.name.as_ref(),
            &metadata.fragments,
        )
        && metadata.predetermined_ranges.is_empty()
    {
        return None;
    }
    let (projected_text, selection_facts) = if metadata.fragments.is_empty() {
        (
            generic_projection.text,
            ProjectionSelectionFacts {
                mode: "generic_fallback",
                available_fragments: 0,
                selected_fragments: 0,
                exact_duplicates_removed: 0,
                selected_ids: Vec::new(),
                omitted_inline_ids: Vec::new(),
                partial_ids: Vec::new(),
            },
        )
    } else {
        select_typed_projection_fragments(&metadata.fragments, limits.applied_limit)
    };

    let original_response = result
        .result
        .to_response_item(&result.call_id, &result.payload);
    let mut canonical = result.result.canonical_result(&result.payload)?;
    let original_output_text = match &original_response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => match &output.body {
            FunctionCallOutputBody::Text(text) => text,
            FunctionCallOutputBody::ContentItems(_) => {
                std::str::from_utf8(&canonical.bytes).ok()?
            }
        },
        ResponseInputItem::McpToolCallOutput { .. } => {
            std::str::from_utf8(&canonical.bytes).ok()?
        }
        _ => return None,
    };
    let preserved_content = preserved_non_text_content(&original_response);
    canonical.sections =
        canonical_projection_sections(&canonical, &metadata.fragments, &selection_facts);
    let validation_material = metadata.fragments.iter().any(|fragment| {
        fragment.kind == ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary
    });
    let semantic_class = if validation_material {
        "validation"
    } else if metadata.fragments.iter().any(|fragment| {
        matches!(
            fragment.kind,
            ToolOutputProjectionFragmentKind::SourcePrimaryImplementation
                | ToolOutputProjectionFragmentKind::SourceCaller
                | ToolOutputProjectionFragmentKind::SourceTest
                | ToolOutputProjectionFragmentKind::SourceContractOrGenerated
        )
    }) {
        "source_evidence"
    } else {
        "tool_output"
    };
    let original_output_sha256 = crate::tool_history::sha256(original_output_text.as_bytes());
    let original_output_tokens = approx_token_count(original_output_text) as u64;
    Some(ModelProjectionInput {
        spillable_text,
        outcome: metadata.outcome,
        essential_inline: metadata.essential_inline,
        origin_call_id: result.call_id.clone(),
        selection_facts,
        applied_token_limit: limits.applied_limit,
        projected_text,
        preserved_content,
        codex_home: invocation.turn.config.codex_home.to_path_buf(),
        thread_id: invocation.session.thread_id.to_string(),
        tool_name: flat_tool_name(&invocation.tool_name).into_owned(),
        canonical,
        original_output_sha256,
        original_output_tokens,
        semantic_class: semantic_class.to_string(),
        projection_eligible: true,
        projection_truncated,
        predetermined_ranges: validated_predetermined_ranges(&metadata.predetermined_ranges),
        original_response,
    })
}

fn requires_canonical_projection_artifact(
    tool_name: &str,
    fragments: &[ToolOutputProjectionFragment],
) -> bool {
    matches!(tool_name, "locate_task" | "read_file_span")
        && fragments.iter().any(|fragment| fragment.id.is_some())
}

fn canonical_projection_sections(
    canonical: &CanonicalToolResult,
    fragments: &[ToolOutputProjectionFragment],
    selection: &ProjectionSelectionFacts,
) -> Vec<ToolProjectionSection> {
    if fragments.is_empty() {
        return vec![ToolProjectionSection {
            id: "result".to_string(),
            // The canonical value already lives in the artifact. Repeating it in
            // the section directory would make metadata grow with the omitted
            // payload and could force the projection to collapse to a bare ID.
            value: None,
            exact_bytes: canonical.exact_bytes,
            inclusion: ToolProjectionInclusion::Omitted,
            canonical_range: Some(CanonicalByteRange::new(0, canonical.exact_bytes)),
            // JSON pointer children use a different selector namespace. They
            // are advertised by pointer selection, not as section IDs.
            children: Vec::new(),
            recovery_chunk_bytes: None,
        }];
    }
    let selected = selection.selected_ids.iter().collect::<HashSet<_>>();
    let partial = selection.partial_ids.iter().collect::<HashSet<_>>();
    let mut cursor = 0_usize;
    fragments
        .iter()
        .enumerate()
        .map(|(index, fragment)| {
            let id = fragment
                .id
                .clone()
                .unwrap_or_else(|| format!("fragment:{index}"));
            let range = canonical.bytes[cursor..]
                .windows(fragment.text.len())
                .position(|window| window == fragment.text.as_bytes())
                .map(|offset| {
                    let start = cursor + offset;
                    let end = start + fragment.text.len();
                    cursor = end;
                    CanonicalByteRange::new(start as u64, end as u64)
                });
            ToolProjectionSection {
                exact_bytes: range.map_or(fragment.text.len() as u64, CanonicalByteRange::len),
                inclusion: if selected.contains(&id) && !partial.contains(&id) {
                    ToolProjectionInclusion::Included
                } else {
                    ToolProjectionInclusion::Omitted
                },
                // `selected_text` is the sole inline copy. Section metadata is
                // an address directory, never a second payload copy.
                value: None,
                canonical_range: range,
                children: Vec::new(),
                recovery_chunk_bytes: None,
                id,
            }
        })
        .collect()
}

const PROJECTION_FRAGMENT_KIND_ORDER: [ToolOutputProjectionFragmentKind; 11] = [
    ToolOutputProjectionFragmentKind::SourcePrimaryImplementation,
    ToolOutputProjectionFragmentKind::SourceCaller,
    ToolOutputProjectionFragmentKind::SourceTest,
    ToolOutputProjectionFragmentKind::SourceContractOrGenerated,
    ToolOutputProjectionFragmentKind::CoreInstructionOrTaskState,
    ToolOutputProjectionFragmentKind::CitationOrExactSpan,
    ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
    ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary,
    ToolOutputProjectionFragmentKind::ProcessFinalStatus,
    ToolOutputProjectionFragmentKind::SearchMatchOrDefinition,
    ToolOutputProjectionFragmentKind::ContextualSpillableText,
];

fn select_typed_projection_fragments(
    fragments: &[ToolOutputProjectionFragment],
    token_limit: usize,
) -> (String, ProjectionSelectionFacts) {
    let mut seen = HashSet::new();
    let unique = fragments
        .iter()
        .filter(|fragment| seen.insert((*fragment).clone()))
        .collect::<Vec<_>>();
    let exact_duplicates_removed = fragments.len().saturating_sub(unique.len());
    let active_kinds = PROJECTION_FRAGMENT_KIND_ORDER
        .iter()
        .filter(|kind| unique.iter().any(|fragment| fragment.kind == **kind))
        .count();
    let active_headings = PROJECTION_FRAGMENT_KIND_ORDER
        .iter()
        .filter(|kind| unique.iter().any(|fragment| fragment.kind == **kind))
        .map(|kind| fragment_section_heading(*kind))
        .collect::<Vec<_>>();
    // Approximate-token counting carries per-call framing overhead. Count the
    // headings as the single string they will become so eleven small typed
    // sections do not incorrectly consume the entire budget before payloads.
    let headings_tokens = approx_token_count(&active_headings.join("\n\n"));
    let section_budget = token_limit
        .saturating_sub(headings_tokens)
        .checked_div(active_kinds.max(1))
        .unwrap_or(0);
    let mut sections = Vec::new();
    let mut selected_fragments = 0;
    let mut selected_ids = Vec::new();
    let mut partial_ids = Vec::new();

    for kind in PROJECTION_FRAGMENT_KIND_ORDER {
        let section_fragments = unique
            .iter()
            .filter(|fragment| fragment.kind == kind)
            .collect::<Vec<_>>();
        if section_fragments.is_empty() {
            continue;
        }
        let mut remaining_budget = section_budget;
        let mut bounded_fragments = Vec::new();
        for fragment in section_fragments {
            if remaining_budget == 0 {
                break;
            }
            let separator_tokens = usize::from(!bounded_fragments.is_empty());
            if remaining_budget <= separator_tokens {
                break;
            }
            remaining_budget = remaining_budget.saturating_sub(separator_tokens);
            let bounded = bounded_fragment_text(&fragment.text, remaining_budget);
            if bounded.is_empty() {
                continue;
            }
            remaining_budget = remaining_budget.saturating_sub(approx_token_count(&bounded));
            if let Some(id) = &fragment.id {
                selected_ids.push(id.clone());
                if bounded != fragment.text {
                    partial_ids.push(id.clone());
                }
            }
            bounded_fragments.push(bounded);
        }
        if bounded_fragments.is_empty() {
            continue;
        }
        selected_fragments += bounded_fragments.len();
        sections.push(format!(
            "{}\n{}",
            fragment_section_heading(kind),
            bounded_fragments.join("\n")
        ));
    }

    let projected = truncate_text_to_token_ceiling(&sections.join("\n\n"), token_limit);
    let selected_id_set = selected_ids.iter().collect::<HashSet<_>>();
    let omitted_inline_ids = unique
        .iter()
        .filter_map(|fragment| fragment.id.as_ref())
        .filter(|id| !selected_id_set.contains(id))
        .cloned()
        .collect();
    (
        projected,
        ProjectionSelectionFacts {
            mode: "typed_fragments",
            available_fragments: fragments.len(),
            selected_fragments,
            exact_duplicates_removed,
            selected_ids,
            omitted_inline_ids,
            partial_ids,
        },
    )
}

fn bounded_fragment_text(text: &str, token_budget: usize) -> String {
    let bounded = truncate_text_to_token_ceiling(text, token_budget);
    if !bounded.is_empty() || token_budget == 0 || text.is_empty() {
        return bounded;
    }
    // The generic middle-truncation marker can consume an extremely small
    // fair-share budget. Typed projection still needs one whole, attributable
    // fragment per active section, so fall back to the longest exact prefix
    // that fits without a marker.
    let mut prefix = String::new();
    for ch in text.chars() {
        prefix.push(ch);
        if approx_token_count(&prefix) > token_budget {
            prefix.pop();
            break;
        }
    }
    prefix
}

fn fragment_section_heading(kind: ToolOutputProjectionFragmentKind) -> &'static str {
    match kind {
        ToolOutputProjectionFragmentKind::SourcePrimaryImplementation => "[primary]",
        ToolOutputProjectionFragmentKind::SourceCaller => "[callers]",
        ToolOutputProjectionFragmentKind::SourceTest => "[tests]",
        ToolOutputProjectionFragmentKind::SourceContractOrGenerated => "[contracts]",
        ToolOutputProjectionFragmentKind::CoreInstructionOrTaskState => "[task state]",
        ToolOutputProjectionFragmentKind::CitationOrExactSpan => "[citations and exact spans]",
        ToolOutputProjectionFragmentKind::SearchMatchOrDefinition => "[search]",
        ToolOutputProjectionFragmentKind::ErrorOrDiagnostic => "[errors and diagnostics]",
        ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary => "[validation]",
        ToolOutputProjectionFragmentKind::ProcessFinalStatus => "[process final status]",
        ToolOutputProjectionFragmentKind::ContextualSpillableText => "[context]",
    }
}

const COMPLETE_SKILL_READ_TOKEN_LIMIT: usize = 4_000;

fn skill_read_projection_limit(tool_name: &str, output: &str) -> Option<usize> {
    if !matches!(
        tool_name,
        "read_file_span" | "functions.read_file_span" | "functions.exec"
    ) {
        return None;
    }

    let normalized;
    let output = if tool_name == "functions.exec" && !output.contains("\nSource file evidence:\n") {
        normalized = output.replace("\\n", "\n");
        normalized.as_str()
    } else {
        output
    };
    let evidence = output.strip_prefix("Source file evidence:\n").or_else(|| {
        output
            .split_once("\nSource file evidence:\n")
            .map(|(_, rest)| rest)
    })?;
    let mut lines = evidence.lines();
    let citation = lines.next()?.strip_prefix("citation: ")?;
    let file_and_span = citation.rsplit(['/', '\\']).next()?;
    if !file_and_span.starts_with("SKILL.md:") {
        return None;
    }
    let metadata = lines.next()?;
    if !metadata.starts_with("total_lines: ") || !metadata.contains(" truncated: false") {
        return None;
    }
    lines
        .next()?
        .starts_with("source_route: ")
        .then_some(COMPLETE_SKILL_READ_TOKEN_LIMIT)
}

async fn project_model_output(input: ModelProjectionInput) -> Option<ModelToolProjection> {
    let ModelProjectionInput {
        spillable_text: _spillable_text,
        outcome,
        essential_inline,
        origin_call_id,
        selection_facts,
        applied_token_limit,
        projected_text,
        preserved_content,
        codex_home,
        thread_id,
        tool_name,
        canonical,
        original_output_sha256,
        original_output_tokens,
        semantic_class,
        projection_eligible,
        projection_truncated,
        predetermined_ranges,
        original_response,
    } = input;
    if !projection_eligible {
        return None;
    }
    let existing_artifact_id = essential_inline
        .get("raw_output_artifact_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let artifact_created = existing_artifact_id.is_none();
    let artifact = if let Some(artifact_id) = existing_artifact_id {
        attach_canonical_output_artifact(&codex_home, &thread_id, &artifact_id, &canonical).await
    } else {
        create_canonical_output_artifact(&codex_home, &thread_id, &canonical).await
    };
    let artifact_id = artifact.artifact_id()?;
    if !artifact.complete
        || artifact.retained_bytes != canonical.exact_bytes
        || !artifact.unavailable_ranges.is_empty()
    {
        return None;
    }
    crate::tools::command_output_artifact::protect_active_tool_history_artifact(
        &codex_home,
        &thread_id,
        &artifact_id,
        canonical.exact_bytes,
        &canonical.sha256,
    )
    .await
    .ok()?;
    let outcome = match outcome {
        ToolOutputOutcome::Success => "success",
        ToolOutputOutcome::Failure => "failure",
        ToolOutputOutcome::TimedOut => "timeout",
        ToolOutputOutcome::Skipped => "skipped",
    };
    let result_value = serde_json::json!({
        "essential": essential_inline,
        "selection": {
            "mode": selection_facts.mode,
            "origin_call_id": origin_call_id.clone(),
            "available_fragments": selection_facts.available_fragments,
            "selected_fragments": selection_facts.selected_fragments,
            "exact_duplicates_removed": selection_facts.exact_duplicates_removed,
            "selected_ids": selection_facts.selected_ids,
            "omitted_inline_ids": selection_facts.omitted_inline_ids,
            "partial_ids": selection_facts.partial_ids,
        },
        "selected_text": "",
        "preserved_content": preserved_content,
        "artifact": {
            "retained_bytes": artifact.retained_bytes,
            "complete": artifact.complete,
            "unavailable_ranges": artifact.unavailable_ranges,
            "error": artifact.error,
        }
    });
    let omitted_sections = canonical
        .sections
        .iter()
        .filter(|section| section.inclusion != ToolProjectionInclusion::Included)
        .map(|section| section.id.clone())
        .collect::<Vec<_>>();
    let omitted_section_count = omitted_sections.len() as u64;
    let envelope = ToolProjectionV1 {
        version: 1,
        tool: tool_name.clone(),
        outcome: outcome.to_string(),
        canonical_sha256: canonical.sha256.clone(),
        canonical_bytes: canonical.exact_bytes,
        canonical_approximate_tokens: canonical.approximate_tokens,
        canonical_complete: canonical.complete,
        model_bytes: 0,
        model_approximate_tokens: 0,
        artifact_id: Some(artifact_id.clone()),
        sections: canonical.sections.clone(),
        omitted_sections,
        result: result_value,
    };
    let (projected_text, deterministic_continuation_receipt) = drain_predetermined_artifact_ranges(
        &codex_home,
        &thread_id,
        &artifact_id,
        &canonical.sha256,
        projected_text,
        predetermined_ranges,
        applied_token_limit,
    )
    .await;
    let (_envelope, rendered) =
        serialize_projection_with_limit(envelope, &projected_text, applied_token_limit)?;
    let projected_tokens = approx_token_count(&rendered) as u64;
    let model_bytes = rendered.len() as u64;
    Some(ModelToolProjection {
        response: projected_response_item(original_response, rendered.clone()),
        candidate: crate::tool_history::ToolHistoryCandidate {
            call_id: origin_call_id,
            tool_identity: tool_name,
            semantic_class,
            artifact_id,
            artifact_bytes: canonical.exact_bytes,
            artifact_sha256: canonical.sha256,
            original_output_sha256,
            original_tokens: original_output_tokens,
            bounded_digest: rendered,
            complete: canonical.complete,
            projection_eligible,
            proof_identity: None,
            supersession_identity: None,
            consumed_by_generation: None,
        },
        projected_tokens,
        canonical_bytes: canonical.exact_bytes,
        canonical_tokens: canonical.approximate_tokens,
        model_bytes,
        artifact_created,
        projection_truncated,
        omitted_sections: omitted_section_count,
        deterministic_continuation_receipt,
    })
}

fn projected_response_item(original: ResponseInputItem, rendered: String) -> ResponseInputItem {
    match original {
        ResponseInputItem::FunctionCallOutput {
            call_id,
            mut output,
        } => {
            output.body = FunctionCallOutputBody::Text(rendered);
            ResponseInputItem::FunctionCallOutput { call_id, output }
        }
        ResponseInputItem::CustomToolCallOutput {
            call_id,
            name,
            mut output,
        } => {
            output.body = FunctionCallOutputBody::Text(rendered);
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                name,
                output,
            }
        }
        ResponseInputItem::McpToolCallOutput {
            call_id,
            mut output,
        } => {
            output
                .content
                .retain(|item| item.get("type").and_then(Value::as_str) != Some("text"));
            output.content.insert(
                0,
                serde_json::json!({
                    "type": "text",
                    "text": rendered,
                }),
            );
            output.structured_content = None;
            ResponseInputItem::McpToolCallOutput { call_id, output }
        }
        original => original,
    }
}

fn validated_predetermined_ranges(
    ranges: &[ToolOutputProjectionRange],
) -> Vec<ToolOutputProjectionRange> {
    if ranges.is_empty() || ranges.len() > 64 {
        return Vec::new();
    }
    let mut normalized = ranges.to_vec();
    normalized.sort_unstable_by_key(|range| (range.start_line, range.end_line, range.id.clone()));
    let mut total_lines = 0_usize;
    let mut prior_end = 0_usize;
    let mut ids = HashSet::new();
    for range in &normalized {
        let Some(lines) = range
            .end_line
            .checked_sub(range.start_line)
            .and_then(|delta| delta.checked_add(1))
        else {
            return Vec::new();
        };
        if range.id.is_empty()
            || range.start_line == 0
            || range.start_line <= prior_end
            || !ids.insert(range.id.clone())
        {
            return Vec::new();
        }
        total_lines = match total_lines.checked_add(lines) {
            Some(total) if total <= 200 => total,
            _ => return Vec::new(),
        };
        prior_end = range.end_line;
    }
    normalized
}

async fn drain_predetermined_artifact_ranges(
    codex_home: &std::path::Path,
    thread_id: &str,
    artifact_id: &str,
    state_revision: &str,
    projected_text: String,
    ranges: Vec<ToolOutputProjectionRange>,
    applied_token_limit: usize,
) -> (String, Option<TurnTimingDeterministicContinuationReceipt>) {
    if ranges.is_empty() {
        return (projected_text, None);
    }
    let selectors = ranges
        .iter()
        .map(|range| ToolOutputSelector::Lines {
            start: range.start_line,
            end: range.end_line,
        })
        .collect();
    let Ok(result) =
        read_tool_output_selectors(codex_home, thread_id, artifact_id, selectors).await
    else {
        return (projected_text, None);
    };
    if !result.complete
        || !result.unavailable_ranges.is_empty()
        || result.results.len() != ranges.len()
    {
        return (projected_text, None);
    }
    for selected in &result.results {
        if selected.status != ToolOutputSelectorStatus::Ok
            || !selected.complete
            || selected.continuation.is_some()
            || selected.text.is_none()
        {
            return (projected_text, None);
        }
    }
    let drained = serde_json::json!({
        "artifact_id": result.artifact_id,
        "canonical_sha256": result.canonical_sha256,
        "predetermined_ranges": ranges.iter().zip(result.results).map(|(range, selected)| {
            serde_json::json!({
                "id": range.id,
                "start_line": range.start_line,
                "end_line": range.end_line,
                "text": selected.text,
                "continuation": selected.continuation,
            })
        }).collect::<Vec<_>>(),
    })
    .to_string();
    if drained.len() > 16 * 1024 {
        return (projected_text, None);
    }
    let combined =
        format!("{projected_text}\nHost-drained predetermined artifact ranges:\n{drained}");
    if approx_token_count(&combined) > applied_token_limit {
        return (projected_text, None);
    }
    (
        combined,
        Some(TurnTimingDeterministicContinuationReceipt {
            class: DeterministicContinuationClass::ArtifactRange,
            resource_identity_hash: crate::tool_history::sha256(artifact_id.as_bytes()),
            state_revision: state_revision.to_string(),
            host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
            suppressed_continuation_count: u32::try_from(ranges.len()).unwrap_or(u32::MAX),
            avoided_token_usage: None,
        }),
    )
}

fn serialize_projection_with_limit(
    mut envelope: ToolProjectionV1,
    output: &str,
    token_limit: usize,
) -> Option<(Value, String)> {
    // A JSON value cannot represent zero tokens. Keep that degenerate request to
    // the smallest valid JSON value while enforcing every positive limit exactly.
    let effective_limit = token_limit.max(1);
    let mut output_limit = effective_limit;
    loop {
        envelope.result["selected_text"] =
            Value::String(truncate_text_to_token_ceiling(output, output_limit));
        let first = serde_json::to_string(&envelope).ok()?;
        envelope.model_bytes = first.len() as u64;
        envelope.model_approximate_tokens = approx_token_count(&first) as u64;
        let rendered = serde_json::to_string(&envelope).ok()?;
        let rendered_tokens = approx_token_count(&rendered);
        if rendered_tokens <= effective_limit {
            return Some((serde_json::to_value(envelope).ok()?, rendered));
        }
        if output_limit == 0 {
            break;
        }
        output_limit = output_limit
            .saturating_sub((rendered_tokens - effective_limit).max(1))
            .min(output_limit - 1);
    }

    // Exceptionally large metadata (for example an inline image) can exceed the
    // limit without any text body. Retain the artifact locator when it fits, then
    // fall back to the smallest valid JSON projection.
    let artifact_id = envelope.artifact_id.clone();
    for fallback in [
        serde_json::json!({ "artifact_id": artifact_id }),
        serde_json::json!({}),
        Value::Null,
    ] {
        let rendered = serde_json::to_string(&fallback).ok()?;
        if approx_token_count(&rendered) <= effective_limit {
            return Some((fallback, rendered));
        }
    }
    None
}

fn preserved_non_text_content(response: &ResponseInputItem) -> Vec<Value> {
    let output = match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => output,
        ResponseInputItem::McpToolCallOutput { output, .. } => {
            return output
                .content
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) != Some("text"))
                .cloned()
                .collect();
        }
        _ => return Vec::new(),
    };
    let FunctionCallOutputBody::ContentItems(items) = &output.body else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| !matches!(item, FunctionCallOutputContentItem::InputText { .. }))
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect()
}

fn function_hook_tool_name(invocation: &ToolInvocation) -> HookToolName {
    if invocation.tool_name.name == "spawn_agent"
        && matches!(
            invocation.tool_name.namespace.as_deref(),
            None | Some(MULTI_AGENT_V1_NAMESPACE)
        )
    {
        return HookToolName::spawn_agent();
    }

    HookToolName::new(flat_tool_name(&invocation.tool_name).into_owned())
}

fn function_hook_tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }

    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn unsupported_tool_call_message(payload: &ToolPayload, tool_name: &ToolName) -> String {
    match payload {
        ToolPayload::Custom { .. } => format!("unsupported custom tool call: {tool_name}"),
        _ => format!("unsupported call: {tool_name}"),
    }
}
#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
