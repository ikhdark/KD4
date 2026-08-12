use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_utils_string::take_bytes_at_char_boundary;
use serde_json::Value as JsonValue;

use crate::ToolPayload;

const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024;
const TELEMETRY_PREVIEW_MAX_LINES: usize = 64;
const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str = "[... telemetry preview truncated ...]";

/// Typed result state used by model-output projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutputOutcome {
    Success,
    Failure,
    TimedOut,
    Skipped,
}

/// Producer-supplied diagnostic classification used by model-output projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutputDiagnosticClass {
    Normal,
    HighSignal,
}

/// Typed inputs for reducing a textual model projection.
///
/// `spillable_text` is producer text that may be represented by an opaque
/// artifact. `essential_inline` contains control/state data that must remain
/// visible. The dispatcher must consume these fields directly and never infer
/// them by parsing a rendered response.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutputProjectionMetadata {
    pub outcome: ToolOutputOutcome,
    pub diagnostic_class: ToolOutputDiagnosticClass,
    pub spillable_text: Vec<String>,
    pub essential_inline: JsonValue,
    pub requested_limit: Option<usize>,
}

/// Model-facing output contract returned by executable tool runtimes.
pub trait ToolOutput: Send {
    fn log_preview(&self) -> String;

    fn success_for_logging(&self) -> bool;

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        if self.success_for_logging() {
            ToolOutputOutcome::Success
        } else {
            ToolOutputOutcome::Failure
        }
    }

    /// Internal, request-local signal consumed by the normal sampling loop.
    /// This is never serialized into the public protocol.
    fn sampling_request_signal(&self) -> Option<JsonValue> {
        None
    }

    /// Whether this output contains external context that should disable memory generation when
    /// `memories.disable_on_external_context` is enabled.
    fn contains_external_context(&self) -> bool {
        false
    }

    /// Returns typed projection inputs for textual output, if this output may
    /// be reduced for a direct or code-mode model consumer.
    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        None
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem;

    /// Returns the tool call id exposed to `PostToolUse` hooks for this output.
    fn post_tool_use_id(&self, call_id: &str) -> String {
        call_id.to_string()
    }

    /// Returns the tool input exposed to `PostToolUse` hooks for this output.
    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<JsonValue> {
        None
    }

    /// Returns the stable value exposed to `PostToolUse` hooks for this tool output.
    ///
    /// Tool handlers decide whether a tool participates in `PostToolUse`, but
    /// this method lets the output type own any conversion from model-facing
    /// response content to hook-facing data. Returning `None` means the output
    /// should not produce a post-use hook payload, not merely that the tool had
    /// empty output.
    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        None
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        response_input_to_code_mode_result(self.to_response_item("", payload))
    }
}

