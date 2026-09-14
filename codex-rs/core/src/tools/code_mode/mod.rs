mod delegate;
mod execute_handler;
pub(crate) mod execute_spec;
mod response_adapter;
mod wait_handler;
pub(crate) mod wait_spec;

use std::collections::HashMap;
use std::collections::HashSet;
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
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use serde::Serialize;
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
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use codex_utils_string::approx_token_count;

use delegate::CodeModeDispatchBroker;
use delegate::CodeModeDispatchWorker;
pub(crate) use execute_handler::CodeModeExecuteHandler;
use response_adapter::into_function_call_output_content_items;
pub(crate) use wait_handler::CodeModeWaitHandler;

pub(crate) const PUBLIC_TOOL_NAME: &str = codex_code_mode::PUBLIC_TOOL_NAME;
pub(crate) const WAIT_TOOL_NAME: &str = codex_code_mode::WAIT_TOOL_NAME;
const FAILED_CELL_ITEM_NAMESPACE: &str = "codex.internal";
const FAILED_CELL_ITEM_TOOL: &str = "code_mode_cell";

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
    cell_parent_call_ids: Mutex<HashMap<String, String>>,
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct CodeModePacketAdmission {
    cells: HashMap<String, CodeModePacketMetrics>,
    advised_turns: HashSet<String>,
}

#[derive(Default)]
struct CodeModePacketMetrics {
    next_nested_ordinal: usize,
    nested_call_count: usize,
    batchable_observation_count: usize,
    result_bytes: usize,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    first_required_terminal: Option<CodeModeNestedTerminal>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodeModeNestedTerminal {
    ordinal: usize,
    cause: RequiredToolTerminalCause,
    message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CodeModeNestedResultEvidence {
    #[serde(skip)]
    ordinal: usize,
    call_id: String,
    parent_call_id: Option<String>,
    parent_cell_id: String,
    runtime_tool_call_id: String,
    tool_name: String,
    output: String,
    output_truncated: bool,
}

struct CodeModePacketReceipt {
    nested_call_count: usize,
    batchable_observation_count: usize,
    result_bytes: usize,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    first_required_terminal: Option<CodeModeNestedTerminal>,
    advisory: Option<&'static str>,
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    total_bytes: usize,
    limit: usize,
}

impl BoundedJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit),
            total_bytes: 0,
            limit,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "Bytes are validated and truncated to the valid UTF-8 prefix immediately before conversion"
    )]
    fn finish(self) -> (String, bool, usize) {
        let mut bytes = self.bytes;
        let mut truncated = self.total_bytes > bytes.len();
        if let Err(error) = std::str::from_utf8(&bytes) {
            bytes.truncate(error.valid_up_to());
            truncated = true;
        }
        (
            String::from_utf8(bytes).expect("validated UTF-8 prefix"),
            truncated,
            self.total_bytes,
        )
    }
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.total_bytes = self.total_bytes.saturating_add(buffer.len());
        let remaining = self.limit.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&buffer[..buffer.len().min(remaining)]);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_serialized_json(value: &JsonValue) -> (String, bool, usize) {
    let mut writer = BoundedJsonWriter::new(MAX_RETAINED_NESTED_RESULT_BYTES);
    if serde_json::to_writer(&mut writer, value).is_err() {
        return (
            "<nested result could not be serialized>".to_string(),
            false,
            0,
        );
    }
    writer.finish()
}

const TINY_PACKET_RESULT_BYTES: usize = 1_024;
const MAX_RETAINED_NESTED_RESULTS: usize = 8;
const MAX_RETAINED_NESTED_RESULT_BYTES: usize = 4_096;
const MAX_FAILED_CELL_ERROR_BYTES: usize = 4_096;
const FAILED_CELL_ERROR_TRUNCATION_MARKER: &str = "\n… [truncated]";
const APPLY_PATCH_ENVELOPE_MARKER: &str = "*** Begin Patch";
const TINY_PACKET_ADVISORY: &str = "Low-density packet: when more evidence is needed, batch only necessary independent reads with known inputs in one exec using Promise.allSettled; print needed evidence with text(...). Reuse unchanged evidence. Stop when the task is answered.";

