mod delegate;
mod execute_handler;
pub(crate) mod execute_spec;
mod response_adapter;
mod wait_handler;
pub(crate) mod wait_spec;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::FunctionCallError;
use crate::session::reasoning_governor::CodeModeToolResult;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::RequiredToolTerminalCause;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::effective_tool_mode;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::parallel::required_tool_error_terminal_cause;
use crate::tools::parallel::required_tool_terminal_cause;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputSkipDisposition;
use codex_tools::can_request_original_image_detail;
use codex_tools::sanitize_original_image_detail as sanitize_image_detail_items;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text_content_items_with_policy;
use codex_utils_output_truncation::resolve_output_limits;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_string::approx_token_count;

use delegate::CodeModeDispatchBroker;
use delegate::CodeModeDispatchWorker;
pub(crate) use execute_handler::CodeModeExecuteHandler;
use response_adapter::into_function_call_output_content_items;
pub(crate) use wait_handler::CodeModeWaitHandler;

pub(crate) const PUBLIC_TOOL_NAME: &str = codex_code_mode::PUBLIC_TOOL_NAME;
pub(crate) const WAIT_TOOL_NAME: &str = codex_code_mode::WAIT_TOOL_NAME;

/// Returns true for the un-namespaced code-mode `exec` tool.
pub(crate) fn is_exec_tool_name(tool_name: &ToolName) -> bool {
    tool_name.namespace.is_none() && tool_name.name == PUBLIC_TOOL_NAME
}

#[derive(Clone)]
pub(crate) struct ExecContext {
    pub(super) session: Arc<Session>,
    pub(super) turn: Arc<TurnContext>,
}

pub(crate) struct CodeModeService {
    session: OnceCell<Arc<dyn CodeModeSession>>,
    session_provider: Arc<dyn CodeModeSessionProvider>,
    dispatch_broker: Arc<CodeModeDispatchBroker>,
    packet_admission: Mutex<CodeModePacketAdmission>,
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct CodeModePacketAdmission {
    cells: HashMap<String, CodeModePacketMetrics>,
    consecutive_tiny_packets_by_turn: HashMap<String, u8>,
}

#[derive(Default)]
struct CodeModePacketMetrics {
    next_nested_ordinal: usize,
    nested_call_count: usize,
    batchable_observation_count: usize,
    result_bytes: usize,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    first_required_terminal: Option<CodeModeNestedTerminal>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodeModeNestedTerminal {
    ordinal: usize,
    cause: RequiredToolTerminalCause,
    message: String,
}

struct CodeModePacketReceipt {
    nested_call_count: usize,
    batchable_observation_count: usize,
    result_bytes: usize,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    first_required_terminal: Option<CodeModeNestedTerminal>,
    advisory: Option<&'static str>,
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: usize,
}

impl std::io::Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_json_len(value: &JsonValue) -> usize {
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value).map_or(0, |()| counter.bytes)
}

const TINY_PACKET_RESULT_BYTES: usize = 1_024;
const TINY_PACKET_ADVISORY: &str = "Low-density packet: on the next decision, batch every known independent read in one exec, await notify(...) per settlement, and use Promise.allSettled only to keep the cell alive. Resume only a documented live handle. If evidence is sufficient or unchanged, synthesize and stop instead of sampling or polling again.";

impl CodeModeService {
    pub(crate) fn new(session_provider: Arc<dyn CodeModeSessionProvider>) -> Self {
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::new());
        Self {
            session: OnceCell::new(),
            session_provider,
            dispatch_broker,
            packet_admission: Mutex::new(CodeModePacketAdmission::default()),
            shutting_down: AtomicBool::new(false),
        }
    }

    pub(crate) fn session_provider(&self) -> Arc<dyn CodeModeSessionProvider> {
        Arc::clone(&self.session_provider)
    }

    pub(crate) async fn execute(
        &self,
        request: codex_code_mode::ExecuteRequest,
    ) -> Result<codex_code_mode::StartedCell, String> {
        self.session().await?.execute(request).await
    }

    pub(crate) async fn wait(
        &self,
        request: codex_code_mode::WaitRequest,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.session().await?.wait(request).await
    }

    pub(crate) async fn wait_for_state_change(
        &self,
        cell_id: codex_code_mode::CellId,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.wait(codex_code_mode::WaitRequest {
            cell_id,
            yield_time_ms: codex_code_mode::OWNER_HELD_STATE_CHANGE_YIELD_TIME_MS,
        })
        .await
    }

    pub(crate) async fn terminate(
        &self,
        cell_id: CellId,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.session().await?.terminate(cell_id).await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutting_down.store(true, Ordering::Release);
        // Join any initialization already in progress without initializing an unused service.
        match self
            .session
            .get_or_try_init(|| async {
                Err::<Arc<dyn CodeModeSession>, String>(
                    "code mode session is shutting down".to_string(),
                )
            })
            .await
        {
            Ok(session) => session.shutdown().await,
            Err(_) => Ok(()),
        }
    }

    pub(crate) fn mark_cell_ready_for_dispatch(&self, cell_id: &codex_code_mode::CellId) {
        self.dispatch_broker.mark_cell_ready_for_dispatch(cell_id);
    }

    pub(crate) fn finish_cell_dispatch(&self, cell_id: &CellId) {
        self.dispatch_broker.close_cell(cell_id);
    }

