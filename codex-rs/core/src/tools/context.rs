use crate::context_manager::truncate_function_output_payload;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::command_output_artifact::RawOutputArtifact;
#[cfg(test)]
use crate::tools::command_output_artifact::ToolOutputArtifactId;
use crate::tools::shell_output_summary::ShellOutputSummaryOptions;
use crate::tools::shell_output_summary::summarize_shell_output_for_model;
use crate::tools::tool_dispatch_trace::ToolDispatchTrace;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_code_mode::CancellationCause;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::CanonicalToolResult;
use codex_tools::CodeModeToolSearchStatus;
use codex_tools::ToolName;
use codex_tools::ToolOutputDiagnosticClass;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputProjectionFragment;
use codex_tools::ToolOutputProjectionFragmentKind;
use codex_tools::ToolOutputProjectionMetadata;
use codex_tools::ToolOutputProjectionRange;
use codex_tools::ToolOutputSkipDisposition;
use codex_tools::code_mode_tool_search_result;
use codex_tools::sanitize_original_image_detail;
use codex_tools::telemetry_preview;
use codex_utils_output_truncation::OutputLimitResolution;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::classify_diagnostic;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_output_truncation::formatted_truncate_text_with_line_markers;
use codex_utils_output_truncation::formatted_truncate_text_with_output_limit;
use codex_utils_output_truncation::resolve_projected_output_limits;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
std::thread_local! {
    static FUNCTION_PROJECTION_METADATA_CALLS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static EXEC_COMMAND_RESPONSE_MATERIALIZATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

pub use codex_tools::ToolOutput;
pub use codex_tools::ToolPayload;

pub(crate) fn boxed_tool_output<T>(output: T) -> Box<dyn ToolOutput>
where
    T: ToolOutput + 'static,
{
    Box::new(output)
}

pub type SharedTurnDiffTracker = Arc<Mutex<TurnDiffTracker>>;

/// Runtime-owned recovery facts. Cancellation never implies rollback or grants
/// permission to replay an effectful request.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub(crate) struct ToolEffectRecovery {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) committed_effects: Vec<JsonValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) uncertain_effects: Vec<String>,
    pub(crate) safe_next_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observed_result: Option<JsonValue>,
}

impl ToolEffectRecovery {
    pub(crate) fn unknown(result: Option<&JsonValue>, error: Option<&str>) -> Self {
        Self {
            committed_effects: Vec::new(),
            uncertain_effects: vec![error.unwrap_or(
                "The runtime has not established which effects committed or stopped."
            ).to_string()],
            safe_next_actions: vec![
                "Inspect retained results and external state; do not replay the request automatically.".into(),
            ],
            observed_result: result.cloned(),
        }
    }

    pub(crate) fn committed(result: JsonValue) -> Self {
        Self {
            committed_effects: vec![result],
            uncertain_effects: Vec::new(),
            safe_next_actions: vec![
                "Reuse the committed result; retry only work explicitly reported as uncommitted.".into(),
            ],
            observed_result: None,
        }
    }
}

/// Linearizes tool admission, ordinary completion, and cancellation. Keeping
/// these transitions in one atomic prevents combinations such as "cancelled
/// before admission" and "handler completed" from being observed together.
#[derive(Debug)]
pub(crate) struct ToolDispatchState {
    state: AtomicU8,
    handler_started: AtomicBool,
    trace: std::sync::OnceLock<ToolDispatchTrace>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ToolDispatchPhase {
    WaitingForAdmission = 0,
    Admitted = 1,
    Completed = 2,
    AbortedBeforeAdmission = 3,
    AbortedAfterAdmission = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolDispatchAbort {
    BeforeAdmission,
    AfterAdmission,
    AlreadyTerminal,
}

impl ToolDispatchState {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(ToolDispatchPhase::WaitingForAdmission as u8),
            handler_started: AtomicBool::new(false),
            trace: std::sync::OnceLock::new(),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "Each dispatch must attach exactly one trace; a duplicate is a programming error"
    )]
    pub(crate) fn attach_trace(&self, trace: ToolDispatchTrace) {
        self.trace.set(trace).expect("one trace per dispatch");
    }

    pub(crate) async fn record_cancelled_trace(&self) {
        if let Some(trace) = self.trace.get() {
            trace.record_cancelled().await;
        }
    }

    pub(crate) async fn record_failed_trace(&self, error: &crate::FunctionCallError) {
        if let Some(trace) = self.trace.get() {
            trace.record_failed(error).await;
        }
    }

