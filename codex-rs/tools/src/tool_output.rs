use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_utils_string::approx_tokens_from_byte_count;
use codex_utils_string::take_bytes_at_char_boundary;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;

use crate::ToolPayload;

const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024;
const TELEMETRY_PREVIEW_MAX_LINES: usize = 64;
const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str = "[... telemetry preview truncated ...]";

/// A zero-based, end-exclusive range in the canonical artifact byte stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalByteRange {
    pub start: u64,
    pub end: u64,
}

impl CanonicalByteRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    pub fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Offset entry produced while canonical JSON is serialized. Ranges cover the
/// lexical JSON value itself (including string quotes and escapes).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalJsonPointer {
    pub range: CanonicalByteRange,
    pub exact_bytes: u64,
    pub direct_children: Vec<String>,
    /// Largest byte-selector payload proven to fit the recovery response
    /// ceiling. Filled by the artifact owner because it owns that envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_chunk_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalToolResultKind {
    Text,
    Bytes,
    Json,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProjectionInclusion {
    Included,
    Omitted,
    Directory,
}

/// A stable semantic address into a canonical result. Atomic sections own one
/// contiguous byte range; directory sections own children and no range.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolProjectionSection {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<JsonValue>,
    pub exact_bytes: u64,
    pub inclusion: ToolProjectionInclusion,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_range: Option<CanonicalByteRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_chunk_bytes: Option<u64>,
}

/// Exact payload and identity from which every model projection and recovery
/// selector is derived.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CanonicalToolResult {
    pub kind: CanonicalToolResultKind,
    #[serde(skip)]
    pub bytes: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<JsonValue>,
    pub sha256: String,
    pub exact_bytes: u64,
    pub approximate_tokens: u64,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_ranges: Vec<CanonicalByteRange>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json_pointers: BTreeMap<String, CanonicalJsonPointer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ToolProjectionSection>,
}

impl CanonicalToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::from_bytes(
            CanonicalToolResultKind::Text,
            text.into_bytes(),
            None,
            BTreeMap::new(),
        )
    }

    pub fn bytes(bytes: Vec<u8>) -> Self {
        Self::from_bytes(CanonicalToolResultKind::Bytes, bytes, None, BTreeMap::new())
    }

    pub fn json(value: JsonValue) -> Self {
        let mut bytes = Vec::new();
        let mut pointers = BTreeMap::new();
        serialize_canonical_json(&value, "", &mut bytes, &mut pointers);
        Self::from_bytes(CanonicalToolResultKind::Json, bytes, Some(value), pointers)
    }

    fn from_bytes(
        kind: CanonicalToolResultKind,
        bytes: Vec<u8>,
        value: Option<JsonValue>,
        json_pointers: BTreeMap<String, CanonicalJsonPointer>,
    ) -> Self {
        let exact_bytes = bytes.len() as u64;
        Self {
            kind,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            approximate_tokens: approx_tokens_from_byte_count(bytes.len()),
            exact_bytes,
            bytes,
            value,
            complete: true,
            unavailable_ranges: Vec::new(),
            json_pointers,
            sections: Vec::new(),
        }
    }
}

/// The single model-facing projection envelope. `result` remains native JSON
/// internally and is serialized only by the direct Responses API boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolProjectionV1 {
    pub version: u8,
    pub tool: String,
    pub outcome: String,
    pub canonical_sha256: String,
    pub canonical_bytes: u64,
    pub canonical_approximate_tokens: u64,
    pub canonical_complete: bool,
    pub model_bytes: u64,
    pub model_approximate_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ToolProjectionSection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted_sections: Vec<String>,
    pub result: JsonValue,
}

fn serialize_canonical_json(
    value: &JsonValue,
    pointer: &str,
    output: &mut Vec<u8>,
    pointers: &mut BTreeMap<String, CanonicalJsonPointer>,
) {
    let start = output.len() as u64;
    let mut direct_children = Vec::new();
    match value {
        JsonValue::Null => output.extend_from_slice(b"null"),
        JsonValue::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        JsonValue::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        JsonValue::String(value) => append_json_string(value, output),
        JsonValue::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                let child = format!("{pointer}/{index}");
                direct_children.push(child.clone());
                serialize_canonical_json(value, &child, output, pointers);
            }
            output.push(b']');
        }
        JsonValue::Object(values) => {
            output.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                append_json_string(key, output);
                output.push(b':');
                let escaped = key.replace('~', "~0").replace('/', "~1");
                let child = format!("{pointer}/{escaped}");
                direct_children.push(child.clone());
                serialize_canonical_json(value, &child, output, pointers);
            }
            output.push(b'}');
        }
    }
    let end = output.len() as u64;
    pointers.insert(
        pointer.to_string(),
        CanonicalJsonPointer {
            range: CanonicalByteRange::new(start, end),
            exact_bytes: end.saturating_sub(start),
            direct_children,
            recovery_chunk_bytes: None,
        },
    );
}

