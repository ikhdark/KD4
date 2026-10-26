use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_utils_string::approx_tokens_from_byte_count;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;

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
            sha256: hex::encode(Sha256::digest(&bytes)),
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
                let escaped = codex_utils_string::json_pointer_segment(key);
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
    /// Execution is still live and can be resumed by the host.
    Yielded,
    Skipped,
}

/// Why a tool result could not exercise the requested capability.
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
pub struct ShellOutputProjectionSource {
    pub exit_code: i32,
    pub command_text: Option<String>,
    /// Exact producer streams, only when they cover the same aggregate chunk.
    pub streams: Option<(String, String)>,
}

/// A typed, handler-supplied fragment eligible for the bounded model projection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolOutputProjectionFragment {
    /// Stable producer-owned identity for exact selection and recovery accounting.
    pub id: Option<String>,
    pub kind: ToolOutputProjectionFragmentKind,
    pub text: String,
    /// Defer diagnostic selection until envelope/section overhead is known.
    /// `text` remains canonical input, not an already-budgeted summary.
    pub shell_output: Option<ShellOutputProjectionSource>,
}

impl ToolOutputProjectionFragment {
    pub fn new(kind: ToolOutputProjectionFragmentKind, text: impl Into<String>) -> Self {
        Self {
            id: None,
            kind,
            text: text.into(),
            shell_output: None,
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

    /// Exact UTF-8 byte and line counts for `log_preview`, when already known.
    /// Returning `None` preserves the existing preview-based logging path.
    fn log_output_stats(&self) -> Option<(usize, usize)> {
        None
    }

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

    /// Whether generic recovery storage may retain this producer's evidence.
    /// Model-only/private outputs must opt out until typed recovery enforces
    /// their access boundary. Fitting outputs stay native; oversized delivery
    /// is explicitly unavailable rather than spilling into ordinary artifacts.
    fn allows_generic_artifact_retention(&self) -> bool {
        true
    }

    /// Host delivery status, distinct from execution success and output text.
    fn model_delivery_unavailable(&self) -> bool {
        false
    }

    /// Bounded host settlement notice that must survive an already-built model
    /// projection. This does not replace the producer's canonical result.
    fn model_delivery_notice(&self) -> Option<&str> {
        None
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

    /// Materializes the provider response and projection metadata together.
    ///
    /// Producers whose response and metadata share an expensive source buffer
    /// can override this to decode or classify that buffer once.
    fn to_response_item_with_projection_metadata(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> (ResponseInputItem, Option<ToolOutputProjectionMetadata>) {
        (
            self.to_response_item(call_id, payload),
            self.projection_metadata(),
        )
    }

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

    /// Display room left in the owning cell. Exact programmatic data remains
    /// independent of this budget unless the output owner explicitly projects it.
    fn code_mode_result_with_budget(&self, payload: &ToolPayload, _budget: usize) -> JsonValue {
        self.code_mode_result(payload)
    }

    /// Whether a Failure/TimedOut result rejects the JavaScript promise.
    /// A process exit status is returned data, even when it records failure;
    /// logging and continuation classification must still see that failure.
    fn code_mode_failure_is_error(&self) -> bool {
        true
    }
}

impl<T> ToolOutput for Box<T>
where
    T: ToolOutput + ?Sized,
{

    fn log_preview(&self) -> String {
        (**self).log_preview()
    }

    fn log_output_stats(&self) -> Option<(usize, usize)> {
        (**self).log_output_stats()
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

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        (**self).projection_metadata()
    }

    fn requires_canonical_artifact(&self) -> bool {
        (**self).requires_canonical_artifact()
    }

    fn allows_generic_artifact_retention(&self) -> bool {
        (**self).allows_generic_artifact_retention()
    }

    fn model_delivery_unavailable(&self) -> bool {
        (**self).model_delivery_unavailable()
    }

    fn model_delivery_notice(&self) -> Option<&str> {
        (**self).model_delivery_notice()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        (**self).canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        (**self).to_response_item(call_id, payload)
    }

    fn to_response_item_with_projection_metadata(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> (ResponseInputItem, Option<ToolOutputProjectionMetadata>) {
        (**self).to_response_item_with_projection_metadata(call_id, payload)
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

    fn code_mode_result_with_budget(&self, payload: &ToolPayload, budget: usize) -> JsonValue {
        (**self).code_mode_result_with_budget(payload, budget)
    }

    fn code_mode_failure_is_error(&self) -> bool {
        (**self).code_mode_failure_is_error()
    }
}

#[derive(Clone)]
pub struct JsonToolOutput {
    value: JsonValue,
    model_value: Option<JsonValue>,
    canonical_artifact_required: bool,
    sampling_signal: Option<JsonValue>,
    success: Option<bool>,
    outcome: Option<ToolOutputOutcome>,
    skip_disposition: Option<ToolOutputSkipDisposition>,
    serialized: Arc<OnceLock<String>>,
    code_mode_failure_as_data: bool,
}

impl std::fmt::Debug for JsonToolOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JsonToolOutput")
            .field("value", &self.value)
            .field("model_value", &self.model_value)
            .field(
                "canonical_artifact_required",
                &self.canonical_artifact_required,
            )
            .field("sampling_signal", &self.sampling_signal)
            .field("success", &self.success)
            .field("outcome", &self.outcome)
            .field("skip_disposition", &self.skip_disposition)
            .field("code_mode_failure_as_data", &self.code_mode_failure_as_data)
            .finish()
    }
}

impl PartialEq for JsonToolOutput {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
            && self.model_value == other.model_value
            && self.canonical_artifact_required == other.canonical_artifact_required
            && self.sampling_signal == other.sampling_signal
            && self.success == other.success
            && self.outcome == other.outcome
            && self.skip_disposition == other.skip_disposition
            && self.code_mode_failure_as_data == other.code_mode_failure_as_data
    }
}

impl JsonToolOutput {
    /// Borrow the canonical JSON without creating another retained copy.
    pub fn value(&self) -> &JsonValue {
        &self.value
    }

    pub fn new(value: JsonValue) -> Self {
        Self {
            value,
            model_value: None,
            canonical_artifact_required: false,
            sampling_signal: None,
            success: Some(true),
            outcome: None,
            skip_disposition: None,
            serialized: Arc::new(OnceLock::new()),
            code_mode_failure_as_data: false,
        }
    }

    pub fn with_success(value: JsonValue, success: Option<bool>) -> Self {
        Self {
            value,
            model_value: None,
            canonical_artifact_required: false,
            sampling_signal: None,
            success,
            outcome: None,
            skip_disposition: None,
            serialized: Arc::new(OnceLock::new()),
            code_mode_failure_as_data: false,
        }
    }

    pub fn skipped(value: JsonValue) -> Self {
        Self {
            value,
            model_value: None,
            canonical_artifact_required: false,
            sampling_signal: None,
            success: Some(false),
            outcome: Some(ToolOutputOutcome::Skipped),
            skip_disposition: None,
            serialized: Arc::new(OnceLock::new()),
            code_mode_failure_as_data: false,
        }
    }

    pub fn skipped_with_disposition(
        value: JsonValue,
        disposition: ToolOutputSkipDisposition,
    ) -> Self {
        Self {
            value,
            model_value: None,
            canonical_artifact_required: false,
            sampling_signal: None,
            success: Some(false),
            outcome: Some(ToolOutputOutcome::Skipped),
            skip_disposition: Some(disposition),
            serialized: Arc::new(OnceLock::new()),
            code_mode_failure_as_data: false,
        }
    }

    /// Return structured partial/failure data to JS without changing the logged
    /// outcome or allowing it to certify successful final delivery.
    pub fn with_code_mode_failure_as_data(mut self) -> Self {
        self.code_mode_failure_as_data = true;
        self
    }

    /// Override only model presentation; JS, hooks and canonical retention stay raw.
    pub fn with_model_value(mut self, value: JsonValue) -> Self {
        self.model_value = Some(value);
        self.serialized = Arc::new(OnceLock::new());
        self
    }

    /// Keep omitted producer content recoverable even when its preview fits inline.
    pub fn with_recoverable_model_value(mut self, value: JsonValue) -> Self {
        self.canonical_artifact_required = value != self.value;
        self.with_model_value(value)
    }

    /// Private evidence metadata, independent of presentation and artifact IDs.
    pub fn with_sampling_request_signal(mut self, signal: JsonValue) -> Self {
        self.sampling_signal = Some(signal);
        self
    }

    fn serialized(&self) -> &str {
        self.serialized
            .get_or_init(|| self.model_value.as_ref().unwrap_or(&self.value).to_string())
            .as_str()
    }
}

impl ToolOutputProjectionMetadata {
    pub fn from_json(value: &JsonValue, success: bool, requested_limit: Option<usize>) -> Self {
        Self::from_json_with_serialized(value, success, requested_limit, value.to_string())
    }

    fn from_json_with_serialized(
        value: &JsonValue,
        success: bool,
        requested_limit: Option<usize>,
        serialized: String,
    ) -> Self {
        Self {
            outcome: if success {
                ToolOutputOutcome::Success
            } else {
                ToolOutputOutcome::Failure
            },
            diagnostic_class: ToolOutputDiagnosticClass::Normal,
            fragments: Vec::new(),
            spillable_text: vec![serialized],
            essential_inline: essential_projection_fields(value),
            requested_limit,
            predetermined_ranges: Vec::new(),
            predetermined_json_pointers: Vec::new(),
        }
    }

    /// Merges control-plane fields from a canonical JSON result into the
    /// bounded inline projection without copying its spillable payload.
    pub fn merge_essential_from_json(&mut self, value: &JsonValue) {
        self.merge_essential_fields(essential_projection_fields(value));
    }

    pub fn merge_essential_fields(&mut self, fields: JsonValue) {
        merge_projection_values(&mut self.essential_inline, fields);
    }
}

/// Returns the small control-plane subset that must survive model projection
/// and wrapper conversion. Payload arrays and text remain spillable unless the
/// producer gives them one of these explicit semantic identities.
pub fn essential_projection_fields(value: &JsonValue) -> JsonValue {
    // Producer-owned metadata is merged separately. Generic key heuristics
    // are only a bounded fallback, never authority to pin application state.
    essential_projection_fields_bounded(value, 0, &mut 8192)
}

fn small_control_value(value: &JsonValue, depth: usize, remaining: &mut usize) -> bool {
    if depth > 4 || *remaining < 2 {
        return false;
    }
    *remaining -= 2;
    match value {
        JsonValue::String(text) => {
            let bytes = text.len().saturating_mul(6);
            if bytes > *remaining {
                return false;
            }
            *remaining -= bytes;
            true
        }
        JsonValue::Array(values) => {
            values.len() <= 16
                && values
                    .iter()
                    .all(|value| small_control_value(value, depth + 1, remaining))
        }
        JsonValue::Object(fields) => {
            fields.len() <= 16
                && fields.iter().all(|(key, value)| {
                    let bytes = key.len().saturating_mul(6).saturating_add(4);
                    if bytes > *remaining {
                        return false;
                    }
                    *remaining -= bytes;
                    small_control_value(value, depth + 1, remaining)
                })
        }
        _ => {
            let bytes = value.to_string().len();
            if bytes > *remaining {
                return false;
            }
            *remaining -= bytes;
            true
        }
    }
}

fn essential_projection_fields_bounded(
    value: &JsonValue,
    depth: usize,
    remaining: &mut usize,
) -> JsonValue {
    if depth > 16 || *remaining < 2 {
        return JsonValue::Null;
    }
    *remaining -= 2;
    match value {
        JsonValue::Object(object) => JsonValue::Object(
            object
                .iter()
                // A lexically earlier payload must not consume the bounded
                // scan before this object's small control fields. The MCP
                // control envelope likewise precedes content and provider
                // metadata. Keep the same depth/byte bounds; no payload is
                // promoted merely because its traversal has higher priority.
                .filter(|(key, _)| is_essential_key(key))
                .chain(
                    object
                        .iter()
                        .filter(|(key, _)| key.as_str() == "structuredContent"),
                )
                .chain(object.iter().filter(|(key, _)| {
                    !is_essential_key(key) && key.as_str() != "structuredContent"
                }))
                .filter_map(|(key, value)| {
                    let key_bytes = key.len().saturating_mul(6).saturating_add(4);
                    if key_bytes > *remaining {
                        return None;
                    }
                    if is_essential_key(key) {
                        if small_control_value(value, 0, &mut 2048) {
                            let bytes = value.to_string().len();
                            if bytes <= 2048 && bytes + key_bytes <= *remaining {
                                *remaining -= bytes + key_bytes;
                                return Some((key.clone(), value.clone()));
                            }
                        }
                    }
                    *remaining -= key_bytes;
                    let nested = essential_projection_fields_bounded(value, depth + 1, remaining);
                    (!is_empty_projection(&nested)).then(|| (key.clone(), nested))
                })
                .collect(),
        ),
        JsonValue::Array(values) => {
            // Null placeholders retain source indexes. If even placeholders
            // cannot fit, leave the entire array in the canonical artifact.
            let placeholders = values.len().saturating_mul(5);
            if placeholders > *remaining {
                return JsonValue::Null;
            }
            *remaining -= placeholders;
            let projected = values
                .iter()
                .map(|value| essential_projection_fields_bounded(value, depth + 1, remaining))
                .map(|value| {
                    if is_empty_projection(&value) {
                        JsonValue::Null
                    } else {
                        value
                    }
                })
                .collect::<Vec<_>>();
            if projected.iter().all(is_empty_projection) {
                JsonValue::Null
            } else {
                JsonValue::Array(projected)
            }
        }
        _ => JsonValue::Null,
    }
}

fn merge_projection_values(target: &mut JsonValue, incoming: JsonValue) {
    match (target, incoming) {
        (JsonValue::Object(target), JsonValue::Object(incoming)) => {
            for (key, value) in incoming {
                match target.get_mut(&key) {
                    Some(existing) => merge_projection_values(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, incoming) if is_empty_projection(target) => *target = incoming,
        _ => {}
    }
}

fn is_empty_projection(value: &JsonValue) -> bool {
    matches!(value, JsonValue::Null)
        || matches!(value, JsonValue::Object(object) if object.is_empty())
        || matches!(value, JsonValue::Array(values) if values.is_empty())
}

fn is_essential_key(key: &str) -> bool {
    key == "id"
        || key.ends_with("_id")
        || key.ends_with("Id")
        || [
            "abort_reason",
            "aborted",
            "abortreason",
            "action",
            "agent_status",
            "attempt_state",
            "command_was_executed",
            "complete",
            "content_identity",
            "coverage_status",
            "cursor",
            "effective_walk_options",
            "error",
            "failure_signature",
            "gate",
            "gates",
            "index_status",
            "lease_state",
            "limits_clamped",
            "matched_tests",
            "next_action",
            "next_cursor",
            "next_cursors",
            "next_required_action",
            "nextcursor",
            "nextcursors",
            "nextrequiredaction",
            "not_exercised",
            "omitted_count",
            "omitted_error_count",
            "omitted_item_counts",
            "omitted_result_count",
            "omittedcount",
            "omittederrorcount",
            "omittedresultcount",
            "outcome",
            "pending_gates",
            "previous_cursor",
            "previous_status",
            "previouscursor",
            "process_status",
            "raw_output_artifact_retention_limit_hit",
            "raw_output_artifact_retention_limit_reason",
            "remaining_count",
            "remaining_match_count",
            "remaining_result_count",
            "remainingcount",
            "remainingmatchcount",
            "remainingresultcount",
            "retention_limit_hit",
            "retention_limit_reason",
            "retentionlimithit",
            "retentionlimitreason",
            "selected_test_count",
            "sha256",
            "state",
            "state_revision",
            "status",
            "truncated",
            "truncated_count",
            "truncatedcount",
            "unchanged",
            "unresolved_gates",
        ]
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

impl ToolOutput for JsonToolOutput {

    fn code_mode_failure_is_error(&self) -> bool {
        !self.code_mode_failure_as_data
    }

    fn requires_canonical_artifact(&self) -> bool {
        self.canonical_artifact_required
    }

    fn log_preview(&self) -> String {
        telemetry_preview(self.serialized())
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

    fn sampling_request_signal(&self) -> Option<JsonValue> {
        if let Some(signal) = &self.sampling_signal {
            return Some(signal.clone());
        }
        let context = self.outcome_context();
        let failure_evidence = match context.outcome {
            ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => true,
            ToolOutputOutcome::Skipped => {
                context.skip_disposition
                    == Some(ToolOutputSkipDisposition::BlockingRequiredOperation)
            }
            ToolOutputOutcome::Success | ToolOutputOutcome::Yielded => false,
        };
        if !failure_evidence {
            return None;
        }
        self.value
            .get("failure_signature")
            .and_then(JsonValue::as_str)
            .filter(|fingerprint| !fingerprint.is_empty())
            .map(|fingerprint| {
                serde_json::json!({
                    "failure": { "fingerprint": fingerprint },
                })
            })
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        let mut metadata = ToolOutputProjectionMetadata::from_json_with_serialized(
            &self.value,
            self.success.unwrap_or(true),
            None,
            self.serialized().to_owned(),
        );
        metadata.outcome = self.outcome_for_logging();
        Some(metadata)
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(CanonicalToolResult::json(self.value.clone()))
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let output = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(self.serialized().to_owned()),
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
        if self.structured_content.as_ref().is_none_or(JsonValue::is_null)
            && self.content.iter().all(|item| item["type"] == "text"
                && item["text"].is_string()
                && item.get("_meta").and_then(|meta| meta.get("codex/encryptedContent"))
                    .and_then(JsonValue::as_bool) != Some(true))
        {
            // This provider shape is just a newline join. Borrow text directly
            // instead of constructing and flattening a full provider payload.
            let error = (!self.success()).then_some("MCP tool reported an error.");
            return telemetry_preview_joined_parts(error.into_iter().chain(
                self.content.iter().filter_map(|item| item["text"].as_str()),
            )).unwrap_or_default();
        }
        let output = self.as_function_call_output_payload();
        telemetry_preview_body(&output.body).unwrap_or_else(|| {
            telemetry_preview_json(&output).unwrap_or_else(|error| error.to_string())
        })
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
            essential_inline: {
                let mut essential = serde_json::json!({
                "is_error": self.is_error,
                "content_items": self.content.len(),
                "has_structured_content": self.structured_content.is_some(),
                "has_meta": self.meta.is_some(),
                });
                merge_projection_values(&mut essential, essential_projection_fields(&value));
                essential
            },
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
                    | codex_protocol::models::ContentItem::OutputText { text }
                    | codex_protocol::models::ContentItem::Refusal { refusal: text } => {
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
            omitted_result_count,
            ..
        } => serde_json::json!({
            "status": status,
            "execution": execution,
            "tools": tools,
            "omitted_result_count": omitted_result_count,
        }),
        ResponseInputItem::McpToolCallOutput { output, .. } => serde_json::to_value(output)
            .unwrap_or_else(|err| {
                JsonValue::String(format!("failed to serialize mcp result: {err}"))
            }),
    }
}

fn content_items_to_code_mode_result(items: &[FunctionCallOutputContentItem]) -> JsonValue {
    let mut output = String::new();
    for text in items.iter().filter_map(|item| match item {
        FunctionCallOutputContentItem::InputText { text } if !text.trim().is_empty() => {
            Some(text.as_str())
        }
        FunctionCallOutputContentItem::InputImage { image_url, .. }
            if !image_url.trim().is_empty() =>
        {
            Some(image_url.as_str())
        }
        FunctionCallOutputContentItem::InputText { .. }
        | FunctionCallOutputContentItem::InputImage { .. }
        | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
    }) {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(text);
    }
    JsonValue::String(output)
}

/// Render only the prefix telemetry can retain, without flattening the body.
pub fn telemetry_preview_body(body: &FunctionCallOutputBody) -> Option<String> {
    match body {
        FunctionCallOutputBody::Text(text) => Some(telemetry_preview(text)),
        FunctionCallOutputBody::ContentItems(items) => telemetry_preview_text_parts(
            items.iter().filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            }),
        ),
    }
}

pub fn telemetry_preview_text_parts<'a>(
    parts: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    telemetry_preview_joined_parts(parts.into_iter().filter(|part| !part.trim().is_empty()))
}

fn telemetry_preview_joined_parts<'a>(
    parts: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    // Four extra bytes admit any UTF-8 character straddling the preview limit
    // and let the existing formatter distinguish a truncated prefix exactly.
    let limit = TELEMETRY_PREVIEW_MAX_BYTES + 4;
    let mut prefix = String::with_capacity(limit);
    let mut found = false;
    for part in parts {
        if found {
            prefix.push('\n');
        }
        found = true;
        let available = limit.saturating_sub(prefix.len());
        prefix.push_str(&part[..part.floor_char_boundary(available.min(part.len()))]);
        if prefix.len() >= TELEMETRY_PREVIEW_MAX_BYTES + 1 {
            break;
        }
    }
    found.then(|| telemetry_preview(&prefix))
}

/// Stop JSON serialization once enough bytes exist to reproduce the preview.
pub fn telemetry_preview_json(value: &impl Serialize) -> Result<String, serde_json::Error> {
    struct PrefixWriter {
        bytes: Vec<u8>,
        overflowed: bool,
    }
    impl std::io::Write for PrefixWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let available = (TELEMETRY_PREVIEW_MAX_BYTES + 4).saturating_sub(self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..bytes.len().min(available)]);
            if bytes.len() > available {
                self.overflowed = true;
                return Err(std::io::Error::other("telemetry prefix complete"));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = PrefixWriter {
        bytes: Vec::with_capacity(TELEMETRY_PREVIEW_MAX_BYTES + 4),
        overflowed: false,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if !writer.overflowed {
        result?;
    }
    // A partial final code point is beyond the retained preview boundary.
    let prefix = String::from_utf8_lossy(&writer.bytes);
    Ok(telemetry_preview(&prefix))
}

pub fn telemetry_preview(content: &str) -> String {
    let truncated_slice = &content[..content.floor_char_boundary(TELEMETRY_PREVIEW_MAX_BYTES)];
    let truncated_by_bytes = truncated_slice.len() < content.len();
    if !truncated_by_bytes
        && content
            .lines()
            .take(TELEMETRY_PREVIEW_MAX_LINES + 1)
            .count()
            <= TELEMETRY_PREVIEW_MAX_LINES
    {
        return content.to_string();
    }

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
