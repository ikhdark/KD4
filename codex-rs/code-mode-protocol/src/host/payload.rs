use std::num::TryFromIntError;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::CellId;
use crate::CodeModeNestedToolCall;
use crate::CodeModeToolKind;
use crate::ExecuteRequest;
use crate::FunctionCallOutputContentItem;
use crate::ImageDetail;
use crate::RuntimeResponse;
use crate::SharedMonotonicNanos;
use crate::ToolDefinition;
use crate::WaitOutcome;
use crate::WaitRequest;
use crate::shared_monotonic_now;

/// A cell identifier with a wire representation owned by protocol V1.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WireCellId(String);

impl WireCellId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<CellId> for WireCellId {
    fn from(value: CellId) -> Self {
        Self(value.as_str().to_string())
    }
}

impl From<&CellId> for WireCellId {
    fn from(value: &CellId) -> Self {
        Self(value.as_str().to_string())
    }
}

impl From<WireCellId> for CellId {
    fn from(value: WireCellId) -> Self {
        Self::new(value.0)
    }
}

/// The V1 wire representation of a tool's stable name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolName {
    pub name: String,
    pub namespace: Option<String>,
}

impl From<ToolName> for WireToolName {
    fn from(value: ToolName) -> Self {
        Self {
            name: value.name,
            namespace: value.namespace,
        }
    }
}

impl From<WireToolName> for ToolName {
    fn from(value: WireToolName) -> Self {
        Self::new(value.namespace, value.name)
    }
}

/// The tool invocation shape supported by protocol V1.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireToolKind {
    Function,
    Freeform,
}

impl From<CodeModeToolKind> for WireToolKind {
    fn from(value: CodeModeToolKind) -> Self {
        match value {
            CodeModeToolKind::Function => Self::Function,
            CodeModeToolKind::Freeform => Self::Freeform,
        }
    }
}

impl From<WireToolKind> for CodeModeToolKind {
    fn from(value: WireToolKind) -> Self {
        match value {
            WireToolKind::Function => Self::Function,
            WireToolKind::Freeform => Self::Freeform,
        }
    }
}

/// A V1 tool definition embedded in an execute request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolDefinition {
    pub name: String,
    pub tool_name: WireToolName,
    pub description: String,
    pub kind: WireToolKind,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
    /// Default for this tool only; an explicit per-call timeout takes precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout_ms: Option<u64>,
}

impl From<ToolDefinition> for WireToolDefinition {
    fn from(value: ToolDefinition) -> Self {
        Self {
            name: value.name,
            tool_name: value.tool_name.into(),
            description: value.description,
            kind: value.kind.into(),
            input_schema: value.input_schema,
            output_schema: value.output_schema,
            default_timeout_ms: value.default_timeout_ms,
        }
    }
}

impl From<WireToolDefinition> for ToolDefinition {
    fn from(value: WireToolDefinition) -> Self {
        Self {
            name: value.name,
            tool_name: value.tool_name.into(),
            description: value.description,
            kind: value.kind.into(),
            input_schema: value.input_schema,
            output_schema: value.output_schema,
            default_timeout_ms: value.default_timeout_ms,
        }
    }
}

/// The complete execute request shape supported by protocol V1.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireExecuteRequest {
    pub tool_call_id: String,
    pub enabled_tools: Vec<WireToolDefinition>,
    pub source: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<i32>,
    /// Absent on peers that predate the per-cell nested-tool deadline; the
    /// runtime then falls back to `DEFAULT_TOOL_TIMEOUT_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tool_timeout_ms: Option<u64>,
}

impl TryFrom<ExecuteRequest> for WireExecuteRequest {
    type Error = TryFromIntError;

    fn try_from(value: ExecuteRequest) -> Result<Self, Self::Error> {
        let max_output_tokens = value.max_output_tokens.map(i32::try_from).transpose()?;
        Ok(Self {
            tool_call_id: value.tool_call_id,
            enabled_tools: value.enabled_tools.into_iter().map(Into::into).collect(),
            source: value.source,
            yield_time_ms: value.yield_time_ms,
            max_output_tokens,
            default_tool_timeout_ms: value.default_tool_timeout_ms,
        })
    }
}

impl TryFrom<WireExecuteRequest> for ExecuteRequest {
    type Error = TryFromIntError;

    fn try_from(value: WireExecuteRequest) -> Result<Self, Self::Error> {
        let max_output_tokens = value.max_output_tokens.map(usize::try_from).transpose()?;
        Ok(Self {
            tool_call_id: value.tool_call_id,
            enabled_tools: value.enabled_tools.into_iter().map(Into::into).collect(),
            source: value.source,
            yield_time_ms: value.yield_time_ms,
            max_output_tokens,
            default_tool_timeout_ms: value.default_tool_timeout_ms,
        })
    }
}