    pub(crate) fn try_admit(&self) -> bool {
        self.state
            .compare_exchange(
                ToolDispatchPhase::WaitingForAdmission as u8,
                ToolDispatchPhase::Admitted as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn try_complete(&self) -> bool {
        self.state
            .compare_exchange(
                ToolDispatchPhase::Admitted as u8,
                ToolDispatchPhase::Completed as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn try_abort(&self) -> ToolDispatchAbort {
        loop {
            let current = self.state.load(Ordering::Acquire);
            let (next, outcome) = match current {
                value if value == ToolDispatchPhase::WaitingForAdmission as u8 => (
                    ToolDispatchPhase::AbortedBeforeAdmission,
                    ToolDispatchAbort::BeforeAdmission,
                ),
                value if value == ToolDispatchPhase::Admitted as u8 => (
                    ToolDispatchPhase::AbortedAfterAdmission,
                    ToolDispatchAbort::AfterAdmission,
                ),
                _ => return ToolDispatchAbort::AlreadyTerminal,
            };
            if self
                .state
                .compare_exchange(current, next as u8, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return outcome;
            }
        }
    }

    pub(crate) fn mark_handler_started(&self) {
        self.handler_started.store(true, Ordering::Release);
    }

    pub(crate) fn handler_started(&self) -> bool {
        self.handler_started.load(Ordering::Acquire)
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            value if value == ToolDispatchPhase::Completed as u8
                || value == ToolDispatchPhase::AbortedBeforeAdmission as u8
                || value == ToolDispatchPhase::AbortedAfterAdmission as u8
        )
    }

    pub(crate) fn is_aborted(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            value if value == ToolDispatchPhase::AbortedBeforeAdmission as u8
                || value == ToolDispatchPhase::AbortedAfterAdmission as u8
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCallSource {
    Direct,
    CodeMode {
        /// Runtime cell that issued the nested tool request.
        cell_id: String,
        /// Model-visible `functions.exec` call that owns the runtime cell.
        parent_call_id: Option<String>,
        /// Code-mode's per-cell tool invocation id. This is useful for
        /// debugging the JS/runtime bridge, but it is not the Codex tool call id
        /// because the runtime id only needs to be unique within one cell.
        runtime_tool_call_id: String,
        /// Instant the runtime's wrapper timeout fires for this nested call, on
        /// this process's clock.
        ///
        /// A handler that waits should complete cooperatively before it and
        /// return a resumable result, because the wrapper's own expiry drops
        /// the handler's future and no recovery can reach the cell. Every stage
        /// before observation — dispatch, hooks, lock acquisition, startup — is
        /// already charged against it. `None` when the runtime supplied none.
        nested_deadline: Option<std::time::Instant>,
        /// Write-once record of why this call's cancellation token fired.
        ///
        /// Read only when reporting a cancelled call, so a runtime deadline, a
        /// turn shutdown, and a user interrupt stay distinguishable instead of
        /// all reading as "aborted by user".
        cancellation_cause: Option<Arc<OnceLock<CancellationCause>>>,
    },
}

impl ToolCallSource {
    /// Remaining nested budget, or `None` for a call with no wrapper deadline.
    pub(crate) fn nested_deadline(&self) -> Option<std::time::Instant> {
        match self {
            Self::Direct => None,
            Self::CodeMode {
                nested_deadline, ..
            } => *nested_deadline,
        }
    }

    /// Why this call was cancelled, when an origin recorded it.
    pub(crate) fn cancellation_cause(&self) -> Option<CancellationCause> {
        match self {
            Self::Direct => None,
            Self::CodeMode {
                cancellation_cause, ..
            } => cancellation_cause.as_ref()?.get().cloned(),
        }
    }
}

/// Internal semantic reason a required model-issued tool fixes the turn's
/// outcome. This deliberately remains separate from the public completion
/// enum: transport completion and semantic success are independent contracts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequiredToolTerminalCause {
    Blocked,
    Failure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RequiredToolTerminal {
    pub(crate) call_id: String,
    pub(crate) cause: RequiredToolTerminalCause,
    pub(crate) message: String,
}

#[derive(Clone)]
pub struct ToolInvocation {
    pub session: Arc<Session>,
    pub(crate) step_context: Arc<StepContext>,
    pub cancellation_token: CancellationToken,
    pub tracker: SharedTurnDiffTracker,
    pub call_id: String,
    pub tool_name: ToolName,
    pub source: ToolCallSource,
    pub payload: ToolPayload,
}

#[derive(Clone, Debug)]
pub struct McpToolOutput {
    result: CallToolResult,
    tool_input: JsonValue,
    wall_time: Duration,
    original_image_detail_supported: bool,
    truncation_policy: TruncationPolicy,
    projections: Arc<McpToolOutputProjections>,
}

#[derive(Debug, Default)]
struct McpToolOutputProjections {
    raw_json: OnceLock<Result<JsonValue, String>>,
    provider_payload: OnceLock<FunctionCallOutputPayload>,
}

impl ToolOutput for McpToolOutput {
    fn log_preview(&self) -> String {
        let payload = self.provider_payload();
        let preview = payload.body.to_text().unwrap_or_else(|| {
            serde_json::to_string(&self.result.content)
                .unwrap_or_else(|err| format!("failed to serialize mcp result: {err}"))
        });
        telemetry_preview(&preview)
    }

    fn success_for_logging(&self) -> bool {
        self.result.success()
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        self.raw_json_projection()
            .ok()
            .cloned()
            .map(|semantic_evidence| {
                if self.result.success() {
                    semantic_evidence_sampling_signal(semantic_evidence)
                } else {
                    semantic_failure_sampling_signal(semantic_evidence)
                }
            })
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        ToolOutput::projection_metadata(&self.result)
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.raw_json_projection()
            .ok()
            .cloned()
            .map(CanonicalToolResult::json)
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: self.provider_payload().clone(),
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.raw_json_projection().map_or_else(
            |err| JsonValue::String(format!("failed to serialize mcp result: {err}")),
            Clone::clone,
        )
    }

    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(self.tool_input.clone())
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        self.raw_json_projection().ok().cloned()
    }
}

impl McpToolOutput {
    pub(crate) fn new(
        result: CallToolResult,
        tool_input: JsonValue,
        wall_time: Duration,
        original_image_detail_supported: bool,
        truncation_policy: TruncationPolicy,
    ) -> Self {
        Self {
            result,
            tool_input,
            wall_time,
            original_image_detail_supported,
            truncation_policy,
            projections: Arc::new(McpToolOutputProjections::default()),
        }
    }

    fn raw_json_projection(&self) -> Result<&JsonValue, &str> {
        match self
            .projections
            .raw_json
            .get_or_init(|| serde_json::to_value(&self.result).map_err(|err| err.to_string()))
        {
            Ok(value) => Ok(value),
            Err(error) => Err(error),
        }
    }

    fn provider_payload(&self) -> &FunctionCallOutputPayload {
        self.projections
            .provider_payload
            .get_or_init(|| self.build_provider_payload())
    }

    fn build_provider_payload(&self) -> FunctionCallOutputPayload {
        let mut payload = self.result.as_function_call_output_payload();
        if let Some(items) = payload.content_items_mut() {
            sanitize_original_image_detail(self.original_image_detail_supported, items);
        }

        let wall_time_seconds = self.wall_time.as_secs_f64();
        let header = format!("Wall time: {wall_time_seconds:.4} seconds\nOutput:");

        match &mut payload.body {
            FunctionCallOutputBody::Text(text) => {
                if text.is_empty() {
                    *text = header;
                } else {
                    *text = format!("{header}\n{text}");
                }
            }
            FunctionCallOutputBody::ContentItems(items) => {
                items.insert(0, FunctionCallOutputContentItem::InputText { text: header });
            }
        }

        // This is the context-injection form, so keep it aligned with the
        // function-call output truncation that conversation history already
        // applies. Code-mode consumers still get the raw `CallToolResult`.
        //
        // The text is serialized again inside the Responses payload, so allow
        // a small buffer for JSON escaping and wrapper overhead.
        truncate_function_output_payload(&payload, self.truncation_policy * 1.2)
    }

    #[cfg(test)]
    pub(crate) fn projection_cache_state(&self) -> (bool, bool) {
        (
            self.projections.raw_json.get().is_some(),
            self.projections.provider_payload.get().is_some(),
        )
    }
}

#[derive(Clone)]
pub struct ToolSearchOutput {
    pub tools: Vec<JsonValue>,
    pub omitted_result_count: usize,
    pub unactivated_matches: Vec<String>,
}

impl ToolOutput for ToolSearchOutput {
    fn log_preview(&self) -> String {
        let tools = serde_json::to_string(&self.tools)
            .unwrap_or_else(|err| format!("failed to serialize tool_search output: {err}"));
        telemetry_preview(&tools)
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::ToolSearchOutput {
            call_id: call_id.to_string(),
            status: if self.omitted_result_count == 0 {
                "completed".to_string()
            } else {
                "incomplete".to_string()
            },
            execution: "client".to_string(),
            tools: self.tools.clone(),
            omitted_result_count: Some(self.omitted_result_count),
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        let status = if self.omitted_result_count == 0 {
            CodeModeToolSearchStatus::Completed
        } else {
            CodeModeToolSearchStatus::Incomplete
        };
        let mut result =
            code_mode_tool_search_result(status, self.tools.clone(), Some(self.omitted_result_count));
        if !self.unactivated_matches.is_empty() {
            result["unactivated_matches"] = serde_json::json!(self.unactivated_matches);
        }
        result
    }
}

pub struct FunctionToolOutput {
    /// Control and recovery information that must survive output projection.
    pub essential_inline: serde_json::Map<String, JsonValue>,
    pub body: Vec<FunctionCallOutputContentItem>,
    /// Complete provider result used only for canonical artifact admission when
    /// `body` is a bounded model-facing projection.
    pub canonical_body: Option<Vec<FunctionCallOutputContentItem>>,
    pub success: Option<bool>,
    pub outcome: Option<ToolOutputOutcome>,
    pub post_tool_use_response: Option<JsonValue>,
    /// Private signal consumed by the request-local turn execution control. This is
    /// never included in the model-facing tool result or public protocol.
    pub sampling_request_signal: Option<JsonValue>,
    pub deterministic_continuation_receipts: Vec<TurnTimingDeterministicContinuationReceipt>,
    pub deterministic_continuation_owner_key: Option<String>,
    pub skip_disposition: Option<ToolOutputSkipDisposition>,
}

#[cfg(test)]
impl FunctionToolOutput {
    pub(crate) fn reset_projection_metadata_call_count() {
        FUNCTION_PROJECTION_METADATA_CALLS.with(|calls| calls.set(0));
    }

    pub(crate) fn projection_metadata_call_count() -> usize {
        FUNCTION_PROJECTION_METADATA_CALLS.with(std::cell::Cell::get)
    }
}

impl FunctionToolOutput {
    pub fn from_text(text: String, success: Option<bool>) -> Self {
        Self {
            essential_inline: Default::default(),
            body: vec![FunctionCallOutputContentItem::InputText { text }],
            canonical_body: None,
            success,
            outcome: None,
            post_tool_use_response: None,
            sampling_request_signal: None,
            deterministic_continuation_receipts: Vec::new(),
            deterministic_continuation_owner_key: None,
            skip_disposition: None,
        }
    }

    pub fn from_content(
        content: Vec<FunctionCallOutputContentItem>,
        success: Option<bool>,
    ) -> Self {
        Self {
            essential_inline: Default::default(),
            body: content,
            canonical_body: None,
            success,
            outcome: None,
            post_tool_use_response: None,
            sampling_request_signal: None,
            deterministic_continuation_receipts: Vec::new(),
            deterministic_continuation_owner_key: None,
            skip_disposition: None,
        }
    }

    pub(crate) fn with_sampling_request_signal(mut self, signal: JsonValue) -> Self {
        self.sampling_request_signal = Some(signal);
        self
    }

    pub(crate) fn with_canonical_body(
        mut self,
        content: Vec<FunctionCallOutputContentItem>,
    ) -> Self {
        self.canonical_body = Some(content);
        self
    }

    pub(crate) fn with_deterministic_continuation_receipt(
        mut self,
        receipt: TurnTimingDeterministicContinuationReceipt,
    ) -> Self {
        self.deterministic_continuation_receipts.push(receipt);
        self
    }

    pub(crate) fn with_deterministic_continuation_owner_key(mut self, owner_key: String) -> Self {
        self.deterministic_continuation_owner_key = Some(owner_key);
        self
    }

    pub(crate) fn with_skip_disposition(mut self, disposition: ToolOutputSkipDisposition) -> Self {
        self.outcome = Some(ToolOutputOutcome::Skipped);
        self.success = None;
        self.skip_disposition = Some(disposition);
        self
    }

    pub(crate) fn with_outcome(mut self, outcome: ToolOutputOutcome) -> Self {
        self.outcome = Some(outcome);
        self.success = match outcome {
            ToolOutputOutcome::Success => Some(true),
            ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => Some(false),
            ToolOutputOutcome::Yielded | ToolOutputOutcome::Skipped => None,
        };
        self
    }

    pub fn into_text(self) -> String {
        function_call_output_content_items_to_text(&self.body).unwrap_or_default()
    }
}

impl ToolOutput for FunctionToolOutput {
    fn log_preview(&self) -> String {
        telemetry_preview(
            &function_call_output_content_items_to_text(&self.body).unwrap_or_default(),
        )
    }

    fn success_for_logging(&self) -> bool {
        matches!(
            self.outcome_for_logging(),
            ToolOutputOutcome::Success | ToolOutputOutcome::Yielded
        )
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        if self.skip_disposition.is_some() {
            ToolOutputOutcome::Skipped
        } else if let Some(outcome) = self.outcome {
            outcome
        } else if self.success.unwrap_or(false) {
            ToolOutputOutcome::Success
        } else {
            ToolOutputOutcome::Failure
        }
    }

    fn outcome_context(&self) -> ToolOutputOutcomeContext {
        if self.outcome_for_logging() == ToolOutputOutcome::Skipped {
            ToolOutputOutcomeContext::skipped(self.skip_disposition)
        } else {
            ToolOutputOutcomeContext::new(self.outcome_for_logging())
        }
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        self.sampling_request_signal.clone()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.deterministic_continuation_receipts.clone()
    }

    fn deterministic_continuation_owner_key(&self) -> Option<String> {
        self.deterministic_continuation_owner_key.clone()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        #[cfg(test)]
        FUNCTION_PROJECTION_METADATA_CALLS.with(|calls| calls.set(calls.get() + 1));

        let model_success = if self.outcome.is_some() || self.skip_disposition.is_some() {
            Some(self.outcome_for_logging() == ToolOutputOutcome::Success)
        } else {
            self.success
        };
        Some(ToolOutputProjectionMetadata {
            outcome: self.outcome_for_logging(),
            diagnostic_class: ToolOutputDiagnosticClass::Normal,
            fragments: Vec::new(),
            spillable_text: self
                .canonical_body
                .as_ref()
                .unwrap_or(&self.body)
                .iter()
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => Some(text.clone()),
                    FunctionCallOutputContentItem::InputImage { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                })
                .collect(),
            essential_inline: {
                let mut essential = self.essential_inline.clone();
                essential.insert("success".to_string(), serde_json::json!(model_success));
                JsonValue::Object(essential)
            },
            requested_limit: None,
            predetermined_ranges: Vec::new(),
            predetermined_json_pointers: Vec::new(),
        })
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        let canonical_body = self.canonical_body.as_ref().unwrap_or(&self.body);
        match canonical_body.as_slice() {
            items
                if items.iter().all(|item| {
                    matches!(item, FunctionCallOutputContentItem::InputText { .. })
                }) =>
            {
                // Keep real line boundaries when command-state receipts or hook
                // feedback add text items. JSON-encoding the combined string
                // makes line/search recovery see an escaped one-line wrapper.
                Some(CanonicalToolResult::text(
                    items
                        .iter()
                        .filter_map(|item| match item {
                            FunctionCallOutputContentItem::InputText { text } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
            }
            _ => {
                let canonical_output = Self {
                    essential_inline: self.essential_inline.clone(),
                    body: canonical_body.clone(),
                    canonical_body: None,
                    success: self.success,
                    outcome: self.outcome,
                    post_tool_use_response: None,
                    sampling_request_signal: None,
                    deterministic_continuation_receipts: Vec::new(),
                    deterministic_continuation_owner_key: None,
                    skip_disposition: self.skip_disposition,
                };
                Some(CanonicalToolResult::json(
                    canonical_output.code_mode_result(payload),
                ))
            }
        }
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let success = if self.outcome.is_some() || self.skip_disposition.is_some() {
            Some(self.outcome_for_logging() == ToolOutputOutcome::Success)
        } else {
            self.success
        };
        let body = if !self.essential_inline.is_empty()
            && self
                .body
                .iter()
                .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
        {
            vec![FunctionCallOutputContentItem::InputText {
                text: function_call_output_content_items_to_text(&self.body).unwrap_or_default(),
            }]
        } else {
            self.body.clone()
        };
        function_tool_response(call_id, payload, body, success)
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        self.post_tool_use_response.clone()
    }
}

pub struct ApplyPatchToolOutput {
    pub text: String,
    pub success: bool,
    pub changes: Vec<JsonValue>,
    pub changes_exact: bool,
    pub environment_id: Option<String>,
    pub retry: Option<JsonValue>,
}

/// Text-only projections carry the retry receipt inline. The structured result
/// already has it as `retry`, so embedding it in `text` too would repeat it.
pub(crate) fn failed_patch_text(text: &str, retry: Option<&JsonValue>) -> String {
    match retry {
        Some(receipt) => format!("{text}\nRetained patch retry: {receipt}"),
        None => text.to_string(),
    }
}

impl ApplyPatchToolOutput {
    pub fn from_text(text: String) -> Self {
        Self::from_delta(text, true, &Default::default(), None)
    }

    pub(crate) fn from_delta(
        text: String,
        success: bool,
        delta: &codex_apply_patch::AppliedPatchDelta,
        environment_id: Option<String>,
    ) -> Self {
        use codex_apply_patch::AppliedPatchFileChange;
        let changes = delta
            .changes()
            .iter()
            .map(|applied| {
                let (kind, move_path) = match &applied.change {
                    AppliedPatchFileChange::Add { .. } => ("add", None),
                    AppliedPatchFileChange::Delete { .. } => ("delete", None),
                    AppliedPatchFileChange::Update { move_path, .. } => {
                        ("update", move_path.as_ref())
                    }
                };
                serde_json::json!({"path": applied.path, "kind": kind, "move_path": move_path})
            })
            .collect();
        Self {
            text,
            success,
            changes,
            changes_exact: delta.is_exact(),
            environment_id,
            retry: None,
        }
    }

    pub(crate) fn with_retry(mut self, retry: Option<JsonValue>) -> Self {
        self.retry = retry;
        self
    }

    fn model_text(&self) -> String {
        if self.success {
            "Success. Updated the files.".to_string()
        } else {
            failed_patch_text(&self.text, self.retry.as_ref())
        }
    }

    fn structured_result(&self) -> JsonValue {
        let mut result = serde_json::json!({
            "success": self.success,
            "text": self.text,
            "changes": self.changes,
            "changes_exact": self.changes_exact,
            "environment_id": self.environment_id,
        });
        if let Some(retry) = &self.retry {
            result["retry"] = retry.clone();
        }
        result
    }
}

impl ToolOutput for ApplyPatchToolOutput {
    fn code_mode_failure_is_error(&self) -> bool {
        // Patch failures are structured results so JavaScript can inspect the
        // applied delta and repair a retained patch without losing its receipt.
        false
    }

    fn log_preview(&self) -> String {
        telemetry_preview(&self.text)
    }

    fn success_for_logging(&self) -> bool {
        self.success
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(CanonicalToolResult::json(self.structured_result()))
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        Some(ToolOutputProjectionMetadata {
            outcome: if self.success {
                ToolOutputOutcome::Success
            } else {
                ToolOutputOutcome::Failure
            },
            diagnostic_class: ToolOutputDiagnosticClass::Normal,
            fragments: Vec::new(),
            spillable_text: vec![self.model_text()],
            essential_inline: serde_json::json!({
                "success": self.success,
                "changes_count": self.changes.len(),
                "changes_exact": self.changes_exact,
                "environment_id": self.environment_id,
                "retry": self.retry,
            }),
            requested_limit: None,
            predetermined_ranges: Vec::new(),
            // Canonical JSON indexes every change automatically when spilled.
            predetermined_json_pointers: Vec::new(),
        })
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.model_text(),
            }],
            Some(self.success),
        )
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(self.structured_result())
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.structured_result()
    }
}

pub struct AbortedToolOutput {
    pub message: String,
}

impl ToolOutput for AbortedToolOutput {
    fn log_preview(&self) -> String {
        telemetry_preview(&self.message)
    }

    fn success_for_logging(&self) -> bool {
        false
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        Some(ToolOutputProjectionMetadata {
            outcome: ToolOutputOutcome::Failure,
            diagnostic_class: ToolOutputDiagnosticClass::Normal,
            fragments: vec![ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
                self.message.clone(),
            )],
            spillable_text: vec![self.message.clone()],
            essential_inline: serde_json::json!({ "state": "aborted" }),
            requested_limit: None,
            predetermined_ranges: Vec::new(),
            predetermined_json_pointers: Vec::new(),
        })
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        Some(serde_json::json!({ "outcome": "recoverable_cancellation" }))
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        match payload {
            ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
                call_id: call_id.to_string(),
                status: "aborted".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
                omitted_result_count: None,
            },
            _ => function_tool_response(
                call_id,
                payload,
                vec![FunctionCallOutputContentItem::InputText {
                    text: self.message.clone(),
                }],
                /*success*/ None,
            ),
        }
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        match payload {
            ToolPayload::ToolSearch { .. } => {
                code_mode_tool_search_result(CodeModeToolSearchStatus::Aborted, Vec::new(), None)
            }
            _ => serde_json::json!({
                "status": "aborted",
                "message": self.message,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExecSessionCapabilities {
    pub stdin: bool,
    pub interrupt: bool,
    pub cancellation: bool,
    pub polling: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecCommandToolOutput {
    /// Bounded process-owned streams, separate from the display projection.
    pub(crate) process_output: Option<Arc<crate::unified_exec::ProcessOutputSnapshot>>,
    pub(crate) error: Option<String>,
    /// Model-declared attribution only; it does not establish successful coverage.
    pub validation: Option<crate::validation::CommandValidation>,
    pub event_call_id: String,
    pub chunk_id: String,
    pub wall_time: Duration,
    /// Raw bytes returned for this unified exec call before any truncation.
    pub raw_output: Vec<u8>,
    pub truncation_policy: TruncationPolicy,
    pub max_output_tokens: Option<usize>,
    pub process_id: Option<u32>,
    pub session_capabilities: Option<ExecSessionCapabilities>,
    pub exit_code: Option<i32>,
    /// Whether the process has exited, even if its output pipe is still draining
    /// or the platform did not provide an exit code.
    pub process_exited: bool,
    /// Exit 1 from a classified standalone search means no matches.
    pub search_no_match: bool,
    pub original_token_count: Option<usize>,
    pub hook_command: Option<String>,
    pub raw_output_artifact: Option<RawOutputArtifact>,
    pub repair_notice: Option<String>,
    /// Deferred results already in history that no request has carried yet.
    ///
    /// Carried on every poll while they are pending so an agent waiting on an
    /// unrelated process learns that the answer it deferred has arrived,
    /// instead of polling until its cell's hard deadline.
    pub pending_deferred_completions: Vec<String>,
}

impl ToolOutput for ExecCommandToolOutput {
    fn code_mode_failure_is_error(&self) -> bool {
        false
    }

    fn log_preview(&self) -> String {
        telemetry_preview(String::from_utf8_lossy(&self.raw_output).as_ref())
    }

    fn success_for_logging(&self) -> bool {
        self.outcome_for_logging() == ToolOutputOutcome::Success
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        if self.error.is_some()
            || (self.process_exited && self.exit_code != Some(0) && !self.search_no_match_is_success())
        {
            ToolOutputOutcome::Failure
        } else if self.process_id.is_some() {
            ToolOutputOutcome::Yielded
        } else if self.exit_code == Some(0) || self.search_no_match_is_success() {
            ToolOutputOutcome::Success
        } else {
            ToolOutputOutcome::Failure
        }
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        let outcome = self.outcome_for_logging();
        // Successful observations may be source text, diffs, or external data.
        // Diagnostic normalization must not erase their whitespace, values, or metadata.
        let semantic_evidence = if outcome == ToolOutputOutcome::Failure {
            failed_command_evidence(&self.raw_output, self.hook_command.as_deref())
        } else {
            successful_command_evidence(&self.raw_output, self.hook_command.as_deref())
        };
        let mut signal = match outcome {
            ToolOutputOutcome::Success => Some(serde_json::json!({
                "kind": "semantic_evidence",
                "semantic_evidence": semantic_evidence,
            })),
            ToolOutputOutcome::Failure => Some(serde_json::json!({
                "kind": "semantic_evidence",
                "outcome": "failure",
                "failure_signature": command_failure_signature(&semantic_evidence, self.exit_code),
                "semantic_evidence": semantic_evidence,
            })),
            ToolOutputOutcome::Yielded => Some(serde_json::json!({})),
            ToolOutputOutcome::TimedOut
            | ToolOutputOutcome::Skipped => None,
        }?;
        signal["process_observation_progress"] =
            serde_json::json!(!self.raw_output.is_empty() || self.process_exited);
        signal["empty_output"] = serde_json::json!(self.raw_output.is_empty());
        signal["command_evidence"] = serde_json::json!(true);
        if outcome == ToolOutputOutcome::Success
            && let Some(evidence) = declared_lineage_evidence(&self.raw_output)
        {
            signal["semantic_evidence"] = evidence;
        }
        if let Some(process_id) = self.process_id {
            signal["background_process_id"] = serde_json::json!(process_id);
        }
        let validation_output = self.process_output.as_ref()
            .map_or(self.raw_output.as_slice(), |output| output.aggregated_output.as_slice());
        attach_command_validation(&mut signal, validation_output, self.validation.as_ref(),
            self.exit_code, self.process_exited && self.error.is_none());
        Some(signal)
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(CanonicalToolResult::bytes(self.raw_output.clone()))
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        let raw_output = String::from_utf8_lossy(&self.raw_output);
        Some(self.projection_metadata_from_raw(raw_output.as_ref()))
    }

    fn to_response_item_with_projection_metadata(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> (ResponseInputItem, Option<ToolOutputProjectionMetadata>) {
        let raw_output = String::from_utf8_lossy(&self.raw_output);
        (
            function_tool_response(
                call_id,
                payload,
                vec![FunctionCallOutputContentItem::InputText {
                    text: self.response_text_from_raw(raw_output.as_ref()),
                }],
                Some(self.success_for_logging()),
            ),
            Some(self.projection_metadata_from_raw(raw_output.as_ref())),
        )
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let raw_output = String::from_utf8_lossy(&self.raw_output);
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.response_text_from_raw(raw_output.as_ref()),
            }],
            Some(self.success_for_logging()),
        )
    }

    fn post_tool_use_id(&self, call_id: &str) -> String {
        if self.event_call_id.is_empty() {
            call_id.to_string()
        } else {
            self.event_call_id.clone()
        }
    }

    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<JsonValue> {
        self.hook_command
            .as_ref()
            .map(|command| serde_json::json!({ "command": command }))
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        if self.process_id.is_some() || self.hook_command.is_none() {
            return None;
        }

        Some(JsonValue::String(
            self.truncated_output(self.model_output_max_tokens()),
        ))
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        #[derive(Serialize)]
        struct UnifiedExecCodeModeResult {
            #[serde(skip_serializing_if = "Option::is_none")]
            error: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            stdout: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            stderr: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            streams_complete: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            chunk_id: Option<String>,
            wall_time_seconds: f64,
            exit_code: Option<i32>,
            execution_state: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_id: Option<u32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_capabilities: Option<ExecSessionCapabilities>,
            process_exited: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            search_no_match: Option<bool>,
            output_complete: bool,
            output_reduced: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            original_token_count: Option<usize>,
            #[serde(skip_serializing_if = "Option::is_none")]
            original_token_count_is_approximate: Option<bool>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_output_artifact_id: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_output_artifact_bytes: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_output_artifact_error: Option<String>,
            raw_output_artifact_retention_limit_hit: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_output_artifact_retention_limit_reason: Option<&'static str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            repair: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            output_decoding_notice: Option<&'static str>,
            output: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            recovery_selector: Option<JsonValue>,
            #[serde(skip_serializing_if = "Option::is_none")]
            validation: Option<JsonValue>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            pending_deferred_completions: Vec<String>,
        }

        let (raw_output_artifact_id, raw_output_artifact_bytes, raw_output_artifact_error) =
            match self.raw_output_artifact.as_ref() {
                Some(artifact) => {
                    let (id, bytes, error) = artifact.model_projection();
                    (id.map(|id| id.to_string()), bytes, error)
                }
                None => (None, None, None),
            };
        // Output is this poll's chunk, including an empty terminal drain.
        // Cumulative exact streams are exposed separately as stdout/stderr;
        // replaying them here would duplicate bytes for polling consumers.
        let raw_output = String::from_utf8_lossy(&self.raw_output);
        // A script shows this result to the model only by printing it into
        // its cell. Bound it to fit there, so the command's own projection,
        // with its summary and artifact recovery, is the only reduction; the
        // direct response keeps the requested budget.
        let model_output = self.projected_model_output(
            raw_output.as_ref(),
            Some(codex_code_mode::MAX_NESTED_COMMAND_OUTPUT_TOKENS),
        );
        let output_reduced = model_output.reduced;
        let output = model_output.text;
        // Marked coordinates are exact even for repeated text; only unmarked
        // projections (summaries, outlined JSON) need text alignment.
        let recovery_selector = (raw_output_artifact_bytes == Some(self.raw_output.len() as u64))
            .then(|| {
                model_output.first_omitted_lines.or_else(|| {
                    codex_utils_output_truncation::first_omitted_line_range(&raw_output, &output)
                })
            })
            .flatten()
            .map(|(start, end)| serde_json::json!({"kind": "lines", "start": start, "end": end}))
            .or_else(|| {
                // Cumulative artifacts have no chunk-relative line mapping.
                // Supply a bounded, executable prefix selector rather than
                // inventing omitted coordinates or requiring discovery first.
                (output_reduced && raw_output_artifact_id.is_some())
                    .then_some(raw_output_artifact_bytes).flatten()
                    .filter(|bytes| *bytes > 0)
                    .map(|bytes| serde_json::json!({"kind": "bytes", "start": 0, "end": bytes.min(4096)}))
            });

        // Exact programmatic access does not inherit the display-token budget.
        // Never offer a truncated or lossy string as JSON. Larger/binary streams
        // remain unavailable here; the combined output artifact is still retained.
        let streams = self.process_output.as_ref().and_then(|snapshot| {
            (self.process_exited
                && self.process_id.is_none()
                && snapshot.streams_are_exact
                && snapshot.stdout.len().saturating_add(snapshot.stderr.len())
                    <= crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES)
                .then(|| {
                    Some((
                        String::from_utf8(snapshot.stdout.clone()).ok()?,
                        String::from_utf8(snapshot.stderr.clone()).ok()?,
                    ))
                })
                .flatten()
        });
        let streams_complete = Some(streams.is_some());
        let (stdout, stderr) = streams
            .map(|(stdout, stderr)| (Some(stdout), Some(stderr)))
            .unwrap_or_default();
        let result = UnifiedExecCodeModeResult {
            error: self
                .error
                .clone()
                .or_else(|| self.exit_code.and_then(windows_abnormal_exit_notice)),
            stdout,
            stderr,
            streams_complete,
            recovery_selector,
            chunk_id: (!self.chunk_id.is_empty()).then(|| self.chunk_id.clone()),
            wall_time_seconds: self.wall_time.as_secs_f64(),
            exit_code: self.exit_code,
            execution_state: self.execution_state(),
            session_id: self.process_id,
            session_capabilities: self.session_capabilities,
            process_exited: self.process_exited,
            search_no_match: self.search_no_match_is_success().then_some(true),
            output_complete: self.process_exited && self.process_id.is_none() && !output_reduced,
            output_reduced,
            original_token_count: self.original_token_count,
            original_token_count_is_approximate: self.original_token_count.map(|_| true),
            raw_output_artifact_id,
            raw_output_artifact_bytes,
            raw_output_artifact_error,
            raw_output_artifact_retention_limit_hit: self
                .raw_output_artifact
                .as_ref()
                .is_some_and(RawOutputArtifact::retention_limit_hit),
            raw_output_artifact_retention_limit_reason: self
                .raw_output_artifact
                .as_ref()
                .and_then(RawOutputArtifact::retention_limit_reason),
            repair: self.repair_notice.clone(),
            output_decoding_notice: self.output_decoding_notice(),
            validation: self.declared_validation_metadata(),
            output,
            pending_deferred_completions: self.pending_deferred_completions.clone(),
        };

        serde_json::to_value(result).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize exec result: {err}"))
        })
    }
}

pub(crate) fn semantic_evidence_for_command_output(raw_output: &[u8]) -> Vec<String> {
    let Ok(output) = std::str::from_utf8(raw_output) else {
        return canonical_output_evidence(raw_output);
    };
    let compiler_framing = output
        .lines()
        .any(|line| is_compiler_location_line(&strip_ansi_sequences(line)));
    let mut facts = Vec::new();
    let mut in_diff = false;
    let mut in_diff_hunk = false;
    for raw_line in output.lines() {
        let line = strip_ansi_sequences(raw_line);
        let line = line.trim_end();
        if line.starts_with("diff --git ") {
            in_diff = true;
            in_diff_hunk = false;
            continue;
        }
        if in_diff {
            if line.starts_with("@@") {
                in_diff_hunk = true;
                continue;
            }
            if line.starts_with("+++")
                || line.starts_with("---")
                || line.starts_with("index ")
                || (!in_diff_hunk && is_git_diff_metadata(line))
            {
                continue;
            }
            if in_diff_hunk {
                if let Some(changed_line) = line.strip_prefix('+') {
                    if let Some(fact) = normalize_semantic_fact_line(changed_line, false) {
                        facts.push(crate::tool_history::sha256(fact.as_bytes()));
                    }
                    continue;
                }
                if let Some(changed_line) = line.strip_prefix('-') {
                    if let Some(fact) = normalize_semantic_fact_line(changed_line, false) {
                        facts.push(crate::tool_history::sha256(
                            format!("removed:{fact}").as_bytes(),
                        ));
                    }
                    continue;
                }
                if let Some(context_line) = line.strip_prefix(' ') {
                    if let Some(fact) = normalize_semantic_fact_line(context_line, false) {
                        facts.push(crate::tool_history::sha256(
                            format!("context:{fact}").as_bytes(),
                        ));
                    }
                    continue;
                }
                if line.starts_with('\\') {
                    continue;
                }
            }
            in_diff = false;
            in_diff_hunk = false;
        }
        let diagnostic = normalize_command_diagnostic_line(line);
        if let Some(fact) = normalize_semantic_fact_line(&diagnostic, compiler_framing) {
            facts.push(crate::tool_history::sha256(fact.as_bytes()));
        }
    }
    if facts.is_empty() {
        return canonical_output_evidence(raw_output);
    }
    vec![format!(
        "command-facts-v1:{}",
        crate::tool_history::sha256(facts.join("\n").as_bytes())
    )]
}

pub(crate) fn semantic_evidence_sampling_signal(semantic_evidence: JsonValue) -> JsonValue {
    serde_json::json!({
        "kind": "semantic_evidence",
        "semantic_evidence": semantic_evidence,
    })
}

pub(crate) fn semantic_failure_sampling_signal(semantic_evidence: JsonValue) -> JsonValue {
    let failure_signature = format!(
        "tool-output-v1:{}",
        crate::tool_history::sha256(
            serde_json::to_vec(&semantic_evidence)
                .unwrap_or_default()
                .as_slice(),
        )
    );
    serde_json::json!({
        "kind": "semantic_evidence",
        "outcome": "failure",
        "failure_signature": failure_signature,
        "semantic_evidence": semantic_evidence,
    })
}

fn canonical_output_evidence(raw_output: &[u8]) -> Vec<String> {
    vec![format!(
        "canonical-output-v1:{}",
        crate::tool_history::sha256(raw_output)
    )]
}

pub(crate) fn declared_lineage_evidence(raw_output: &[u8]) -> Option<JsonValue> {
    declared_lineage_evidence_with_integrity(raw_output, false)
}

pub(crate) fn declared_file_lineage_evidence(raw_output: &[u8]) -> Option<JsonValue> {
    declared_lineage_evidence_with_integrity(raw_output, true)
}

fn declared_lineage_evidence_with_integrity(
    raw_output: &[u8],
    require_content_hash: bool,
) -> Option<JsonValue> {
    // Any producer whose whole output is a JSON object may declare the source,
    // optional scope, and identity it projects, so re-rendering retained
    // evidence is not counted as new source evidence. Attribution only, never
    // completion authority or permission to skip a command. Output cannot
    // claim harness-owned sources; undeclared output stays exact byte evidence.
    const KEY: &[u8] = b"\"evidence_lineage\"";
    if !raw_output.windows(KEY.len()).any(|window| window == KEY) {
        return None;
    }
    let mut body = None;
    let mut value: JsonValue = serde_json::from_slice(raw_output).ok().or_else(|| {
        // Human-readable reports carry one explicit header, not arbitrary JSON
        // found in quoted source or examples elsewhere in the document.
        let (header, contents) = std::str::from_utf8(raw_output).ok()?.split_once('\n')?;
        body = Some(contents.as_bytes());
        serde_json::from_str(header.strip_prefix("<!-- codex-evidence: ")?
            .strip_suffix(" -->")?).ok()
    })?;
    let lineage = value.as_object_mut()?.remove("evidence_lineage")?;
    if require_content_hash || lineage.get("content_sha256").is_some() {
        let expected = lineage.get("content_sha256")?.as_str()?;
        let actual = match body {
            Some(body) => crate::tool_history::sha256(body),
            None => {
                // File producers use compact UTF-8 JSON with recursively
                // sorted keys, excluding the top-level lineage metadata.
                value.sort_all_objects();
                crate::tool_history::sha256(&serde_json::to_vec(&value).ok()?)
            }
        };
        if expected != actual {
            return None;
        }
    }
    let source = lineage.get("source")?.as_str().filter(|source| {
        !source.is_empty() && !matches!(*source, "read_file" | "artifact" | "workspace-command")
    })?;
    let identity = lineage.get("identity").filter(|identity| !identity.is_null())?;
    Some(serde_json::json!({
        "source": source,
        "scope": lineage.get("scope").cloned().unwrap_or(JsonValue::Null),
        "identity": identity,
    }))
}

pub(crate) fn successful_command_evidence(raw_output: &[u8], command: Option<&str>) -> Vec<String> {
    // The executor owns the command string (also retained for live-process
    // polls). Only a recognized validation producer may normalize diagnostic
    // framing on success; ordinary reads still identify the exact source bytes.
    if command.is_some_and(|command| matches!(
        crate::validation::classify_validation_script(command),
        crate::validation::ValidationClassification::Validation { .. }
    )) {
        semantic_evidence_for_command_output(raw_output)
    } else {
        canonical_output_evidence(raw_output)
    }
}

pub(crate) fn failed_command_evidence(raw_output: &[u8], command: Option<&str>) -> Vec<String> {
    if command.is_some_and(|command| matches!(
        crate::validation::classify_validation_script(command),
        crate::validation::ValidationClassification::Validation { leaves, .. }
            if leaves.iter().any(|leaf| leaf.operation == crate::validation::ValidationOperation::Test)
    )) && let Some(evidence) = test_failure_evidence(raw_output) {
        return evidence;
    }
    semantic_evidence_for_command_output(raw_output)
}

fn test_failure_evidence(raw_output: &[u8]) -> Option<Vec<String>> {
    static FAILED_TEST: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(concat!(
            r"^(?:FAIL\s+\[\s*[^]]+\]\s+(?:\(\s*\d+/\d+\)\s+)?(?P<nextest>.+)",
            r"|test (?P<rust>.+) \.\.\. FAILED",
            r"|FAILED (?P<pytest>\S+)(?: - (?P<message>.*))?)$"
        )).expect("valid failed test regex")
    });
    static RUNNER_INDEX: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(r"\(\s*\d+/\d+\)\s*").expect("valid runner index regex")
    });
    let output = strip_ansi_sequences(std::str::from_utf8(raw_output).ok()?);
    let mut names = std::collections::BTreeSet::new();
    let mut diagnostics = Vec::<(String, Vec<String>)>::new();
    let mut current = None;
    let mut section = String::new();
    for line in output.lines() {
        if let Some(captures) = FAILED_TEST.captures(line.trim()) {
            let name = captures.name("nextest").or_else(|| captures.name("rust"))
                .or_else(|| captures.name("pytest"))?;
            names.insert(name.as_str().to_string());
            if let Some(message) = captures.name("message") {
                diagnostics.push((name.as_str().to_string(),
                    vec![normalize_test_diagnostic_line(message.as_str())]));
            }
            current = None;
            continue;
        }
        let normalized = normalize_test_diagnostic_line(line);
        let line = normalized.trim();
        if line.starts_with("---") || line.starts_with("___") {
            // nextest/libtest/pytest delimit diagnostic blocks. Retain the
            // owning test/binary rather than mixing messages across tests.
            section = RUNNER_INDEX.replace_all(line.trim_matches(['-', '_', ' ']), "").into_owned();
            current = None;
        } else if line.starts_with("thread '") && line.contains(" panicked at ") {
            let (owner, location) = line.split_once(" panicked at ")?;
            diagnostics.push((format!("{section}\n{owner}"), vec![location.to_string()]));
            current = Some(diagnostics.len() - 1);
        } else if line.starts_with("stack backtrace:")
            || line.starts_with("note: run with ")
            || line.starts_with("test result:")
            || line.starts_with("Summary ")
            || line.starts_with("failures:")
            || line.starts_with("error: test failed")
            || ["PASS ", "SKIP ", "TIMEOUT ", "test "].iter().any(|prefix| line.starts_with(prefix))
        {
            current = None;
        } else if !line.is_empty() {
            if let Some(index) = current {
                // Keep the whole assertion, including left/right values and
                // multiline messages. Only block order is insignificant.
                diagnostics[index].1.push(line.to_string());
            } else if line.starts_with("E ") {
                diagnostics.push((section.clone(), vec![line.to_string()]));
            }
        }
    }
    if names.is_empty() || diagnostics.is_empty() {
        return None;
    }
    diagnostics.sort();
    Some(vec![format!("test-failures-v2:{}", crate::tool_history::sha256(
        serde_json::to_vec(&(names, diagnostics)).ok()?.as_slice()
    ))])
}

fn normalize_test_diagnostic_line(line: &str) -> String {
    if ["left:", "right:", "expected:", "actual:"].iter()
        .any(|prefix| line.trim_start().starts_with(prefix))
    {
        return line.to_string();
    }
    static VOLATILE: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(concat!(
            r"(?P<duration>^\s*(?:FAIL|PASS|SKIP|TIMEOUT)\s+\[)\s*\d+(?:\.\d+)?s\]",
            r"|(?P<thread>^thread '[^']*') \(\d+\)",
            r"|(?P<id>\b(?:run[-_ ]id|log[-_ ]id)[=: ]+)[A-Za-z0-9_-]+"
        )).expect("valid test diagnostic regex")
    });
    VOLATILE.replace_all(&normalize_command_diagnostic_line(line),
        "${duration}${thread}${id}<volatile>").into_owned()
}

