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

pub(super) fn split_valid_utf8_prefix_with_max<'a>(
    buffer: &mut &'a [u8],
    max_bytes: usize,
    flush_incomplete: bool,
) -> Option<&'a [u8]> {
    if buffer.is_empty() || max_bytes == 0 {
        return None;
    }

    let max_len = buffer.len().min(max_bytes);
    let split = match std::str::from_utf8(&buffer[..max_len]) {
        Ok(_) => max_len,
        Err(error) => {
            let valid_up_to = error.valid_up_to();
            if valid_up_to > 0 {
                valid_up_to
            } else if error.error_len().is_some() || flush_incomplete {
                // Definitively invalid bytes must make progress immediately. At the
                // end of the stream, treat a permanently incomplete sequence the
                // same way so every received byte is emitted exactly once.
                1
            } else {
                return None;
            }
        }
    };

    let (prefix, remaining) = buffer.split_at(split);
    *buffer = remaining;
    Some(prefix)
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
    /// Ascending producer positions (retained plus omitted bytes) of lag
    /// seams, excluding seams inside the omitted middle. Once full, later
    /// seams share `lag_overflow`, whose inner positions are unknown.
    lag_seams: Vec<usize>,
    lag_overflow: Option<(usize, usize)>,
}

const MAX_LAG_SEAMS: usize = 1024;

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
            lag_seams: Vec::new(),
            lag_overflow: None,
        }
    }

    #[allow(dead_code)]
    /// Total bytes currently retained by the buffer (head + tail).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

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
            let at = self.retained_bytes().saturating_add(self.omitted_bytes);
            // An omission already reports loss inside the omitted middle.
            let (head, tail_start) = (self.head.len(), self.tail_start());
            self.lag_seams.retain(|&seam| seam < head || seam >= tail_start);
            if self
                .lag_overflow
                .is_some_and(|(first, last)| first >= head && last < tail_start)
            {
                self.lag_overflow = None;
            }
            if let Some((_, last)) = self.lag_overflow.as_mut() {
                *last = at;
            } else if self.lag_seams.last() != Some(&at) {
                if self.lag_seams.len() < MAX_LAG_SEAMS {
                    self.lag_seams.push(at);
                } else {
                    self.lag_overflow = Some((at, at));
                }
            }
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

    /// Display ordering no longer has contiguous coordinates in the raw artifact.
    pub(crate) fn invalidate_output_ranges(&mut self) {
        self.start_offset = None;
        if let Some(report) = self.pending_report.as_mut() {
            report.invalidate_output_ranges();
        }
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
                start_offset: self
                    .start_offset
                    .and_then(|start| start.checked_sub(self.utf8_tail.len() as u64)),
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

    pub(crate) fn projected_pending_output(
        &mut self,
        closed: bool,
        suffix: &[u8],
    ) -> (Vec<u8>, Option<OutputChunkRanges>) {
        let Some(report) = self.pending_report.as_mut() else {
            return (Vec::new(), None);
        };
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
        let end = start
            .checked_add(self.retained_bytes() as u64)?
            .checked_add(self.omitted_bytes as u64)?;
        let gap_start = start.checked_add(self.head.len() as u64)?;
        Some(OutputChunkRanges {
            range: start..end,
            gap: (self.omitted_bytes > 0)
                .then_some(gap_start..gap_start.checked_add(self.omitted_bytes as u64)?),
        })
    }

    fn tail_start(&self) -> usize {
        self.head.len().saturating_add(self.omitted_bytes)
    }

    pub(crate) fn stream_layout(&self) -> StreamLayout {
        StreamLayout {
            head: self.head.len(),
            omitted: self.omitted_bytes,
            lag_seams: self.lag_seams.clone(),
            lag_overflow: self.lag_overflow,
        }
    }

    /// Retained bytes of an undrained cumulative stream buffer.
    pub(super) fn retained_stream(&self) -> RetainedStream<'_> {
        let (front, back) = self.tail.as_slices();
        RetainedStream {
            head: &self.head,
            tail: [front, back],
            omitted: self.omitted_bytes,
            lag_seams: &self.lag_seams,
            lag_overflow: self.lag_overflow,
        }
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
        self.start_offset = self
            .start_offset
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

/// Producer coordinates of a stream snapshot taken with
/// `to_bytes_with_loss_notice(&[])`, so a retired process rescans it exactly.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct StreamLayout {
    head: usize,
    omitted: usize,
    lag_seams: Vec<usize>,
    lag_overflow: Option<(usize, usize)>,
}