fn append_json_string(value: &str, output: &mut Vec<u8>) {
    let start = output.len();
    if serde_json::to_writer(&mut *output, value).is_err() {
        // `str` serialization is infallible for serde_json today. Preserve a
        // valid canonical JSON stream if that contract ever changes.
        output.truncate(start);
        output.extend_from_slice(b"null");
    }
}

/// Typed result state used by model-output projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutputOutcome {
    Success,
    Failure,
    TimedOut,
    Skipped,
}

/// Why a tool intentionally did not execute.
///
/// A skipped result is non-success, but only `BlockingRequiredOperation`
/// constitutes failure evidence for the sampling governor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputSkipDisposition {
    Deferred,
    Suppressed,
    NotApplicable,
    BlockingRequiredOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolOutputOutcomeContext {
    pub outcome: ToolOutputOutcome,
    pub skip_disposition: Option<ToolOutputSkipDisposition>,
}

impl ToolOutputOutcomeContext {
    pub const fn new(outcome: ToolOutputOutcome) -> Self {
        Self {
            outcome,
            skip_disposition: None,
        }
    }

    pub const fn skipped(disposition: Option<ToolOutputSkipDisposition>) -> Self {
        Self {
            outcome: ToolOutputOutcome::Skipped,
            skip_disposition: disposition,
        }
    }
}

/// Producer-supplied diagnostic classification used by model-output projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutputDiagnosticClass {
    Normal,
    HighSignal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeModeToolSearchStatus {
    Completed,
    Incomplete,
    Aborted,
}

impl CodeModeToolSearchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Aborted => "aborted",
        }
    }
}

pub fn code_mode_tool_search_result(
    status: CodeModeToolSearchStatus,
    tools: Vec<JsonValue>,
    omitted_result_count: Option<usize>,
) -> JsonValue {
    serde_json::json!({
        "status": status.as_str(),
        "execution": "client",
        "tools": tools,
        "omitted_result_count": omitted_result_count,
    })
}

/// Producer-owned category for a structured model-projection fragment.
///
/// Categories describe semantics only. They are not serialized into the public
/// protocol and producers remain free to omit fragments entirely.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolOutputProjectionFragmentKind {
    ErrorOrDiagnostic,
    ValidationFailureOrFinalSummary,
    ProcessFinalStatus,
    ContextualSpillableText,
}

/// A typed, handler-supplied fragment eligible for the bounded model projection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolOutputProjectionFragment {
    /// Stable producer-owned identity for exact selection and recovery accounting.
    pub id: Option<String>,
    pub kind: ToolOutputProjectionFragmentKind,
    pub text: String,
}