fn command_failure_signature(semantic_evidence: &[String], exit_code: Option<i32>) -> String {
    format!(
        "command-failure-v1:{}",
        crate::tool_history::sha256(
            format!("exit_code={exit_code:?}\n{}", semantic_evidence.join("\n")).as_bytes()
        )
    )
}

fn normalize_semantic_fact_line(line: &str, compiler_framing: bool) -> Option<String> {
    let mut line = line.trim();
    if line.is_empty() || (compiler_framing && is_compiler_location_line(line)) {
        return None;
    }
    if compiler_framing
        && let Some((prefix, body)) = line.split_once('|')
        && prefix.trim().parse::<u64>().is_ok()
    {
        line = body.trim();
    } else if let Some(body) = strip_location_prefix(line) {
        line = body.trim();
    }
    if line.is_empty() || (compiler_framing && is_compiler_marker_line(line)) {
        return None;
    }
    Some(line.to_string())
}

/// Normalize only recognizable producer framing. Durations in source, diffs,
/// application data, or assertion messages remain substantive evidence.
fn normalize_command_diagnostic_line(line: &str) -> String {
    static RUNNER_DURATION: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(concat!(
            r"^(?P<prefix>\s*(?:",
            r"test result: (?:ok|FAILED)\. .+; finished in ",
            r"|Finished (?:test|(?:`[^`]+`|\w+) profile.*) in ",
            r"|(?:=+\s*)?\d+ (?:passed|failed|skipped|error|errors|xfailed|xpassed)",
            r"(?:, \d+ (?:passed|failed|skipped|error|errors|xfailed|xpassed))* in ",
            r"|Summary \[\s*)",
            r")(?P<duration>\d+(?:\.\d+)?(?:ms|s| seconds?))",
            r"(?P<suffix>\s*(?:=+)?|\].*)$"
        )).expect("valid runner duration regex")
    });
    static LOG_TIMESTAMP: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(concat!(
            r"^(?P<prefix>\s*)",
            r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?",
            r"(?P<suffix>\s+(?:TRACE|DEBUG|INFO|WARN|ERROR|trace|debug|info|warn|error)\b.*)$"
        )).expect("valid diagnostic log prefix regex")
    });
    let line = RUNNER_DURATION.replace(line, "${prefix}<time>${suffix}");
    LOG_TIMESTAMP.replace(&line, "${prefix}<time>${suffix}").into_owned()
}