impl CodeModeService {
    pub(crate) fn new(session_provider: Arc<dyn CodeModeSessionProvider>) -> Self {
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::new());
        Self {
            session: OnceCell::new(),
            session_provider,
            dispatch_broker,
            packet_admission: Mutex::new(CodeModePacketAdmission::default()),
            cell_parent_call_ids: Mutex::new(HashMap::new()),
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

    /// Starts the code-mode host before the first turn needs it so the first
    /// cell does not pay for isolate and host startup.
    pub(crate) async fn prewarm(&self) -> Result<(), String> {
        self.session().await.map(|_| ())
    }

    #[cfg(test)]
    fn is_initialized(&self) -> bool {
        self.session.initialized()
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

    pub(crate) fn record_cell_parent_call_id(&self, cell_id: &CellId, call_id: &str) {
        self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .entry(cell_id.to_string())
            .or_default();
        self.cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(cell_id.to_string(), call_id.to_string());
    }

    pub(crate) fn cell_parent_call_id(&self, cell_id: &CellId) -> Option<String> {
        self.cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id.as_str())
            .cloned()
    }

    pub(crate) fn finish_cell_dispatch(&self, cell_id: &CellId) {
        self.dispatch_broker.close_cell(cell_id);
        // Only registration creates packet state. Accepted child operations
        // keep their own cleanup owner, but cannot recreate a closed packet.
        self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .remove(cell_id.as_str());
        self.cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(cell_id.as_str());
    }

    fn begin_packet_call(&self, cell_id: &CellId) -> Option<usize> {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = admission.cells.get_mut(cell_id.as_str())?;
        let ordinal = metrics.next_nested_ordinal;
        metrics.next_nested_ordinal = metrics.next_nested_ordinal.saturating_add(1);
        metrics.nested_call_count = metrics.nested_call_count.saturating_add(1);
        Some(ordinal)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep the per-call accounting and terminal evidence explicit"
    )]
    fn complete_packet_call(
        &self,
        cell_id: &CellId,
        ordinal: usize,
        batchable_observation: bool,
        result_bytes: usize,
        post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
        nested_result: Option<CodeModeNestedResultEvidence>,
        required_terminal: Option<(RequiredToolTerminalCause, String)>,
    ) {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(metrics) = admission.cells.get_mut(cell_id.as_str()) else {
            return;
        };
        metrics.batchable_observation_count = metrics
            .batchable_observation_count
            .saturating_add(usize::from(batchable_observation));
        metrics.result_bytes = metrics.result_bytes.saturating_add(result_bytes);
        metrics
            .post_tool_use_feedback
            .extend(post_tool_use_feedback);
        if let Some(nested_result) = nested_result {
            if metrics.nested_results.len() < MAX_RETAINED_NESTED_RESULTS {
                metrics.nested_results.push(nested_result);
            } else if let Some((latest_index, latest)) = metrics
                .nested_results
                .iter()
                .enumerate()
                .max_by_key(|(_, result)| result.ordinal)
                && nested_result.ordinal < latest.ordinal
            {
                metrics.nested_results[latest_index] = nested_result;
            }
        }
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
        self.record_cell_parent_call_id(cell_id, "test-exec");
        let ordinal = self.begin_packet_call(cell_id).expect("registered cell");
        self.complete_packet_call(
            cell_id,
            ordinal,
            batchable_observation,
            result_bytes,
            post_tool_use_feedback,
            None,
            None,
        );
    }

    fn finish_packet(&self, cell_id: &str, turn_id: &str) -> CodeModePacketReceipt {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut metrics = admission
            .cells
            .get_mut(cell_id)
            .map(|metrics| {
                let next_nested_ordinal = metrics.next_nested_ordinal;
                let packet = std::mem::take(metrics);
                // Live output and steering can cross outstanding child calls.
                // Drain response data, but keep registration order for the cell.
                metrics.next_nested_ordinal = next_nested_ordinal;
                packet
            })
            .unwrap_or_default();
        metrics
            .nested_results
            .sort_unstable_by_key(|result| result.ordinal);
        let tiny_read_only_packet = metrics.nested_call_count == 1
            && metrics.batchable_observation_count == 1
            && metrics.result_bytes <= TINY_PACKET_RESULT_BYTES;
        // The guidance is unchanged across packets. Keep it once in the turn's
        // history even when a larger packet separates two small reads.
        let advisory = (tiny_read_only_packet
            && admission.advised_turns.insert(turn_id.to_string()))
        .then_some(TINY_PACKET_ADVISORY);
        CodeModePacketReceipt {
            nested_call_count: metrics.nested_call_count,
            batchable_observation_count: metrics.batchable_observation_count,
            result_bytes: metrics.result_bytes,
            post_tool_use_feedback: metrics.post_tool_use_feedback,
            nested_results: metrics.nested_results,
            first_required_terminal: metrics.first_required_terminal,
            advisory,
        }
    }

    pub(crate) fn finish_turn(&self, turn_id: &str) {
        self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advised_turns
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
        retained_nested_result_count = packet.nested_results.len(),
        low_density_advisory = packet.advisory.is_some(),
        "code-mode packet admission receipt"
    );
    let nested_results = if response_needs_retained_nested_results(&response) {
        packet.nested_results
    } else {
        Vec::new()
    };
    let mut output = format_runtime_response(
        response,
        max_output_tokens,
        hard_limit,
        original_image_detail_supported,
        started_at,
        packet.post_tool_use_feedback,
        nested_results,
        packet.first_required_terminal,
    );
    if let Some(advisory) = packet.advisory {
        output.body.push(FunctionCallOutputContentItem::InputText {
            text: advisory.to_string(),
        });
    }
    Ok(output)
}

fn failed_code_mode_cell_item(
    call_id: &str,
    response: &RuntimeResponse,
    duration: Duration,
) -> Option<DynamicToolCallItem> {
    let (cell_id, error) = match response {
        RuntimeResponse::Result {
            cell_id,
            error_text: Some(error),
            ..
        } => (cell_id.as_str(), bounded_failed_cell_error(error)),
        RuntimeResponse::Terminated { cell_id, .. } => (
            cell_id.as_str(),
            "code-mode cell terminated before completion".to_string(),
        ),
        RuntimeResponse::Yielded { .. }
        | RuntimeResponse::ExplicitYield { .. }
        | RuntimeResponse::Result {
            error_text: None, ..
        } => return None,
    };

    Some(DynamicToolCallItem {
        id: format!("code-mode-cell:{cell_id}"),
        namespace: Some(FAILED_CELL_ITEM_NAMESPACE.to_string()),
        tool: FAILED_CELL_ITEM_TOOL.to_string(),
        arguments: serde_json::json!({
            "call_id": call_id,
            "cell_id": cell_id,
        }),
        status: DynamicToolCallStatus::Failed,
        content_items: None,
        success: Some(false),
        error: Some(error),
        duration: Some(duration),
    })
}

