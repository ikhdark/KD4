use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use std::collections::VecDeque;

/// Absolute byte coordinates in the process-owned cumulative output artifact.
/// Notices inserted for display are not counted as producer bytes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutputChunkRanges {
    pub(crate) range: std::ops::Range<u64>,
    pub(crate) gap: Option<std::ops::Range<u64>>,
}

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
    pending_report: Option<Box<HeadTailBuffer>>,
    /// At most three incomplete UTF-8 bytes, carried between acknowledged
    /// observations. Raw producer artifacts are independent of this projection.
    utf8_tail: Vec<u8>,
    start_offset: Option<u64>,
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
            pending_report: None,
            utf8_tail: Vec::new(),
            start_offset: Some(0),
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
        if skipped > 0 {
            // A chunk count does not establish the missing byte count.
            self.start_offset = None;
        }
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
    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) {
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

    pub(crate) fn push_display_notice(&mut self, notice: &[u8]) {
        // These bytes are not present in the cumulative producer artifact.
        self.start_offset = None;
        self.push_chunk(notice);
    }

    pub(crate) fn has_unreported_output(&self) -> bool {
        self.pending_report.is_some() || !self.utf8_tail.is_empty() || self.has_uncollected_output()
    }

    pub(crate) fn has_uncollected_output(&self) -> bool {
        self.retained_bytes() > 0
            || self.unreported_omitted_bytes > 0
            || self.unreported_lagged_chunks > 0
    }

    /// Keep the in-flight polling receipt with the producer until preparation
    /// completes. Cancelling a polling future leaves this evidence recoverable.
    pub(crate) fn begin_output_report(&mut self) {
        if self.pending_report.is_none() {
            let mut report = Self {
                start_offset: self.start_offset.and_then(|start| start.checked_sub(self.utf8_tail.len() as u64)),
                ..Self::default()
            };
            report.push_chunk(&std::mem::take(&mut self.utf8_tail));
            self.pending_report = Some(Box::new(report));
        }
    }

    pub(crate) fn collect_pending_output(&mut self) -> bool {
        self.begin_output_report();
        let mut report = self
            .pending_report
            .take()
            .unwrap_or_else(|| Box::new(Self::default()));
        let meaningful = self.drain_into(&mut report);
        self.pending_report = Some(report);
        meaningful
    }

    pub(crate) fn pending_output(&self) -> Option<&Self> {
        self.pending_report.as_deref()
    }

    pub(crate) fn acknowledge_pending_output(&mut self) {
        if let Some(report) = self.pending_report.take() {
            self.utf8_tail = report.utf8_tail;
        }
    }

    pub(crate) fn projected_pending_output(&mut self, closed: bool, suffix: &[u8]) -> (Vec<u8>, Option<OutputChunkRanges>) {
        let Some(report) = self.pending_report.as_mut() else { return (Vec::new(), None); };
        let mut output = report.to_bytes_with_loss_notice(suffix);
        let mut ranges = report.output_ranges();
        report.utf8_tail.clear();
        if !closed && suffix.is_empty() {
            // Validate only the suffix: prior invalid bytes must remain lossy
            // errors, not cause a genuinely incomplete final character to flush.
            let mut offset = output.len().saturating_sub(4);
            while offset < output.len() {
                match std::str::from_utf8(&output[offset..]) {
                    Ok(_) => break,
                    Err(error) => {
                        offset += error.valid_up_to();
                        if let Some(length) = error.error_len() {
                            offset += length;
                        } else {
                            report.utf8_tail.extend_from_slice(&output[offset..]);
                            output.truncate(offset);
                            if let Some(ranges) = &mut ranges {
                                ranges.range.end -= report.utf8_tail.len() as u64;
                            }
                            break;
                        }
                    }
                }
            }
        }
        (output, ranges)
    }

    pub(crate) fn output_ranges(&self) -> Option<OutputChunkRanges> {
        let start = self.start_offset?;
        let end = start.checked_add(self.retained_bytes() as u64)?
            .checked_add(self.omitted_bytes as u64)?;
        let gap_start = start.checked_add(self.head.len() as u64)?;
        Some(OutputChunkRanges {
            range: start..end,
            gap: (self.omitted_bytes > 0)
                .then_some(gap_start..gap_start.checked_add(self.omitted_bytes as u64)?),
        })
    }

    /// Drain into another bounded buffer without turning omission notices into
    /// output bytes. Returns whether this batch contains meaningful progress.
    pub(crate) fn drain_into(&mut self, target: &mut Self) -> bool {
        let omitted = self.take_unreported_omitted_bytes();
        let lagged = self.take_unreported_lagged_chunks();
        let consumed = (self.retained_bytes() as u64).checked_add(omitted as u64);
        if self.start_offset.is_none() {
            target.start_offset = None;
        }
        self.start_offset = self.start_offset
            .and_then(|start| start.checked_add(consumed?));
        let meaningful = omitted > 0
            || lagged > 0
            || self
                .head
                .iter()
                .chain(self.tail.iter())
                .any(|byte| !byte.is_ascii_whitespace());
        target.push_chunk(&self.head);
        if omitted > 0 {
            // A source gap must remain at the target's head/tail seam. Freeze
            // a partially filled head and discard the pre-gap tail so bytes
            // from opposite sides cannot be joined into fabricated output.
            target.head_budget = target.head.len();
            target.record_omitted_bytes(omitted.saturating_add(target.tail.len()));
            target.tail.clear();
        }
        let (front, back) = self.tail.as_slices();
        target.push_chunk(front);
        target.push_chunk(back);
        target.record_lagged_chunks(lagged);
        self.head.clear();
        self.tail.clear();
        meaningful
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
    #[cfg(test)]
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        self.to_bytes_with_markers(&[], &[])
    }

    /// Return retained output with an explicit marker at the head/tail seam.
    #[cfg(test)]
    pub(crate) fn to_bytes_with_omission_marker(&self, omission_marker: &[u8]) -> Vec<u8> {
        self.to_bytes_with_markers(omission_marker, &[])
    }

    /// Snapshot output, formatting a retention notice only when bytes were lost.
    /// Reserve space for an optional trailing loss notice in the same allocation.
    pub(crate) fn to_bytes_with_loss_notice(&self, suffix: &[u8]) -> Vec<u8> {
        let marker = if self.omitted_bytes > 0 {
            omitted_output_marker(self.omitted_bytes)
        } else {
            Vec::new()
        };
        self.to_bytes_with_markers(&marker, suffix)
    }

    fn to_bytes_with_markers(&self, omission_marker: &[u8], suffix: &[u8]) -> Vec<u8> {
        let omission_marker = if self.omitted_bytes > 0 {
            omission_marker
        } else {
            &[]
        };
        let mut out = Vec::with_capacity(
            self.retained_bytes()
                .saturating_add(omission_marker.len())
                .saturating_add(suffix.len()),
        );
        out.extend_from_slice(&self.head);
        out.extend_from_slice(omission_marker);
        let (front, back) = self.tail.as_slices();
        out.extend_from_slice(front);
        out.extend_from_slice(back);
        out.extend_from_slice(suffix);
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
    #[cfg(test)]
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