/// Remove volatile diagnostics, not substantive counts or error messages.
/// Shared by tool-error and stop-hook fingerprinting; command output uses
/// producer-specific framing above so source data retains its timing values.
pub(crate) fn normalize_observation_text(text: &str) -> String {
    static VOLATILE: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(concat!(
            r"(?ix)\b(?:",
            r"\d{4}-\d{2}-\d{2}[T\x20]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?",
            r"|\d{2}:\d{2}:\d{2}(?:\.\d+)?",
            r"|\d+(?:\.\d+)?\s*(?:ns|us|µs|ms|seconds?|secs?|s|minutes?|mins?|hours?|hrs?)\b",
            r")"
        )).expect("valid diagnostic normalization regex")
    });
    VOLATILE.replace_all(text, "<time>").into_owned()
}

pub(crate) fn normalize_tool_failure_text(text: &str) -> String {
    static LOCATION: std::sync::LazyLock<regex_lite::Regex> = std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(r"(?i)(:\d+(?::\d+)?|line \d+(?: column \d+)?)")
            .expect("valid diagnostic location regex")
    });
    LOCATION.replace_all(&normalize_observation_text(text), "<location>").into_owned()
}

fn is_compiler_location_line(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("--> ")
        .and_then(strip_location_prefix)
        .is_some()
}

