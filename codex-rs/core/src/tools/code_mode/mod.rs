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

use crate::function_tool::FunctionCallError;
use crate::original_image_detail::can_request_original_image_detail;
use crate::original_image_detail::sanitize_original_image_detail as sanitize_image_detail_items;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::effective_tool_mode;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use crate::tools::router::build_function_tool_payload;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolFailureClass;
use codex_tools::ToolFailureDiagnostic;
use codex_tools::ToolName;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text_content_items_with_policy;
use codex_utils_output_truncation::resolve_output_limits;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;

use delegate::CodeModeDispatchBroker;
use delegate::CodeModeDispatchWorker;
pub(crate) use execute_handler::CodeModeExecuteHandler;
use response_adapter::into_function_call_output_content_items;
pub(crate) use wait_handler::CodeModeWaitHandler;

pub(crate) const PUBLIC_TOOL_NAME: &str = codex_code_mode::PUBLIC_TOOL_NAME;
pub(crate) const WAIT_TOOL_NAME: &str = codex_code_mode::WAIT_TOOL_NAME;
pub(crate) const DEFAULT_WAIT_YIELD_TIME_MS: u64 = codex_code_mode::DEFAULT_WAIT_YIELD_TIME_MS;

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
    shutting_down: AtomicBool,
    cell_batch_observations: Mutex<HashMap<CellId, CellBatchObservation>>,
    read_only_singleton_streaks: Mutex<HashMap<String, u8>>,
}

