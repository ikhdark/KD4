use crate::context_manager::truncate_function_output_payload;
use crate::original_image_detail::sanitize_original_image_detail;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::TELEMETRY_PREVIEW_MAX_BYTES;
use crate::tools::TELEMETRY_PREVIEW_MAX_LINES;
use crate::tools::TELEMETRY_PREVIEW_TRUNCATION_NOTICE;
use crate::tools::command_output_artifact::RawOutputArtifact;
#[cfg(test)]
use crate::tools::command_output_artifact::ToolOutputArtifactId;
use crate::tools::shell_output_summary::ShellOutputSummaryOptions;
use crate::tools::shell_output_summary::summarize_shell_output_for_model;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::CanonicalToolResult;
use codex_tools::CodeModeToolSearchStatus;
use codex_tools::LoadableToolSpec;
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
use codex_utils_output_truncation::OutputLimitResolution;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::classify_diagnostic;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_output_truncation::formatted_truncate_text_with_output_limit;
use codex_utils_output_truncation::resolve_projected_output_limits;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use codex_utils_string::take_bytes_at_char_boundary;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub use codex_tools::ToolOutput;
pub use codex_tools::ToolPayload;

pub(crate) fn boxed_tool_output<T>(output: T) -> Box<dyn ToolOutput>
where
    T: ToolOutput + 'static,
{
    Box::new(output)
}

pub type SharedTurnDiffTracker = Arc<Mutex<TurnDiffTracker>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCallSource {
    Direct,
    CodeMode {
        /// Runtime cell that issued the nested tool request.
        cell_id: String,
        /// Code-mode's per-cell tool invocation id. This is useful for
        /// debugging the JS/runtime bridge, but it is not the Codex tool call id
        /// because the runtime id only needs to be unique within one cell.
        runtime_tool_call_id: String,
    },
}

#[derive(Clone)]
pub struct ToolInvocation {
    pub session: Arc<Session>,
    // TODO(sayan): Remove this compatibility field once handlers use `step_context.turn`.
    pub turn: Arc<TurnContext>,
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
    pub result: CallToolResult,
    pub tool_input: JsonValue,
    pub wall_time: Duration,
    pub original_image_detail_supported: bool,
    pub truncation_policy: TruncationPolicy,
}

impl ToolOutput for McpToolOutput {
    fn log_preview(&self) -> String {
        let payload = self.response_payload();
        let preview = payload.body.to_text().unwrap_or_else(|| {
            serde_json::to_string(&self.result.content)
                .unwrap_or_else(|err| format!("failed to serialize mcp result: {err}"))
        });
        telemetry_preview(&preview)
    }

    fn success_for_logging(&self) -> bool {
        self.result.success()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        ToolOutput::projection_metadata(&self.result)
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        serde_json::to_value(&self.result)
            .ok()
            .map(CanonicalToolResult::json)
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: self.response_payload(),
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        serde_json::to_value(&self.result).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize mcp result: {err}"))
        })
    }

    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(self.tool_input.clone())
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        serde_json::to_value(&self.result).ok()
    }
}

