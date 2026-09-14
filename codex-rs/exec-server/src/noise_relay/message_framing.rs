use std::io::Write;

use codex_exec_server_protocol::JSONRPCMessage;

use crate::ExecServerError;

const LENGTH_PREFIX_BYTES: usize = size_of::<u32>();
const MAX_NOISE_JSONRPC_MESSAGE_LEN: usize = 64 * 1024 * 1024;
pub(crate) const NOISE_RECORD_PLAINTEXT_LEN: usize = 60 * 1024;
// Retain ordinary working buffers, but release occasional multi-megabyte responses.
const MAX_RETAINED_BUFFER_CAPACITY: usize = 1024 * 1024;

/// Serialize one JSON-RPC message into the encrypted record byte stream.
///
/// Clatter limits an individual Noise message to 65,535 bytes, while valid
/// exec-server responses can be much larger. A four-byte authenticated length
/// prefix lets the caller split this byte stream into bounded Noise records and
/// lets the receiver reconstruct exact JSON-RPC message boundaries.
pub(crate) fn frame_jsonrpc_message(message: &JSONRPCMessage) -> Result<Vec<u8>, ExecServerError> {
    frame_jsonrpc_message_with_limit(message, MAX_NOISE_JSONRPC_MESSAGE_LEN)
}

fn frame_jsonrpc_message_with_limit(
    message: &JSONRPCMessage,
    limit: usize,
) -> Result<Vec<u8>, ExecServerError> {
    let mut framed = vec![0; LENGTH_PREFIX_BYTES];
    let mut writer = BoundedMessageWriter {
        framed: &mut framed,
        remaining: limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut writer, message);
    if writer.exceeded {
        return Err(ExecServerError::Protocol(
            "Noise relay JSON-RPC message exceeds maximum length".to_string(),
        ));
    }
    result?;
    let message_len = framed.len() - LENGTH_PREFIX_BYTES;
    framed[..LENGTH_PREFIX_BYTES].copy_from_slice(&(message_len as u32).to_be_bytes());
    Ok(framed)
}

struct BoundedMessageWriter<'a> {
    framed: &'a mut Vec<u8>,
    remaining: usize,
    exceeded: bool,
}

impl Write for BoundedMessageWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(std::io::Error::other("Noise relay message length exceeded"));
        }
        self.framed.extend_from_slice(bytes);
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Incrementally reconstructs authenticated JSON-RPC messages from Noise records.
///
/// The length prefix is encrypted along with the message. It is still bounded
/// here so a bad authenticated peer cannot grow the reassembly buffer forever.
#[derive(Default)]
pub(crate) struct JsonRpcMessageDecoder {
    buffered: Vec<u8>,
}

impl JsonRpcMessageDecoder {
    /// Append one decrypted record and return all complete framed messages.
    pub(crate) fn push(
        &mut self,
        plaintext_record: &[u8],
    ) -> Result<Vec<JSONRPCMessage>, ExecServerError> {
        if plaintext_record.len() > NOISE_RECORD_PLAINTEXT_LEN {
            return Err(ExecServerError::Protocol(
                "Noise relay plaintext record exceeds maximum length".to_string(),
            ));
        }
        self.buffered.extend_from_slice(plaintext_record);

        // One record can finish multiple messages, and one message can span
        // multiple records. Parse only after the authenticated length prefix
        // and the full declared payload are present.
        let mut messages = Vec::new();
        let mut consumed = 0;
        while let Some(prefix) = self.buffered.get(consumed..consumed + LENGTH_PREFIX_BYTES) {
            let message_len =
                u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
            // Reject the authenticated length before waiting for its payload.
            if message_len == 0 || message_len > MAX_NOISE_JSONRPC_MESSAGE_LEN {
                return Err(ExecServerError::Protocol(
                    "Noise relay JSON-RPC message has invalid length".to_string(),
                ));
            }
            let framed_len = consumed + LENGTH_PREFIX_BYTES + message_len;
            if self.buffered.len() < framed_len {
                break;
            }
            messages.push(serde_json::from_slice(
                &self.buffered[consumed + LENGTH_PREFIX_BYTES..framed_len],
            )?);
            consumed = framed_len;
        }
        self.buffered.drain(..consumed);
        if self.buffered.is_empty() && self.buffered.capacity() > MAX_RETAINED_BUFFER_CAPACITY {
            self.buffered = Vec::new();
        }

        // Even before a message is complete, keep reassembly memory bounded.
        if self.buffered.len() > LENGTH_PREFIX_BYTES + MAX_NOISE_JSONRPC_MESSAGE_LEN {
            return Err(ExecServerError::Protocol(
                "Noise relay JSON-RPC reassembly buffer exceeds maximum length".to_string(),
            ));
        }
        Ok(messages)
    }
}

#[cfg(test)]
#[path = "message_framing_tests.rs"]
mod tests;