#[derive(Debug)]
struct CellBatchObservation {
    call_count: usize,
    all_read_only: bool,
    all_successful: bool,
    failures: Vec<AggregatedNestedFailure>,
    omitted_failure_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AggregatedNestedFailure {
    #[serde(flatten)]
    diagnostic: ToolFailureDiagnostic,
    occurrences: usize,
}

#[derive(Debug, Default)]
struct CellCompletionFeedback {
    batching_feedback: Option<String>,
    failures: Vec<AggregatedNestedFailure>,
    omitted_failure_count: usize,
}

const MAX_NESTED_FAILURE_DIAGNOSTICS: usize = 8;

impl Default for CellBatchObservation {
    fn default() -> Self {
        Self {
            call_count: 0,
            all_read_only: true,
            all_successful: true,
            failures: Vec::new(),
            omitted_failure_count: 0,
        }
    }
}

impl CodeModeService {
    pub(crate) fn new(session_provider: Arc<dyn CodeModeSessionProvider>) -> Self {
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::new());
        Self {
            session: OnceCell::new(),
            session_provider,
            dispatch_broker,
            shutting_down: AtomicBool::new(false),
            cell_batch_observations: Mutex::new(HashMap::new()),
            read_only_singleton_streaks: Mutex::new(HashMap::new()),
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

    pub(crate) fn has_waitable_cells(&self) -> bool {
        self.dispatch_broker.has_waitable_cells()
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

    fn record_nested_tool_observation(
        &self,
        cell_id: &CellId,
        tool_name: &ToolName,
        payload: &ToolPayload,
        successful: bool,
        failure: Option<ToolFailureDiagnostic>,
    ) {
        let mut observations = self
            .cell_batch_observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let observation = observations.entry(cell_id.clone()).or_default();
        observation.call_count = observation.call_count.saturating_add(1);
        observation.all_read_only &= nested_call_is_known_read_only(tool_name, payload);
        observation.all_successful &= successful;
        if let Some(failure) = failure {
            if let Some(existing) = observation
                .failures
                .iter_mut()
                .find(|existing| existing.diagnostic.fingerprint == failure.fingerprint)
            {
                existing.occurrences = existing.occurrences.saturating_add(1);
            } else if observation.failures.len() < MAX_NESTED_FAILURE_DIAGNOSTICS {
                observation.failures.push(AggregatedNestedFailure {
                    diagnostic: failure,
                    occurrences: 1,
                });
            } else {
                observation.omitted_failure_count =
                    observation.omitted_failure_count.saturating_add(1);
            }
        }
    }

    fn take_cell_completion_feedback(
        &self,
        turn_id: &str,
        cell_id: &CellId,
        terminal_success: Option<bool>,
    ) -> CellCompletionFeedback {
        let Some(terminal_success) = terminal_success else {
            return CellCompletionFeedback::default();
        };
        let observation = self
            .cell_batch_observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(cell_id)
            .unwrap_or_default();
        let singleton_read = terminal_success
            && observation.call_count == 1
            && observation.all_read_only
            && observation.all_successful;
        let mut streaks = self
            .read_only_singleton_streaks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let streak = streaks.entry(turn_id.to_string()).or_default();
        *streak = if singleton_read {
            (*streak).saturating_add(1)
        } else {
            0
        };
        let batching_feedback = if *streak < 2 {
            None
        } else {
            *streak = 0;
            Some(
                "Batching hint: the last two successful code-mode packets each performed one known read-only call. If the next reads are independent, issue them together with Promise.all in one exec packet."
                    .to_string(),
            )
        };
        CellCompletionFeedback {
            batching_feedback,
            failures: observation.failures,
            omitted_failure_count: observation.omitted_failure_count,
        }
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

async fn handle_runtime_response(
    exec: &ExecContext,
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    recovery_queries: &[String],
    completion_feedback: CellCompletionFeedback,
    started_at: std::time::Instant,
) -> Result<FunctionToolOutput, String> {
    // Nested tool results have already crossed their owning tool boundary. Keep
    // one coherent, model-safe exec packet here instead of applying the much
    // smaller generic per-tool diagnostic budget a second time.
    let hard_limit = codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL
        .min(TruncationPolicy::from(exec.turn.model_info.truncation_policy).token_budget());
    let original_image_detail_supported = can_request_original_image_detail(&exec.turn.model_info);

    Ok(format_runtime_response(
        response,
        max_output_tokens,
        recovery_queries,
        completion_feedback,
        hard_limit,
        original_image_detail_supported,
        started_at,
    ))
}

fn format_runtime_response(
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    recovery_queries: &[String],
    completion_feedback: CellCompletionFeedback,
    hard_limit: usize,
    original_image_detail_supported: bool,
    started_at: std::time::Instant,
) -> FunctionToolOutput {
    let continuation_owner_key = match &response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id.to_string(),
    };
    let script_status = format_script_status(&response);
    let (mut content_items, outcome, success) = match response {
        RuntimeResponse::Yielded { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::TimedOut, true)
        }
        RuntimeResponse::Terminated { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::Failure, true)
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

    sanitize_image_detail_items(original_image_detail_supported, &mut content_items);
    if let Some(recovery) = targeted_recovery_contexts(&content_items, recovery_queries) {
        content_items.push(FunctionCallOutputContentItem::InputText { text: recovery });
    }
    let mut content_items =
        truncate_code_mode_result(content_items, max_output_tokens, outcome, hard_limit);
    if !completion_feedback.failures.is_empty() || completion_feedback.omitted_failure_count != 0 {
        let total_occurrences = completion_feedback
            .failures
            .iter()
            .map(|failure| failure.occurrences)
            .sum::<usize>()
            .saturating_add(completion_feedback.omitted_failure_count);
        let summary = serde_json::json!({
            "nested_tool_failures": {
                "total_occurrences": total_occurrences,
                "distinct_retained": completion_feedback.failures.len(),
                "omitted_occurrences": completion_feedback.omitted_failure_count,
                "failures": completion_feedback.failures,
            }
        });
        content_items.push(FunctionCallOutputContentItem::InputText {
            text: format!("Nested tool failure summary:\n{summary}"),
        });
    }
    if let Some(feedback) = completion_feedback.batching_feedback {
        content_items.push(FunctionCallOutputContentItem::InputText { text: feedback });
    }
    prepend_script_status(&mut content_items, &script_status, started_at.elapsed());
    let typed_outcome = match outcome {
        OutputOutcome::Success => codex_tools::ToolOutputOutcome::Success,
        OutputOutcome::Failure => codex_tools::ToolOutputOutcome::Failure,
        OutputOutcome::TimedOut => codex_tools::ToolOutputOutcome::TimedOut,
        OutputOutcome::Skipped => codex_tools::ToolOutputOutcome::Skipped,
    };
    FunctionToolOutput::from_content(content_items, Some(success))
        .with_outcome(typed_outcome)
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
    let limits = resolve_output_limits(
        max_output_tokens,
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

    truncate_function_output_items_with_policy(&items, policy)
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
        runtime_tool_call_id,
        tool_name,
        tool_kind,
        input,
    } = invocation;
    if is_exec_tool_name(&tool_name) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{PUBLIC_TOOL_NAME} cannot invoke itself"
        )));
    }

