use codex_protocol::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::CellId;
use crate::CodeModeToolKind;
use crate::FunctionCallOutputContentItem;
use crate::ToolDefinition;

pub const DEFAULT_EXEC_YIELD_TIME_MS: u64 = 10_000;
pub const DEFAULT_WAIT_YIELD_TIME_MS: u64 = 10_000;
/// Reserved internal wait value that asks the runtime to wake only when a cell
/// produces output or reaches a terminal state. Model-facing schemas cap
/// ordinary yield intervals far below this value.
pub const OWNER_HELD_STATE_CHANGE_YIELD_TIME_MS: u64 = u64::MAX;
/// Reserved owner wait that buffers output until explicit yield or completion.
/// The caller owns its interruption and idle deadline.
pub const OWNER_HELD_DECISION_YIELD_TIME_MS: u64 = u64::MAX - 1;
/// Default coherent evidence-packet budget when no per-call limit is requested.
pub const DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL: usize = 10_000;
/// Maximum coherent evidence-packet budget accepted from an explicit request.
/// The core also caps this at the active model's hard output limit.
pub const MAX_OUTPUT_TOKENS_PER_EXEC_CALL: usize = 10_000;
/// Output budget of a nested command result returned to a script. Printing
/// the result JSON-escapes its output and adds lifecycle fields, so the budget
/// stays below the cell cap; otherwise the cell cuts the result a second time.
pub const MAX_NESTED_COMMAND_OUTPUT_TOKENS: usize = MAX_OUTPUT_TOKENS_PER_EXEC_CALL * 4 / 5;
/// Hard deadline applied to a single nested tool call when the host supplies no
/// per-cell default. A host-supplied default must still leave room for the
/// longest wait its own tools can be asked to perform.
pub const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;
/// Ceiling for both the per-cell default and an explicit per-call
/// `{timeout_ms}` override.
pub const MAX_TOOL_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecuteRequest {
    pub tool_call_id: String,
    pub enabled_tools: Vec<ToolDefinition>,
    pub source: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
    /// Per-cell hard deadline for a nested tool call that does not pass an
    /// explicit `{timeout_ms}`. Absent falls back to `DEFAULT_TOOL_TIMEOUT_MS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tool_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WaitRequest {
    pub cell_id: CellId,
    pub yield_time_ms: u64,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum WaitOutcome {
    LiveCell(RuntimeResponse),
    MissingCell(RuntimeResponse),
}

impl From<WaitOutcome> for RuntimeResponse {
    /// Discards cell existence information at the response presentation boundary.
    /// Perform existence-sensitive bookkeeping or recovery before converting.
    fn from(outcome: WaitOutcome) -> Self {
        match outcome {
            WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response) => response,
        }
    }
}

/// Output rejected by runtime admission is discarded, not retained for recovery.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct OutputLoss {
    pub discarded_items: u64,
    /// A lower bound because full buffers may reject values before conversion.
    pub discarded_bytes_lower_bound: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum RuntimeResponse {
    Yielded {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
    },
    ExplicitYield {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
    },
    Terminated {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
    },
    Result {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
        error_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_loss: Option<OutputLoss>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CodeModeNestedToolCall {
    pub cell_id: CellId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    pub runtime_tool_call_id: String,
    pub tool_name: ToolName,
    pub tool_kind: CodeModeToolKind,
    /// Missing input is distinct from an explicitly supplied JSON null.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_input"
    )]
    pub input: Option<JsonValue>,
    /// Instant at which the runtime's wrapper timeout fires for this call,
    /// expressed on the receiving process's own clock.
    ///
    /// Every stage before observation is charged against it, so a handler that
    /// can wait should complete cooperatively before it rather than be
    /// cancelled at it. Not serialized: an `Instant` is process local, so the
    /// out-of-process wire form carries a shared-monotonic reading instead and
    /// the receiver converts once, at receipt, into this field.
    #[serde(skip)]
    pub nested_deadline: Option<std::time::Instant>,
}

pub(crate) fn deserialize_present_input<'de, D>(
    deserializer: D,
) -> Result<Option<JsonValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    JsonValue::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::CodeModeNestedToolCall;
    use super::ExecuteRequest;
    use serde_json::json;

    fn execute_request() -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call-1".to_string(),
            enabled_tools: Vec::new(),
            source: "text('ok');".to_string(),
            yield_time_ms: None,
            max_output_tokens: None,
            default_tool_timeout_ms: None,
        }
    }

    #[test]
    fn execute_request_round_trips_with_and_without_a_default_tool_timeout() {
        let absent = execute_request();
        let encoded = serde_json::to_value(&absent).expect("encode");
        assert!(
            encoded.get("default_tool_timeout_ms").is_none(),
            "an absent default must not be serialized: {encoded}"
        );
        assert_eq!(
            serde_json::from_value::<ExecuteRequest>(encoded).expect("decode"),
            absent
        );

        let present = ExecuteRequest {
            default_tool_timeout_ms: Some(75_000),
            ..execute_request()
        };
        let encoded = serde_json::to_value(&present).expect("encode");
        assert_eq!(encoded["default_tool_timeout_ms"], json!(75_000));
        assert_eq!(
            serde_json::from_value::<ExecuteRequest>(encoded).expect("decode"),
            present
        );
    }

    #[test]
    fn nested_tool_input_decodes_field_presence_from_json() {
        let absent = json!({
            "cell_id": "cell-input",
            "runtime_tool_call_id": "nested-call",
            "tool_name": { "name": "example", "namespace": null },
            "tool_kind": "function"
        });
        let mut explicit_null = absent.clone();
        explicit_null["input"] = json!(null);
        let mut object = absent.clone();
        object["input"] = json!({ "value": null });

        for (encoded, expected) in [
            (absent, None),
            (explicit_null, Some(json!(null))),
            (object, Some(json!({ "value": null }))),
        ] {
            let invocation: CodeModeNestedToolCall =
                serde_json::from_value(encoded).expect("decode invocation");
            assert_eq!(invocation.input, expected);
        }
    }
}
