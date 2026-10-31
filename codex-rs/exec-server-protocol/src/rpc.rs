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
        #[serde(untagged)]
        enum CapabilityRevision {
            Revision(String),
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
            #[serde(rename = "capabilityRevision")]
            capability_revision: Option<CapabilityRevision>,
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
                capability_revision: match envelope.capability_revision {
                    Some(CapabilityRevision::Revision(revision)) => Some(revision),
                    Some(CapabilityRevision::Invalid(_)) => {
                        return Err(serde::de::Error::custom("invalid capability revision metadata"));
                    }
                    None => None,
                },
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
    /// Expected support snapshot. Checked again before execution, never an authority token.
    #[serde(
        default,
        rename = "capabilityRevision",
        skip_serializing_if = "Option::is_none"
    )]
    pub capability_revision: Option<String>,
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

// Stable wire spellings, independent of Rust's Debug formatting and localized
// OS messages. Unknown future kinds remain errors with legacy classification.
const FILE_SYSTEM_ERROR_KINDS: &[(&str, std::io::ErrorKind)] = &[
    ("notFound", std::io::ErrorKind::NotFound),
    ("permissionDenied", std::io::ErrorKind::PermissionDenied),
    ("alreadyExists", std::io::ErrorKind::AlreadyExists),
    ("notADirectory", std::io::ErrorKind::NotADirectory),
    ("isADirectory", std::io::ErrorKind::IsADirectory),
    ("directoryNotEmpty", std::io::ErrorKind::DirectoryNotEmpty),
    ("readOnlyFilesystem", std::io::ErrorKind::ReadOnlyFilesystem),
    ("invalidInput", std::io::ErrorKind::InvalidInput),
    ("invalidData", std::io::ErrorKind::InvalidData),
    ("unsupported", std::io::ErrorKind::Unsupported),
    ("interrupted", std::io::ErrorKind::Interrupted),
    ("timedOut", std::io::ErrorKind::TimedOut),
    ("brokenPipe", std::io::ErrorKind::BrokenPipe),
    ("wouldBlock", std::io::ErrorKind::WouldBlock),
    ("writeZero", std::io::ErrorKind::WriteZero),
    ("unexpectedEof", std::io::ErrorKind::UnexpectedEof),
    ("storageFull", std::io::ErrorKind::StorageFull),
    ("quotaExceeded", std::io::ErrorKind::QuotaExceeded),
    ("fileTooLarge", std::io::ErrorKind::FileTooLarge),
    ("resourceBusy", std::io::ErrorKind::ResourceBusy),
    ("executableFileBusy", std::io::ErrorKind::ExecutableFileBusy),
    ("deadlock", std::io::ErrorKind::Deadlock),
    ("crossesDevices", std::io::ErrorKind::CrossesDevices),
    ("tooManyLinks", std::io::ErrorKind::TooManyLinks),
    ("invalidFilename", std::io::ErrorKind::InvalidFilename),
    (
        "argumentListTooLong",
        std::io::ErrorKind::ArgumentListTooLong,
    ),
    ("outOfMemory", std::io::ErrorKind::OutOfMemory),
    ("connectionRefused", std::io::ErrorKind::ConnectionRefused),
    ("connectionReset", std::io::ErrorKind::ConnectionReset),
    ("connectionAborted", std::io::ErrorKind::ConnectionAborted),
    ("notConnected", std::io::ErrorKind::NotConnected),
    ("addrInUse", std::io::ErrorKind::AddrInUse),
    ("addrNotAvailable", std::io::ErrorKind::AddrNotAvailable),
    ("networkDown", std::io::ErrorKind::NetworkDown),
    ("networkUnreachable", std::io::ErrorKind::NetworkUnreachable),
    ("hostUnreachable", std::io::ErrorKind::HostUnreachable),
    ("other", std::io::ErrorKind::Other),
];

pub fn file_system_error_data(kind: std::io::ErrorKind) -> serde_json::Value {
    let name = FILE_SYSTEM_ERROR_KINDS
        .iter()
        .find_map(|(name, candidate)| (*candidate == kind).then_some(*name))
        .unwrap_or("other");
    serde_json::json!({"filesystemError": {"version": 1, "kind": name}})
}

pub fn file_system_error_kind(data: Option<&serde_json::Value>) -> Option<std::io::ErrorKind> {
    let payload = data?.get("filesystemError")?;
    if payload.get("version")?.as_u64()? != 1 {
        return None;
    }
    let name = payload.get("kind")?.as_str()?;
    FILE_SYSTEM_ERROR_KINDS
        .iter()
        .find_map(|(candidate, kind)| (*candidate == name).then_some(*kind))
}

