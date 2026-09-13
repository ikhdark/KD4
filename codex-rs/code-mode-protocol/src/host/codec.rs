use std::io;
use std::mem::size_of;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

/// Maximum JSON payload size accepted for one IPC frame.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

struct LimitedPayload {
    bytes: Vec<u8>,
    limit: usize,
}

impl io::Write for LimitedPayload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("code-mode IPC frame exceeds {} bytes", self.limit),
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A serialized IPC frame that has already passed the payload size limit.
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    payload: Vec<u8>,
}

impl EncodedFrame {
    pub fn encode<T>(message: &T) -> io::Result<Self>
    where
        T: Serialize,
    {
        let mut payload = LimitedPayload {
            // Preserve serde_json::to_vec's small-message allocation behavior.
            bytes: Vec::with_capacity(128),
            limit: MAX_FRAME_BYTES,
        };
        serde_json::to_writer(&mut payload, message).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to encode code-mode IPC frame: {err}"),
            )
        })?;
        Ok(Self {
            payload: payload.bytes,
        })
    }
}

/// Decodes JSON messages prefixed by a four-byte little-endian payload length.
pub struct FramedReader<R> {
    reader: R,
}

impl<R> FramedReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// Reads the next frame, returning `None` only for EOF at a frame boundary.
    ///
    /// This operation is not cancellation-safe. After partial progress, finish
    /// the same future or discard the connection; restarting loses framing.
    /// I/O errors and oversized headers also require discarding the connection.
    /// A JSON decoding error occurs after the complete frame has been consumed.
    pub async fn read<T>(&mut self) -> io::Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let mut length_bytes = [0_u8; size_of::<u32>()];
        if self.reader.read(&mut length_bytes[..1]).await? == 0 {
            return Ok(None);
        }
        self.reader.read_exact(&mut length_bytes[1..]).await?;

        let length = u32::from_le_bytes(length_bytes) as usize;
        if length > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("code-mode IPC frame length {length} exceeds {MAX_FRAME_BYTES} bytes"),
            ));
        }

        let mut payload = vec![0; length];
        self.reader.read_exact(&mut payload).await?;
        serde_json::from_slice(&payload).map(Some).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to decode code-mode IPC frame: {err}"),
            )
        })
    }
}

/// Encodes JSON messages with a four-byte little-endian payload length.
pub struct FramedWriter<W> {
    writer: W,
}

impl<W> FramedWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Writes and flushes one complete frame.
    ///
    /// Encoding errors leave the connection untouched. Once encoding succeeds,
    /// the cancellation and I/O error requirements of [`Self::write_frame`] apply.
    pub async fn write<T>(&mut self, message: &T) -> io::Result<()>
    where
        T: Serialize,
    {
        self.write_frame(&EncodedFrame::encode(message)?).await
    }

    /// Writes and flushes a frame encoded before it entered an I/O queue.
    ///
    /// This operation is not cancellation-safe. After partial progress, finish
    /// the same future or discard the connection; do not restart the frame on
    /// that stream. Discard the connection after an I/O error as well.
    pub async fn write_frame(&mut self, frame: &EncodedFrame) -> io::Result<()> {
        let length = u32::try_from(frame.payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "code-mode IPC frame length exceeds u32",
            )
        })?;

        self.writer.write_all(&length.to_le_bytes()).await?;
        self.writer.write_all(&frame.payload).await?;
        self.writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use pretty_assertions::assert_eq;

    use super::EncodedFrame;
    use super::LimitedPayload;
    use super::MAX_FRAME_BYTES;

    #[test]
    fn encoded_frame_accepts_exact_serialized_limit_and_rejects_one_extra_byte() {
        // Quotes add two bytes, and the final newline adds two escaped bytes.
        let mut message = "x".repeat(MAX_FRAME_BYTES - 4);
        message.push('\n');
        let frame = EncodedFrame::encode(&message).expect("exact frame limit");
        assert_eq!(frame.payload.len(), MAX_FRAME_BYTES);
        assert_eq!(frame.payload[0], b'"');
        assert!(
            frame.payload[1..MAX_FRAME_BYTES - 3]
                .iter()
                .all(|byte| *byte == b'x')
        );
        assert_eq!(&frame.payload[MAX_FRAME_BYTES - 3..], b"\\n\"");
        drop(frame);

        message.push('x');
        let err = EncodedFrame::encode(&message).expect_err("one serialized byte over the limit");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn payload_budget_accepts_exact_limit_and_rejects_without_appending() {
        let mut payload = LimitedPayload {
            bytes: Vec::new(),
            limit: 4,
        };
        payload.write_all(b"ab").expect("first chunk");
        let err = payload.write_all(b"cde").expect_err("oversized chunk");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(payload.bytes, b"ab");
        payload.write_all(b"cd").expect("exact limit");
        assert_eq!(payload.bytes, b"abcd");
        assert_eq!(payload.write(b"").expect("empty write at limit"), 0);
        assert_eq!(
            payload.write_all(b"e").expect_err("full budget").kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(payload.bytes, b"abcd");
    }

    #[test]
    fn payload_budget_counts_json_escaping_and_structure() {
        // A newline takes four serialized bytes: a quote, backslash, n, quote.
        let mut exact = LimitedPayload {
            bytes: Vec::new(),
            limit: 4,
        };
        serde_json::to_writer(&mut exact, "\n").expect("exact serialized limit");
        assert_eq!(exact.bytes, br#""\n""#);

        let mut short = LimitedPayload {
            bytes: Vec::new(),
            limit: 3,
        };
        assert!(serde_json::to_writer(&mut short, "\n").is_err());
        assert_eq!(short.bytes, b"\"\\n");
    }
}
