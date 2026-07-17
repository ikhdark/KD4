use crate::context_manager::truncate_function_output_payload;
use crate::exec::ExecCommandOutcome;
use crate::original_image_detail::sanitize_original_image_detail;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::TELEMETRY_PREVIEW_MAX_BYTES;
use crate::tools::TELEMETRY_PREVIEW_MAX_LINES;
use crate::tools::TELEMETRY_PREVIEW_TRUNCATION_NOTICE;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::shell_output_summary::ShellOutputSummaryOptions;
use crate::tools::shell_output_summary::reduce_shell_output_for_model_with_budget;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::unified_exec::OutputBudgetClass;
use crate::unified_exec::resolve_adaptive_max_tokens;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_tools::LoadableToolSpec;
use codex_tools::ToolName;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_bytes_for_tokens;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_string::take_bytes_at_char_boundary;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::OnceLock;
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
        truncate_function_output_payload(
            &payload,
            self.truncation_policy * 1.2,
            /*preserve_bounded_shell_evidence*/ false,
        )
    }
}

#[derive(Clone)]
pub struct ToolSearchOutput {
    pub tools: Vec<LoadableToolSpec>,
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
            status: "completed".to_string(),
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
        }
    }
}

pub struct FunctionToolOutput {
    pub body: Vec<FunctionCallOutputContentItem>,
    pub success: Option<bool>,
    pub post_tool_use_response: Option<JsonValue>,
}

impl FunctionToolOutput {
    pub fn from_text(text: String, success: Option<bool>) -> Self {
        Self {
            body: vec![FunctionCallOutputContentItem::InputText { text }],
            success,
            post_tool_use_response: None,
        }
    }

    pub fn from_content(
        content: Vec<FunctionCallOutputContentItem>,
        success: Option<bool>,
    ) -> Self {
        Self {
            body: content,
            success,
            post_tool_use_response: None,
        }
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
        self.success.unwrap_or(true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(call_id, payload, self.body.clone(), self.success)
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        self.post_tool_use_response.clone()
    }
}

pub struct ApplyPatchToolOutput {
    pub text: String,
    result: Option<JsonValue>,
    success: bool,
}

impl ApplyPatchToolOutput {
    pub fn from_text(text: String) -> Self {
        Self {
            text,
            result: None,
            success: true,
        }
    }