impl McpToolOutput {
    fn response_payload(&self) -> FunctionCallOutputPayload {
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
}

#[derive(Clone)]
pub struct ToolSearchOutput {
    pub tools: Vec<LoadableToolSpec>,
    pub omitted_result_count: usize,
}

impl ToolOutput for ToolSearchOutput {
    fn log_preview(&self) -> String {
        let tools = self
            .tools
            .iter()
            .map(|tool| {
                serde_json::to_value(tool).unwrap_or_else(|err| {
                    JsonValue::String(format!("failed to serialize tool_search output: {err}"))
                })
            })
            .collect();
        telemetry_preview(&JsonValue::Array(tools).to_string())
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
            tools: self
                .tools
                .iter()
                .map(|tool| {
                    serde_json::to_value(tool).unwrap_or_else(|err| {
                        JsonValue::String(format!("failed to serialize tool_search output: {err}"))
                    })
                })
                .collect(),
            omitted_result_count: Some(self.omitted_result_count),
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        let status = if self.omitted_result_count == 0 {
            CodeModeToolSearchStatus::Completed
        } else {
            CodeModeToolSearchStatus::Incomplete
        };
        code_mode_tool_search_result(
            status,
            self.tools
                .iter()
                .map(|tool| {
                    serde_json::to_value(tool).unwrap_or_else(|err| {
                        JsonValue::String(format!("failed to serialize tool_search output: {err}"))
                    })
                })
                .collect(),
            Some(self.omitted_result_count),
        )
    }
}

pub struct FunctionToolOutput {
    pub body: Vec<FunctionCallOutputContentItem>,
    pub success: Option<bool>,
    pub outcome: Option<ToolOutputOutcome>,
    pub post_tool_use_response: Option<JsonValue>,
    /// Private signal consumed by the request-local reasoning governor. This is
    /// never included in the model-facing tool result or public protocol.
    pub sampling_request_signal: Option<JsonValue>,
    pub deterministic_continuation_receipts: Vec<TurnTimingDeterministicContinuationReceipt>,
    pub deterministic_continuation_owner_key: Option<String>,
    pub skip_disposition: Option<ToolOutputSkipDisposition>,
}

impl FunctionToolOutput {
    pub fn from_text(text: String, success: Option<bool>) -> Self {
        Self {
            body: vec![FunctionCallOutputContentItem::InputText { text }],
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
            body: content,
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
            ToolOutputOutcome::Skipped => None,
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
        self.outcome_for_logging() == ToolOutputOutcome::Success
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        if self.skip_disposition.is_some() {
            ToolOutputOutcome::Skipped
        } else if let Some(outcome) = self.outcome {
            outcome
        } else if self.success.unwrap_or(true) {
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
                .body
                .iter()
                .filter_map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => Some(text.clone()),
                    FunctionCallOutputContentItem::InputImage { .. }
                    | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
                })
                .collect(),
            essential_inline: serde_json::json!({ "success": model_success }),
            requested_limit: None,
            predetermined_ranges: Vec::new(),
            predetermined_json_pointers: Vec::new(),
        })
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let success = if self.outcome.is_some() || self.skip_disposition.is_some() {
            Some(self.outcome_for_logging() == ToolOutputOutcome::Success)
        } else {
            self.success
        };
        function_tool_response(call_id, payload, self.body.clone(), success)
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        self.post_tool_use_response.clone()
    }
}

pub struct ApplyPatchToolOutput {
    pub text: String,
}

impl ApplyPatchToolOutput {
    pub fn from_text(text: String) -> Self {
        Self { text }
    }
}

impl ToolOutput for ApplyPatchToolOutput {
    fn log_preview(&self) -> String {
        telemetry_preview(&self.text)
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        Some(ToolOutputProjectionMetadata {
            outcome: ToolOutputOutcome::Success,
            diagnostic_class: ToolOutputDiagnosticClass::Normal,
            fragments: Vec::new(),
            spillable_text: vec![self.text.clone()],
            essential_inline: JsonValue::Object(serde_json::Map::new()),
            requested_limit: None,
            predetermined_ranges: Vec::new(),
            predetermined_json_pointers: Vec::new(),
        })
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.text.clone(),
            }],
            Some(true),
        )
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(JsonValue::String(self.text.clone()))
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        JsonValue::Object(serde_json::Map::new())
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

#[derive(Debug, Clone, PartialEq)]
pub struct ExecCommandToolOutput {
    pub event_call_id: String,
    pub chunk_id: String,
    pub wall_time: Duration,
    /// Raw bytes returned for this unified exec call before any truncation.
    pub raw_output: Vec<u8>,
    pub truncation_policy: TruncationPolicy,
    pub max_output_tokens: Option<usize>,
    pub process_id: Option<i32>,
    pub exit_code: Option<i32>,
    pub original_token_count: Option<usize>,
    pub hook_command: Option<String>,
    pub raw_output_artifact: Option<RawOutputArtifact>,
    pub repair_notice: Option<String>,
}

impl ToolOutput for ExecCommandToolOutput {
    fn log_preview(&self) -> String {
        telemetry_preview(&self.response_text())
    }

