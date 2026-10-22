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
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum JSONRPCMessage {
    Request(JSONRPCRequest),
    Notification(JSONRPCNotification),
    Response(JSONRPCResponse),
    Error(JSONRPCError),
}

impl JsonSchema for JSONRPCMessage {
    fn schema_name() -> String {
        "JSONRPCMessage".into()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        // Presence, not the decoded value, selects the runtime envelope. Only
        // reserved fields are exclusive; unrelated extension fields stay legal.
        let branches = [
            (
                generator.subschema_for::<JSONRPCRequest>(),
                &["result", "error"][..],
            ),
            (
                generator.subschema_for::<JSONRPCNotification>(),
                &["id", "result", "error"][..],
            ),
            (
                generator.subschema_for::<JSONRPCResponse>(),
                &["method", "error"][..],
            ),
            (
                generator.subschema_for::<JSONRPCError>(),
                &["method", "result"][..],
            ),
        ]
        .into_iter()
        .map(|(shape, forbidden)| {
            serde_json::json!({
                "allOf": [shape, {"not": {"anyOf": forbidden.iter().map(|field| {
                    serde_json::json!({"required": [field]})
                }).collect::<Vec<_>>()}}]
            })
        })
        .collect::<Vec<_>>();
        // Statically valid; `expect_used` is denied in library code, and a
        // degraded schema fails the envelope test and fixture checks.
        serde_json::from_value(serde_json::json!({
            "description": "Refers to any valid JSON-RPC object that can be decoded off the wire, or encoded to be sent.",
            "anyOf": branches,
        })).unwrap_or(schemars::schema::Schema::Bool(true))
    }
}

impl<'de> Deserialize<'de> for JSONRPCMessage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;

        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("expected RPC object"))?;
        // Select by reserved-field presence before decoding. Invalid IDs must never
        // cause a request to fall through to the notification shape.
        match (
            object.contains_key("method"),
            object.contains_key("id"),
            object.contains_key("result"),
            object.contains_key("error"),
        ) {
            (true, true, false, false) => serde_json::from_value(value).map(Self::Request),
            (true, false, false, false) => serde_json::from_value(value).map(Self::Notification),
            (false, true, true, false) => serde_json::from_value(value).map(Self::Response),
            (false, true, false, true) => serde_json::from_value(value).map(Self::Error),
            _ => {
                return Err(D::Error::custom(
                    "invalid or conflicting RPC envelope fields",
                ));
            }
        }
        .map_err(D::Error::custom)
    }
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
    fn generated_envelope_schema_matches_reserved_field_decoder() {
        let schema = serde_json::to_value(schemars::schema_for!(JSONRPCMessage)).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for (input, valid) in [
            (
                r#"{"id":1,"method":"future/method","params":null,"extension":true}"#,
                true,
            ),
            (r#"{"method":"future/notification","extension":true}"#, true),
            (r#"{"id":"request","result":null}"#, true),
            (r#"{"id":1,"error":{"code":-1,"message":"failure"}}"#, true),
            (r#"{"id":null,"method":"example"}"#, false),
            (r#"{"id":{},"method":"example"}"#, false),
            (
                r#"{"id":1,"result":null,"error":{"code":-1,"message":"failure"}}"#,
                false,
            ),
            (r#"{"method":"example","result":null}"#, false),
            (r#"{"method":"example","error":null}"#, false),
            (r#"{"id":1}"#, false),
            (r#"{"id":1,"method":null}"#, false),
            (r#"[]"#, false),
        ] {
            let value: serde_json::Value = serde_json::from_str(input).unwrap();
            assert_eq!(
                serde_json::from_value::<JSONRPCMessage>(value.clone()).is_ok(),
                valid,
                "decoder: {input}"
            );
            assert_eq!(validator.is_valid(&value), valid, "schema: {input}");
        }
    }

    #[test]
    fn wire_decoder_rejects_malformed_and_conflicting_envelopes() {
        for input in [
            r#"{"id":{},"method":"example"}"#,
            r#"{"id":null,"method":"example"}"#,
            r#"{"id":1,"result":"success","error":{"code":-1,"message":"failure"}}"#,
            r#"{"method":"example","result":null}"#,
            r#"{"method":"example","error":null}"#,
            r#"{"id":1}"#,
        ] {
            assert!(
                serde_json::from_str::<JSONRPCMessage>(input).is_err(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn wire_decoder_preserves_valid_envelopes_and_ignores_extension_fields() {
        for input in [
            r#"{"id":1,"method":"example","params":null}"#,
            r#"{"method":"example"}"#,
            r#"{"id":"request","result":null}"#,
            r#"{"id":1,"error":{"code":-1,"message":"failure"}}"#,
        ] {
            let expected: JSONRPCMessage = serde_json::from_str(input).unwrap();
            assert_eq!(serde_json::to_value(&expected).unwrap(), {
                let mut value: serde_json::Value = serde_json::from_str(input).unwrap();
                value.as_object_mut().unwrap().remove("params");
                value
            });
            let mut extended: serde_json::Value = serde_json::from_str(input).unwrap();
            extended["extension"] = serde_json::json!(true);
            assert_eq!(
                serde_json::from_value::<JSONRPCMessage>(extended).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn overloaded_error_has_one_code_and_typed_retryable_data() {
        let error = overloaded_error(OverloadReason::TransportIngress, "queue full");

        // Clients match on the numeric wire value, so pin it rather than the constant.
        assert_eq!(error.code, -32001);
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