impl ToolOutputProjectionFragment {
    pub fn new(kind: ToolOutputProjectionFragmentKind, text: impl Into<String>) -> Self {
        Self {
            id: None,
            kind,
            text: text.into(),
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
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
    /// Optional structured high-signal content. An empty collection preserves
    /// the generic spillable-text projection used by existing producers.
    pub fragments: Vec<ToolOutputProjectionFragment>,
    pub spillable_text: Vec<String>,
    pub essential_inline: JsonValue,
    pub requested_limit: Option<usize>,
    /// Producer-owned exact line ranges that are safe to drain from the raw
    /// artifact without another model decision. Empty means fail open to the
    /// existing model-driven `read_tool_output` path.
    pub predetermined_ranges: Vec<ToolOutputProjectionRange>,
    /// Producer-owned exact JSON Pointer selectors that are safe to drain from
    /// the canonical JSON artifact without another model decision. Empty means
    /// fail open to the existing model-driven `read_tool_output` path.
    pub predetermined_json_pointers: Vec<ToolOutputProjectionJsonPointer>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ToolOutputProjectionRange {
    /// Stable producer-owned range identity; never inferred from rendered text.
    pub id: String,
    /// Inclusive one-based line bounds in the canonical raw artifact.
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ToolOutputProjectionJsonPointer {
    /// Stable producer-owned selector identity; never inferred from rendered text.
    pub id: String,
    /// RFC 6901 pointer into the canonical JSON artifact.
    pub pointer: String,
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

    fn outcome_context(&self) -> ToolOutputOutcomeContext {
        ToolOutputOutcomeContext::new(self.outcome_for_logging())
    }

    /// Internal, request-local signal consumed by the normal sampling loop.
    /// This is never serialized into the public protocol.
    fn sampling_request_signal(&self) -> Option<JsonValue> {
        None
    }

    /// Owner-issued proof that deterministic continuations were completed by
    /// the host before this result was returned to the model.
    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        Vec::new()
    }

    /// Request-local owner key used to carry deterministic nested-tool evidence
    /// to the model-facing result that owns the next sampling boundary.
    fn deterministic_continuation_owner_key(&self) -> Option<String> {
        None
    }

    /// Exact model-facing values recovered by the tool owner. The runtime only
    /// accepts these when paired with one deterministic continuation receipt.
    fn deterministic_continuation_content(&self) -> Vec<JsonValue> {
        Vec::new()
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

    /// Whether the dispatcher must materialize the canonical result even when
    /// the already-bounded model projection would otherwise fit inline.
    ///
    /// Producers use this when their visible response is intentionally only a
    /// projection of a larger authoritative result.
    fn requires_canonical_artifact(&self) -> bool {
        false
    }

    /// Returns the exact producer result before any model-facing rendering.
    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        let metadata = self.projection_metadata()?;
        if metadata.spillable_text.len() == 1 {
            Some(CanonicalToolResult::text(
                metadata
                    .spillable_text
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
            ))
        } else {
            Some(CanonicalToolResult::json(self.code_mode_result(payload)))
        }
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

    fn outcome_context(&self) -> ToolOutputOutcomeContext {
        (**self).outcome_context()
    }

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        (**self).sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        (**self).deterministic_continuation_receipts()
    }

    fn deterministic_continuation_owner_key(&self) -> Option<String> {
        (**self).deterministic_continuation_owner_key()
    }

    fn deterministic_continuation_content(&self) -> Vec<JsonValue> {
        (**self).deterministic_continuation_content()
    }

    fn contains_external_context(&self) -> bool {
        (**self).contains_external_context()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        (**self).projection_metadata()
    }

    fn requires_canonical_artifact(&self) -> bool {
        (**self).requires_canonical_artifact()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        (**self).canonical_result(payload)
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
    skip_disposition: Option<ToolOutputSkipDisposition>,
    contains_external_context: bool,
}

impl JsonToolOutput {
    pub fn new(value: JsonValue) -> Self {
        Self {
            value,
            success: Some(true),
            outcome: None,
            skip_disposition: None,
            contains_external_context: false,
        }
    }

    pub fn with_success(value: JsonValue, success: Option<bool>) -> Self {
        Self {
            value,
            success,
            outcome: None,
            skip_disposition: None,
            contains_external_context: false,
        }
    }

    pub fn skipped(value: JsonValue) -> Self {
        Self {
            value,
            success: Some(false),
            outcome: Some(ToolOutputOutcome::Skipped),
            skip_disposition: None,
            contains_external_context: false,
        }
    }

    pub fn skipped_with_disposition(
        value: JsonValue,
        disposition: ToolOutputSkipDisposition,
    ) -> Self {
        Self {
            value,
            success: Some(false),
            outcome: Some(ToolOutputOutcome::Skipped),
            skip_disposition: Some(disposition),
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
            fragments: Vec::new(),
            spillable_text: vec![value.to_string()],
            essential_inline: essential_json_fields(value),
            requested_limit,
            predetermined_ranges: Vec::new(),
            predetermined_json_pointers: Vec::new(),
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

    fn outcome_context(&self) -> ToolOutputOutcomeContext {
        match self.outcome_for_logging() {
            ToolOutputOutcome::Skipped => ToolOutputOutcomeContext::skipped(self.skip_disposition),
            outcome => ToolOutputOutcomeContext::new(outcome),
        }
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

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(CanonicalToolResult::json(self.value.clone()))
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

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        let value = serde_json::to_value(self).ok()?;
        let serialized = value.to_string();
        let mut fragments = Vec::new();
        if self.is_error == Some(true) {
            fragments.push(ToolOutputProjectionFragment::new(
                ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
                "MCP tool result reported is_error=true",
            ));
        }
        fragments.push(ToolOutputProjectionFragment::new(
            ToolOutputProjectionFragmentKind::ContextualSpillableText,
            serialized.clone(),
        ));
        Some(ToolOutputProjectionMetadata {
            outcome: if self.success() {
                ToolOutputOutcome::Success
            } else {
                ToolOutputOutcome::Failure
            },
            diagnostic_class: if self.is_error == Some(true) {
                ToolOutputDiagnosticClass::HighSignal
            } else {
                ToolOutputDiagnosticClass::Normal
            },
            fragments,
            spillable_text: vec![serialized],
            essential_inline: serde_json::json!({
                "is_error": self.is_error,
                "content_items": self.content.len(),
                "has_structured_content": self.structured_content.is_some(),
                "has_meta": self.meta.is_some(),
            }),
            requested_limit: None,
            predetermined_ranges: Vec::new(),
            predetermined_json_pointers: Vec::new(),
        })
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        serde_json::to_value(self)
            .ok()
            .map(CanonicalToolResult::json)
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
        ResponseInputItem::ToolSearchOutput {
            status,
            execution,
            tools,
            ..
        } => serde_json::json!({
            "status": status,
            "execution": execution,
            "tools": tools,
            "omitted_result_count": JsonValue::Null,
        }),
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

#[cfg(test)]
mod canonical_tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_recursively_and_records_lexical_ranges() {
        let value = serde_json::json!({
            "z": ["line\nvalue", {"b": 2, "a/b~": true}],
            "a": {"d": 4, "c": 3},
        });

        let canonical = CanonicalToolResult::json(value);

        assert_eq!(
            String::from_utf8(canonical.bytes.clone()).unwrap(),
            r#"{"a":{"c":3,"d":4},"z":["line\nvalue",{"a/b~":true,"b":2}]}"#
        );
        let string = &canonical.json_pointers["/z/0"];
        assert_eq!(
            &canonical.bytes[string.range.start as usize..string.range.end as usize],
            br#""line\nvalue""#
        );
        assert_eq!(string.exact_bytes, string.range.len());
        assert_eq!(
            canonical.json_pointers["/z/1"].direct_children,
            vec!["/z/1/a~1b~0", "/z/1/b"]
        );
        assert_eq!(canonical.json_pointers[""].range.end, canonical.exact_bytes);
    }

    #[test]
    fn mcp_canonical_result_preserves_hash_modalities_and_provenance() {
        let result = codex_protocol::mcp::CallToolResult {
            content: vec![
                serde_json::json!({"type": "text", "text": "hello"}),
                serde_json::json!({"type": "image", "data": "pixels", "mimeType": "image/png"}),
            ],
            structured_content: Some(serde_json::json!({"z": 2, "a": 1})),
            is_error: Some(false),
            meta: Some(serde_json::json!({"provider": "fixture"})),
        };

        let canonical = result
            .canonical_result(&ToolPayload::Function {
                arguments: "{}".to_string(),
            })
            .expect("canonical MCP result");
        assert_eq!(canonical.kind, CanonicalToolResultKind::Json);
        assert_eq!(
            canonical.sha256,
            format!("{:x}", Sha256::digest(&canonical.bytes)),
        );
        let value = canonical.value.expect("typed MCP value");
        assert_eq!(value["content"][1]["type"], "image");
        assert_eq!(value["structuredContent"]["a"], 1);
        assert_eq!(value["_meta"]["provider"], "fixture");
    }

    #[test]
    fn json_projection_cannot_claim_producer_owned_predetermined_selectors() {
        let value = serde_json::json!({
            "predetermined_ranges": [{
                "id": "untrusted",
                "start_line": 1,
                "end_line": 2,
            }],
            "predetermined_json_pointers": [{
                "id": "untrusted-json",
                "pointer": "/output",
            }],
            "output": "fixture",
        });

        let metadata = ToolOutputProjectionMetadata::from_json(&value, true, None);

        assert!(metadata.predetermined_ranges.is_empty());
        assert!(metadata.predetermined_json_pointers.is_empty());
        assert_eq!(metadata.spillable_text, vec![value.to_string()]);
    }

    #[test]
    fn generic_tool_search_conversion_preserves_status_and_execution() {
        assert_eq!(
            response_input_to_code_mode_result(ResponseInputItem::ToolSearchOutput {
                call_id: "search".to_string(),
                status: "incomplete".to_string(),
                execution: "client".to_string(),
                tools: vec![serde_json::json!({"name": "lookup"})],
            }),
            serde_json::json!({
                "status": "incomplete",
                "execution": "client",
                "tools": [{"name": "lookup"}],
                "omitted_result_count": null,
            })
        );
    }
}
