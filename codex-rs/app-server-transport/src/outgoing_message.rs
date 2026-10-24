use std::fmt;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use serde::Serialize;
use tokio::sync::oneshot;

/// Stable identifier for a transport connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConnectionId(pub u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Outgoing message from the server to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
// Keep the typed wire variants inline; boxing only this notification would add
// allocation and API churn at every transport consumer.
#[allow(clippy::large_enum_variant)]
pub enum OutgoingMessage {
    Request(ServerRequest),
    /// AppServerNotification is specific to the case where this is run as an
    /// "app server" as opposed to an MCP server.
    AppServerNotification(ServerNotification),
    Response(OutgoingResponse),
    Error(OutgoingError),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutgoingResponse {
    pub id: RequestId,
    pub result: Result,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutgoingError {
    pub error: JSONRPCErrorError,
    pub id: RequestId,
}

#[derive(Debug)]
pub struct EncodedJson {
    pub bytes: Vec<u8>,
    /// Retain admission credit until the physical writer finishes this frame.
    pub delivery_credit: Option<tokio::sync::OwnedSemaphorePermit>,
}

#[derive(Debug)]
pub struct QueuedOutgoingMessage {
    pub message: OutgoingMessage,
    /// Exact JSON without a trailing newline, prepared by bounded stdio admission.
    /// The message must not be modified after this representation is prepared.
    pub encoded_json: Option<EncodedJson>,
    /// Signals successful completion of this transport's write/flush, not peer receipt
    /// or execution. Cancellation leaves delivery uncertain and does not establish safe replay.
    /// The monotonic timestamp is captured by the writer, not its scheduled observer.
    pub write_complete_tx: Option<oneshot::Sender<std::time::Instant>>,
}

impl QueuedOutgoingMessage {
    pub fn new(message: OutgoingMessage) -> Self {
        Self {
            message,
            encoded_json: None,
            write_complete_tx: None,
        }
    }

    /// Retain the same encoding used for exact byte admission and the wire write.
    pub fn encode_json(&mut self, limit: usize) -> serde_json::Result<usize> {
        struct BoundedBuffer {
            bytes: Vec<u8>,
            limit: usize,
        }
        impl std::io::Write for BoundedBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                    return Err(std::io::Error::other("stdio delivery byte budget exceeded"));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = BoundedBuffer { bytes: Vec::with_capacity(limit.min(16 * 1024)), limit };
        serde_json::to_writer(&mut buffer, &self.message)?;
        let len = buffer.bytes.len();
        self.encoded_json = Some(EncodedJson { bytes: buffer.bytes, delivery_credit: None });
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_encoding_counts_escaped_utf8_wire_bytes() {
        let mut message = QueuedOutgoingMessage::new(OutgoingMessage::Response(OutgoingResponse {
            id: RequestId::Integer(1),
            result: serde_json::json!({"text": "\"\\\n雪"}),
        }));
        let expected = b"{\"id\":1,\"result\":{\"text\":\"\\\"\\\\\\n\xe9\x9b\xaa\"}}";
        assert!(message.encode_json(expected.len() - 1).is_err());
        assert!(message.encoded_json.is_none());
        assert_eq!(message.encode_json(expected.len()).unwrap(), expected.len());
        assert_eq!(message.encoded_json.as_ref().map(|encoded| encoded.bytes.as_slice()), Some(expected.as_slice()));
    }
}
