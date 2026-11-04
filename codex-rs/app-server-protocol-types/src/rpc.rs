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

const MAX_SAFE_INTEGER_REQUEST_ID: i64 = 9_007_199_254_740_991;
const INVALID_INTEGER_REQUEST_ID: &str =
    "numeric request IDs must be between -9007199254740991 and 9007199254740991; use a string ID outside this range";

/// A request identifier. Numeric IDs must be integers in the inclusive range
/// -9007199254740991 to 9007199254740991 (JavaScript safe integers).
/// Use a string ID outside this range to preserve identity in JavaScript clients.
#[derive(
    Debug, Clone, PartialEq, PartialOrd, Ord, Deserialize, Serialize, Hash, Eq, JsonSchema, TS,
)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    #[ts(type = "number")]
    Integer(
        #[serde(
            deserialize_with = "deserialize_request_id_integer",
            serialize_with = "serialize_request_id_integer"
        )]
        #[schemars(schema_with = "request_id_integer_schema")]
        i64,
    ),
}

fn request_id_integer_is_safe(value: i64) -> bool {
    (-MAX_SAFE_INTEGER_REQUEST_ID..=MAX_SAFE_INTEGER_REQUEST_ID).contains(&value)
}

fn deserialize_request_id_integer<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = i64::deserialize(deserializer)?;
    if !request_id_integer_is_safe(value) {
        return Err(serde::de::Error::custom(INVALID_INTEGER_REQUEST_ID));
    }
    Ok(value)
}

fn serialize_request_id_integer<S>(
    value: &i64,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if !request_id_integer_is_safe(*value) {
        return Err(serde::ser::Error::custom(INVALID_INTEGER_REQUEST_ID));
    }
    serializer.serialize_i64(*value)
}

fn request_id_integer_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema = i64::json_schema(generator).into_object();
    let validation = schema.number.get_or_insert_with(Default::default);
    validation.minimum = Some(-9_007_199_254_740_991.0);
    validation.maximum = Some(9_007_199_254_740_991.0);
    schema.into()
}

impl From<i64> for RequestId {
    /// Preserve integer identity, using a string outside the numeric wire range.
    fn from(value: i64) -> Self {
        if request_id_integer_is_safe(value) {
            Self::Integer(value)
        } else {
            Self::String(value.to_string())
        }
    }
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
    #[ts(as = "RpcRequestShape")]
    Request(JSONRPCRequest),
    #[ts(as = "RpcNotificationShape")]
    Notification(JSONRPCNotification),
    #[ts(as = "RpcResponseShape")]
    Response(JSONRPCResponse),
    #[ts(as = "RpcErrorShape")]
    Error(JSONRPCError),
}

// TypeScript projection of reserved-field exclusions already enforced by the
// decoder and JSON Schema. The index retains unrelated JSON extension fields.
#[derive(TS)]
pub struct RpcRequestShape {
    #[ts(flatten)]
    pub request: JSONRPCRequest,
    #[ts(optional, type = "never")]
    pub result: Option<()>,
    #[ts(optional, type = "never")]
    pub error: Option<()>,
    #[ts(flatten)]
    pub extensions: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(TS)]
pub struct RpcNotificationShape {
    #[ts(flatten)]
    pub notification: JSONRPCNotification,
    #[ts(optional, type = "never")]
    pub id: Option<()>,
    #[ts(optional, type = "never")]
    pub result: Option<()>,
    #[ts(optional, type = "never")]
    pub error: Option<()>,
    #[ts(flatten)]
    pub extensions: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(TS)]
pub struct RpcResponseShape {
    #[ts(flatten)]
    pub response: JSONRPCResponse,
    #[ts(optional, type = "never")]
    pub method: Option<()>,
    #[ts(optional, type = "never")]
    pub error: Option<()>,
    #[ts(flatten)]
    pub extensions: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(TS)]
pub struct RpcErrorShape {
    #[ts(flatten)]
    pub error: JSONRPCError,
    #[ts(optional, type = "never")]
    pub method: Option<()>,
    #[ts(optional, type = "never")]
    pub result: Option<()>,
    #[ts(flatten)]
    pub extensions: std::collections::HashMap<String, serde_json::Value>,
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
        // Statically valid; `expect_used` is denied in library code.
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
        struct EnvelopeVisitor;
        impl<'de> serde::de::Visitor<'de> for EnvelopeVisitor {
            type Value = JSONRPCMessage;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an RPC object with exclusive reserved fields")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                use serde::de::Error;
                let (mut id, mut method, mut params, mut trace, mut result, mut error) =
                    (None, None, None, None, None, None);
                let mut seen = 0_u8;
                while let Some(key) = map.next_key::<String>()? {
                    let bit = match key.as_str() {
                        "id" => 1, "method" => 2, "params" => 4,
                        "trace" => 8, "result" => 16, "error" => 32,
                        _ => 0,
                    };
                    if bit != 0 && seen & bit != 0 {
                        return Err(A::Error::custom(format!("duplicate RPC field: {key}")));
                    }
                    seen |= bit;
                    match key.as_str() {
                        "id" => id = Some(map.next_value::<RequestId>()?),
                        "method" => method = Some(map.next_value::<String>()?),
                        "params" => params = map.next_value::<Option<serde_json::Value>>()?,
                        "trace" => trace = map.next_value::<Option<serde_json::Value>>()?,
                        "result" => result = Some(map.next_value::<serde_json::Value>()?),
                        "error" => error = Some(map.next_value::<JSONRPCErrorError>()?),
                        _ => { map.next_value::<serde::de::IgnoredAny>()?; }
                    }
                }
                match (method, id, result, error) {
                    (Some(method), Some(id), None, None) => Ok(JSONRPCMessage::Request(JSONRPCRequest {
                        id, method, params, trace: trace.map(serde_json::from_value).transpose().map_err(A::Error::custom)?
                    })),
                    (Some(method), None, None, None) => Ok(JSONRPCMessage::Notification(JSONRPCNotification { method, params })),
                    (None, Some(id), Some(result), None) => Ok(JSONRPCMessage::Response(JSONRPCResponse { id, result })),
                    (None, Some(id), None, Some(error)) => Ok(JSONRPCMessage::Error(JSONRPCError { id, error })),
                    _ => Err(A::Error::custom("invalid or conflicting RPC envelope fields")),
                }
            }
        }
        deserializer.deserialize_map(EnvelopeVisitor)
    }
}

/// A request that expects a response.
#[derive(Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
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
#[derive(Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct JSONRPCNotification {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub params: Option<serde_json::Value>,
}

/// A successful (non-error) response to a request.
#[derive(Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
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

#[derive(Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct JSONRPCErrorError {
    pub code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub data: Option<serde_json::Value>,
    pub message: String,
}

// Opaque wire payloads can contain credentials even when typed values redact Debug.
impl fmt::Debug for JSONRPCRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JSONRPCRequest").field("id", &self.id).field("method", &self.method)
            .field("params", &self.params.as_ref().map(|_| "[REDACTED]")).finish_non_exhaustive()
    }
}

impl fmt::Debug for JSONRPCNotification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JSONRPCNotification").field("method", &self.method)
            .field("params", &self.params.as_ref().map(|_| "[REDACTED]")).finish()
    }
}

impl fmt::Debug for JSONRPCResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JSONRPCResponse").field("id", &self.id).field("result", &"[REDACTED]").finish()
    }
}

impl fmt::Debug for JSONRPCErrorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JSONRPCErrorError").field("code", &self.code)
            .field("message", &"[REDACTED]")
            .field("data", &self.data.as_ref().map(|_| "[REDACTED]")).finish()
    }
}