/// The complete wait request shape supported by protocol V1.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireWaitRequest {
    pub cell_id: WireCellId,
    pub yield_time_ms: u64,
}

impl From<WaitRequest> for WireWaitRequest {
    fn from(value: WaitRequest) -> Self {
        Self {
            cell_id: value.cell_id.into(),
            yield_time_ms: value.yield_time_ms,
        }
    }
}

impl From<WireWaitRequest> for WaitRequest {
    fn from(value: WireWaitRequest) -> Self {
        Self {
            cell_id: value.cell_id.into(),
            yield_time_ms: value.yield_time_ms,
        }
    }
}

/// Image detail values accepted in a V1 runtime response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WireImageDetail {
    Auto,
    Low,
    High,
    Original,
}

impl From<ImageDetail> for WireImageDetail {
    fn from(value: ImageDetail) -> Self {
        match value {
            ImageDetail::Auto => Self::Auto,
            ImageDetail::Low => Self::Low,
            ImageDetail::High => Self::High,
            ImageDetail::Original => Self::Original,
        }
    }
}

impl From<WireImageDetail> for ImageDetail {
    fn from(value: WireImageDetail) -> Self {
        match value {
            WireImageDetail::Auto => Self::Auto,
            WireImageDetail::Low => Self::Low,
            WireImageDetail::High => Self::High,
            WireImageDetail::Original => Self::Original,
        }
    }
}

/// One output item emitted by a V1 runtime response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum WireContentItem {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<WireImageDetail>,
    },
}

impl From<FunctionCallOutputContentItem> for WireContentItem {
    fn from(value: FunctionCallOutputContentItem) -> Self {
        match value {
            FunctionCallOutputContentItem::InputText { text } => Self::InputText { text },
            FunctionCallOutputContentItem::InputImage { image_url, detail } => Self::InputImage {
                image_url,
                detail: detail.map(Into::into),
            },
        }
    }
}

impl From<WireContentItem> for FunctionCallOutputContentItem {
    fn from(value: WireContentItem) -> Self {
        match value {
            WireContentItem::InputText { text } => Self::InputText { text },
            WireContentItem::InputImage { image_url, detail } => Self::InputImage {
                image_url,
                detail: detail.map(Into::into),
            },
        }
    }
}

/// Runtime output returned over the V1 host connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum WireRuntimeResponse {
    Yielded {
        cell_id: WireCellId,
        content_items: Vec<WireContentItem>,
    },
    ExplicitYield {
        cell_id: WireCellId,
        content_items: Vec<WireContentItem>,
    },
    Terminated {
        cell_id: WireCellId,
        content_items: Vec<WireContentItem>,
    },
    Result {
        cell_id: WireCellId,
        content_items: Vec<WireContentItem>,
        error_text: Option<String>,
    },
}