fn bounded_failed_cell_error(error: &str) -> String {
    if error.len() <= MAX_FAILED_CELL_ERROR_BYTES {
        return error.to_string();
    }

    let content_limit =
        MAX_FAILED_CELL_ERROR_BYTES.saturating_sub(FAILED_CELL_ERROR_TRUNCATION_MARKER.len());
    let mut end = content_limit.min(error.len());
    while end > 0 && !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &error[..end], FAILED_CELL_ERROR_TRUNCATION_MARKER)
}

pub(super) async fn emit_failed_code_mode_cell_item(
    exec: &ExecContext,
    call_id: &str,
    response: &RuntimeResponse,
    started_at: std::time::Instant,
) {
    let Some(item) = failed_code_mode_cell_item(call_id, response, started_at.elapsed()) else {
        return;
    };
    exec.session
        .emit_turn_item_completed(exec.turn.as_ref(), TurnItem::DynamicToolCall(item))
        .await;
}

fn merge_code_mode_signal(mut output: FunctionToolOutput, signal: JsonValue) -> FunctionToolOutput {
    if let (Some(existing), Some(additional)) = (
        output
            .sampling_request_signal
            .as_mut()
            .and_then(JsonValue::as_object_mut),
        signal.as_object(),
    ) {
        existing.extend(additional.clone());
    } else {
        output.sampling_request_signal = Some(signal);
    }
    output
}

fn fold_nested_required_terminal(
    output: FunctionToolOutput,
    terminal: CodeModeNestedTerminal,
) -> FunctionToolOutput {
    let (output, outcome) = match terminal.cause {
        RequiredToolTerminalCause::Blocked => (
            output.with_skip_disposition(ToolOutputSkipDisposition::BlockingRequiredOperation),
            "blocked",
        ),
        RequiredToolTerminalCause::Failure => {
            (output.with_outcome(ToolOutputOutcome::Failure), "failure")
        }
        RequiredToolTerminalCause::TimedOut => {
            (output.with_outcome(ToolOutputOutcome::TimedOut), "timeout")
        }
        RequiredToolTerminalCause::RecoverableCancellation => (
            output.with_outcome(ToolOutputOutcome::Failure),
            "recoverable_cancellation",
        ),
    };
    merge_code_mode_signal(
        output,
        serde_json::json!({
            "outcome": outcome,
            "nested_ordinal": terminal.ordinal,
        }),
    )
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
        | RuntimeResponse::ExplicitYield { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id.as_str(),
    }
}

fn response_needs_retained_nested_results(response: &RuntimeResponse) -> bool {
    match response {
        RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::ExplicitYield { content_items, .. }
        | RuntimeResponse::Terminated { content_items, .. } => content_items.is_empty(),
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => error_text.is_some() || content_items.is_empty(),
    }
}

fn nested_result_content_items(
    nested_results: Vec<CodeModeNestedResultEvidence>,
) -> Vec<FunctionCallOutputContentItem> {
    nested_results
        .into_iter()
        .map(|result| {
            let encoded = serde_json::to_string(&result)
                .unwrap_or_else(|_| "{\"output\":\"<unavailable>\"}".to_string());
            FunctionCallOutputContentItem::InputText {
                text: format!("Nested tool result:\n{encoded}"),
            }
        })
        .collect()
}

#[expect(
    clippy::too_many_arguments,
    reason = "Rendering consumes output budgets, timing, feedback, and terminal evidence together"
)]
fn format_runtime_response(
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    hard_limit: usize,
    original_image_detail_supported: bool,
    started_at: std::time::Instant,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    required_terminal: Option<CodeModeNestedTerminal>,
) -> FunctionToolOutput {
    let continuation_owner_key = match &response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::ExplicitYield { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id.to_string(),
    };
    let script_status = format_script_status(&response);
    let yielded = matches!(
        &response,
        RuntimeResponse::Yielded { .. } | RuntimeResponse::ExplicitYield { .. }
    );
    let (mut content_items, mut outcome, mut success, script_error) = match response {
        RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::ExplicitYield { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::Success, true, None)
        }
        RuntimeResponse::Terminated { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::Failure, false, None)
        }
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => {
            let content_items = into_function_call_output_content_items(content_items);
            let success = error_text.is_none();
            let outcome = if success {
                OutputOutcome::Success
            } else {
                OutputOutcome::Failure
            };
            (content_items, outcome, success, error_text)
        }
    };

    content_items.extend(nested_result_content_items(nested_results));
    content_items.extend(post_tool_use_feedback);
    let mut diagnostic = required_terminal.as_ref().map(|terminal| {
        success = false;
        outcome = match terminal.cause {
            RequiredToolTerminalCause::Blocked => OutputOutcome::Skipped,
            RequiredToolTerminalCause::TimedOut => OutputOutcome::TimedOut,
            RequiredToolTerminalCause::Failure
            | RequiredToolTerminalCause::RecoverableCancellation => OutputOutcome::Failure,
        };
        format!("Required nested tool outcome: {}", terminal.message)
    });
    if let Some(error_text) = script_error {
        let script_error = format!("Script error:\n{error_text}");
        match &mut diagnostic {
            Some(diagnostic) => {
                diagnostic.push('\n');
                diagnostic.push_str(&script_error);
            }
            None => diagnostic = Some(script_error),
        }
    }
    let diagnostic_index = diagnostic.map(|text| {
        let index = content_items.len();
        content_items.push(FunctionCallOutputContentItem::InputText { text });
        index
    });
    sanitize_image_detail_items(original_image_detail_supported, &mut content_items);
    let mut canonical_content_items = content_items.clone();
    let mut content_items = truncate_code_mode_result(
        content_items,
        max_output_tokens,
        outcome,
        hard_limit,
        diagnostic_index,
    );
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
    let output = FunctionToolOutput::from_content(content_items, Some(success))
        .with_canonical_body(canonical_content_items)
        .with_outcome(typed_outcome)
        .with_sampling_request_signal(sampling_request_signal)
        .with_deterministic_continuation_owner_key(continuation_owner_key);
    match required_terminal {
        Some(terminal) => fold_nested_required_terminal(output, terminal),
        None => output,
    }
}