fn is_compiler_marker_line(line: &str) -> bool {
    let marker = line.strip_prefix('|').unwrap_or(line).trim();
    !marker.is_empty()
        && marker
            .chars()
            .all(|character| matches!(character, '^' | '-' | '_' | '~'))
}

fn strip_location_prefix(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b':' {
            continue;
        }
        let digits_start = index + 1;
        let digits_end = bytes[digits_start..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map(|offset| digits_start + offset)
            .unwrap_or(bytes.len());
        if digits_end == digits_start || bytes.get(digits_end) != Some(&b':') {
            continue;
        }
        if !looks_like_source_path(&line[..index]) {
            continue;
        }
        return line.get(digits_end + 1..);
    }
    None
}

fn looks_like_source_path(prefix: &str) -> bool {
    let prefix = prefix.trim().to_ascii_lowercase();
    if prefix.contains("://") {
        return false;
    }
    let has_path_separator = prefix.contains('/') || prefix.contains('\\');
    let file_name = prefix.rsplit(['/', '\\']).next().unwrap_or(&prefix);
    matches!(file_name, "dockerfile" | "gemfile" | "makefile" | "readme")
        || file_name.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && !extension.is_empty()
                && extension.len() <= 16
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
                && (has_path_separator || is_common_source_extension(extension))
        })
}

