//! We do not do true JSON-RPC 2.0, as we neither send nor expect the
//! "jsonrpc": "2.0" field.

use codex_protocol::protocol::W3cTraceContext;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use ts_rs::TS;

pub const JSONRPC_VERSION: &str = "2.0";
pub const OVERLOADED_ERROR_CODE: i64 = -32001;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum OverloadReason {
    TransportIngress,
    SerializedRequestQueue,
    InProcessRequestQueue,
    InProcessServerRequestQueue,
    RemoteAppServerEventQueue,
    InProcessAppServerEventQueue,
    InFlightTaskCapacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct OverloadErrorData {
    pub reason: OverloadReason,
    pub retryable: bool,
}

pub fn overloaded_error(reason: OverloadReason, message: impl Into<String>) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: OVERLOADED_ERROR_CODE,
        message: message.into(),
        data: Some(serde_json::json!({
            "reason": reason,
            "retryable": true,
        })),
    }
}

#[derive(
    Debug, Clone, PartialEq, PartialOrd, Ord, Deserialize, Serialize, Hash, Eq, JsonSchema, TS,
)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    #[ts(type = "number")]
    Integer(i64),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => f.write_str(value),
            Self::Integer(value) => write!(f, "{value}"),
        }
    }
}

pub type Result = serde_json::Value;

/// Refers to any valid JSON-RPC object that can be decoded off the wire, or encoded to be sent.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(untagged)]
pub enum JSONRPCMessage {
    Request(JSONRPCRequest),
    Notification(JSONRPCNotification),
    Response(JSONRPCResponse),
    Error(JSONRPCError),
}

/// A request that expects a response.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct JSONRPCRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub params: Option<serde_json::Value>,
    /// Optional W3C Trace Context for distributed tracing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trace: Option<W3cTraceContext>,
}

/// A notification which does not expect a response.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct JSONRPCNotification {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub params: Option<serde_json::Value>,
}

/// A successful (non-error) response to a request.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct JSONRPCResponse {
    pub id: RequestId,
    pub result: Result,
}

/// A response to a request that indicates an error occurred.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct JSONRPCError {
    pub error: JSONRPCErrorError,
    pub id: RequestId,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct JSONRPCErrorError {
    pub code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub data: Option<serde_json::Value>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overloaded_error_has_one_code_and_typed_retryable_data() {
        let error = overloaded_error(OverloadReason::TransportIngress, "queue full");

        assert_eq!(error.code, OVERLOADED_ERROR_CODE);
        assert_eq!(error.message, "queue full");
        assert_eq!(
            serde_json::from_value::<OverloadErrorData>(error.data.expect("overload data"))
                .expect("valid overload data"),
            OverloadErrorData {
                reason: OverloadReason::TransportIngress,
                retryable: true,
            }
        );
    }
}