    fn success_for_logging(&self) -> bool {
        self.outcome_for_logging() == ToolOutputOutcome::Success
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        if self.process_id.is_some() {
            ToolOutputOutcome::TimedOut
        } else if self.exit_code.is_some_and(|code| code != 0) {
            ToolOutputOutcome::Failure
        } else {
            ToolOutputOutcome::Success
        }
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(CanonicalToolResult::bytes(self.raw_output.clone()))
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        let raw_output = String::from_utf8_lossy(&self.raw_output).to_string();
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
        let response_text = self.response_text();
        Some(ToolOutputProjectionMetadata {
            outcome,
            diagnostic_class: match classify_diagnostic(self.hook_command.as_deref(), &raw_output) {
                codex_utils_output_truncation::OutputDiagnosticClass::Normal => {
                    ToolOutputDiagnosticClass::Normal
                }
                codex_utils_output_truncation::OutputDiagnosticClass::HighSignal => {
                    ToolOutputDiagnosticClass::HighSignal
                }
            },
            fragments: vec![
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ProcessFinalStatus,
                    format!(
                        "process final status: exit_code={:?}, session_id={:?}",
                        self.exit_code, self.process_id
                    ),
                ),
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::ContextualSpillableText,
                    response_text.clone(),
                ),
            ],
            // Unified exec has already sent the exact raw bytes through the
            // existing artifact path. Project its current model text here so
            // the common boundary does not create a duplicate raw artifact.
            spillable_text: vec![response_text],
            essential_inline: serde_json::json!({
                "chunk_id": &self.chunk_id,
                "exit_code": self.exit_code,
                "session_id": self.process_id,
                "original_token_count": self.original_token_count,
                "raw_output_artifact_id": raw_output_artifact_id,
                "raw_output_artifact_bytes": raw_output_artifact_bytes,
                "raw_output_artifact_error": raw_output_artifact_error,
                "raw_output_artifact_retention_limit_hit": raw_output_artifact_retention_limit_hit,
                "raw_output_artifact_retention_limit_reason": raw_output_artifact_retention_limit_reason,
            }),
            requested_limit: self.max_output_tokens,
            predetermined_ranges: predetermined_validation_ranges(
                &raw_output,
                self.hook_command.as_deref(),
            ),
            predetermined_json_pointers: Vec::new(),
        })
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.response_text(),
            }],
            Some(self.outcome_for_logging() == ToolOutputOutcome::Success),
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
            chunk_id: Option<String>,
            wall_time_seconds: f64,
            #[serde(skip_serializing_if = "Option::is_none")]
            exit_code: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_id: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            original_token_count: Option<usize>,
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
            output: String,
        }

        let (raw_output_artifact_id, raw_output_artifact_bytes, raw_output_artifact_error) =
            match self.raw_output_artifact.as_ref() {
                Some(artifact) => {
                    let (id, bytes, error) = artifact.model_projection();
                    (id.map(|id| id.to_string()), bytes, error)
                }
                None => (None, None, None),
            };
        let model_output = self.projected_model_output();
        let output = self.output_with_reduction_notice(model_output);
        let output = if output.is_empty() {
            match (self.exit_code, self.process_id) {
                (Some(exit_code), _) => {
                    format!("Command completed with no output (exit code {exit_code}).")
                }
                (None, Some(_)) => {
                    "Command is still running and has not produced output.".to_string()
                }
                (None, None) => "Command returned no output.".to_string(),
            }
        } else {
            output
        };

        let result = UnifiedExecCodeModeResult {
            chunk_id: (!self.chunk_id.is_empty()).then(|| self.chunk_id.clone()),
            wall_time_seconds: self.wall_time.as_secs_f64(),
            exit_code: self.exit_code,
            session_id: self.process_id,
            original_token_count: self.original_token_count,
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
            output,
        };

        serde_json::to_value(result).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize exec result: {err}"))
        })
    }
}

impl ExecCommandToolOutput {
    fn model_output_limits(&self, raw_output: &str) -> OutputLimitResolution {
        resolve_projected_output_limits(
            self.max_output_tokens,
            OutputOutcome::from_exit_status(self.exit_code, self.process_id.is_some()),
            classify_diagnostic(self.hook_command.as_deref(), raw_output),
            self.truncation_policy.token_budget(),
        )
    }

    fn model_output_max_tokens(&self) -> usize {
        let raw = String::from_utf8_lossy(&self.raw_output);
        self.model_output_limits(raw.as_ref()).applied_limit
    }

    pub(crate) fn truncated_output(&self, max_tokens: usize) -> String {
        let text = String::from_utf8_lossy(&self.raw_output).to_string();
        formatted_truncate_text(&text, TruncationPolicy::Tokens(max_tokens))
    }

