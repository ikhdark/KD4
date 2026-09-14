//! JSON-RPC wire envelopes used by exec-server.
//!
//! Exec-server uses the Codex JSON-RPC dialect, which omits the
//! `"jsonrpc": "2.0"` field on the wire.

use std::fmt;

use codex_protocol::protocol::W3cTraceContext;
use serde::Deserialize;
use serde::Serialize;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, PartialOrd, Ord, Deserialize, Serialize, Hash, Eq)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
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

/// Any valid exec-server JSON-RPC object that can be decoded from or encoded onto the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum JSONRPCMessage {
    Request(JSONRPCRequest),
    Notification(JSONRPCNotification),
    Response(JSONRPCResponse),
    Error(JSONRPCError),
}

impl<'de> Deserialize<'de> for JSONRPCMessage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Preserve field presence: a null id is invalid, while a null result is a
        // successful response. Deriving this struct also rejects duplicate known
        // fields without discarding additive extension fields through a JSON map.
        fn present<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
        where
            D: serde::Deserializer<'de>,
            T: Deserialize<'de>,
        {
            T::deserialize(deserializer).map(Some)
        }

        // Keep duplicate trace fields visible to the typed decoder. Invalid
        // metadata is still ignored on messages where trace is an extension.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Trace {
            Context(W3cTraceContext),
            Invalid(serde::de::IgnoredAny),
        }

        #[derive(Deserialize)]
        struct Envelope {
            #[serde(default, deserialize_with = "present")]
            id: Option<RequestId>,
            #[serde(default, deserialize_with = "present")]
            method: Option<String>,
            #[serde(default, deserialize_with = "present")]
            result: Option<Result>,
            #[serde(default, deserialize_with = "present")]
            error: Option<JSONRPCErrorError>,
            params: Option<serde_json::Value>,
            // Only requests interpret trace metadata; other message types have
            // historically accepted it as an unknown extension field.
            trace: Option<Trace>,
        }

        struct EnvelopeVisitor;

        impl<'de> serde::de::Visitor<'de> for EnvelopeVisitor {
            type Value = Envelope;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an exec-server JSON-RPC object")
            }

            fn visit_map<A>(self, map: A) -> std::result::Result<Envelope, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                Envelope::deserialize(serde::de::value::MapAccessDeserializer::new(map))
            }
        }

        let envelope = deserializer.deserialize_map(EnvelopeVisitor)?;
        match (
            envelope.method,
            envelope.id,
            envelope.result,
            envelope.error,
        ) {
            (Some(method), Some(id), None, None) => Ok(Self::Request(JSONRPCRequest {
                id,
                method,
                params: envelope.params,
                trace: match envelope.trace {
                    Some(Trace::Context(trace)) => Some(trace),
                    Some(Trace::Invalid(_)) => {
                        return Err(serde::de::Error::custom("invalid request trace metadata"));
                    }
                    None => None,
                },
            })),
            (Some(method), None, None, None) => Ok(Self::Notification(JSONRPCNotification {
                method,
                params: envelope.params,
            })),
            (None, Some(id), Some(result), None) => {
                Ok(Self::Response(JSONRPCResponse { id, result }))
            }
            (None, Some(id), None, Some(error)) => Ok(Self::Error(JSONRPCError { id, error })),
            _ => Err(serde::de::Error::custom(
                "expected a method with optional id, or an id with exactly one of result and error",
            )),
        }
    }
}

/// A request that expects a response.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JSONRPCRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<W3cTraceContext>,
}

/// A notification that does not expect a response.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JSONRPCNotification {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A successful response to a request.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JSONRPCResponse {
    pub id: RequestId,
    pub result: Result,
}

/// A response indicating that a request failed.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JSONRPCError {
    pub error: JSONRPCErrorError,
    pub id: RequestId,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JSONRPCErrorError {
    pub code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn wire_messages_preserve_category_and_payload() {
        let cases = [
            (
                r#"{"id":"r","method":"process/read","params":{"processId":"p"},"trace":{"traceparent":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"},"extension":true}"#,
                JSONRPCMessage::Request(JSONRPCRequest {
                    id: RequestId::String("r".into()),
                    method: "process/read".into(),
                    params: Some(json!({"processId": "p"})),
                    trace: Some(W3cTraceContext {
                        traceparent: Some(
                            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
                        ),
                        tracestate: None,
                    }),
                }),
            ),
            (
                r#"{"method":"ready","params":null,"trace":false,"extension":1,"extension":2}"#,
                JSONRPCMessage::Notification(JSONRPCNotification {
                    method: "ready".into(),
                    params: None,
                }),
            ),
            (
                r#"{"result":null,"id":1,"extension":true}"#,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: RequestId::Integer(1),
                    result: serde_json::Value::Null,
                }),
            ),
            (
                r#"{"error":{"code":-32000,"message":"failed","data":{"retry":false}},"id":"e"}"#,
                JSONRPCMessage::Error(JSONRPCError {
                    id: RequestId::String("e".into()),
                    error: JSONRPCErrorError {
                        code: -32000,
                        message: "failed".into(),
                        data: Some(json!({"retry": false})),
                    },
                }),
            ),
        ];
        for (wire, expected) in cases {
            assert_eq!(
                serde_json::from_str::<JSONRPCMessage>(wire).unwrap(),
                expected
            );
            let encoded = serde_json::to_value(&expected).unwrap();
            assert!(encoded.get("jsonrpc").is_none());
            assert_eq!(
                serde_json::from_value::<JSONRPCMessage>(encoded).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn wire_messages_reject_ambiguous_or_incomplete_envelopes() {
        for wire in [
            r#"{"id":null,"method":"process/read","params":{"processId":"p"}}"#,
            r#"{"id":true,"method":"process/read"}"#,
            r#"{"id":1.5,"method":"process/read"}"#,
            r#"{"id":1,"result":{},"error":{"code":-32000,"message":"failed"}}"#,
            r#"{"id":1,"result":null,"error":null}"#,
            r#"{"id":1,"method":"read","result":null}"#,
            r#"{"method":"read","error":{"code":-32000,"message":"failed"}}"#,
            r#"{"id":1,"method":null,"result":{}}"#,
            r#"{"id":1,"method":"read","trace":false}"#,
            r#"{"id":1}"#,
            r#"{"result":null}"#,
            r#"{}"#,
            r#"[1,"read"]"#,
        ] {
            assert!(
                serde_json::from_str::<JSONRPCMessage>(wire).is_err(),
                "accepted {wire}"
            );
        }
    }

    #[test]
    fn wire_messages_reject_duplicate_known_fields_including_nulls() {
        for wire in [
            r#"{"id":1,"id":2,"method":"read"}"#,
            r#"{"method":"one","method":"two"}"#,
            r#"{"id":1,"result":null,"result":{}}"#,
            r#"{"id":1,"error":{"code":1,"message":"a"},"error":{"code":2,"message":"b"}}"#,
            r#"{"method":"read","params":null,"params":{}}"#,
            r#"{"id":1,"method":"read","trace":null,"trace":null}"#,
            r#"{"id":1,"method":"read","trace":{"traceparent":"a","traceparent":"b"}}"#,
            r#"{"id":1,"error":{"code":1,"code":2,"message":"failed"}}"#,
        ] {
            assert!(
                serde_json::from_str::<JSONRPCMessage>(wire).is_err(),
                "accepted {wire}"
            );
        }
    }
}