    fn begin_packet_call(&self, cell_id: &CellId) -> usize {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = admission.cells.entry(cell_id.to_string()).or_default();
        let ordinal = metrics.next_nested_ordinal;
        metrics.next_nested_ordinal = metrics.next_nested_ordinal.saturating_add(1);
        metrics.nested_call_count = metrics.nested_call_count.saturating_add(1);
        ordinal
    }

    fn complete_packet_call(
        &self,
        cell_id: &CellId,
        ordinal: usize,
        batchable_observation: bool,
        result_bytes: usize,
        post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
        required_terminal: Option<(RequiredToolTerminalCause, String)>,
    ) {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = admission.cells.entry(cell_id.to_string()).or_default();
        metrics.batchable_observation_count = metrics
            .batchable_observation_count
            .saturating_add(usize::from(batchable_observation));
        metrics.result_bytes = metrics.result_bytes.saturating_add(result_bytes);
        metrics
            .post_tool_use_feedback
            .extend(post_tool_use_feedback);
        if let Some((cause, message)) = required_terminal {
            let candidate = CodeModeNestedTerminal {
                ordinal,
                cause,
                message,
            };
            if metrics
                .first_required_terminal
                .as_ref()
                .is_none_or(|current| candidate.ordinal < current.ordinal)
            {
                metrics.first_required_terminal = Some(candidate);
            }
        }
    }

    #[cfg(test)]
    fn record_packet_call(
        &self,
        cell_id: &CellId,
        batchable_observation: bool,
        result_bytes: usize,
        post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    ) {
        let ordinal = self.begin_packet_call(cell_id);
        self.complete_packet_call(
            cell_id,
            ordinal,
            batchable_observation,
            result_bytes,
            post_tool_use_feedback,
            None,
        );
    }

    fn finish_packet(&self, cell_id: &str, turn_id: &str) -> CodeModePacketReceipt {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = admission.cells.remove(cell_id).unwrap_or_default();
        let tiny_read_only_packet = metrics.nested_call_count == 1
            && metrics.batchable_observation_count == 1
            && metrics.result_bytes <= TINY_PACKET_RESULT_BYTES;
        let consecutive = admission
            .consecutive_tiny_packets_by_turn
            .entry(turn_id.to_string())
            .or_default();
        if tiny_read_only_packet {
            *consecutive = consecutive.saturating_add(1);
        } else {
            *consecutive = 0;
        }
        let advisory = (*consecutive == 1).then_some(TINY_PACKET_ADVISORY);
        CodeModePacketReceipt {
            nested_call_count: metrics.nested_call_count,
            batchable_observation_count: metrics.batchable_observation_count,
            result_bytes: metrics.result_bytes,
            post_tool_use_feedback: metrics.post_tool_use_feedback,
            first_required_terminal: metrics.first_required_terminal,
            advisory,
        }
    }

    pub(crate) fn finish_turn(&self, turn_id: &str) {
        self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .consecutive_tiny_packets_by_turn
            .remove(turn_id);
    }

    pub(crate) fn record_owner_drained_continuation(
        &self,
        cell_id: &CellId,
        continuation: crate::session::reasoning_governor::PendingOwnerDrainedContinuation,
    ) {
        self.dispatch_broker
            .record_continuation(cell_id, continuation);
    }

    pub(crate) fn owner_drained_continuation_snapshot(
        &self,
        owner_key: &str,
    ) -> Vec<crate::session::reasoning_governor::PendingOwnerDrainedContinuation> {
        self.dispatch_broker
            .continuation_snapshot(&CellId::new(owner_key.to_string()))
    }

    pub(crate) fn acknowledge_owner_drained_continuations(
        &self,
        owner_key: &str,
        accepted: &[codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt],
    ) {
        self.dispatch_broker
            .acknowledge_continuations(&CellId::new(owner_key.to_string()), accepted);
    }

    pub(crate) fn start_turn_worker(
        &self,
        session: &Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
        request_signals: crate::session::reasoning_governor::SamplingRequestSignalCollector,
    ) -> Option<CodeModeDispatchWorker> {
        let turn = &step_context.turn;
        let tool_mode = effective_tool_mode(turn);
        if !matches!(tool_mode, ToolMode::CodeMode | ToolMode::CodeModeOnly) {
            return None;
        }

        let exec = ExecContext {
            session: Arc::clone(session),
            turn: Arc::clone(turn),
        };
        Some(
            self.dispatch_broker
                .start_turn_worker(exec, step_context, tracker, request_signals),
        )
    }

    async fn session(&self) -> Result<Arc<dyn CodeModeSession>, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("code mode session is shutting down".to_string());
        }
        self.session
            .get_or_try_init(|| async {
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err("code mode session is shutting down".to_string());
                }
                let session = self
                    .session_provider
                    .create_session(self.dispatch_broker.clone())
                    .await?;
                if self.shutting_down.load(Ordering::Acquire) {
                    let _ = session.shutdown().await;
                    return Err("code mode session is shutting down".to_string());
                }
                Ok(session)
            })
            .await
            .map(Arc::clone)
    }
}