fn is_common_source_extension(extension: &str) -> bool {
    matches!(
        extension,
        "bash"
            | "c"
            | "cc"
            | "cpp"
            | "cs"
            | "css"
            | "cxx"
            | "fish"
            | "go"
            | "h"
            | "hpp"
            | "html"
            | "java"
            | "js"
            | "json"
            | "jsonl"
            | "jsx"
            | "kt"
            | "kts"
            | "less"
            | "lock"
            | "md"
            | "php"
            | "proto"
            | "ps1"
            | "py"
            | "rb"
            | "rs"
            | "sass"
            | "scala"
            | "scss"
            | "sh"
            | "sql"
            | "swift"
            | "toml"
            | "ts"
            | "tsx"
            | "txt"
            | "xml"
            | "yaml"
            | "yml"
            | "zsh"
    )
}

fn is_git_diff_metadata(line: &str) -> bool {
    [
        "Binary files ",
        "GIT binary patch",
        "deleted file mode ",
        "dissimilarity index ",
        "new file mode ",
        "new mode ",
        "old mode ",
        "rename from ",
        "rename to ",
        "similarity index ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn strip_ansi_sequences(line: &str) -> String {
    let mut stripped = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            stripped.push(character);
            continue;
        }
        if characters.peek() != Some(&'[') {
            continue;
        }
        characters.next();
        for character in characters.by_ref() {
            if ('@'..='~').contains(&character) {
                break;
            }
        }
    }
    stripped
}

pub(crate) fn declared_validation_metadata(
    validation: &codex_protocol::validation::ValidationCommandContext,
) -> JsonValue {
    serde_json::json!({
        "covered_paths": validation.covered_paths,
        "coverage_status": "unverified",
    })
}

/// Only a repository-declared producer's typed completed-test ledger can establish test
/// execution. Plain command output, build success, and zero-test summaries do
/// not become proof. Compound/wrapper commands remain conservatively unproven.
fn runner_execution_receipt(raw_output: &[u8], trusted_runner: Option<&str>) -> Option<JsonValue> {
    let runner = trusted_runner.filter(|runner| !runner.is_empty())?;
    let text = std::str::from_utf8(raw_output).ok()?;
    let mut receipts = text.lines().filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .filter(|value| value["kind"] == "codex_test_execution_v1");
    let receipt = receipts.next()?;
    if receipts.next().is_some() || receipt["runner"] != runner
        || receipt["exit_code"].as_i64() != Some(0)
        || receipt["selected_targets"].as_array()?.is_empty()
        || receipt["selected_targets"].as_array()?.iter()
            .any(|target| target.as_str().is_none_or(str::is_empty))
        || !receipt["runner_input_fingerprint"].as_str()?
            .bytes().all(|byte| byte.is_ascii_hexdigit())
        || receipt["runner_input_fingerprint"].as_str()?.len() != 64
    {
        return None;
    }
    let mut executed = 0_u64;
    if let Some(manifest) = receipt.get("dependency_manifest") {
        // The instrumented runner records provenance, not hermeticity. Keep
        // the complete declaration with its receipt, but never let an opaque
        // script authorize replay or narrower freshness from its own claims.
        if manifest["version"] != 1
            || manifest["producer"] != runner
            || manifest["captured"] != "before_execution"
            || manifest["coverage"] != "declared_not_exhaustive"
            || manifest["automatic_replay_allowed"] != false
            || manifest["file_inputs"].as_array()?.len() > 256
            || manifest["source_roots"].as_array()?.len() > 256
            || manifest["execution_context_sha256"].as_str()?.len() != 64
            || !manifest["execution_context_sha256"].as_str()?
                .bytes().all(|byte| byte.is_ascii_hexdigit())
            || manifest["unresolved_dependencies"].as_array()?.is_empty()
        {
            return None;
        }
    }
    for tests in receipt["completed_tests"].as_object()?.values() {
        let tests = tests.as_array()?;
        let names = tests.iter().map(JsonValue::as_str).collect::<Option<std::collections::BTreeSet<_>>>()?;
        if names.len() != tests.len() || names.iter().any(|name| name.is_empty()) {
            return None;
        }
        executed = executed.checked_add(tests.len() as u64)?;
    }
    (executed > 0 && receipt["executed_tests"].as_u64() == Some(executed)).then_some(receipt)
}

