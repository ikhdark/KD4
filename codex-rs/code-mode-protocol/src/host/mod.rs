//! Messages and local IPC framing for the code-mode host boundary.
//!
//! Protocol version 1 multiplexes session operations and delegate callbacks by
//! request ID over one ordered connection. Optional capabilities are negotiated
//! before sending extension fields; legacy peers retain the original V1 shape.

mod codec;
mod error;
mod message;
mod payload;
mod types;

pub use codec::EncodedFrame;
pub use codec::FramedReader;
pub use codec::FramedWriter;
pub use codec::MAX_FRAME_BYTES;
pub use error::HandshakeRejectReason;
pub use message::ClientHello;
pub use message::ClientHelloError;
pub use message::ClientToHost;
pub use message::DelegateRequest;
pub use message::DelegateResponse;
pub use message::HostHello;
pub use message::HostRequest;
pub use message::HostResponse;
pub use message::HostToClient;
pub use message::WireResult;
pub use payload::WireCellId;
pub use payload::WireContentItem;
pub use payload::WireExecuteRequest;
pub use payload::WireImageDetail;
pub use payload::WireNestedToolCall;
pub use payload::WireRuntimeResponse;
pub use payload::WireToolDefinition;
pub use payload::WireToolCatalog;
pub use payload::WireToolKind;
pub use payload::WireToolName;
pub use payload::WireWaitOutcome;
pub use payload::WireWaitRequest;
pub use types::Capability;
pub use types::CapabilitySet;
pub use types::DelegateRequestId;
pub use types::DuplicateCapability;
pub use types::InvalidIdentifier;
pub use types::InvalidSupportedProtocolVersions;
pub use types::ProtocolVersion;
pub use types::RequestId;
pub use types::SessionId;
pub use types::SupportedProtocolVersions;

/// Operation requests a V1 host runs at once; it rejects requests beyond this.
pub const MAX_IN_FLIGHT_REQUESTS: usize = 256;
/// Delegate requests a V1 host leaves awaiting client responses at once;
/// further nested calls fail inside the host without reaching the client.
pub const MAX_PENDING_DELEGATE_REQUESTS: usize = 256;
pub const TOOL_CATALOG_CAPABILITY: &str = "tool-catalog-v1";
pub const NAMED_STATE_CAPABILITY: &str = "named-state-v1";
pub const RECEIPT_RECOVERY_CAPABILITY: &str = "terminal-receipt-recovery-v1";

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codec_tests.rs"]
mod codec_tests;