pub(super) fn handle_runtime_response(
    exec: &ExecContext,
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    started_at: std::time::Instant,
) -> Result<FunctionToolOutput, String> {
    // Nested tool results have already crossed their owning tool boundary. Keep
    // one coherent, model-safe exec packet here instead of applying the much
    // smaller generic per-tool diagnostic budget a second time.
    let hard_limit = codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL
        .min(TruncationPolicy::from(exec.turn.model_info.truncation_policy).token_budget());
    let original_image_detail_supported = can_request_original_image_detail(&exec.turn.model_info);

    let cell_id = runtime_response_cell_id(&response);
    let packet = exec
        .session
        .services
        .code_mode_service
        .finish_packet(cell_id, exec.turn.sub_id.as_str());
    tracing::info!(
        target: "codex.code_mode.packet",
        cell_id,
        nested_call_count = packet.nested_call_count,
        batchable_observation_count = packet.batchable_observation_count,
        result_bytes = packet.result_bytes,
        post_tool_use_feedback_count = packet.post_tool_use_feedback.len(),
        low_density_advisory = packet.advisory.is_some(),
        "code-mode packet admission receipt"
    );
    let mut output = format_runtime_response(
        response,
        max_output_tokens,
        hard_limit,
        original_image_detail_supported,
        started_at,
        packet.post_tool_use_feedback,
    );
    if let Some(required_terminal) = packet.first_required_terminal {
        output = fold_nested_required_terminal(output, required_terminal);
    }
    if let Some(advisory) = packet.advisory {
        output.body.push(FunctionCallOutputContentItem::InputText {
            text: advisory.to_string(),
        });
    }
    Ok(output)
}

fn fold_nested_required_terminal(
    mut output: FunctionToolOutput,
    terminal: CodeModeNestedTerminal,
) -> FunctionToolOutput {
    output.body.push(FunctionCallOutputContentItem::InputText {
        text: format!("Required nested tool outcome: {}", terminal.message),
    });
    match terminal.cause {
        RequiredToolTerminalCause::Blocked => output
            .with_skip_disposition(ToolOutputSkipDisposition::BlockingRequiredOperation)
            .with_sampling_request_signal(serde_json::json!({
                "outcome": "blocked",
                "nested_ordinal": terminal.ordinal,
            })),
        RequiredToolTerminalCause::Failure => output
            .with_outcome(ToolOutputOutcome::Failure)
            .with_sampling_request_signal(serde_json::json!({
                "outcome": "failure",
                "nested_ordinal": terminal.ordinal,
            })),
        RequiredToolTerminalCause::TimedOut => output
            .with_outcome(ToolOutputOutcome::TimedOut)
            .with_sampling_request_signal(serde_json::json!({
                "outcome": "timeout",
                "nested_ordinal": terminal.ordinal,
            })),
        RequiredToolTerminalCause::RecoverableCancellation => output
            .with_outcome(ToolOutputOutcome::Failure)
            .with_sampling_request_signal(serde_json::json!({
                "outcome": "recoverable_cancellation",
                "nested_ordinal": terminal.ordinal,
            })),
    }
}

fn required_nested_tool_terminal_cause(
    outcome_context: ToolOutputOutcomeContext,
    signal: Option<&JsonValue>,
) -> Option<RequiredToolTerminalCause> {
    let failed = outcome_context.outcome == ToolOutputOutcome::Failure;
    required_tool_terminal_cause(outcome_context, signal)
        .or_else(|| failed.then_some(RequiredToolTerminalCause::Failure))
}

fn runtime_response_cell_id(response: &RuntimeResponse) -> &str {
    match response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id.as_str(),
    }
}

fn format_runtime_response(
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    hard_limit: usize,
    original_image_detail_supported: bool,
    started_at: std::time::Instant,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
) -> FunctionToolOutput {
    let continuation_owner_key = match &response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id.to_string(),
    };
    let script_status = format_script_status(&response);
    let yielded = matches!(&response, RuntimeResponse::Yielded { .. });
    let (mut content_items, outcome, success) = match response {
        RuntimeResponse::Yielded { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::Success, true)
        }
        RuntimeResponse::Terminated { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::Failure, false)
        }
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => {
            let mut content_items = into_function_call_output_content_items(content_items);
            let success = error_text.is_none();
            if let Some(error_text) = error_text {
                content_items.push(FunctionCallOutputContentItem::InputText {
                    text: format!("Script error:\n{error_text}"),
                });
            }
            let outcome = if success {
                OutputOutcome::Success
            } else {
                OutputOutcome::Failure
            };
            (content_items, outcome, success)
        }
    };

    content_items.extend(post_tool_use_feedback);
    sanitize_image_detail_items(original_image_detail_supported, &mut content_items);
    let mut canonical_content_items = content_items.clone();
    let mut content_items =
        truncate_code_mode_result(content_items, max_output_tokens, outcome, hard_limit);
    let semantic_evidence = serde_json::json!({
        "status": &script_status,
        "content_items": &content_items,
    });
    let elapsed = started_at.elapsed();
    prepend_script_status(&mut content_items, &script_status, elapsed);
    prepend_script_status(&mut canonical_content_items, &script_status, elapsed);
    let typed_outcome = match (yielded, outcome) {
        (true, _) => codex_tools::ToolOutputOutcome::Yielded,
        (false, OutputOutcome::Success) => codex_tools::ToolOutputOutcome::Success,
        (false, OutputOutcome::Failure) => codex_tools::ToolOutputOutcome::Failure,
        (false, OutputOutcome::TimedOut) => codex_tools::ToolOutputOutcome::TimedOut,
        (false, OutputOutcome::Skipped) => codex_tools::ToolOutputOutcome::Skipped,
    };
    let sampling_request_signal = if success {
        crate::tools::context::semantic_evidence_sampling_signal(semantic_evidence)
    } else {
        crate::tools::context::semantic_failure_sampling_signal(semantic_evidence)
    };
    FunctionToolOutput::from_content(content_items, Some(success))
        .with_canonical_body(canonical_content_items)
        .with_outcome(typed_outcome)
        .with_sampling_request_signal(sampling_request_signal)
        .with_deterministic_continuation_owner_key(continuation_owner_key)
}