pub(crate) fn attach_command_validation(
    signal: &mut JsonValue,
    raw_output: &[u8],
    validation: Option<&crate::validation::CommandValidation>,
    exit_code: Option<i32>,
    terminal: bool,
) {
    let Some(validation) = validation else { return };
    signal["command_validation"] = validation.signal();
    if !terminal || !validation.is_validation() {
        return;
    }
    let failed = exit_code != Some(0);
    let evidence = if failed && validation.is_test() {
        test_failure_evidence(raw_output).unwrap_or_else(|| {
            let normalized = String::from_utf8_lossy(raw_output).lines()
                .map(normalize_test_diagnostic_line).collect::<Vec<_>>().join("\n");
            semantic_evidence_for_command_output(normalized.as_bytes())
        })
    } else {
        semantic_evidence_for_command_output(raw_output)
    };
    signal["semantic_evidence"] = serde_json::json!(evidence);
    if failed {
        signal["failure_signature"] = serde_json::json!(command_failure_signature(&evidence, exit_code));
    } else {
        if let Some(lineage) = declared_lineage_evidence(raw_output) {
            signal["semantic_evidence"] = lineage;
        }
        if validation.is_test()
            && let Some(receipt) = runner_execution_receipt(raw_output, validation.receipt_runner.as_deref())
        {
            signal["runner_execution_receipt"] = receipt;
        }
    }
}

impl ExecCommandToolOutput {
    fn search_no_match_is_success(&self) -> bool {
        // PowerShell can map native rg errors to exit 1 too. Classification of
        // the command alone cannot turn its stderr (or lost streams) into a
        // successful empty search. Use the process-owned cumulative evidence.
        self.search_no_match && self.exit_code == Some(1) && self.error.is_none()
            && self.process_exited && self.process_id.is_none()
            && self.process_output.as_ref().is_some_and(|snapshot| {
                snapshot.streams_are_exact && snapshot.stderr.is_empty()
            })
    }

    fn execution_state(&self) -> &'static str {
        if self.process_exited {
            "exited"
        } else if self.process_id.is_some() {
            "running"
        } else {
            "unknown"
        }
    }

    fn output_decoding_notice(&self) -> Option<&'static str> {
        std::str::from_utf8(&self.raw_output).err().map(|_| {
            "Output contained invalid UTF-8 bytes, which were replaced with U+FFFD. The displayed text is not byte-exact."
        })
    }

    fn declared_validation_metadata(&self) -> Option<JsonValue> {
        self.validation.as_ref().and_then(|validation| validation.declared.as_ref())
            .map(declared_validation_metadata)
    }

    fn projection_metadata_from_raw(&self, raw_output: &str) -> ToolOutputProjectionMetadata {
        let (raw_output_artifact_id, raw_output_artifact_bytes, raw_output_artifact_error) = self
            .raw_output_artifact
            .as_ref()
            .map_or((None, None, None), |artifact| {
                let (id, bytes, error) = artifact.model_projection();
                (id.map(|id| id.to_string()), bytes, error)
            });
        let raw_output_artifact_retention_limit_hit = self
            .raw_output_artifact
            .as_ref()
            .is_some_and(RawOutputArtifact::retention_limit_hit);
        let raw_output_artifact_retention_limit_reason = self
            .raw_output_artifact
            .as_ref()
            .and_then(RawOutputArtifact::retention_limit_reason);
        let outcome = self.outcome_for_logging();
        let mut fragments = vec![
            ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                format!(
                    "process final status: exit_code={:?}, session_id={:?}",
                    self.exit_code, self.process_id,
                ),
            )
            .with_id("process_status"),
        ];
        if let Some(repair_notice) = &self.repair_notice {
            fragments.push(
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
                    repair_notice.clone(),
                )
                .with_id("repair_notice"),
            );
        }
        if let Some(notice) = self.output_decoding_notice() {
            fragments.push(
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
                    notice.to_string(),
                )
                .with_id("output_decoding_notice"),
            );
        }
        let summary = self.summarized_output(raw_output, self.model_output_limits(raw_output, None).applied_limit);
        fragments.push(
            ToolOutputProjectionFragment::new(
                if summary.is_some() {
                    ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary
                } else {
                    ToolOutputProjectionFragmentKind::ContextualSpillableText
                },
                summary.clone().unwrap_or_else(|| raw_output.replace("\r\n", "\n")),
            )
            .with_id("output"),
        );
        ToolOutputProjectionMetadata {
            outcome,
            diagnostic_class: match classify_diagnostic(self.hook_command.as_deref(), raw_output) {
                codex_utils_output_truncation::OutputDiagnosticClass::Normal => {
                    ToolOutputDiagnosticClass::Normal
                }
                codex_utils_output_truncation::OutputDiagnosticClass::HighSignal => {
                    ToolOutputDiagnosticClass::HighSignal
                }
            },
            fragments,
            // Give the shared projection boundary the authoritative output so
            // it applies the model budget exactly once. The process status is
            // already preserved below as essential inline metadata, and the
            // existing artifact ID lets the boundary reuse the raw artifact.
            spillable_text: vec![raw_output.replace("\r\n", "\n")],
            essential_inline: {
                let mut metadata = serde_json::json!({
                "chunk_id": &self.chunk_id,
                "exit_code": self.exit_code,
                "session_id": self.process_id,
                "session_capabilities": self.session_capabilities,
                "process_exited": self.process_exited,
                "execution_state": self.execution_state(),
                "wall_time_seconds": self.wall_time.as_secs_f64(),
                "original_token_count": self.original_token_count,
                "repair_notice": &self.repair_notice,
                "raw_output_artifact_id": raw_output_artifact_id,
                "raw_output_artifact_bytes": raw_output_artifact_bytes,
                "raw_output_artifact_error": raw_output_artifact_error,
                "raw_output_artifact_retention_limit_hit": raw_output_artifact_retention_limit_hit,
                "raw_output_artifact_retention_limit_reason": raw_output_artifact_retention_limit_reason,
                });
                if let Some(validation) = self.declared_validation_metadata() {
                    metadata["validation"] = validation;
                }
                if let Some(error) = &self.error {
                    metadata["error"] = JsonValue::String(error.clone());
                }
                if self.original_token_count.is_some() {
                    metadata["original_token_count_is_approximate"] = JsonValue::Bool(true);
                }
                if let Some(notice) = self.output_decoding_notice() {
                    metadata["output_decoding_notice"] = JsonValue::String(notice.to_string());
                }
                metadata
            },
            requested_limit: self.requested_model_output_tokens(),
            predetermined_ranges: if summary.is_some() {
                Vec::new()
            } else {
                predetermined_validation_ranges(raw_output, self.hook_command.as_deref())
            },
            predetermined_json_pointers: Vec::new(),
        }
    }

    fn requested_model_output_tokens(&self) -> Option<usize> {
        self.max_output_tokens.or_else(|| {
            self.hook_command
                .as_deref()
                .and_then(crate::tools::shell_output_summary::source_read_output_budget)
        })
    }

    /// `hard_limit_cap` bounds the projection for a consumer whose own output
    /// ceiling is below the command ceiling.
    fn model_output_limits(
        &self,
        raw_output: &str,
        hard_limit_cap: Option<usize>,
    ) -> OutputLimitResolution {
        let outcome = match self.outcome_for_logging() {
            ToolOutputOutcome::Success | ToolOutputOutcome::Yielded => OutputOutcome::Success,
            ToolOutputOutcome::Failure => OutputOutcome::Failure,
            ToolOutputOutcome::TimedOut => OutputOutcome::TimedOut,
            ToolOutputOutcome::Skipped => OutputOutcome::Skipped,
        };
        let hard_limit = self.truncation_policy.token_budget().min(10_000);
        resolve_projected_output_limits(
            self.requested_model_output_tokens(),
            outcome,
            classify_diagnostic(self.hook_command.as_deref(), raw_output),
            hard_limit_cap.map_or(hard_limit, |cap| hard_limit.min(cap)),
        )
    }

    fn model_output_max_tokens(&self) -> usize {
        let raw = String::from_utf8_lossy(&self.raw_output);
        self.model_output_limits(raw.as_ref(), None).applied_limit
    }

    pub(crate) fn truncated_output(&self, max_tokens: usize) -> String {
        let text = String::from_utf8_lossy(&self.raw_output).to_string();
        formatted_truncate_text(&text, TruncationPolicy::Tokens(max_tokens))
    }

    fn summarized_output(&self, raw_output: &str, token_limit: usize) -> Option<String> {
        // A maximum output budget is not a target for successful validation.
        // Compact only terminal validation with recoverable raw bytes; the
        // authoritative streams and execution-evidence parser stay unchanged.
        let compact_validation = self.process_id.is_none()
            && self.process_exited
            && self.exit_code == Some(0)
            && self.validation.as_ref().is_some_and(|validation| validation.is_validation())
            && self.raw_output_artifact.as_ref().is_some_and(|artifact| {
                artifact.model_projection().0.is_some()
                    && artifact.retained_bytes().is_some_and(|bytes| bytes >= raw_output.len() as u64)
            });
        if compact_validation && raw_output.len() > 2_048
            && let Some(mut receipt) = runner_execution_receipt(
                raw_output.as_bytes(),
                self.validation.as_ref().and_then(|validation| validation.receipt_runner.as_deref()),
            )
            && receipt["completed_tests"].to_string().len() > 2_048
        {
            let groups = receipt["completed_tests"].as_object()?.len();
            receipt["kind"] = serde_json::json!("codex_test_execution_summary_v1");
            receipt["completed_tests"] = serde_json::json!({
                "groups": groups,
                "tests": receipt["executed_tests"],
                "detail": "exact test names retained in raw output; this display is not an execution receipt",
            });
            let compact = receipt.to_string();
            return Some(raw_output.lines().map(|line| {
                if serde_json::from_str::<JsonValue>(line).ok()
                    .is_some_and(|value| value["kind"] == "codex_test_execution_v1")
                {
                    compact.as_str()
                } else {
                    line
                }
            }).collect::<Vec<_>>().join("\n"));
        }
        if !compact_validation && codex_utils_string::approx_token_count(raw_output) <= token_limit {
            return None;
        }
        match (self.process_id, self.exit_code) {
            (None, Some(exit_code)) => summarize_shell_output_for_model(
                raw_output,
                exit_code,
                false,
                ShellOutputSummaryOptions {
                    enabled: true,
                    applied_token_limit: Some(token_limit),
                    command_text: self.hook_command.as_deref(),
                    launched_as_validation: self
                        .validation
                        .as_ref()
                        .is_some_and(|validation| validation.is_validation()),
                },
            ),
            _ => None,
        }
    }

    fn projected_model_output(
        &self,
        raw_output: &str,
        hard_limit_cap: Option<usize>,
    ) -> ProjectedModelOutput {
        let source_bytes = raw_output.len() as u64;
        // Normalize only the model projection; canonical artifacts retain exact bytes.
        let normalized;
        let raw_output = if raw_output.contains("\r\n") {
            normalized = raw_output.replace("\r\n", "\n");
            normalized.as_str()
        } else {
            raw_output
        };
        let limits = self.model_output_limits(raw_output, hard_limit_cap);
        let summarized = self.summarized_output(raw_output, limits.applied_limit);
        let artifact_has_more_bytes = self
            .raw_output_artifact
            .as_ref()
            .and_then(RawOutputArtifact::retained_bytes)
            .is_some_and(|bytes| bytes > source_bytes);
        let (truncated, first_omitted_lines) = match summarized.as_deref() {
            // Line coordinates are recovery references only when the text is
            // the retained source itself, not a summary or one chunk of a
            // longer cumulative artifact.
            None if !artifact_has_more_bytes => {
                let marked = formatted_truncate_text_with_line_markers(raw_output, limits);
                let first_omitted_lines = marked.first_omitted_line_range();
                (marked.output, first_omitted_lines)
            }
            content => (
                formatted_truncate_text_with_output_limit(content.unwrap_or(raw_output), limits),
                None,
            ),
        };
        let was_truncated = truncated.was_truncated;
        let mut projected_text = truncated.text;
        if summarized.is_some()
            && !was_truncated
            && let Some(original_tokens) = self.original_token_count
        {
            let marker =
                format!("Warning: output summarized from approximately {original_tokens} tokens");
            let candidate = format!("{marker}\n{projected_text}");
            // The summary already identifies itself. An optional notice must not
            // displace useful output that fits the caller's budget.
            if codex_utils_string::approx_token_count(&candidate) <= limits.applied_limit {
                projected_text = candidate;
            }
        }
        ProjectedModelOutput {
            reduced: summarized.is_some() || was_truncated || artifact_has_more_bytes,
            text: projected_text,
            first_omitted_lines,
        }
    }

    #[cfg(test)]
    pub(crate) fn response_text(&self) -> String {
        let raw_output = String::from_utf8_lossy(&self.raw_output);
        self.response_text_from_raw(raw_output.as_ref())
    }

    fn response_text_from_raw(&self, raw_output: &str) -> String {
        #[cfg(test)]
        EXEC_COMMAND_RESPONSE_MATERIALIZATIONS.with(|calls| calls.set(calls.get() + 1));

        let projected = self.projected_model_output(raw_output, None);
        let mut fields = serde_json::Map::new();
        if let Some(code) = self.exit_code {
            fields.insert("exit_code".into(), code.into());
        }
        if let Some(id) = self.process_id {
            fields.insert("session_id".into(), id.into());
            // write_stdin's contract tells callers to inspect these before
            // sending input, interrupting, or polling a live session.
            if let Some(capabilities) = self.session_capabilities {
                fields.insert("session_capabilities".into(), serde_json::json!(capabilities));
            }
        }
        if projected.reduced
            && let Some(id) = self
                .raw_output_artifact
                .as_ref()
                .and_then(|a| a.model_projection().0)
        {
            fields.insert("artifact_id".into(), id.to_string().into());
        }
        let mut text = projected.text;
        if self.process_exited && self.exit_code.is_none() {
            text.push_str("\nProcess exited without an available exit code");
        }
        if let Some(notice) = self.exit_code.and_then(windows_abnormal_exit_notice) {
            text.push_str(&format!("\n{notice}"));
        }
        if let Some(repair) = &self.repair_notice {
            text.push_str(&format!("\n{repair}"));
        }
        if let Some(notice) = self.output_decoding_notice() {
            text.push_str(&format!("\n{notice}"));
        }
        fields.insert("output".into(), text.into());
        if let Some(validation) = self.declared_validation_metadata() {
            fields.insert("validation".into(), validation);
        }
        JsonValue::Object(fields).to_string()
    }

    #[cfg(test)]
    pub(crate) fn reset_response_materialization_count() {
        EXEC_COMMAND_RESPONSE_MATERIALIZATIONS.with(|calls| calls.set(0));
    }

    #[cfg(test)]
    pub(crate) fn response_materialization_count() -> usize {
        EXEC_COMMAND_RESPONSE_MATERIALIZATIONS.with(std::cell::Cell::get)
    }
}