/// Retained bytes of a cumulative stream in producer coordinates.
pub(super) struct RetainedStream<'a> {
    head: &'a [u8],
    tail: [&'a [u8]; 2],
    omitted: usize,
    lag_seams: &'a [usize],
    lag_overflow: Option<(usize, usize)>,
}

impl<'a> RetainedStream<'a> {
    pub(super) fn frozen(snapshot: &'a [u8], layout: &'a StreamLayout) -> Self {
        let marker = if layout.omitted > 0 {
            omitted_output_marker(layout.omitted).len()
        } else {
            0
        };
        let head = layout.head.min(snapshot.len());
        RetainedStream {
            head: &snapshot[..head],
            tail: [snapshot.get(head.saturating_add(marker)..).unwrap_or_default(), &[]],
            omitted: layout.omitted,
            lag_seams: &layout.lag_seams,
            lag_overflow: layout.lag_overflow,
        }
    }

    fn tail_start(&self) -> usize {
        self.head.len().saturating_add(self.omitted)
    }

    pub(super) fn total(&self) -> usize {
        self.tail_start()
            .saturating_add(self.tail[0].len())
            .saturating_add(self.tail[1].len())
    }

    /// Whether a scan from producer position `after` examines lost bytes: an
    /// omission ending after it, or a lag seam at or after it. A seam belongs
    /// to the scan that examines the byte following it, so a cursor chain
    /// reports it once; at the end of closed output it stays visible, as exit
    /// does. Overflowed seams are reported conservatively.
    pub(super) fn lost_since(&self, after: usize, closed: bool) -> bool {
        let total = self.total();
        let examined = |seam: usize| seam < total || closed;
        (self.omitted > 0 && self.tail_start() > after)
            || self
                .lag_seams
                .get(self.lag_seams.partition_point(|&seam| seam < after))
                .is_some_and(|&seam| examined(seam))
            || self
                .lag_overflow
                .is_some_and(|(first, last)| last >= after && examined(first.max(after)))
    }

    /// Visit retained producer bytes from position `from` in contiguous pieces.
    /// A piece is not `joined` to the bytes before it across an omission or a
    /// lag seam; bytes between overflowed seams are skipped. The caller owns
    /// its cursor; this never acknowledges normal polling output. Returns the
    /// producer position after the last byte.
    pub(super) fn visit_since(&self, from: usize, mut visit: impl FnMut(usize, &[u8], bool)) -> usize {
        let tail_start = self.tail_start();
        let unknown = self.lag_overflow.map_or(0..0, |(first, last)| first..last);
        let mut seams = self.lag_seams[self.lag_seams.partition_point(|&seam| seam < from)..]
            .iter()
            .copied()
            .chain(self.lag_overflow.map(|(first, _)| first))
            .peekable();
        let mut position = from;
        let mut joined = true;
        for (start, bytes) in [
            (0, self.head),
            (tail_start, self.tail[0]),
            (tail_start.saturating_add(self.tail[0].len()), self.tail[1]),
        ] {
            let end = start.saturating_add(bytes.len());
            if end <= position {
                continue;
            }
            if start > position {
                position = start;
                joined = false;
            }
            while position < end {
                if unknown.contains(&position) {
                    position = unknown.end;
                    joined = false;
                    continue;
                }
                while let Some(seam) = seams.next_if(|&seam| seam <= position) {
                    joined &= seam != position;
                }
                let stop = seams.peek().map_or(end, |&seam| seam.min(end));
                visit(position, &bytes[position - start..stop - start], joined);
                position = stop;
                joined = true;
            }
        }
        self.total()
    }
}