    fn projected_model_output(&self) -> ProjectedModelOutput {
        let raw = String::from_utf8_lossy(&self.raw_output);
        let summarized = match (self.process_id, self.exit_code) {
            (None, Some(exit_code)) => summarize_shell_output_for_model(
                raw.as_ref(),
                exit_code,
                /*timed_out*/ false,
                ShellOutputSummaryOptions {
                    enabled: true,
                    turn_cost_guard: false,
                    command_text: self.hook_command.as_deref(),
                },
            ),
            _ => None,
        };
        let content = summarized.as_deref().unwrap_or(raw.as_ref());
        let truncated = formatted_truncate_text_with_output_limit(
            content,
            self.model_output_limits(raw.as_ref()),
        );
        let was_truncated = truncated.was_truncated;
        let mut projected_text = truncated.text;
        if (summarized.is_some() || was_truncated)
            && let Some(original_tokens) = self.original_token_count
        {
            let omitted_tokens =
                original_tokens.saturating_sub(self.truncation_policy.token_budget());
            let marker = format!("Warning: truncated output\n{omitted_tokens} tokens truncated");
            let limit = self.model_output_limits(raw.as_ref()).applied_limit;
            let notice_limit = self.max_output_tokens.unwrap_or(limit).max(limit);
            let candidate = format!("{marker}\n{projected_text}");
            projected_text = if codex_utils_string::approx_token_count(&candidate) <= notice_limit {
                candidate
            } else {
                truncate_text_to_token_ceiling(&marker, notice_limit)
            };
        }
        let artifact_has_more_bytes = self
            .raw_output_artifact
            .as_ref()
            .and_then(RawOutputArtifact::retained_bytes)
            .is_some_and(|bytes| bytes > self.raw_output.len() as u64);
        ProjectedModelOutput {
            reduced: summarized.is_some() || was_truncated || artifact_has_more_bytes,
            text: projected_text,
        }
    }

    fn output_with_reduction_notice(&self, projected: ProjectedModelOutput) -> String {
        let mut output = projected.text;
        if projected.reduced
            && let Some(notice) = self
                .raw_output_artifact
                .as_ref()
                .and_then(RawOutputArtifact::reduction_notice)
        {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&notice);
        }
        output
    }

    fn response_text(&self) -> String {
        let raw_output = String::from_utf8_lossy(&self.raw_output);
        let max_tokens = self.model_output_limits(raw_output.as_ref()).applied_limit;
        let mut sections = Vec::new();

        if !self.chunk_id.is_empty() {
            sections.push(format!("Chunk ID: {}", self.chunk_id));
        }

        let wall_time_seconds = self.wall_time.as_secs_f64();
        sections.push(format!("Wall time: {wall_time_seconds:.4} seconds"));

        if let Some(exit_code) = self.exit_code {
            sections.push(format!("Process exited with code {exit_code}"));
        }

        if let Some(process_id) = &self.process_id {
            sections.push(format!("Process running with session ID {process_id}"));
        }

        if let Some(original_token_count) = self.original_token_count {
            sections.push(format!("Original token count: {original_token_count}"));
        }

        if let Some(repair_notice) = &self.repair_notice {
            sections.push(repair_notice.clone());
        }

        if let Some(raw_output_artifact) = &self.raw_output_artifact {
            sections.push(raw_output_artifact.render_for_model());
        }

        sections.push("Output:".to_string());
        sections.push(self.output_with_reduction_notice(self.projected_model_output()));

        truncate_text_to_token_ceiling(&sections.join("\n"), max_tokens)
    }
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
    let tail_start = total_lines - 71;
    let middle_start =
        (total_lines.saturating_sub(64) / 2 + 1).clamp(65, tail_start.saturating_sub(64));
    vec![
        ToolOutputProjectionRange {
            id: "validation-head".to_string(),
            start_line: 1,
            end_line: 64,
        },
        ToolOutputProjectionRange {
            id: "validation-middle".to_string(),
            start_line: middle_start,
            end_line: middle_start + 63,
        },
        ToolOutputProjectionRange {
            id: "validation-tail".to_string(),
            start_line: tail_start,
            end_line: total_lines,
        },
    ]
}

struct ProjectedModelOutput {
    text: String,
    reduced: bool,
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

fn telemetry_preview(content: &str) -> String {
    let truncated_slice = take_bytes_at_char_boundary(content, TELEMETRY_PREVIEW_MAX_BYTES);
    let truncated_by_bytes = truncated_slice.len() < content.len();

    let mut preview = String::new();
    let mut lines_iter = truncated_slice.lines();
    for idx in 0..TELEMETRY_PREVIEW_MAX_LINES {
        match lines_iter.next() {
            Some(line) => {
                if idx > 0 {
                    preview.push('\n');
                }
                preview.push_str(line);
            }
            None => break,
        }
    }
    let truncated_by_lines = lines_iter.next().is_some();

    if !truncated_by_bytes && !truncated_by_lines {
        return content.to_string();
    }

    if preview.len() < truncated_slice.len()
        && truncated_slice
            .as_bytes()
            .get(preview.len())
            .is_some_and(|byte| *byte == b'\n')
    {
        preview.push('\n');
    }

    if !preview.is_empty() && !preview.ends_with('\n') {
        preview.push('\n');
    }
    preview.push_str(TELEMETRY_PREVIEW_TRUNCATION_NOTICE);

    preview
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
