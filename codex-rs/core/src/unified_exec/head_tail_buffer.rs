use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use std::collections::VecDeque;

pub(super) fn omitted_output_marker(omitted_bytes: usize) -> Vec<u8> {
    format!(
        "\n[output truncated: {omitted_bytes} byte(s) omitted from the middle by the output retention limit]\n"
    )
    .into_bytes()
}

/// A capped buffer that preserves a stable prefix ("head") and suffix ("tail"),
/// dropping the middle once it exceeds the configured maximum. The buffer is
/// symmetric meaning 50% of the capacity is allocated to the head and 50% is
/// allocated to the tail.
#[derive(Debug)]
pub(crate) struct HeadTailBuffer {
    max_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted_bytes: usize,
    unreported_omitted_bytes: usize,
    lagged_chunks: u64,
    unreported_lagged_chunks: u64,
}

impl Default for HeadTailBuffer {
    fn default() -> Self {
        Self::new(UNIFIED_EXEC_OUTPUT_MAX_BYTES)
    }
}

impl HeadTailBuffer {
    /// Create a new buffer that retains at most `max_bytes` of output.
    ///
    /// The retained output is split across a prefix ("head") and suffix ("tail")
    /// budget, dropping bytes from the middle once the limit is exceeded.
    pub(crate) fn new(max_bytes: usize) -> Self {
        let head_budget = max_bytes / 2;
        let tail_budget = max_bytes.saturating_sub(head_budget);
        Self {
            max_bytes,
            head_budget,
            tail_budget,
            head: Vec::new(),
            tail: VecDeque::new(),
            omitted_bytes: 0,
            unreported_omitted_bytes: 0,
            lagged_chunks: 0,
            unreported_lagged_chunks: 0,
        }
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes currently retained by the buffer (head + tail).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes that were dropped from the middle due to the size cap.
    pub(crate) fn omitted_bytes(&self) -> usize {
        self.omitted_bytes
    }

    /// Consume capacity-omitted bytes not yet reported by an interim response.
    ///
    /// The cumulative `omitted_bytes` value remains available for final output.
    pub(crate) fn take_unreported_omitted_bytes(&mut self) -> usize {
        std::mem::take(&mut self.unreported_omitted_bytes)
    }

    pub(crate) fn record_lagged_chunks(&mut self, skipped: u64) {
        self.lagged_chunks = self.lagged_chunks.saturating_add(skipped);
        self.unreported_lagged_chunks = self.unreported_lagged_chunks.saturating_add(skipped);
    }

    pub(crate) fn lagged_chunks(&self) -> u64 {
        self.lagged_chunks
    }

    /// Consume the lag count not yet reported by an interim tool response.
    ///
    /// The cumulative `lagged_chunks` value remains available for the final
    /// aggregate, so draining output cannot make a prior gap disappear.
    pub(crate) fn take_unreported_lagged_chunks(&mut self) -> u64 {
        std::mem::take(&mut self.unreported_lagged_chunks)
    }

    /// Append a chunk of bytes to the buffer.
    ///
    /// Bytes are first added to the head until the head budget is full; any
    /// remaining bytes are added to the tail, with older tail bytes being
    /// dropped to preserve the tail budget.
    pub(crate) fn push_chunk(&mut self, chunk: Vec<u8>) {
        if chunk.is_empty() {
            return;
        }
        if self.max_bytes == 0 {
            self.record_omitted_bytes(chunk.len());
            return;
        }

        // Fill the head budget first, then keep a capped tail.
        let remaining_head = self.head_budget.saturating_sub(self.head.len());
        let head_len = remaining_head.min(chunk.len());
        if head_len > 0 {
            self.head.extend_from_slice(&chunk[..head_len]);
        }
        self.push_to_tail(&chunk[head_len..]);
    }

    /// Snapshot the retained output as a list of chunks.
    ///
    /// The returned chunks are ordered as: head chunks first, then tail chunks.
    /// Omitted bytes are not represented in the snapshot.
    #[cfg(test)]
    pub(crate) fn snapshot_chunks(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(2);
        if !self.head.is_empty() {
            out.push(self.head.clone());
        }
        if !self.tail.is_empty() {
            out.push(self.tail.iter().copied().collect());
        }
        out
    }

    /// Return the retained output as a single byte vector.
    ///
    /// The output is formed by concatenating head chunks, then tail chunks.
    /// Omitted bytes are not represented in the returned value.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        out.extend_from_slice(&self.head);
        out.extend(self.tail.iter().copied());
        out
    }

    /// Return retained output with an explicit marker at the head/tail seam.
    pub(crate) fn to_bytes_with_omission_marker(&self, omission_marker: &[u8]) -> Vec<u8> {
        if self.omitted_bytes == 0 || self.retained_bytes() == 0 {
            return self.to_bytes();
        }

        let mut out =
            Vec::with_capacity(self.retained_bytes().saturating_add(omission_marker.len()));
        out.extend_from_slice(&self.head);
        out.extend_from_slice(omission_marker);
        out.extend(self.tail.iter().copied());
        out
    }

    /// Drain all retained chunks from the buffer and reset its byte state.
    ///
    /// The drained chunks are returned in head-then-tail order. Omitted bytes
    /// are discarded along with the retained content. Cumulative and pending
    /// omission/lag accounting are preserved until the caller explicitly
    /// consumes their pending counts.
    #[cfg(test)]
    pub(crate) fn drain_chunks(&mut self) -> Vec<Vec<u8>> {
        self.drain_chunks_with_omission_marker(None)
    }

    /// Drain retained chunks with an optional marker at the head/tail seam.
    pub(crate) fn drain_chunks_with_omission_marker(
        &mut self,
        omission_marker: Option<Vec<u8>>,
    ) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(3);
        if !self.head.is_empty() {
            out.push(std::mem::take(&mut self.head));
        }
        if let Some(marker) = omission_marker {
            out.push(marker);
        }
        if !self.tail.is_empty() {
            out.push(Vec::from(std::mem::take(&mut self.tail)));
        }
        out
    }

    fn push_to_tail(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.tail_budget == 0 {
            self.record_omitted_bytes(chunk.len());
            return;
        }

        if chunk.len() >= self.tail_budget {
            // This single chunk is larger than the whole tail budget. Keep only the last
            // tail_budget bytes and drop everything else.
            let start = chunk.len().saturating_sub(self.tail_budget);
            let kept = &chunk[start..];
            let dropped = chunk.len().saturating_sub(kept.len());
            self.record_omitted_bytes(self.tail.len().saturating_add(dropped));
            self.tail.clear();
            self.tail.extend(kept);
            return;
        }

        self.tail.extend(chunk);
        self.trim_tail_to_budget();
    }

    fn trim_tail_to_budget(&mut self) {
        let excess = self.tail.len().saturating_sub(self.tail_budget);
        if excess > 0 {
            drop(self.tail.drain(..excess));
            self.record_omitted_bytes(excess);
        }
    }

    fn record_omitted_bytes(&mut self, omitted: usize) {
        self.omitted_bytes = self.omitted_bytes.saturating_add(omitted);
        self.unreported_omitted_bytes = self.unreported_omitted_bytes.saturating_add(omitted);
    }
}

#[cfg(test)]
#[path = "head_tail_buffer_tests.rs"]
mod tests;