    pub fn from_execution(
        text: String,
        execution_succeeded: bool,
        action: &codex_apply_patch::ApplyPatchAction,
        delta: &codex_apply_patch::AppliedPatchDelta,
    ) -> Self {
        let status = apply_patch_status(execution_succeeded, delta.is_empty(), delta.is_exact());
        let operations = action
            .operations()
            .iter()
            .map(|operation| {
                let (kind, move_path) = match operation.change() {
                    codex_apply_patch::ApplyPatchFileChange::Add { .. } => ("add", None),
                    codex_apply_patch::ApplyPatchFileChange::Delete { .. } => ("delete", None),
                    codex_apply_patch::ApplyPatchFileChange::Update { move_path, .. } => (
                        "update",
                        move_path
                            .as_ref()
                            .map(codex_utils_path_uri::PathUri::inferred_native_path_string),
                    ),
                };
                let destination_fingerprint = operation
                    .move_destination()
                    .map(|(_, fingerprint)| fingerprint.stable_id());
                serde_json::json!({
                    "path": operation.path().inferred_native_path_string(),
                    "operation": kind,
                    "move_path": move_path,
                    "expected_old_fingerprint": operation.expected_old_fingerprint().stable_id(),
                    "expected_move_destination_fingerprint": destination_fingerprint,
                    "new_fingerprint": operation.new_fingerprint().stable_id(),
                })
            })
            .collect::<Vec<_>>();
        let committed_delta = delta
            .changes()
            .iter()
            .map(|change| match &change.change {
                codex_apply_patch::AppliedPatchFileChange::Add {
                    content,
                    overwritten_content,
                } => serde_json::json!({
                    "path": change.path.display().to_string(),
                    "operation": "add",
                    "old_content": overwritten_content,
                    "new_content": content,
                    "old_fingerprint": codex_apply_patch::patch_content_fingerprint(
                        overwritten_content.as_deref().map(str::as_bytes),
                    ),
                    "new_fingerprint": codex_apply_patch::patch_content_fingerprint(
                        Some(content.as_bytes()),
                    ),
                }),
                codex_apply_patch::AppliedPatchFileChange::Delete { content } => {
                    serde_json::json!({
                        "path": change.path.display().to_string(),
                        "operation": "delete",
                        "old_content": content,
                        "new_content": JsonValue::Null,
                        "old_fingerprint": codex_apply_patch::patch_content_fingerprint(
                            Some(content.as_bytes()),
                        ),
                        "new_fingerprint": codex_apply_patch::patch_content_fingerprint(None),
                    })
                }
                codex_apply_patch::AppliedPatchFileChange::Update {
                    move_path,
                    old_content,
                    overwritten_move_content,
                    new_content,
                } => serde_json::json!({
                    "path": change.path.display().to_string(),
                    "operation": "update",
                    "move_path": move_path.as_ref().map(|path| path.display().to_string()),
                    "old_content": old_content,
                    "overwritten_move_content": overwritten_move_content,
                    "new_content": new_content,
                    "old_fingerprint": codex_apply_patch::patch_content_fingerprint(
                        Some(old_content.as_bytes()),
                    ),
                    "overwritten_move_fingerprint": codex_apply_patch::patch_content_fingerprint(
                        overwritten_move_content.as_deref().map(str::as_bytes),
                    ),
                    "new_fingerprint": codex_apply_patch::patch_content_fingerprint(
                        Some(new_content.as_bytes()),
                    ),
                }),
            })
            .collect::<Vec<_>>();
        let result = serde_json::json!({
            "status": status,
            "exact": delta.is_exact(),
            "operations": operations,
            "committed_delta": committed_delta,
            "summary": text,
        });
        Self {
            text,
            result: Some(result),
            success: execution_succeeded,
        }
    }
}

fn apply_patch_status(
    execution_succeeded: bool,
    delta_is_empty: bool,
    delta_is_exact: bool,
) -> &'static str {
    match (execution_succeeded, delta_is_empty, delta_is_exact) {
        (true, true, true) => "no_op",
        (true, _, _) => "completed",
        (false, true, true) => "failed",
        (false, _, _) => "partial",
    }
}

impl ToolOutput for ApplyPatchToolOutput {
    fn log_preview(&self) -> String {
        telemetry_preview(&self.text)
    }

    fn success_for_logging(&self) -> bool {
        self.success
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.text.clone(),
            }],
            Some(self.success),
        )
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(
            self.result
                .clone()
                .unwrap_or_else(|| JsonValue::String(self.text.clone())),
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.result
            .clone()
            .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new()))
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

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        match payload {
            ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
                call_id: call_id.to_string(),
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
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
}

#[derive(Debug, Default)]
pub(crate) struct ExecCommandOutputAnalysis {
    decoded_output: OnceLock<String>,
    model_output_max_tokens: OnceLock<usize>,
    hook_output: OnceLock<String>,
    model_output: OnceLock<String>,
    response_text: OnceLock<String>,
    preview: OnceLock<String>,
}

#[derive(Debug, Clone)]
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
    pub timed_out: bool,
    pub original_token_count: Option<usize>,
    pub hook_command: Option<String>,
    pub raw_output_artifact: Option<RawOutputArtifact>,
    pub repair_notice: Option<String>,
    pub(crate) analysis: Arc<ExecCommandOutputAnalysis>,
}

impl PartialEq for ExecCommandToolOutput {
    fn eq(&self, other: &Self) -> bool {
        self.event_call_id == other.event_call_id
            && self.chunk_id == other.chunk_id
            && self.wall_time == other.wall_time
            && self.raw_output == other.raw_output
            && self.truncation_policy == other.truncation_policy
            && self.max_output_tokens == other.max_output_tokens
            && self.process_id == other.process_id
            && self.exit_code == other.exit_code
            && self.timed_out == other.timed_out
            && self.original_token_count == other.original_token_count
            && self.hook_command == other.hook_command
            && self.raw_output_artifact == other.raw_output_artifact
            && self.repair_notice == other.repair_notice
    }
}