fn format_script_status(response: &RuntimeResponse) -> String {
    match response {
        RuntimeResponse::Yielded { cell_id, .. } => {
            format!("Script running with cell ID {cell_id}")
        }
        RuntimeResponse::ExplicitYield { cell_id, .. } => {
            format!("Script running with cell ID {cell_id} after explicit yield")
        }
        RuntimeResponse::Terminated { cell_id, .. } => {
            format!("Script terminated with cell ID {cell_id}")
        }
        RuntimeResponse::Result {
            cell_id,
            error_text,
            ..
        } => {
            if error_text.is_none() {
                format!("Script completed with cell ID {cell_id}")
            } else {
                format!("Script failed with cell ID {cell_id}")
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
    diagnostic_index: Option<usize>,
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
    if let Some(error_index) = diagnostic_index {
        return truncate_code_mode_failure(items, error_index, limits.applied_limit);
    }
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let (truncated_items, _) =
            formatted_truncate_text_content_items_with_policy(&items, policy);
        return truncated_items;
    }

    truncate_function_output_items_with_policy(&items, policy)
}

fn truncate_code_mode_failure(
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
    let mut projected = if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        formatted_truncate_text_content_items_with_policy(&items, other_policy).0
    } else {
        truncate_function_output_items_with_policy(&items, other_policy)
    };
    let error_text = if error_tokens <= reserved_error_tokens {
        error_text
    } else {
        truncate_text_to_token_ceiling(&error_text, reserved_error_tokens)
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
        .begin_packet_call(&cell_id)
        .ok_or_else(|| FunctionCallError::RespondToModel("code mode cell is closed".to_string()))?;
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
                None,
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
                    None,
                    Some((RequiredToolTerminalCause::Failure, error.clone())),
                );
            return Err(FunctionCallError::RespondToModel(error));
        }
    };
    if let Some(message) = wrapped_patch_rejection(
        &tool_name,
        &payload,
        tool_runtime.has_registered_tool(&ToolName::plain("apply_patch")),
    ) {
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
                None,
                Some((RequiredToolTerminalCause::Failure, message.clone())),
            );
        return Err(FunctionCallError::RespondToModel(message));
    }

    let nested_call_id = format!(
        "{PUBLIC_TOOL_NAME}-{}-{runtime_tool_call_id}",
        cell_id.as_str()
    );
    let call = ToolCall {
        tool_name: tool_name.clone(),
        call_id: nested_call_id.clone(),
        payload: payload.clone(),
    };
    let result = tool_runtime
        .clone()
        .handle_tool_call_with_source(
            call,
            ToolCallSource::CodeMode {
                cell_id: cell_id.to_string(),
                parent_call_id: parent_tool_call_id.clone(),
                runtime_tool_call_id: runtime_tool_call_id.clone(),
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
                    None,
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
    let (retained_output, output_truncated, result_bytes) = bounded_serialized_json(&result_value);
    if !output_truncated && let Some(parent_call_id) = parent_tool_call_id.as_ref() {
        exec.session
            .register_code_mode_nested_evidence(
                parent_call_id.clone(),
                nested_call_id.clone(),
                retained_output.clone(),
            )
            .await;
    }
    let nested_result = CodeModeNestedResultEvidence {
        ordinal: packet_ordinal,
        call_id: nested_call_id,
        parent_call_id: parent_tool_call_id,
        parent_cell_id: cell_id.to_string(),
        runtime_tool_call_id,
        tool_name: tool_name.to_string(),
        output: retained_output,
        output_truncated,
    };
    let required_terminal = required_nested_tool_terminal_cause(outcome_context, signal.as_ref())
        .map(|cause| {
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
            result_bytes,
            post_tool_use_feedback,
            Some(nested_result),
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

fn nested_command_argv(tool_name: &ToolName, payload: &ToolPayload) -> Option<Vec<String>> {
    if tool_name.namespace.is_some()
        || !matches!(
            tool_name.name.as_str(),
            "exec_command" | "shell_command" | "unified_exec"
        )
    {
        return None;
    }
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    let arguments = serde_json::from_str::<JsonValue>(arguments).ok()?;
    arguments
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
        })
}

/// A patch envelope routed through a shell wrapper spawns a process, hides the
/// patch exit status behind the wrapper, and bypasses the native apply_patch
/// interception. When the typed tool is registered, fail fast with the exact
/// call form instead of running the wrapper.
fn wrapped_patch_rejection(
    tool_name: &ToolName,
    payload: &ToolPayload,
    apply_patch_available: bool,
) -> Option<String> {
    if !apply_patch_available {
        return None;
    }
    let command = nested_command_argv(tool_name, payload)?;
    if !command
        .iter()
        .any(|part| part.contains(APPLY_PATCH_ENVELOPE_MARKER))
        || is_native_apply_patch_invocation(&command)
        || !is_wrapped_apply_patch_invocation(&command)
    {
        return None;
    }
    Some(format!(
        "Patch envelopes must go through the apply_patch tool, not a shell wrapper: call `await tools.apply_patch(patch)` with the same `{APPLY_PATCH_ENVELOPE_MARKER}` body. The wrapped `{}` command was not run.",
        tool_name.name
    ))
}

fn is_wrapped_apply_patch_invocation(command: &[String]) -> bool {
    let script = match command {
        [script] => script.as_str(),
        _ => match codex_shell_command::parse_command::extract_shell_command(command) {
            Some((_, script)) => script,
            None => return false,
        },
    };
    // Recognize the complete PowerShell literal-pipeline shape. Marker-bearing
    // searches, printed examples, and ambiguous scripts use ordinary dispatch.
    let script = script.trim();
    for (opening, closing) in [("@'", "'@"), ("@\"", "\"@")] {
        if let Some(body) = script.strip_prefix(opening).and_then(|body| {
            body.strip_prefix("\r\n")
                .or_else(|| body.strip_prefix('\n'))
        }) && let Some((_, tail)) = body.split_once(&format!("\n{closing}"))
        {
            return matches!(
                tail.trim().strip_prefix('|').map(str::trim),
                Some("apply_patch" | "applypatch")
            );
        }
    }
    codex_shell_command::bash::try_parse_shell(script)
        .and_then(|tree| {
            codex_shell_command::bash::try_parse_word_only_commands_sequence(&tree, script)
        })
        .is_some_and(|commands| {
            commands.iter().any(|argv| {
                matches!(
                    argv.first().map(String::as_str),
                    Some("apply_patch" | "applypatch")
                )
            })
        })
}

/// Plain `apply_patch` heredoc forms are intercepted natively without a
/// process spawn; only wrapped or piped envelopes are rejected.
fn is_native_apply_patch_invocation(command: &[String]) -> bool {
    fn script_starts_with_apply_patch(script: &str) -> bool {
        let script = script.trim_start();
        script
            .strip_prefix("apply_patch")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    }
    fn program_stem(program: &str) -> String {
        if program.chars().any(char::is_whitespace) {
            return String::new();
        }
        std::path::Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(program)
            .to_ascii_lowercase()
    }
    match command {
        [script] => script_starts_with_apply_patch(script),
        [program, ..] if program_stem(program) == "apply_patch" => true,
        [shell, flag, script]
            if matches!(program_stem(shell).as_str(), "bash" | "sh" | "zsh")
                && matches!(flag.as_str(), "-lc" | "-c") =>
        {
            script_starts_with_apply_patch(script)
        }
        _ => false,
    }
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
    let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
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
    use super::MAX_RETAINED_NESTED_RESULT_BYTES;
    use super::OutputOutcome;
    use super::bounded_serialized_json;
    use super::build_nested_tool_payload;
    use super::fold_nested_required_terminal;
    use super::format_runtime_response;
    use super::is_batchable_observation;
    use super::nested_failure_fingerprint;
    use super::required_nested_tool_terminal_cause;
    use super::result_has_live_exec_session;
    use super::truncate_code_mode_result;
    use super::wrapped_patch_rejection;
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
    fn retained_nested_result_counts_full_json_bytes_including_utf8() {
        let value = json!({
            "text": "multi-byte: é",
            "nested": [true, null, {"count": 17}],
        });

        let (retained, truncated, bytes) = bounded_serialized_json(&value);
        let expected = r#"{"text":"multi-byte: é","nested":[true,null,{"count":17}]}"#;
        assert_eq!(bytes, expected.len());
        assert!(!truncated);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&retained).unwrap(),
            value
        );
    }

    #[test]
    fn retained_nested_result_serialization_is_bounded() {
        let value = json!({
            "a_prefix": "kept",
            "body": "x".repeat(MAX_RETAINED_NESTED_RESULT_BYTES * 2),
            "tail": "MUST_NOT_BE_RETAINED",
        });

        let (retained, truncated, bytes) = bounded_serialized_json(&value);
        let expected = format!(
            r#"{{"a_prefix":"kept","body":"{}","tail":"MUST_NOT_BE_RETAINED"}}"#,
            "x".repeat(MAX_RETAINED_NESTED_RESULT_BYTES * 2)
        );

        assert!(truncated);
        assert_eq!(retained, expected[..MAX_RETAINED_NESTED_RESULT_BYTES]);
        assert_eq!(bytes, expected.len());
        assert!(!retained.contains("MUST_NOT_BE_RETAINED"));
    }

    #[test]
    fn retained_nested_result_bound_never_splits_utf8() {
        let value = json!({
            "body": "🙂".repeat(MAX_RETAINED_NESTED_RESULT_BYTES),
        });

        let (retained, truncated, bytes) = bounded_serialized_json(&value);

        assert!(truncated);
        assert_eq!(retained, format!("{{\"body\":\"{}", "🙂".repeat(1_021)));
        assert_eq!(bytes, 16_395);
    }

    #[test]
    fn cell_parent_call_id_lives_until_dispatch_finishes() {
        let service = test_service();
        let cell = CellId::new("cell-parent".to_string());

        service.record_cell_parent_call_id(&cell, "outer-exec-call");
        assert_eq!(
            service.cell_parent_call_id(&cell).as_deref(),
            Some("outer-exec-call")
        );

        service.finish_cell_dispatch(&cell);
        assert_eq!(service.cell_parent_call_id(&cell), None);
    }

    #[tokio::test]
    async fn prewarm_initializes_the_code_mode_session_once() {
        let service =
            CodeModeService::new(Arc::new(codex_code_mode::InProcessCodeModeSessionProvider));
        assert!(!service.is_initialized());

        service.prewarm().await.expect("in-process host prewarm");
        assert!(service.is_initialized());
        let first = service.session().await.expect("prewarmed session");
        let second = service.session().await.expect("reused session");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn wrapped_patch_envelopes_are_rejected_only_when_apply_patch_is_registered() {
        let exec_command = ToolName::plain("exec_command");
        let envelope = "*** Begin Patch\n*** Update File: a.py\n*** End Patch";
        let wrapped = ToolPayload::Function {
            arguments: json!({ "cmd": format!("@'\n{envelope}\n'@ | apply_patch") }).to_string(),
        };
        let rejection = wrapped_patch_rejection(&exec_command, &wrapped, true)
            .expect("a piped envelope must be rejected");
        assert!(rejection.contains("await tools.apply_patch(patch)"));
        assert!(rejection.contains("was not run"));
        assert_eq!(
            wrapped_patch_rejection(&exec_command, &wrapped, false),
            None
        );

        let lt = '<';
        let heredoc = format!("apply_patch {lt}{lt}'EOF'\n{envelope}\nEOF");
        let native = ToolPayload::Function {
            arguments: json!({ "cmd": heredoc }).to_string(),
        };
        assert_eq!(wrapped_patch_rejection(&exec_command, &native, true), None);
        let native_argv = ToolPayload::Function {
            arguments: json!({ "program": "bash", "args": ["-lc", heredoc] }).to_string(),
        };
        assert_eq!(
            wrapped_patch_rejection(&exec_command, &native_argv, true),
            None
        );
        let ordinary = ToolPayload::Function {
            arguments: json!({ "cmd": "git diff" }).to_string(),
        };
        assert_eq!(
            wrapped_patch_rejection(&exec_command, &ordinary, true),
            None
        );
        assert_eq!(
            wrapped_patch_rejection(&ToolName::plain("read_tool_output"), &wrapped, true),
            None
        );
        for cmd in [
            r#"rg --fixed-strings "*** Begin Patch" src"#,
            r#"printf '%s' '*** Begin Patch | apply_patch'"#,
            r#"Write-Output '*** Begin Patch | apply_patch'"#,
            "@'\n*** Begin Patch\n'@ | Write-Output",
        ] {
            let payload = ToolPayload::Function {
                arguments: json!({ "cmd": cmd }).to_string(),
            };
            assert_eq!(
                wrapped_patch_rejection(&exec_command, &payload, true),
                None,
                "{cmd}"
            );
        }
        let piped = ToolPayload::Function {
            arguments: json!({ "cmd": format!("printf '%s' '{envelope}' | apply_patch") })
                .to_string(),
        };
        assert!(wrapped_patch_rejection(&exec_command, &piped, true).is_some());
    }

    /// Builds a real session, step context, and tool runtime with the given
    /// registered tools, exactly as the turn loop does for nested calls.
    async fn nested_call_fixture(
        tools: Vec<Arc<dyn crate::tools::registry::CoreToolRuntime>>,
    ) -> (
        Arc<crate::session::session::Session>,
        Arc<crate::session::turn_context::TurnContext>,
        crate::tools::parallel::ToolCallRuntime,
    ) {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let step_context = crate::session::step_context::StepContext::for_test(Arc::clone(&turn));
        let router = Arc::new(crate::tools::router::ToolRouter::from_parts(
            crate::tools::registry::ToolRegistry::from_tools(tools),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        ));
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            Arc::clone(&session),
            step_context,
            tracker,
        );
        (session, turn, runtime)
    }

    fn wrapped_patch_invocation(cell_id: &CellId) -> codex_code_mode::CodeModeNestedToolCall {
        let envelope = "*** Begin Patch\n*** Update File: a.py\n*** End Patch";
        codex_code_mode::CodeModeNestedToolCall {
            cell_id: cell_id.clone(),
            parent_tool_call_id: Some("outer-exec".to_string()),
            runtime_tool_call_id: "runtime-call-1".to_string(),
            tool_name: ToolName::plain("exec_command"),
            tool_kind: CodeModeToolKind::Function,
            input: Some(json!({ "cmd": format!("@'\n{envelope}\n'@ | apply_patch") })),
        }
    }

    #[tokio::test]
    async fn wrapped_patch_through_a_real_nested_exec_command_is_rejected_before_dispatch() {
        let apply_patch: Arc<dyn crate::tools::registry::CoreToolRuntime> = Arc::new(
            crate::tools::handlers::ApplyPatchHandler::new(/*multi_environment*/ false),
        );
        let (session, turn, runtime) = nested_call_fixture(vec![apply_patch]).await;
        let cell_id = CellId::new("cell-patch".to_string());
        session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, "outer-exec");
        let exec = super::ExecContext {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn),
        };

        let result = super::call_nested_tool(
            exec,
            runtime,
            wrapped_patch_invocation(&cell_id),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        let Err(crate::FunctionCallError::RespondToModel(message)) = result else {
            panic!("a wrapped patch must be rejected before dispatch, got {result:?}");
        };
        assert!(message.contains("await tools.apply_patch(patch)"));
        assert!(message.contains("was not run"));
        let packet = session
            .services
            .code_mode_service
            .finish_packet(cell_id.as_str(), turn.sub_id.as_str());
        assert_eq!(packet.nested_call_count, 1);
        let terminal = packet
            .first_required_terminal
            .expect("the rejection is recorded as the cell's required terminal failure");
        assert_eq!(terminal.cause, RequiredToolTerminalCause::Failure);
        assert!(terminal.message.contains("was not run"));
    }

    #[tokio::test]
    async fn harmless_patch_marker_reaches_the_registered_shell_handler() {
        let apply_patch: Arc<dyn crate::tools::registry::CoreToolRuntime> =
            Arc::new(crate::tools::handlers::ApplyPatchHandler::new(false));
        let shell: Arc<dyn crate::tools::registry::CoreToolRuntime> =
            Arc::new(crate::tools::handlers::ShellCommandHandler::new(
                crate::tools::handlers::ShellCommandHandlerOptions {
                    allow_login_shell: false,
                    allow_escalated_sandbox_permissions: false,
                    exec_permission_approvals_enabled: false,
                },
            ));
        let (session, turn, runtime) = nested_call_fixture(vec![apply_patch, shell]).await;
        let cell_id = CellId::new("harmless-patch-marker".to_string());
        let service = &session.services.code_mode_service;
        service.record_cell_parent_call_id(&cell_id, "outer-exec");
        let result = super::call_nested_tool(
            super::ExecContext {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn),
            },
            runtime,
            codex_code_mode::CodeModeNestedToolCall {
                cell_id: cell_id.clone(),
                parent_tool_call_id: Some("outer-exec".to_string()),
                runtime_tool_call_id: "print-marker".to_string(),
                tool_name: ToolName::plain("shell_command"),
                tool_kind: CodeModeToolKind::Function,
                input: Some(json!({ "command": "echo '*** Begin Patch'", "login": false })),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("printing patch-shaped data must reach ordinary shell dispatch");
        assert!(result.to_string().contains("*** Begin Patch"));
        let packet = service.finish_packet(cell_id.as_str(), turn.sub_id.as_str());
        assert_eq!(packet.nested_call_count, 1);
        assert!(packet.first_required_terminal.is_none());
        service.finish_cell_dispatch(&cell_id);
    }

    #[tokio::test]
    async fn wrapped_patch_is_not_rejected_when_no_apply_patch_tool_is_registered() {
        let (session, turn, runtime) = nested_call_fixture(Vec::new()).await;
        let cell_id = CellId::new("cell-no-patch-tool".to_string());
        session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, "outer-exec");
        let exec = super::ExecContext {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn),
        };

        let result = super::call_nested_tool(
            exec,
            runtime,
            wrapped_patch_invocation(&cell_id),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        // Without a typed patch tool the call reaches ordinary dispatch, which
        // fails here only because this fixture registers no exec_command tool.
        let Err(error) = result else {
            panic!("dispatch without a registered exec_command tool must fail");
        };
        assert!(
            !error.to_string().contains("await tools.apply_patch(patch)"),
            "the wrapped-patch rejection must depend on apply_patch being registered: {error}"
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
        assert!(
            advisory
                .contains("batch only necessary independent reads with known inputs in one exec")
        );
        assert!(
            !advisory.contains("notify"),
            "the advisory must not steer the model into notify-driven yields"
        );
        assert!(advisory.contains("Promise.allSettled"));
        assert!(advisory.contains("print needed evidence with text(...)"));
        assert!(advisory.contains("Reuse unchanged evidence"));
        assert!(advisory.contains("Stop when the task is answered"));
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
        service.record_packet_call(&cell, true, 128, Vec::new());
        assert!(service.finish_packet("cell", "turn").advisory.is_none());
        service.record_packet_call(&cell, true, 128, Vec::new());
        assert_eq!(
            service.finish_packet("cell", "next-turn").advisory,
            Some(advisory)
        );
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
                .advised_turns
                .contains("finished-turn")
        );

        service.finish_turn("finished-turn");

        assert!(
            !service
                .packet_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .advised_turns
                .contains("finished-turn")
        );
        service.record_packet_call(&cell, true, 128, Vec::new());
        assert!(
            service
                .finish_packet("cell", "finished-turn")
                .advisory
                .is_some()
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
        service.record_cell_parent_call_id(&cell, "outer-exec");
        let first = service.begin_packet_call(&cell).unwrap();
        let second = service.begin_packet_call(&cell).unwrap();

        service.complete_packet_call(
            &cell,
            second,
            false,
            0,
            Vec::new(),
            None,
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
            None,
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
        assert_eq!(
            output.sampling_request_signal().unwrap()["outcome"],
            "blocked"
        );
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
    fn transported_nested_tool_input_preserves_dispatch_validation() {
        use codex_code_mode::CodeModeNestedToolCall;
        use codex_code_mode::host::WireNestedToolCall;

        for (input, expected) in [
            (
                None,
                Ok(ToolPayload::Function {
                    arguments: "{}".to_string(),
                }),
            ),
            (
                Some(json!(null)),
                Err("tool `example` expects a JSON object for arguments".to_string()),
            ),
            (
                Some(json!({ "value": null })),
                Ok(ToolPayload::Function {
                    arguments: r#"{"value":null}"#.to_string(),
                }),
            ),
        ] {
            let invocation = CodeModeNestedToolCall {
                cell_id: codex_code_mode::CellId::new("cell-input".to_string()),
                parent_tool_call_id: None,
                runtime_tool_call_id: "nested-call".to_string(),
                tool_name: ToolName::plain("example"),
                tool_kind: CodeModeToolKind::Function,
                input,
            };
            let runtime_json = serde_json::to_vec(&invocation).expect("encode runtime input");
            let runtime: CodeModeNestedToolCall =
                serde_json::from_slice(&runtime_json).expect("decode runtime input");
            let wire = WireNestedToolCall::from(invocation.clone());
            let wire_json = serde_json::to_vec(&wire).expect("encode host input");
            let wire: WireNestedToolCall =
                serde_json::from_slice(&wire_json).expect("decode host input");

            for received in [invocation, runtime, wire.into()] {
                assert_eq!(
                    build_nested_tool_payload(
                        received.tool_kind,
                        &received.tool_name,
                        received.input
                    ),
                    expected
                );
            }
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
    fn nested_failure_fingerprint_preserves_diagnostic_numbers() {
        let tool_name = ToolName::plain("example");
        assert_ne!(
            nested_failure_fingerprint(&tool_name, "request failed with status 403"),
            nested_failure_fingerprint(&tool_name, "request failed with status 404"),
        );
        assert_eq!(
            nested_failure_fingerprint(&tool_name, "request 17 failed"),
            nested_failure_fingerprint(&tool_name, "request  17\nfailed"),
        );
        assert_eq!(
            nested_failure_fingerprint(
                &tool_name,
                r#"tool failed: {"fingerprint":"owner.stable.failure"}"#,
            ),
            "owner.stable.failure"
        );
        assert_ne!(
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
            truncate_code_mode_result(items, Some(5), OutputOutcome::Success, usize::MAX, None);
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

    #[tokio::test]
    async fn artifact_recovery_leaves_space_for_the_outer_exec_envelope() {
        use crate::tools::command_output_artifact::ToolOutputSelector;
        use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
        use crate::tools::command_output_artifact::create_canonical_output_artifact;
        use crate::tools::handlers::execute_recovery_transaction;
        use codex_tools::CanonicalToolResult;

        let home = tempfile::tempdir().expect("artifact home");
        // Both fixtures fit automatic direct recovery. The larger body exceeds the
        // nested aggregate budget, exercising the outer envelope reserve itself.
        for size in [9_000, 14_000] {
            let text = "x".repeat(size);
            let artifact = create_canonical_output_artifact(
                home.path(),
                "envelope-thread",
                &CanonicalToolResult::text(&text),
            )
            .await;
            let id = artifact.artifact_id().expect("canonical artifact admitted");
            let selectors = vec![ToolOutputSelector::Bytes {
                start: 0,
                end: size as u64,
            }];
            let (direct, _) = execute_recovery_transaction(
                home.path(),
                "envelope-thread",
                &id,
                selectors.clone(),
                false,
            )
            .await
            .expect("direct recovery");
            assert!(direct.complete);
            assert_eq!(direct.results[0].status, ToolOutputSelectorStatus::Ok);
            assert_eq!(direct.results[0].exact_bytes, Some(size as u64));
            assert_eq!(direct.results[0].text.as_deref(), Some(text.as_str()));
            let direct_tokens = codex_utils_string::approx_token_count(
                &serde_json::to_string(&direct).expect("serialize direct recovery"),
            );
            if size == 9_000 {
                assert!(
                    direct_tokens <= 3_000,
                    "fitting body must enter nested exact recovery"
                );
            } else {
                assert!(
                    direct_tokens > 3_128 && direct_tokens <= 10_000,
                    "actual serialized body must exceed nested recovery plus retry margin while fitting direct recovery"
                );
            }
            let (nested, _) =
                execute_recovery_transaction(home.path(), "envelope-thread", &id, selectors, true)
                    .await
                    .expect("code-mode recovery");
            if size == 9_000 {
                assert_eq!(nested.results[0].status, ToolOutputSelectorStatus::Ok);
                assert_eq!(nested.results[0].text.as_deref(), Some(text.as_str()));
            } else {
                assert_ne!(nested.results[0].status, ToolOutputSelectorStatus::Ok);
                assert!(
                    nested.results[0].text.is_none(),
                    "overflow must not clip exact text"
                );
                assert!(
                    !nested.results[0].child_selectors.is_empty()
                        || nested.results[0].continuation.is_some(),
                    "overflow must remain recoverable"
                );
            }
            let rendered = serde_json::to_string(&nested).expect("serialize nested recovery");
            let outer = format_runtime_response(
                RuntimeResponse::Result {
                    cell_id: CellId::new("recovery-envelope".to_string()),
                    content_items: vec![
                        codex_code_mode::FunctionCallOutputContentItem::InputText {
                            text: rendered.clone(),
                        },
                    ],
                    error_text: None,
                },
                Some(4_000),
                4_000,
                false,
                std::time::Instant::now(),
                Vec::new(),
                Vec::new(),
                None,
            );
            let projected =
                codex_protocol::models::function_call_output_content_items_to_text(&outer.body)
                    .expect("outer exec output");
            assert!(
                projected.contains(&rendered),
                "outer formatting must preserve the whole recovery JSON"
            );
            assert!(!projected.contains("Warning: truncated output"));
            assert!(
                codex_utils_string::approx_token_count(&projected) <= 4_000,
                "actual outer status plus recovery must fit the requested exec budget"
            );
        }
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
            Vec::new(),
            None,
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

        let truncated = truncate_code_mode_result(items, Some(20), OutputOutcome::Success, 5, None);
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
                text: format!(
                    "Script error:\nOLD_LOG {}",
                    "ordinary output ".repeat(1_000)
                ),
            },
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "opaque".to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: "Script error:\nROOT_CAUSE_SENTINEL".to_string(),
            },
        ];

        let projected =
            truncate_code_mode_result(items, Some(40), OutputOutcome::Failure, usize::MAX, Some(2));

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

        let projected =
            truncate_code_mode_result(items, None, OutputOutcome::Success, usize::MAX, None);

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
        let capped =
            truncate_code_mode_result(oversized, None, OutputOutcome::Success, usize::MAX, None);
        let [FunctionCallOutputContentItem::InputText { text: capped_text }] = capped.as_slice()
        else {
            panic!("expected one capped text item");
        };
        assert!(capped_text.starts_with("Warning: truncated output"));
        let capped_tokens = codex_utils_string::approx_token_count(capped_text);
        assert!(
            capped_tokens <= codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL + 64,
            "the truncation body should honor the hard limit with only bounded warning metadata; got {capped_tokens} tokens"
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