    let payload = match build_nested_tool_payload(tool_kind, &tool_name, input) {
        Ok(payload) => payload,
        Err(error) => return Err(FunctionCallError::RespondToModel(error)),
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
                runtime_tool_call_id,
            },
            cancellation_token,
        )
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let failure = nested_failure_from_error(&tool_name, &error.to_string());
            exec.session
                .services
                .code_mode_service
                .record_nested_tool_observation(
                    &cell_id,
                    &tool_name,
                    &payload,
                    false,
                    Some(failure),
                );
            return Err(error);
        }
    };
    let signal = result.sampling_request_signal();
    let failure = nested_failure_from_signal(signal.as_ref());
    exec.session
        .services
        .code_mode_service
        .record_nested_tool_observation(&cell_id, &tool_name, &payload, failure.is_none(), failure);
    let receipts = result.intrinsic_deterministic_continuation_receipts();
    if let Some(continuation) = result.owner_drained_continuation() {
        exec.session
            .services
            .code_mode_service
            .record_owner_drained_continuation(&cell_id, continuation);
    }
    let result_value = result.code_mode_result();
    tool_runtime.record_code_mode_result(
        &tool_name,
        &payload,
        signal.as_ref(),
        result_value.clone(),
        &receipts,
    );
    Ok(result_value)
}

fn nested_failure_from_signal(signal: Option<&JsonValue>) -> Option<ToolFailureDiagnostic> {
    signal
        .and_then(|signal| signal.get("failure"))
        .and_then(|failure| serde_json::from_value(failure.clone()).ok())
}

fn nested_failure_from_error(tool_name: &ToolName, error: &str) -> ToolFailureDiagnostic {
    if let Some(json_start) = error.find('{')
        && let Ok(diagnostic) = serde_json::from_str(&error[json_start..])
    {
        return diagnostic;
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
    let fingerprint = format!(
        "code_mode.nested_tool.{:x}",
        Sha256::digest(format!("{tool_name}\0{normalized}").as_bytes())
    );
    ToolFailureDiagnostic::model_visible(
        ToolFailureClass::ToolExecution,
        fingerprint,
        format!("nested `{tool_name}` call failed"),
    )
    .with_owner_hint(tool_name.to_string())
    .with_next_action("inspect the retained nested failure before changing strategy")
}

pub(super) fn build_nested_tool_payload(
    tool_kind: CodeModeToolKind,
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match tool_kind {
        CodeModeToolKind::Function => {
            let arguments = serialize_function_tool_arguments(tool_name, input)?;
            build_function_tool_payload(tool_name, arguments)
        }
        CodeModeToolKind::Freeform => build_freeform_tool_payload(tool_name, input),
    }
}

fn targeted_recovery_contexts(
    items: &[FunctionCallOutputContentItem],
    queries: &[String],
) -> Option<String> {
    if queries.is_empty() {
        return None;
    }
    let text = code_mode_text_content(items);
    let lines = text.lines().collect::<Vec<_>>();
    let mut selected = std::collections::BTreeSet::new();
    for query in queries {
        for (index, _) in lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(query))
            .take(4)
        {
            let start = index.saturating_sub(1);
            let end = (index + 1).min(lines.len().saturating_sub(1));
            selected.extend(start..=end);
        }
    }
    if selected.is_empty() {
        return Some("Targeted recovery contexts: no declared query matched.".to_string());
    }
    let mut output = String::from("Targeted recovery contexts (exact pre-truncation lines):\n");
    for index in selected {
        let rendered = format!("{}: {}\n", index + 1, lines[index]);
        if output.len().saturating_add(rendered.len()) > 12_000 {
            output.push_str("[recovery contexts bounded at 12000 bytes]\n");
            break;
        }
        output.push_str(&rendered);
    }
    Some(output)
}

fn nested_call_is_known_read_only(tool_name: &ToolName, payload: &ToolPayload) -> bool {
    if tool_name.namespace.is_some() {
        return false;
    }
    match tool_name.name.as_str() {
        "read_tool_output"
        | "tool_search"
        | "view_image"
        | "list_mcp_resources"
        | "list_mcp_resource_templates"
        | "read_mcp_resource" => true,
        "exec_command" => exec_command_is_known_read_only(payload),
        _ => false,
    }
}