fn format_script_status(response: &RuntimeResponse) -> String {
    match response {
        RuntimeResponse::Yielded { cell_id, .. } => {
            format!("Script running with cell ID {cell_id}")
        }
        RuntimeResponse::Terminated { .. } => "Script terminated".to_string(),
        RuntimeResponse::Result { error_text, .. } => {
            if error_text.is_none() {
                "Script completed".to_string()
            } else {
                "Script failed".to_string()
            }
        }
    }
}

fn prepend_script_status(
    content_items: &mut Vec<FunctionCallOutputContentItem>,
    status: &str,
    wall_time: Duration,
) {
    let wall_time_seconds = ((wall_time.as_secs_f32()) * 10.0).round() / 10.0;
    let header = format!("{status}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n");
    content_items.insert(0, FunctionCallOutputContentItem::InputText { text: header });
}

fn truncate_code_mode_result(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
    outcome: OutputOutcome,
    hard_limit: usize,
) -> Vec<FunctionCallOutputContentItem> {
    let diagnostic_text = code_mode_text_content(&items);
    let requested_limit =
        max_output_tokens.unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
    let limits = resolve_output_limits(
        Some(requested_limit),
        outcome,
        None,
        &diagnostic_text,
        hard_limit,
    );
    let policy = TruncationPolicy::Tokens(limits.applied_limit);
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let (truncated_items, _) =
            formatted_truncate_text_content_items_with_policy(&items, policy);
        return truncated_items;
    }

    if outcome == OutputOutcome::Failure
        && let Some(error_index) = items.iter().position(|item| {
            matches!(
                item,
                FunctionCallOutputContentItem::InputText { text }
                    if text.starts_with("Script error:\n")
            )
        })
    {
        return truncate_mixed_code_mode_failure(items, error_index, limits.applied_limit);
    }

    truncate_function_output_items_with_policy(&items, policy)
}

fn truncate_mixed_code_mode_failure(
    mut items: Vec<FunctionCallOutputContentItem>,
    error_index: usize,
    token_limit: usize,
) -> Vec<FunctionCallOutputContentItem> {
    let FunctionCallOutputContentItem::InputText { text: error_text } = items.remove(error_index)
    else {
        unreachable!("the caller identifies a script-error text item")
    };
    let error_tokens = approx_token_count(&error_text);
    let reserved_error_tokens = error_tokens.min(token_limit);
    let other_policy = TruncationPolicy::Tokens(token_limit.saturating_sub(reserved_error_tokens));
    let mut projected = truncate_function_output_items_with_policy(&items, other_policy);
    let error_text = if error_tokens <= reserved_error_tokens {
        error_text
    } else {
        truncate_text(&error_text, TruncationPolicy::Tokens(reserved_error_tokens))
    };
    if !error_text.is_empty() {
        projected.push(FunctionCallOutputContentItem::InputText { text: error_text });
    }
    projected
}