/// Windows reports an abnormal termination as an NTSTATUS exit code, which reads
/// like an ordinary failure status unless it is named.
pub(crate) fn windows_abnormal_exit_notice(exit_code: i32) -> Option<String> {
    let status = u32::from_ne_bytes(exit_code.to_ne_bytes());
    let (name, meaning) = match status {
        0xC000_0005 => ("STATUS_ACCESS_VIOLATION", "invalid memory access"),
        0xC000_001D => ("STATUS_ILLEGAL_INSTRUCTION", "illegal CPU instruction"),
        0xC000_0094 => ("STATUS_INTEGER_DIVIDE_BY_ZERO", "integer division by zero"),
        0xC000_00FD => ("STATUS_STACK_OVERFLOW", "stack overflow"),
        0xC000_0135 => ("STATUS_DLL_NOT_FOUND", "a required DLL was not found"),
        0xC000_0142 => ("STATUS_DLL_INIT_FAILED", "DLL initialization failed"),
        0xC000_0374 => ("STATUS_HEAP_CORRUPTION", "heap corruption"),
        0xC000_0409 => ("STATUS_STACK_BUFFER_OVERRUN", "fail-fast abort or stack buffer overrun"),
        0x8000_0003 => ("STATUS_BREAKPOINT", "breakpoint or debug assertion"),
        _ => return None,
    };
    Some(format!(
        "environment_crash: exit code {exit_code} is Windows status 0x{status:08X} {name} ({meaning}): the process crashed rather than reporting a failure."
    ))
}

fn predetermined_validation_ranges(
    raw_output: &str,
    command_text: Option<&str>,
) -> Vec<ToolOutputProjectionRange> {
    if classify_diagnostic(command_text, raw_output)
        != codex_utils_output_truncation::OutputDiagnosticClass::HighSignal
    {
        return Vec::new();
    }
    let total_lines = raw_output.lines().count();
    if total_lines <= 200 {
        return Vec::new();
    }
    crate::tools::handlers::validation_diagnostic_range(
        "validation:diagnostics",
        raw_output.as_bytes(),
    )
    .into_iter()
    .collect()
}

struct ProjectedModelOutput {
    text: String,
    reduced: bool,
    /// Bounded first run the text's line markers report omitted from the raw output.
    first_omitted_lines: Option<(usize, usize)>,
}

fn function_tool_response(
    call_id: &str,
    payload: &ToolPayload,
    body: Vec<FunctionCallOutputContentItem>,
    success: Option<bool>,
) -> ResponseInputItem {
    let body = match body.as_slice() {
        [FunctionCallOutputContentItem::InputText { text }] => {
            FunctionCallOutputBody::Text(text.clone())
        }
        _ => FunctionCallOutputBody::ContentItems(body),
    };

    if matches!(payload, ToolPayload::Custom { .. }) {
        return ResponseInputItem::CustomToolCallOutput {
            call_id: call_id.to_string(),
            name: None,
            output: FunctionCallOutputPayload { body, success },
        };
    }

    ResponseInputItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload { body, success },
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
