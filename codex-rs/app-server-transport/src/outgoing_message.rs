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
    /// Complete JSON for buffered frames; empty when streamed_len is present.
    pub bytes: Vec<u8>,
    /// Validated wire length for a frame that must be serialized in bounded chunks.
    pub streamed_len: Option<usize>,
    /// Retain admission credit until the physical writer finishes this frame.
    pub delivery_credit: Option<tokio::sync::OwnedSemaphorePermit>,
}

#[derive(Debug)]
pub struct QueuedOutgoingMessage {
    pub message: OutgoingMessage,
    /// Buffered JSON or a validated streaming plan, without a trailing newline.
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
        self.prepare_json(limit, false)
    }

    /// Preserve large legal frames without retaining an unbounded encoded copy.
    /// Finish validation/counting before the writer can emit any part of the frame.
    pub fn prepare_stdio_json(&mut self, limit: usize) -> serde_json::Result<usize> {
        self.prepare_json(limit, true)
    }

    fn prepare_json(&mut self, limit: usize, allow_streaming: bool) -> serde_json::Result<usize> {
        struct BoundedBuffer {
            bytes: Vec<u8>,
            limit: usize,
            len: usize,
            allow_streaming: bool,
        }
        impl std::io::Write for BoundedBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let len = self.len.checked_add(bytes.len()).ok_or_else(|| {
                    std::io::Error::other("stdio message length overflow")
                })?;
                if len > self.limit {
                    if !self.allow_streaming {
                        return Err(std::io::Error::other("stdio delivery byte budget exceeded"));
                    }
                    // Once the frame exceeds the staging budget, keep only its
                    // exact length. The physical writer will stream the value.
                    self.bytes = Vec::new();
                } else {
                    self.bytes.extend_from_slice(bytes);
                }
                self.len = len;
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = BoundedBuffer {
            bytes: Vec::with_capacity(limit.min(16 * 1024)),
            limit,
            len: 0,
            allow_streaming,
        };
        serde_json::to_writer(&mut buffer, &self.message)?;
        let len = buffer.len;
        self.encoded_json = Some(EncodedJson {
            bytes: buffer.bytes,
            streamed_len: (len > limit).then_some(len),
            delivery_credit: None,
        });
        Ok(len)
    }
}