fn code_mode_text_content(items: &[FunctionCallOutputContentItem]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
            FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call_nested_tool(
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    invocation: CodeModeNestedToolCall,
    cancellation_token: CancellationToken,
) -> Result<JsonValue, FunctionCallError> {
    let CodeModeNestedToolCall {
        cell_id,
        parent_tool_call_id,
        runtime_tool_call_id,
        tool_name,
        tool_kind,
        input,
    } = invocation;
    let packet_ordinal = exec
        .session
        .services
        .code_mode_service
        .begin_packet_call(&cell_id);
    if is_exec_tool_name(&tool_name) {
        let message = format!("{PUBLIC_TOOL_NAME} cannot invoke itself");
        exec.session
            .services
            .code_mode_service
            .complete_packet_call(
                &cell_id,
                packet_ordinal,
                false,
                0,
                Vec::new(),
                Some((RequiredToolTerminalCause::Failure, message.clone())),
            );
        return Err(FunctionCallError::RespondToModel(message));
    }

    let payload = match build_nested_tool_payload(tool_kind, &tool_name, input) {
        Ok(payload) => payload,
        Err(error) => {
            tool_runtime.record_code_mode_failure(
                cell_id.as_str(),
                &tool_name,
                None,
                nested_failure_fingerprint(&tool_name, &error),
            );
            exec.session
                .services
                .code_mode_service
                .complete_packet_call(
                    &cell_id,
                    packet_ordinal,
                    false,
                    0,
                    Vec::new(),
                    Some((RequiredToolTerminalCause::Failure, error.clone())),
                );
            return Err(FunctionCallError::RespondToModel(error));
        }
    };

    let call = ToolCall {
        tool_name: tool_name.clone(),
        call_id: format!("{PUBLIC_TOOL_NAME}-{}", uuid::Uuid::new_v4()),
        payload: payload.clone(),
    };
    let result = tool_runtime
        .clone()
        .handle_tool_call_with_source(
            call,
            ToolCallSource::CodeMode {
                cell_id: cell_id.to_string(),
                parent_call_id: parent_tool_call_id.clone(),
                runtime_tool_call_id,
            },
            cancellation_token,
        )
        .await;
    let mut result = match result {
        Ok(result) => result,
        Err(error) => {
            let message = error.to_string();
            let terminal_cause = required_tool_error_terminal_cause(&error);
            tool_runtime.record_code_mode_failure(
                cell_id.as_str(),
                &tool_name,
                Some(&payload),
                nested_failure_fingerprint(&tool_name, &message),
            );
            exec.session
                .services
                .code_mode_service
                .complete_packet_call(
                    &cell_id,
                    packet_ordinal,
                    false,
                    0,
                    Vec::new(),
                    terminal_cause.map(|cause| (cause, message)),
                );
            return Err(error);
        }
    };
    let outcome_context = result.outcome_context();
    let signal = result.sampling_request_signal();
    let canonical_artifact_required = result.requires_canonical_artifact();
    let receipts = result.intrinsic_deterministic_continuation_receipts();
    let source_dependencies = result.projected_source_dependencies().cloned();
    if let Some(continuation) = result.owner_drained_continuation() {
        exec.turn
            .turn_timing_state
            .record_owner_drained_continuation();
        exec.session
            .services
            .code_mode_service
            .record_owner_drained_continuation(&cell_id, continuation);
    }
    let post_tool_use_feedback = result.take_code_mode_feedback();
    let result_value = result.code_mode_result();
    let required_terminal =
        required_nested_tool_terminal_cause(outcome_context, signal.as_ref()).map(|cause| {
            let label = match cause {
                RequiredToolTerminalCause::Blocked => "blocked",
                RequiredToolTerminalCause::Failure => "failed",
                RequiredToolTerminalCause::TimedOut => "timed out",
                RequiredToolTerminalCause::RecoverableCancellation => "was cancelled",
            };
            (
                cause,
                format!("required nested tool `{}` {label}", tool_name.name),
            )
        });
    exec.session
        .services
        .code_mode_service
        .complete_packet_call(
            &cell_id,
            packet_ordinal,
            is_batchable_observation(&tool_name, &payload)
                && !result_has_live_exec_session(&result_value),
            serialized_json_len(&result_value),
            post_tool_use_feedback,
            required_terminal,
        );
    tool_runtime.record_code_mode_result(
        CodeModeToolResult {
            cell_id: cell_id.as_str(),
            tool_name: &tool_name,
            payload: &payload,
            source_dependencies,
            outcome_context,
            signal: signal.as_ref(),
            result: &result_value,
            canonical_artifact_required,
        },
        &receipts,
    );
    Ok(result_value)
}

fn is_batchable_observation(tool_name: &ToolName, payload: &ToolPayload) -> bool {
    if tool_name.namespace.is_none()
        && matches!(tool_name.name.as_str(), "read_tool_output" | "tool_search")
    {
        return true;
    }
    if tool_name.namespace.is_some()
        || !matches!(
            tool_name.name.as_str(),
            "exec_command" | "shell_command" | "unified_exec"
        )
    {
        return false;
    }
    let ToolPayload::Function { arguments } = payload else {
        return false;
    };
    let Ok(arguments) = serde_json::from_str::<JsonValue>(arguments) else {
        return false;
    };
    let command = arguments
        .get("program")
        .and_then(JsonValue::as_str)
        .map(|program| {
            let mut command = vec![program.to_string()];
            command.extend(
                arguments
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_string),
            );
            command
        })
        .or_else(|| {
            ["script_body", "cmd", "command"]
                .into_iter()
                .find_map(|field| match arguments.get(field) {
                    Some(JsonValue::String(command)) => Some(vec![command.clone()]),
                    Some(JsonValue::Array(command)) => command
                        .iter()
                        .map(|part| part.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>(),
                    _ => None,
                })
        });
    command.is_some_and(|command| !crate::turn_diff_tracker::command_may_mutate(&command))
}

fn result_has_live_exec_session(result: &JsonValue) -> bool {
    [
        (
            result.get("session_id"),
            result.get("process_exited"),
            result.get("exit_code"),
        ),
        (
            result.pointer("/result/essential/session_id"),
            result.pointer("/result/essential/process_exited"),
            result.pointer("/result/essential/exit_code"),
        ),
    ]
    .into_iter()
    .any(|(session_id, process_exited, exit_code)| {
        session_id.is_some_and(|session_id| !session_id.is_null())
            && !process_exited.and_then(JsonValue::as_bool).unwrap_or(false)
            && exit_code.is_none_or(JsonValue::is_null)
    })
}

fn nested_failure_fingerprint(tool_name: &ToolName, error: &str) -> String {
    if let Some(json_start) = error.find('{')
        && let Ok(value) = serde_json::from_str::<JsonValue>(&error[json_start..])
        && let Some(fingerprint) = value
            .get("fingerprint")
            .or_else(|| {
                value
                    .get("failure")
                    .and_then(|failure| failure.get("fingerprint"))
            })
            .and_then(JsonValue::as_str)
            .filter(|fingerprint| !fingerprint.is_empty())
    {
        return fingerprint.to_string();
    }
    let normalized = error
        .split_whitespace()
        .map(|part| {
            if part.chars().all(|character| character.is_ascii_digit()) {
                "#"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "code_mode.nested_tool.{:x}",
        Sha256::digest(format!("{tool_name}\0{normalized}").as_bytes())
    )
}

fn build_nested_tool_payload(
    tool_kind: CodeModeToolKind,
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match tool_kind {
        CodeModeToolKind::Function
            if tool_name.namespace.is_none()
                && tool_name.name == codex_tools::TOOL_SEARCH_TOOL_NAME =>
        {
            build_tool_search_payload(tool_name, input)
        }
        CodeModeToolKind::Function => build_function_tool_payload(tool_name, input),
        CodeModeToolKind::Freeform => build_freeform_tool_payload(tool_name, input),
    }
}

fn build_tool_search_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    let arguments = serialize_function_tool_arguments(tool_name, input)?;
    let arguments = serde_json::from_str(&arguments)
        .map_err(|err| format!("failed to parse tool `{tool_name}` arguments: {err}"))?;
    Ok(ToolPayload::ToolSearch { arguments })
}

fn build_function_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    let arguments = serialize_function_tool_arguments(tool_name, input)?;
    Ok(ToolPayload::Function { arguments })
}

fn serialize_function_tool_arguments(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<String, String> {
    match input {
        None => Ok("{}".to_string()),
        Some(JsonValue::Object(map)) => serde_json::to_string(&JsonValue::Object(map))
            .map_err(|err| format!("failed to serialize tool `{tool_name}` arguments: {err}")),
        Some(_) => Err(format!(
            "tool `{tool_name}` expects a JSON object for arguments"
        )),
    }
}

fn build_freeform_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match input {
        Some(JsonValue::String(input)) => Ok(ToolPayload::Custom { input }),
        _ => Err(format!("tool `{tool_name}` expects a string input")),
    }
}

#[cfg(test)]
#[path = "response_tests.rs"]
mod response_tests;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CodeModeService;
    use super::OutputOutcome;
    use super::build_nested_tool_payload;
    use super::fold_nested_required_terminal;
    use super::format_runtime_response;
    use super::is_batchable_observation;
    use super::nested_failure_fingerprint;
    use super::required_nested_tool_terminal_cause;
    use super::result_has_live_exec_session;
    use super::serialized_json_len;
    use super::truncate_code_mode_result;
    use crate::tools::context::FunctionToolOutput;
    use crate::tools::context::RequiredToolTerminalCause;
    use crate::tools::context::ToolPayload;
    use codex_code_mode::CellId;
    use codex_code_mode::CodeModeToolKind;
    use codex_code_mode::ExecuteRequest;
    use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
    use codex_code_mode::RuntimeResponse;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::SearchToolCallParams;
    use codex_tools::ToolName;
    use codex_tools::ToolOutput;
    use codex_tools::ToolOutputOutcome;
    use codex_tools::ToolOutputOutcomeContext;
    use codex_tools::ToolOutputSkipDisposition;
    use serde_json::json;

    fn test_service() -> CodeModeService {
        CodeModeService::new(Arc::new(ProcessOwnedCodeModeSessionProvider::default()))
    }

    #[test]
    fn packet_result_byte_count_matches_compact_json_without_allocating_a_buffer() {
        let value = json!({
            "text": "multi-byte: é",
            "nested": [true, null, {"count": 17}],
        });

        assert_eq!(
            serialized_json_len(&value),
            serde_json::to_vec(&value)
                .expect("serialize expected packet result")
                .len()
        );
    }

    #[test]
    fn first_tiny_read_only_packet_produces_one_runtime_advisory() {
        let service = test_service();
        let cell = CellId::new("cell".to_string());

        service.record_packet_call(&cell, true, 128, Vec::new());
        let advisory = service
            .finish_packet("cell", "turn")
            .advisory
            .expect("first tiny packet should include an advisory");
        assert!(advisory.contains("batch every known independent read in one exec"));
        assert!(advisory.contains("await notify(...) per settlement"));
        assert!(advisory.contains("Promise.allSettled"));
        assert!(advisory.contains("only to keep the cell alive"));
        assert!(advisory.contains("Resume only a documented live handle"));
        assert!(advisory.contains("evidence is sufficient or unchanged"));
        assert!(advisory.contains("synthesize and stop"));
        assert!(advisory.contains("instead of sampling or polling again"));
        service.record_packet_call(&cell, true, 128, Vec::new());
        assert!(service.finish_packet("cell", "turn").advisory.is_none());
        service.record_packet_call(&cell, true, 128, Vec::new());
        assert!(service.finish_packet("cell", "turn").advisory.is_none());

        for _ in 0..6 {
            service.record_packet_call(&cell, true, 128, Vec::new());
        }
        let batched = service.finish_packet("cell", "turn");
        assert_eq!(batched.nested_call_count, 6);
        assert!(batched.advisory.is_none());
    }

    #[test]
    fn turn_completion_releases_tiny_packet_admission_state() {
        let service = test_service();
        let cell = CellId::new("cell".to_string());
        service.record_packet_call(&cell, true, 128, Vec::new());
        service.finish_packet("cell", "finished-turn");
        assert!(
            service
                .packet_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .consecutive_tiny_packets_by_turn
                .contains_key("finished-turn")
        );

        service.finish_turn("finished-turn");

        assert!(
            !service
                .packet_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .consecutive_tiny_packets_by_turn
                .contains_key("finished-turn")
        );
    }

    #[test]
    fn post_tool_feedback_is_drained_once_with_code_mode_packet() {
        let service = test_service();
        let cell = CellId::new("feedback-cell".to_string());
        let feedback = vec![FunctionCallOutputContentItem::InputText {
            text: "hook feedback".to_string(),
        }];

        service.record_packet_call(&cell, false, 32, feedback.clone());
        assert_eq!(
            service
                .finish_packet("feedback-cell", "turn")
                .post_tool_use_feedback,
            feedback
        );
        assert!(
            service
                .finish_packet("feedback-cell", "turn")
                .post_tool_use_feedback
                .is_empty()
        );
    }

    #[test]
    fn nested_terminal_fold_uses_registration_order_and_preserves_blocked_status() {
        let service = test_service();
        let cell = CellId::new("terminal-cell".to_string());
        let first = service.begin_packet_call(&cell);
        let second = service.begin_packet_call(&cell);

        service.complete_packet_call(
            &cell,
            second,
            false,
            0,
            Vec::new(),
            Some((
                RequiredToolTerminalCause::Failure,
                "second nested failure".to_string(),
            )),
        );
        service.complete_packet_call(
            &cell,
            first,
            false,
            0,
            Vec::new(),
            Some((
                RequiredToolTerminalCause::Blocked,
                "first nested block".to_string(),
            )),
        );

        let terminal = service
            .finish_packet("terminal-cell", "turn")
            .first_required_terminal
            .expect("the first registered terminal nested call must be retained");
        assert_eq!(terminal.ordinal, first);
        assert_eq!(terminal.cause, RequiredToolTerminalCause::Blocked);

        let output = fold_nested_required_terminal(
            FunctionToolOutput::from_text("script completed".to_string(), Some(true)),
            terminal,
        );
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Skipped);
        assert_eq!(
            output.skip_disposition,
            Some(ToolOutputSkipDisposition::BlockingRequiredOperation)
        );
        assert!(output.into_text().contains("first nested block"));
    }

    #[test]
    fn nested_terminal_classification_promotes_failed_tool_results() {
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                None,
            ),
            Some(RequiredToolTerminalCause::Failure),
        );
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                Some(&json!({ "outcome": "blocked" })),
            ),
            Some(RequiredToolTerminalCause::Blocked),
        );
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                None,
            ),
            None,
        );
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
                None,
            ),
            None,
        );
    }

    #[test]
    fn packet_admission_only_classifies_known_read_only_argv_calls() {
        let rg = ToolPayload::Function {
            arguments: json!({"kind": "argv", "program": "rg", "args": ["needle"]}).to_string(),
        };
        let git_status = ToolPayload::Function {
            arguments: json!({"kind": "argv", "program": "git", "args": ["status", "--short"]})
                .to_string(),
        };
        let git_commit = ToolPayload::Function {
            arguments: json!({"kind": "argv", "program": "git", "args": ["commit"]}).to_string(),
        };
        let read_only_shell_script = ToolPayload::Function {
            arguments: json!({"kind": "script", "cmd": "rg needle"}).to_string(),
        };
        let mutating_shell_script = ToolPayload::Function {
            arguments: json!({"command": "Remove-Item output.txt"}).to_string(),
        };

        assert!(is_batchable_observation(
            &ToolName::plain("exec_command"),
            &rg
        ));
        assert!(is_batchable_observation(
            &ToolName::plain("exec_command"),
            &git_status
        ));
        assert!(!is_batchable_observation(
            &ToolName::plain("exec_command"),
            &git_commit
        ));
        assert!(!is_batchable_observation(
            &ToolName::plain("exec_command"),
            &read_only_shell_script
        ));
        assert!(!is_batchable_observation(
            &ToolName::plain("shell_command"),
            &mutating_shell_script
        ));
    }

    #[test]
    fn tool_result_correctness_projected_live_exec_session_is_detected() {
        assert!(result_has_live_exec_session(&json!({"session_id": 7})));
        assert!(result_has_live_exec_session(&json!({
            "version": 1,
            "result": {
                "essential": {
                    "session_id": 7,
                },
                "selected_text": "",
            },
        })));
        assert!(!result_has_live_exec_session(&json!({
            "result": {
                "essential": {
                    "session_id": null,
                },
            },
        })));
        assert!(!result_has_live_exec_session(&json!({"exit_code": 0})));
        assert!(!result_has_live_exec_session(&json!({
            "session_id": 7,
            "exit_code": 7,
            "process_exited": true,
        })));
        assert!(!result_has_live_exec_session(&json!({
            "version": 1,
            "result": {
                "essential": {
                    "session_id": 7,
                    "exit_code": null,
                    "process_exited": true,
                },
            },
        })));
    }

    #[test]
    fn build_nested_tool_payload_uses_function_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain("example"),
            Some(json!({ "value": 1 })),
        )
        .expect("function payload should serialize");

        match payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, r#"{"value":1}"#.to_string());
            }
            other => panic!("expected function payload, got {other:?}"),
        }
    }

    #[test]
    fn build_nested_tool_payload_uses_tool_search_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain(codex_tools::TOOL_SEARCH_TOOL_NAME),
            Some(json!({ "query": "repo atlas", "limit": 8 })),
        )
        .expect("tool search payload should parse");

        assert_eq!(
            payload,
            ToolPayload::ToolSearch {
                arguments: SearchToolCallParams {
                    query: "repo atlas".to_string(),
                    limit: Some(8),
                },
            }
        );
    }

    #[test]
    fn build_nested_tool_payload_uses_freeform_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Freeform,
            &ToolName::plain("example"),
            Some(json!("hello")),
        )
        .expect("freeform payload should preserve string input");

        match payload {
            ToolPayload::Custom { input } => {
                assert_eq!(input, "hello".to_string());
            }
            other => panic!("expected freeform payload, got {other:?}"),
        }
    }

    #[test]
    fn nested_failure_fingerprint_normalizes_numeric_noise() {
        let tool_name = ToolName::plain("example");
        assert_eq!(
            nested_failure_fingerprint(
                &tool_name,
                r#"tool failed: {"fingerprint":"owner.stable.failure"}"#,
            ),
            "owner.stable.failure"
        );
        assert_eq!(
            nested_failure_fingerprint(&tool_name, "request 17 failed after 2 attempts"),
            nested_failure_fingerprint(&tool_name, "request 91 failed after 8 attempts")
        );
        assert_ne!(
            nested_failure_fingerprint(&tool_name, "request 17 failed"),
            nested_failure_fingerprint(&ToolName::plain("other"), "request 17 failed")
        );
    }

    #[test]
    fn truncated_text_output_starts_with_warning() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "0123456789012345678901234567890123456789".to_string(),
        }];

        let truncated_items =
            truncate_code_mode_result(items, Some(5), OutputOutcome::Success, usize::MAX);
        assert_eq!(
            truncated_items,
            vec![FunctionCallOutputContentItem::InputText {
                text: concat!(
                    "Warning: truncated output (original token count: 10)\n",
                    "Total output lines: 1\n\n",
                    "…10 tokens truncated…"
                )
                .to_string(),
            }]
        );
    }

    #[test]
    fn code_mode_truncation_preserves_full_canonical_recovery() {
        let sentinel = "CANONICAL_SENTINEL_AFTER_THE_MODEL_LIMIT";
        let output = format_runtime_response(
            RuntimeResponse::Result {
                cell_id: CellId::new("canonical-cell".to_string()),
                content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText {
                    text: format!("{}{}", "x".repeat(400), sentinel),
                }],
                error_text: None,
            },
            Some(5),
            5,
            false,
            std::time::Instant::now(),
            Vec::new(),
        );

        let projected =
            codex_protocol::models::function_call_output_content_items_to_text(&output.body)
                .expect("projected code-mode text");
        assert!(projected.contains("Warning: truncated output"));
        assert!(!projected.contains(sentinel));

        let canonical = output
            .canonical_result(&ToolPayload::Custom {
                input: "return a large result".to_string(),
            })
            .expect("canonical code-mode result");
        assert!(String::from_utf8_lossy(&canonical.bytes).contains(sentinel));
        assert!(
            output
                .projection_metadata()
                .expect("projection metadata")
                .spillable_text
                .iter()
                .any(|text| text.contains(sentinel)),
            "artifact admission must receive the complete provider output"
        );
    }

    #[test]
    fn code_mode_truncation_applies_hard_limit() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "x".repeat(400),
        }];

        let truncated = truncate_code_mode_result(items, Some(20), OutputOutcome::Success, 5);
        let [FunctionCallOutputContentItem::InputText { text }] = truncated.as_slice() else {
            panic!("expected one truncated text item");
        };
        assert!(text.starts_with("Warning: truncated output"));
        assert!(text.contains("tokens truncated"));
        assert!(!text.contains(&"x".repeat(100)));
    }

    #[test]
    fn over_truncation_mixed_code_mode_failure_preserves_the_script_error() {
        let items = vec![
            FunctionCallOutputContentItem::InputText {
                text: "ordinary output ".repeat(1_000),
            },
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "opaque".to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: "Script error:\nROOT_CAUSE_SENTINEL".to_string(),
            },
        ];

        let projected =
            truncate_code_mode_result(items, Some(40), OutputOutcome::Failure, usize::MAX);

        assert!(projected.iter().any(|item| matches!(
            item,
            FunctionCallOutputContentItem::InputText { text }
                if text == "Script error:\nROOT_CAUSE_SENTINEL"
        )));
    }

    #[test]
    fn default_outer_success_budget_preserves_output_above_the_old_cap() {
        let text = "x".repeat(24_000);
        let original_tokens = codex_utils_string::approx_token_count(&text);
        assert!(original_tokens > 4_000);
        assert!(original_tokens < codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
        let items = vec![FunctionCallOutputContentItem::InputText { text: text.clone() }];

        let projected = truncate_code_mode_result(items, None, OutputOutcome::Success, usize::MAX);

        let [
            FunctionCallOutputContentItem::InputText {
                text: projected_text,
            },
        ] = projected.as_slice()
        else {
            panic!("expected one projected text item");
        };
        assert_eq!(projected_text, &text);

        let oversized = vec![FunctionCallOutputContentItem::InputText {
            text: "x".repeat(48_000),
        }];
        let capped = truncate_code_mode_result(oversized, None, OutputOutcome::Success, usize::MAX);
        let [FunctionCallOutputContentItem::InputText { text: capped_text }] = capped.as_slice()
        else {
            panic!("expected one capped text item");
        };
        assert!(capped_text.starts_with("Warning: truncated output"));
        assert!(
            codex_utils_string::approx_token_count(capped_text)
                <= codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL
        );
    }

    #[tokio::test]
    async fn missing_process_host_is_reported_without_failing_service_creation() {
        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let error = service
            .execute(ExecuteRequest {
                tool_call_id: "call-1".to_string(),
                enabled_tools: Vec::new(),
                source: "text('unreachable')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            })
            .await
            .err()
            .expect("missing host should reject execution");

        assert!(error.contains("failed to spawn code-mode host"));
        service.shutdown().await.expect("shutdown unused service");
    }
}
