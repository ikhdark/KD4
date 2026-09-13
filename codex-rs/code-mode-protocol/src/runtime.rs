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
/// Default coherent evidence-packet budget when no per-call limit is requested.
pub const DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL: usize = 10_000;
/// Maximum coherent evidence-packet budget accepted from an explicit request.
/// The core also caps this at the active model's hard output limit.
pub const MAX_OUTPUT_TOKENS_PER_EXEC_CALL: usize = 10_000;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecuteRequest {
    pub tool_call_id: String,
    pub enabled_tools: Vec<ToolDefinition>,
    pub source: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
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
    use serde_json::json;

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