impl<T> ToolOutput for Box<T>
where
    T: ToolOutput + ?Sized,
{
    fn log_preview(&self) -> String {
        (**self).log_preview()
    }

    fn success_for_logging(&self) -> bool {
        (**self).success_for_logging()
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        (**self).outcome_for_logging()
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        (**self).sampling_request_signal()
    }

    fn contains_external_context(&self) -> bool {
        (**self).contains_external_context()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        (**self).projection_metadata()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        (**self).to_response_item(call_id, payload)
    }

    fn post_tool_use_id(&self, call_id: &str) -> String {
        (**self).post_tool_use_id(call_id)
    }

    fn post_tool_use_input(&self, payload: &ToolPayload) -> Option<JsonValue> {
        (**self).post_tool_use_input(payload)
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<JsonValue> {
        (**self).post_tool_use_response(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        (**self).code_mode_result(payload)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JsonToolOutput {
    value: JsonValue,
    success: Option<bool>,
    outcome: Option<ToolOutputOutcome>,
    contains_external_context: bool,
}

impl JsonToolOutput {
    pub fn new(value: JsonValue) -> Self {
        Self {
            value,
            success: Some(true),
            outcome: None,
            contains_external_context: false,
        }
    }

    pub fn with_success(value: JsonValue, success: Option<bool>) -> Self {
        Self {
            value,
            success,
            outcome: None,
            contains_external_context: false,
        }
    }

    pub fn skipped(value: JsonValue) -> Self {
        Self {
            value,
            success: Some(true),
            outcome: Some(ToolOutputOutcome::Skipped),
            contains_external_context: false,
        }
    }

    pub fn with_external_context(mut self) -> Self {
        self.contains_external_context = true;
        self
    }
}

impl ToolOutputProjectionMetadata {
    pub fn from_json(value: &JsonValue, success: bool, requested_limit: Option<usize>) -> Self {
        Self {
            outcome: if success {
                ToolOutputOutcome::Success
            } else {
                ToolOutputOutcome::Failure
            },
            diagnostic_class: ToolOutputDiagnosticClass::Normal,
            spillable_text: vec![value.to_string()],
            essential_inline: essential_json_fields(value),
            requested_limit,
        }
    }
}

fn essential_json_fields(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(object) => JsonValue::Object(
            object
                .iter()
                .filter_map(|(key, value)| {
                    let nested = essential_json_fields(value);
                    (is_essential_key(key) || !is_empty_projection(&nested)).then(|| {
                        (
                            key.clone(),
                            if is_essential_key(key) {
                                value.clone()
                            } else {
                                nested
                            },
                        )
                    })
                })
                .collect(),
        ),
        JsonValue::Array(values) => JsonValue::Array(
            values
                .iter()
                .map(essential_json_fields)
                .filter(|value| !is_empty_projection(value))
                .collect(),
        ),
        _ => JsonValue::Null,
    }
}

fn is_empty_projection(value: &JsonValue) -> bool {
    matches!(value, JsonValue::Null)
        || matches!(value, JsonValue::Object(object) if object.is_empty())
        || matches!(value, JsonValue::Array(values) if values.is_empty())
}

fn is_essential_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "id"
        || key.ends_with("_id")
        || key.ends_with("id")
        || key.contains("cursor")
        || key.contains("status")
        || key.contains("state")
        || key.contains("gate")
        || key.contains("next_action")
        || key.contains("nextrequiredaction")
        || key == "action"
        || key == "outcome"
}

impl ToolOutput for JsonToolOutput {
    fn log_preview(&self) -> String {
        telemetry_preview(&self.value.to_string())
    }

    fn success_for_logging(&self) -> bool {
        self.success.unwrap_or(true)
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        self.outcome.unwrap_or_else(|| {
            if self.success.unwrap_or(true) {
                ToolOutputOutcome::Success
            } else {
                ToolOutputOutcome::Failure
            }
        })
    }

    fn contains_external_context(&self) -> bool {
        self.contains_external_context
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        let mut metadata = ToolOutputProjectionMetadata::from_json(
            &self.value,
            self.success.unwrap_or(true),
            None,
        );
        metadata.outcome = self.outcome_for_logging();
        Some(metadata)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let output = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(self.value.to_string()),
            success: self.success,
        };

        if matches!(payload, ToolPayload::Custom { .. }) {
            return ResponseInputItem::CustomToolCallOutput {
                call_id: call_id.to_string(),
                name: None,
                output,
            };
        }

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(self.value.clone())
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.value.clone()
    }
}

impl ToolOutput for codex_protocol::mcp::CallToolResult {
    fn log_preview(&self) -> String {
        let output = self.as_function_call_output_payload();
        let preview = output.body.to_text().unwrap_or_else(|| output.to_string());
        telemetry_preview(&preview)
    }

    fn success_for_logging(&self) -> bool {
        self.success()
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::McpToolCallOutput {
            call_id: call_id.to_string(),
            output: self.clone(),
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        serde_json::to_value(self).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize mcp result: {err}"))
        })
    }
}

fn response_input_to_code_mode_result(response: ResponseInputItem) -> JsonValue {
    match response {
        ResponseInputItem::Message { content, .. } => content_items_to_code_mode_result(
            &content
                .into_iter()
                .map(|item| match item {
                    codex_protocol::models::ContentItem::InputText { text }
                    | codex_protocol::models::ContentItem::OutputText { text } => {
                        FunctionCallOutputContentItem::InputText { text }
                    }
                    codex_protocol::models::ContentItem::InputImage { image_url, detail } => {
                        FunctionCallOutputContentItem::InputImage {
                            image_url,
                            detail: detail.or(Some(DEFAULT_IMAGE_DETAIL)),
                        }
                    }
                })
                .collect::<Vec<_>>(),
        ),
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => match output.body {
            FunctionCallOutputBody::Text(text) => JsonValue::String(text),
            FunctionCallOutputBody::ContentItems(items) => {
                content_items_to_code_mode_result(&items)
            }
        },
        ResponseInputItem::ToolSearchOutput { tools, .. } => JsonValue::Array(tools),
        ResponseInputItem::McpToolCallOutput { output, .. } => serde_json::to_value(output)
            .unwrap_or_else(|err| {
                JsonValue::String(format!("failed to serialize mcp result: {err}"))
            }),
    }
}

fn content_items_to_code_mode_result(items: &[FunctionCallOutputContentItem]) -> JsonValue {
    JsonValue::String(
        items
            .iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } if !text.trim().is_empty() => {
                    Some(text.clone())
                }
                FunctionCallOutputContentItem::InputImage { image_url, .. }
                    if !image_url.trim().is_empty() =>
                {
                    Some(image_url.clone())
                }
                FunctionCallOutputContentItem::InputText { .. }
                | FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
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
