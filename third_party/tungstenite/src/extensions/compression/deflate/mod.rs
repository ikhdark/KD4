// KD4: production-only 4fffad30 vendor; test/doc examples omitted, handshake log redacted. See README.md.
//! Implements "permessage-deflate" PMCE defined in [RFC 7692 Section 7]
//!
//! [RFC 7692 Section 7]: https://tools.ietf.org/html/rfc7692#section-7
use bytes::Bytes;
use flate2::{Compress, Decompress, FlushCompress, FlushDecompress, Status};
use thiserror::Error;

use crate::{extensions::compression::DecompressionError, protocol::Role};

mod config;
#[cfg_attr(not(feature = "handshake"), allow(unused_imports))]
pub(crate) use config::ParameterError as DeflateParameterError;
pub use config::{
    DeflateConfig, NegotiationError as DeflateNegotiationError, PermessageDeflateConfig,
    PER_MESSAGE_DEFLATE as EXTENSION_NAME,
};

#[derive(Debug)]
/// Manages per message compression using DEFLATE.
pub struct DeflateContext {
    compress: DeflateCompress,
    decompress: DeflateDecompress,
}

/// Errors from `permessage-deflate` extension.
#[derive(Copy, Clone, Debug, Error, PartialEq, Eq)]
pub enum DeflateError {
    /// Compress failed
    #[error("Failed to compress")]
    Compress,
    /// Decompress failed
    #[error("Failed to decompress")]
    Decompress,
}

#[derive(Debug)]
struct DeflateCompress {
    own_context_takeover: bool,
    /// The actual compressor to run payloads through.
    ///
    /// Use the low-level [`Compress`] API instead of the higher-level
    /// [`flate2::zlib::write::ZlibEncoder`] so we can compress directly into
    /// the output buffer instead of the intermediate one that that type holds.
    compressor: Compress,
}

#[derive(Debug)]
struct DeflateDecompress {
    /// The actual decompressor to run payloads through.
    ///
    /// Use the low-level [`Decompress`] API instead of the higher-level
    /// [`flate2::zlib::write::ZlibDecoder`] so we can decompress directly into
    /// the output buffer instead of the intermediate one that that type holds.
    /// This also lets us avoid some decompression errors that the higher-level
    /// version exhibited with certain highly-compressed payloads.
    decompressor: Decompress,
    peer_context_takeover: bool,
}

impl DeflateContext {
    pub(crate) fn new(role: Role, config: DeflateConfig) -> Self {
        let DeflateConfig {
            server_no_context_takeover,
            client_no_context_takeover,
            compression,
            ..
        } = config;

        // Per RFC 7692 Section 7:
        //
        //      These parameters enable two methods (no_context_takeover and
        //      max_window_bits) of constraining memory usage that may be
        //      applied independently to either direction of WebSocket traffic.
        //      The extension parameters with the "client_" prefix are used by
        //      the client to configure its compressor and by the server to
        //      configure its decompressor.  The extension parameters with the
        //      "server_" prefix are used by the server to configure its
        //      compressor and by the client to configure its decompressor.  All
        //      four parameters are defined for both a client's extension
        //      negotiation offer and a server's extension negotiation response.
        //
        // Here `role` is for our own end of the connection, as opposed to the
        // peer end.
        let (own_no_context_takeover, peer_no_context_takeover) = match role {
            Role::Client => (client_no_context_takeover, server_no_context_takeover),
            Role::Server => (server_no_context_takeover, client_no_context_takeover),
        };

        // Both ends of the connection act as both compressor and decompressor.
        // We compress with the window size for our role and decompress with the
        // size for the opposite role.
        let (compressor_window_bits, decompressor_window_bits) = match role {
            Role::Client => (config.client_max_window_bits(), config.server_max_window_bits()),
            Role::Server => (config.server_max_window_bits(), config.client_max_window_bits()),
        };

        DeflateContext {
            compress: DeflateCompress {
                own_context_takeover: !own_no_context_takeover,
                compressor: Compress::new_with_window_bits(
                    compression,
                    false,
                    compressor_window_bits.get(),
                ),
            },
            decompress: DeflateDecompress {
                peer_context_takeover: !peer_no_context_takeover,
                decompressor: Decompress::new_with_window_bits(
                    false,
                    decompressor_window_bits.get(),
                ),
            },
        }
    }

    /// Compress the payload of an outgoing message.
    pub(crate) fn compress(&mut self, data: &[u8]) -> Result<Bytes, DeflateError> {
        self.compress.compress(data).map_err(|e| {
            log::debug!("compression failed: {e}");
            DeflateError::Compress
        })
    }