fn exec_command_is_known_read_only(payload: &ToolPayload) -> bool {
    let ToolPayload::Function { arguments } = payload else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<JsonValue>(arguments) else {
        return false;
    };
    let Some(program) = value.get("program").and_then(JsonValue::as_str) else {
        return false;
    };
    match program.to_ascii_lowercase().as_str() {
        "rg" | "cat" | "head" | "tail" => true,
        "git" => value
            .get("args")
            .and_then(JsonValue::as_array)
            .and_then(|args| args.first())
            .and_then(JsonValue::as_str)
            .is_some_and(|subcommand| {
                matches!(
                    subcommand,
                    "status" | "diff" | "log" | "show" | "grep" | "ls-files" | "rev-parse"
                )
            }),
        _ => false,
    }
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
    use super::nested_call_is_known_read_only;
    use super::targeted_recovery_contexts;
    use super::truncate_code_mode_result;
    use crate::tools::context::ToolPayload;
    use codex_code_mode::CodeModeToolKind;
    use codex_code_mode::ExecuteRequest;
    use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_tools::ToolName;
    use serde_json::json;

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
    fn build_nested_tool_payload_uses_native_tool_search_payload() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain("tool_search"),
            Some(json!({ "query": "repo atlas", "limit": 4 })),
        )
        .expect("tool_search payload should deserialize");

        match payload {
            ToolPayload::ToolSearch { arguments } => {
                assert_eq!(arguments.query, "repo atlas");
                assert_eq!(arguments.limit, Some(4));
            }
            other => panic!("expected tool_search payload, got {other:?}"),
        }
    }

    #[test]
    fn build_nested_tool_payload_rejects_malformed_tool_search_payload() {
        let error = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain("tool_search"),
            Some(json!({ "query": 42 })),
        )
        .expect_err("malformed nested tool_search should fail to build");

        assert!(error.starts_with("failed to parse tool_search arguments:"));
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
                    "0123456789…5 tokens truncated…0123456789"
                )
                .to_string(),
            }]
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
        assert!(text.contains("95 tokens truncated"));
    }

    #[test]
    fn default_outer_success_budget_preserves_a_multi_tool_evidence_packet() {
        let text = "x".repeat(24_000);
        assert!(codex_utils_string::approx_token_count(&text) > 4_000);
        let items = vec![FunctionCallOutputContentItem::InputText { text: text.clone() }];

        let projected = truncate_code_mode_result(items, None, OutputOutcome::Success, usize::MAX);

        assert_eq!(
            projected,
            vec![FunctionCallOutputContentItem::InputText { text }]
        );
        assert_eq!(
            codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS,
            codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
        );
    }

    #[test]
    fn targeted_recovery_selects_exact_pre_truncation_context() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "first\nowner: task_evidence\nvalidation: focused\nlast".to_string(),
        }];

        let recovery = targeted_recovery_contexts(&items, &["owner:".to_string()])
            .expect("declared recovery query should produce context");

        assert!(recovery.contains("1: first"));
        assert!(recovery.contains("2: owner: task_evidence"));
        assert!(recovery.contains("3: validation: focused"));
        assert!(!recovery.contains("4: last"));
    }

    #[test]
    fn adaptive_batching_feedback_requires_two_successful_singleton_reads() {
        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program("unused".into()),
        ));
        let payload = ToolPayload::Function {
            arguments: serde_json::to_string(&json!({
                "kind": "argv",
                "program": "rg",
                "args": ["needle", "src"]
            }))
            .expect("serialize exec arguments"),
        };
        assert!(nested_call_is_known_read_only(
            &ToolName::plain("exec_command"),
            &payload
        ));

        let first = codex_code_mode::CellId::new("cell-first".to_string());
        service.record_nested_tool_observation(
            &first,
            &ToolName::plain("exec_command"),
            &payload,
            true,
            None,
        );
        assert!(
            service
                .take_cell_completion_feedback("turn-a", &first, Some(true))
                .batching_feedback
                .is_none()
        );

        let second = codex_code_mode::CellId::new("cell-second".to_string());
        service.record_nested_tool_observation(
            &second,
            &ToolName::plain("exec_command"),
            &payload,
            true,
            None,
        );
        assert!(
            service
                .take_cell_completion_feedback("turn-a", &second, Some(true))
                .batching_feedback
                .is_some()
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