impl From<RuntimeResponse> for WireRuntimeResponse {
    fn from(value: RuntimeResponse) -> Self {
        match value {
            RuntimeResponse::Yielded {
                cell_id,
                content_items,
            } => Self::Yielded {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            RuntimeResponse::ExplicitYield {
                cell_id,
                content_items,
            } => Self::ExplicitYield {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            RuntimeResponse::Terminated {
                cell_id,
                content_items,
            } => Self::Terminated {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            RuntimeResponse::Result {
                cell_id,
                content_items,
                error_text,
            } => Self::Result {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
                error_text,
            },
        }
    }
}

impl From<WireRuntimeResponse> for RuntimeResponse {
    fn from(value: WireRuntimeResponse) -> Self {
        match value {
            WireRuntimeResponse::Yielded {
                cell_id,
                content_items,
            } => Self::Yielded {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            WireRuntimeResponse::ExplicitYield {
                cell_id,
                content_items,
            } => Self::ExplicitYield {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            WireRuntimeResponse::Terminated {
                cell_id,
                content_items,
            } => Self::Terminated {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            WireRuntimeResponse::Result {
                cell_id,
                content_items,
                error_text,
            } => Self::Result {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
                error_text,
            },
        }
    }
}

/// Whether a waited-for cell remained live in protocol V1.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum WireWaitOutcome {
    LiveCell(WireRuntimeResponse),
    MissingCell(WireRuntimeResponse),
}

impl From<WaitOutcome> for WireWaitOutcome {
    fn from(value: WaitOutcome) -> Self {
        match value {
            WaitOutcome::LiveCell(response) => Self::LiveCell(response.into()),
            WaitOutcome::MissingCell(response) => Self::MissingCell(response.into()),
        }
    }
}

impl From<WireWaitOutcome> for WaitOutcome {
    fn from(value: WireWaitOutcome) -> Self {
        match value {
            WireWaitOutcome::LiveCell(response) => Self::LiveCell(response.into()),
            WireWaitOutcome::MissingCell(response) => Self::MissingCell(response.into()),
        }
    }
}

/// A nested tool invocation sent over the V1 host connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireNestedToolCall {
    pub cell_id: WireCellId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub runtime_tool_call_id: String,
    pub tool_name: WireToolName,
    pub tool_kind: WireToolKind,
    /// Omit absent input; a present null remains an explicit tool argument.
    /// Both peers must preserve presence: legacy senders encoded absent input
    /// as null, which cannot be distinguished from an explicit null on receipt.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::runtime::deserialize_present_input"
    )]
    pub input: Option<JsonValue>,
    /// The wrapper deadline as a reading of the machine-wide monotonic clock.
    ///
    /// Both peers run on one machine and read one monotonic source, so the
    /// receiver recovers the sender's own deadline by measuring the offset
    /// against that source at receipt. A wall-clock adjustment in between
    /// cannot move it, and transit time is charged to the budget rather than
    /// refunded. Absent when the platform exposes no shared source, or when the
    /// sending peer predates this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_shared_monotonic_nanos: Option<SharedMonotonicNanos>,
    /// Budget left when the call was sent, used only when no shared monotonic
    /// source is available.
    ///
    /// This deliberately does **not** preserve the original budget: it is
    /// charged from receipt, so transit time goes uncharged and the receiver
    /// may observe slightly longer than the sender's wrapper allows. The
    /// wrapper's hard timeout remains the fallback there, and the reported
    /// cause still comes from whichever origin recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_ms_at_send: Option<u64>,
}

impl From<CodeModeNestedToolCall> for WireNestedToolCall {
    fn from(value: CodeModeNestedToolCall) -> Self {
        let (deadline_shared_monotonic_nanos, remaining_ms_at_send) = value
            .nested_deadline
            .map(encode_nested_deadline)
            .unwrap_or((None, None));
        Self {
            cell_id: value.cell_id.into(),
            parent_tool_call_id: value.parent_tool_call_id,
            runtime_tool_call_id: value.runtime_tool_call_id,
            tool_name: value.tool_name.into(),
            tool_kind: value.tool_kind.into(),
            input: value.input,
            deadline_shared_monotonic_nanos,
            remaining_ms_at_send,
        }
    }
}

impl From<WireNestedToolCall> for CodeModeNestedToolCall {
    fn from(value: WireNestedToolCall) -> Self {
        Self {
            cell_id: value.cell_id.into(),
            parent_tool_call_id: value.parent_tool_call_id,
            runtime_tool_call_id: value.runtime_tool_call_id,
            tool_name: value.tool_name.into(),
            tool_kind: value.tool_kind.into(),
            input: value.input,
            nested_deadline: decode_nested_deadline(
                value.deadline_shared_monotonic_nanos,
                value.remaining_ms_at_send,
            ),
        }
    }
}

/// Renders a local deadline for transport. Prefers the shared monotonic
/// timeline; the remaining-duration form is only a fallback.
fn encode_nested_deadline(deadline: Instant) -> (Option<SharedMonotonicNanos>, Option<u64>) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let remaining_ms = u64::try_from(remaining.as_millis()).ok();
    let Some(now_nanos) = shared_monotonic_now() else {
        return (None, remaining_ms);
    };
    let deadline_nanos = u64::try_from(remaining.as_nanos())
        .ok()
        .and_then(|remaining_nanos| now_nanos.checked_add(remaining_nanos));
    // Always send the fallback too: the receiving peer may be on a platform or
    // build without a shared source even when this one has it.
    (deadline_nanos, remaining_ms)
}

/// Recovers the sender's deadline on this process's own clock.
///
/// The monotonic path charges transit time to the budget. The fallback path
/// does not, and is documented as not preserving the original budget.
fn decode_nested_deadline(
    deadline_shared_monotonic_nanos: Option<SharedMonotonicNanos>,
    remaining_ms_at_send: Option<u64>,
) -> Option<Instant> {
    let now = Instant::now();
    if let Some(deadline_nanos) = deadline_shared_monotonic_nanos
        && let Some(now_nanos) = shared_monotonic_now()
    {
        return Some(now + Duration::from_nanos(deadline_nanos.saturating_sub(now_nanos)));
    }
    remaining_ms_at_send.map(|remaining_ms| now + Duration::from_millis(remaining_ms))
}