impl ToolOutput for ExecCommandToolOutput {
    fn log_preview(&self) -> String {
        self.preview().to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.outcome().is_success()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        function_tool_response(
            call_id,
            payload,
            vec![FunctionCallOutputContentItem::InputText {
                text: self.response_text().to_string(),
            }],
            Some(self.outcome().is_success()),
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

        Some(JsonValue::String(self.hook_output().to_string()))
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        #[derive(Serialize)]
        struct UnifiedExecCodeModeResult {
            outcome: ExecCommandOutcome,
            timed_out: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            exit_code: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_id: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            original_token_count: Option<usize>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_output_artifact: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_output_artifact_bytes: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            raw_output_artifact_error: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            repair: Option<String>,
            output: String,
        }

        let (raw_output_artifact, raw_output_artifact_bytes, raw_output_artifact_error) =
            match self.raw_output_artifact.as_ref() {
                Some(RawOutputArtifact::Stored { path, bytes }) => (
                    Some(path.to_string_lossy().into_owned()),
                    Some(*bytes),
                    None,
                ),
                Some(RawOutputArtifact::Failed {
                    message,
                    owned_path,
                    bytes,
                }) => (
                    owned_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    owned_path.as_ref().map(|_| *bytes),
                    Some(message.clone()),
                ),
                None => (None, None, None),
            };

        let result = UnifiedExecCodeModeResult {
            outcome: self.outcome(),
            timed_out: self.timed_out,
            exit_code: self.exit_code,
            session_id: self.process_id,
            original_token_count: self.original_token_count,
            raw_output_artifact,
            raw_output_artifact_bytes,
            raw_output_artifact_error,
            repair: self.repair_notice.clone(),
            output: self.model_output().to_string(),
        };

        serde_json::to_value(result).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize exec result: {err}"))
        })
    }
}

impl ExecCommandToolOutput {
    pub(crate) fn outcome(&self) -> ExecCommandOutcome {
        ExecCommandOutcome::from_process_facts(
            self.process_id,
            self.exit_code,
            self.timed_out,
            /*cancelled*/ false,
            /*launch_failed*/ false,
        )
    }

    fn decoded_output(&self) -> &str {
        self.analysis
            .decoded_output
            .get_or_init(|| String::from_utf8_lossy(&self.raw_output).into_owned())
    }

    fn model_output_max_tokens(&self) -> usize {
        *self.analysis.model_output_max_tokens.get_or_init(|| {
            let class = if self.process_id.is_some() || self.exit_code != Some(0) {
                OutputBudgetClass::FailureOrTimeout
            } else {
                OutputBudgetClass::Success
            };
            resolve_adaptive_max_tokens(
                self.max_output_tokens,
                class,
                self.hook_command.as_deref(),
                self.decoded_output(),
            )
            .min(self.truncation_policy.token_budget())
        })
    }

    pub(crate) fn truncated_output(&self, max_tokens: usize) -> String {
        formatted_truncate_text(self.decoded_output(), TruncationPolicy::Tokens(max_tokens))
    }

    fn hook_output(&self) -> &str {
        self.analysis.hook_output.get_or_init(|| {
            formatted_truncate_text(
                self.decoded_output(),
                TruncationPolicy::Tokens(self.model_output_max_tokens()),
            )
        })
    }

    fn model_output(&self) -> &str {
        self.analysis.model_output.get_or_init(|| {
            let raw = self.decoded_output();
            let max_tokens = self.model_output_max_tokens();
            let summarized = reduce_shell_output_for_model_with_budget(
                raw,
                self.exit_code.unwrap_or_default(),
                self.timed_out,
                ShellOutputSummaryOptions {
                    enabled: true,
                    turn_cost_guard: false,
                    command_text: self.hook_command.as_deref(),
                },
                approx_bytes_for_tokens(max_tokens),
            );
            summarized.map_or_else(|| raw.to_string(), |reduction| reduction.summary)
        })
    }

    fn response_text(&self) -> &str {
        self.analysis.response_text.get_or_init(|| {
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
            sections.push(self.model_output().to_string());

            sections.join("\n")
        })
    }

    fn preview(&self) -> &str {
        self.analysis
            .preview
            .get_or_init(|| telemetry_preview(self.response_text()))
    }
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