    /// Decompress the payload in a received frame.
    ///
    /// The `is_final` argument should only be set when calling with the contents of the last frame in a message.
    pub(crate) fn decompress(
        &mut self,
        data: &[u8],
        is_final: bool,
        size_limit: usize,
    ) -> Result<Bytes, DecompressionError<DeflateError>> {
        self.decompress.decompress(data, is_final, size_limit).map_err(|e| {
            e.map(|e: std::io::Error| {
                log::debug!("decompression failed: {e}");
                DeflateError::Decompress
            })
        })
    }
}

const ELIDED_TRAILER_BLOCK_CONTENTS: &[u8] = &[0x00, 0x00, 0xff, 0xff];

impl DeflateCompress {
    /// Compress the contents of an entire message.
    ///
    /// This is asymmetric with [`DeflateDecompress::decompress`] in that it
    /// operates on the contents of an entire message, not the comprising frames.
    fn compress(&mut self, mut data: &[u8]) -> Result<Bytes, std::io::Error> {
        log::trace!("compressing message payload with {} bytes", data.len());
        if data.is_empty() {
            // Fast path for an empty payload: it gets DEFLATE compressed to a
            // zero-length uncompressed block, which conveniently is
            // concat([0x00], ELIDED_TRAILER_BLOCK_CONTENTS). Then, per the RFC,
            // we elide the trailing 4 bytes to get a single 0x00 byte as the
            // compressed payload.
            return Ok(Bytes::from_static(&[0x00]));
        }

        let mut output = Vec::new();

        // The amount of space that should be available in `output` before
        // attempting to compress data into it.
        const REQUIRED_OUTPUT_SPACE: usize = 4096;

        // Per RFC 7692 Section 7.2.1:
        //
        //     An endpoint uses the following algorithm to compress a message.
        //
        //     1.  Compress all the octets of the payload of the message using
        //         DEFLATE.
        //

        {
            let mut total_read = self.compressor.total_in();
            loop {
                // Make sure there's space for compress_vec to write to.
                output.reserve(REQUIRED_OUTPUT_SPACE);

                let r = self.compressor.compress_vec(data, &mut output, FlushCompress::None)?;

                let read_before = std::mem::replace(&mut total_read, self.compressor.total_in());
                let read = (total_read - read_before) as usize;

                data = &data[read..];
                log::trace!(
                    "compressed {read} bytes, {} remaining; partial output is {} bytes",
                    data.len(),
                    output.len()
                );

                match r {
                    Status::Ok => continue,
                    Status::BufError if read == 0 => {
                        // We made no progress, so this BufError means that
                        // we're out of input.
                        break;
                    }
                    Status::BufError => {
                        // We made some progress, so we can continue after
                        // making more output space.
                        continue;
                    }
                    Status::StreamEnd => break,
                }
            }
        }

        log::trace!("flushing compressed data");

        //     2.  If the resulting data does not end with an empty DEFLATE
        //         block with no compression (the "BTYPE" bits are set to 00),
        //         append an empty DEFLATE block with no compression to the tail
        //         end.

        // Ideally, at this point, we'd be able to just call compress_vec once
        // with an empty slice, FlushCompress::Sync, and a vector with more than
        // enough output space, and then we'd get an empty block and be done.
        // After all, compress_vec is documented to output "as much output as
        // possible".  Unfortunately, compress_vec does not actually do that for
        // all backends. See:
        // - https://github.com/rust-lang/flate2-rs/blob/1.1.2/src/ffi/rust.rs#L169
        // - https://github.com/Frommi/miniz_oxide/blob/0.8.8/miniz_oxide/src/deflate/stream.rs#L82
        // - https://github.com/Frommi/miniz_oxide/issues/105
        //
        // This causes compress_vec to return Ok as soon as the compressor
        // writes *any* output when called with an empty slice.
        //
        // So, instead, we need to keep calling compress_vec with an empty slice
        // until we stop making progress.
        //
        // Once we have done that properly, we should always have an empty block
        // at the end of the output, and then we can truncate the output to
        // remove the empty block, per the RFC.
        {
            let mut total_out = self.compressor.total_out();
            loop {
                output.reserve(REQUIRED_OUTPUT_SPACE);
                let output_len_before = output.len();
                let output_available_before = output.capacity() - output_len_before;

                let _ = self.compressor.compress_vec(&[], &mut output, FlushCompress::Sync)?;
                log::trace!(
                    "flushed {} bytes into an available {output_available_before} bytes",
                    output.len() - output_len_before,
                );
                let out_before = std::mem::replace(&mut total_out, self.compressor.total_out());
                if total_out == out_before {
                    break;
                }
            }
        }

        //     3.  Remove 4 octets (that are 0x00 0x00 0xff 0xff) from the tail
        //         end.  After this step, the last octet of the compressed data
        //         contains (possibly part of) the DEFLATE header bits with the
        //         "BTYPE" bits set to 00.

        debug_assert!(output.ends_with(ELIDED_TRAILER_BLOCK_CONTENTS), "output is {output:02x?}");
        output.truncate(output.len() - ELIDED_TRAILER_BLOCK_CONTENTS.len());

        if !self.own_context_takeover {
            // Reset if the next frame isn't supposed to be starting with the
            // same compression window.
            self.compressor.reset();
        }

        log::trace!("finished compression into {} bytes", output.len());
        Ok(Bytes::from(output))
    }
}

impl DeflateDecompress {
    /// Decompress the contents of a single frame.
    ///
    /// The `is_final` argument must be `true` if and only if the frame is the
    /// last one in a message. The `size_limit` argument is the maximum number
    /// of bytes that can be decompressed. If the input `data` decompresses to
    /// more than `size_limit` bytes, [`DecompressionError::SizeLimitReached`]
    /// will be returned.
    fn decompress(
        &mut self,
        data: &[u8],
        is_final: bool,
        size_limit: usize,
    ) -> Result<Bytes, DecompressionError<std::io::Error>> {
        // From RFC 7692 Section 7.2.2:
        //
        //   An endpoint uses the following algorithm to decompress a message.
        //
        //   1.  Append 4 octets of 0x00 0x00 0xff 0xff to the tail end of the
        //       payload of the message.
        //
        //   2.  Decompress the resulting data using DEFLATE.

        let mut output = Vec::new();

        log::trace!(
            "decompressing {} bytes in {} frame",
            data.len(),
            if is_final { "final" } else { "intermediate" }
        );
        let mut total_read = self.decompressor.total_in();

        let mut decompress_from = |mut data: &[u8]| {
            loop {
                // Make sure there's some space to decompress into,
                // optimistically assuming a 50% compression ratio of the input.
                // This might put us slightly beyond the requested size limit
                // but it also might not all be used.
                output.reserve(2 * data.len());

                let r =
                    self.decompressor.decompress_vec(data, &mut output, FlushDecompress::None)?;

                if output.len() > size_limit {
                    return Err(DecompressionError::SizeLimitReached);
                }
                let read_before = std::mem::replace(&mut total_read, self.decompressor.total_in());

                let read = (total_read - read_before) as usize;

                data = &data[read..];

                match r {
                    Status::Ok => continue,
                    Status::BufError => {
                        // We've either run out of input data or output space.
                        // Since we reserve space ahead of time, this must mean
                        // we're out of input.
                        break;
                    }
                    Status::StreamEnd => {
                        // Finished a block with BFINAL set. This is legal; from
                        // RFC 7692 Section 7.2.3.4:
                        //
                        //   On platforms on which the flush method using an
                        //   empty DEFLATE block with no compression is not
                        //   available, implementors can choose to flush data
                        //   using DEFLATE blocks with "BFINAL" set to 1.
                        //
                        // On the decompression end we reset the compressor in
                        // response. This relies on the assumption that the
                        // client produced the block with BFINAL set by
                        // informing their compressor that the stream was
                        // ending, and so any blocks afterwards won't reference
                        // any context from this block or earlier. It's
                        // obviously not a perfect assumption, but it matches
                        // the behavior of other widely-deployed
                        // permessage-deflate implementations.
                        self.decompressor.reset(false);
                        total_read = 0;
                    }
                }
            }
            Ok(())
        };

        decompress_from(data)?;

        if is_final {
            // Decompress the final block that is part of the logical input to
            // DEFLATE but is elided from the message payloads. This implicitly
            // flushes out any pending bytes that were part of the previous
            // block and doesn't leave any others since the trailer is explicitly
            // an empty block.
            decompress_from(&ELIDED_TRAILER_BLOCK_CONTENTS)?;

            if !self.peer_context_takeover {
                self.decompressor.reset(false);
            }
        }

        Ok(Bytes::from(output))
    }
}

impl From<DeflateContext> for super::PerMessageCompressionContext {
    fn from(value: DeflateContext) -> Self {
        Self::Deflate(value)
    }
}

