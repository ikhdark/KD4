use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use std::sync::Arc;
use std::sync::Mutex;

const OUTPUT_EVIDENCE_MARKER_MAX_BYTES: usize = 256;
const MAX_RETAINED_CHUNKS: usize = 2;

#[derive(Debug, Default)]
pub(crate) struct OutputRecoveryEvidence {
    generation: u64,
    detail: Option<String>,
}

pub(crate) type SharedOutputRecoveryEvidence = Arc<Mutex<OutputRecoveryEvidence>>;

/// A capped buffer that preserves a stable prefix ("head") and suffix ("tail"),
/// dropping the middle once it exceeds the configured maximum. The buffer is
/// symmetric meaning 50% of the capacity is allocated to the head and 50% is
/// allocated to the tail.
#[derive(Debug)]
pub(crate) struct HeadTailBuffer {
    max_bytes: usize,
    head_budget: usize,
    head: Vec<u8>,
    tail: CircularByteBuffer,
    omitted_bytes: usize,
    omitted_lines: usize,
    unreported_omitted_bytes: usize,
    unreported_omitted_lines: usize,
    lagged_chunks: u64,
    unreported_lagged_chunks: u64,
    recovery_evidence: SharedOutputRecoveryEvidence,
    reported_recovery_generation: u64,
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
        Self::new_with_recovery_evidence(max_bytes, Arc::new(Mutex::new(Default::default())))
    }

    pub(crate) fn new_with_recovery_evidence(
        max_bytes: usize,
        recovery_evidence: SharedOutputRecoveryEvidence,
    ) -> Self {
        let head_budget = max_bytes / 2;
        let tail_budget = max_bytes.saturating_sub(head_budget);
        Self {
            max_bytes,
            head_budget,
            head: Vec::with_capacity(head_budget),
            tail: CircularByteBuffer::new(tail_budget),
            omitted_bytes: 0,
            omitted_lines: 0,
            unreported_omitted_bytes: 0,
            unreported_omitted_lines: 0,
            lagged_chunks: 0,
            unreported_lagged_chunks: 0,
            recovery_evidence,
            reported_recovery_generation: 0,
        }
    }

    pub(crate) fn record_recovery_detail(&self, detail: String) -> bool {
        Self::record_shared_recovery_detail(&self.recovery_evidence, detail)
    }

    pub(crate) fn record_shared_recovery_detail(
        recovery_evidence: &SharedOutputRecoveryEvidence,
        detail: String,
    ) -> bool {
        let mut evidence = recovery_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if evidence.detail.as_deref() == Some(detail.as_str()) {
            return false;
        }
        evidence.detail = Some(detail);
        evidence.generation = evidence.generation.saturating_add(1);
        true
    }

    pub(crate) fn take_unreported_recovery_detail(&mut self) -> Option<String> {
        let evidence = self
            .recovery_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if evidence.generation <= self.reported_recovery_generation {
            return None;
        }
        self.reported_recovery_generation = evidence.generation;
        evidence.detail.clone()
    }

    pub(crate) fn record_external_omitted_bytes(&mut self, omitted: usize) {
        self.record_omission(OutputOmission {
            bytes: omitted,
            lines: 0,
        });
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

    #[allow(dead_code)]
    pub(crate) fn omitted_lines(&self) -> usize {
        self.omitted_lines
    }

    /// Consume capacity-omitted bytes not yet reported by an interim response.
    ///
    /// The cumulative `omitted_bytes` value remains available for final output.
    pub(crate) fn take_unreported_omitted_bytes(&mut self) -> usize {
        std::mem::take(&mut self.unreported_omitted_bytes)
    }

    pub(crate) fn take_unreported_omitted_lines(&mut self) -> usize {
        std::mem::take(&mut self.unreported_omitted_lines)
    }

    pub(crate) fn record_lagged_chunks(&mut self, skipped: u64) {
        self.lagged_chunks = self.lagged_chunks.saturating_add(skipped);
        self.unreported_lagged_chunks = self.unreported_lagged_chunks.saturating_add(skipped);
    }

    #[allow(dead_code)]
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
        if self.max_bytes == 0 {
            self.record_omission(OutputOmission::from_bytes(&chunk));
            return;
        }

        // Fill the head budget first, then keep a capped tail.
        let mut remaining = chunk.as_slice();
        if self.head.len() < self.head_budget {
            let head_len = remaining
                .len()
                .min(self.head_budget.saturating_sub(self.head.len()));
            self.head.extend_from_slice(&remaining[..head_len]);
            remaining = &remaining[head_len..];
        }

        let omitted = self.tail.push(remaining);
        self.record_omission(omitted);
    }

    /// Snapshot the retained output as a list of chunks.
    ///
    /// The returned chunks are ordered as: head chunks first, then tail chunks.
    /// Omitted bytes are not represented in the snapshot.
    pub(crate) fn snapshot_chunks(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(MAX_RETAINED_CHUNKS);
        if !self.head.is_empty() {
            out.push(self.head.clone());
        }
        if !self.tail.is_empty() {
            out.push(self.tail.to_bytes());
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
        self.tail.extend_into(&mut out);
        out
    }

    pub(crate) fn render_bytes(&self) -> Vec<u8> {
        self.render_with_fallback(&[])
    }

    pub(crate) fn render_with_fallback(&self, fallback: &[u8]) -> Vec<u8> {
        let retained = self.to_bytes();
        let data = if retained.is_empty() {
            fallback
        } else {
            retained.as_slice()
        };
        let recovery_detail = self
            .recovery_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .detail
            .clone();
        render_capped_output(
            data,
            self.max_bytes,
            self.omitted_bytes,
            self.omitted_lines,
            self.lagged_chunks,
            recovery_detail.as_deref(),
        )
    }

    pub(crate) fn render_external_bytes(&self, data: &[u8]) -> Vec<u8> {
        let recovery_detail = self
            .recovery_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .detail
            .clone();
        render_capped_output(
            data,
            self.max_bytes,
            self.omitted_bytes,
            self.omitted_lines,
            self.lagged_chunks,
            recovery_detail.as_deref(),
        )
    }

    /// Drain all retained chunks from the buffer and reset its byte state.
    ///
    /// The drained chunks are returned in head-then-tail order. Omitted bytes
    /// are discarded along with the retained content. Cumulative and pending
    /// omission/lag accounting are preserved until the caller explicitly
    /// consumes their pending counts.
    pub(crate) fn drain_chunks(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(MAX_RETAINED_CHUNKS);
        if !self.head.is_empty() {
            out.push(self.head.clone());
            self.head.clear();
        }
        if !self.tail.is_empty() {
            out.push(self.tail.take_bytes());
        }
        out
    }

    fn record_omission(&mut self, omitted: OutputOmission) {
        self.omitted_bytes = self.omitted_bytes.saturating_add(omitted.bytes);
        self.omitted_lines = self.omitted_lines.saturating_add(omitted.lines);
        self.unreported_omitted_bytes = self.unreported_omitted_bytes.saturating_add(omitted.bytes);
        self.unreported_omitted_lines = self.unreported_omitted_lines.saturating_add(omitted.lines);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OutputOmission {
    bytes: usize,
    lines: usize,
}

impl OutputOmission {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.len(),
            lines: count_line_breaks(bytes),
        }
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_add(other.bytes),
            lines: self.lines.saturating_add(other.lines),
        }
    }
}

/// Fixed-capacity suffix storage. Once full, new bytes overwrite the oldest
/// bytes in place, so both memory use and append work are bounded by bytes
/// rather than by the number of incoming chunks.
#[derive(Debug)]
struct CircularByteBuffer {
    bytes: Vec<u8>,
    start: usize,
    capacity: usize,
}

impl CircularByteBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            start: 0,
            capacity,
        }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Append bytes and return the number of previously or newly supplied
    /// bytes that no longer fit in the retained suffix.
    fn push(&mut self, input: &[u8]) -> OutputOmission {
        if input.is_empty() {
            return OutputOmission::default();
        }
        if self.capacity == 0 {
            return OutputOmission::from_bytes(input);
        }
        if input.len() >= self.capacity {
            let omitted = OutputOmission::from_bytes(&self.to_bytes()).saturating_add(
                OutputOmission::from_bytes(&input[..input.len().saturating_sub(self.capacity)]),
            );
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&input[input.len() - self.capacity..]);
            self.start = 0;
            return omitted;
        }

        let fill_len = input
            .len()
            .min(self.capacity.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&input[..fill_len]);
        let overwrite = &input[fill_len..];
        if overwrite.is_empty() {
            return OutputOmission::default();
        }

        debug_assert_eq!(self.bytes.len(), self.capacity);
        let overwritten = if self.start + overwrite.len() <= self.capacity {
            OutputOmission::from_bytes(&self.bytes[self.start..self.start + overwrite.len()])
        } else {
            let first = OutputOmission::from_bytes(&self.bytes[self.start..]);
            let second_len = overwrite.len() - (self.capacity - self.start);
            first.saturating_add(OutputOmission::from_bytes(&self.bytes[..second_len]))
        };
        let first_len = overwrite.len().min(self.capacity - self.start);
        self.bytes[self.start..self.start + first_len].copy_from_slice(&overwrite[..first_len]);
        let second_len = overwrite.len().saturating_sub(first_len);
        if second_len > 0 {
            self.bytes[..second_len].copy_from_slice(&overwrite[first_len..]);
        }
        self.start = (self.start + overwrite.len()) % self.capacity;
        overwritten
    }

    fn extend_into(&self, output: &mut Vec<u8>) {
        if self.bytes.len() < self.capacity || self.start == 0 {
            output.extend_from_slice(&self.bytes);
            return;
        }
        output.extend_from_slice(&self.bytes[self.start..]);
        output.extend_from_slice(&self.bytes[..self.start]);
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.bytes.len());
        self.extend_into(&mut output);
        output
    }

    fn take_bytes(&mut self) -> Vec<u8> {
        let output = self.to_bytes();
        self.bytes.clear();
        self.start = 0;
        output
    }
}

fn render_capped_output(
    data: &[u8],
    cap: usize,
    recorded_omitted_bytes: usize,
    recorded_omitted_lines: usize,
    lagged_chunks: u64,
    recovery_detail: Option<&str>,
) -> Vec<u8> {
    if cap == 0 {
        return Vec::new();
    }

    let base_omitted_bytes = recorded_omitted_bytes.saturating_add(data.len().saturating_sub(cap));
    let base_omitted_lines = recorded_omitted_lines;
    if base_omitted_bytes == 0 && lagged_chunks == 0 && recovery_detail.is_none() {
        return data.to_vec();
    }

    let mut omitted_bytes = base_omitted_bytes;
    let mut omitted_lines = base_omitted_lines;
    let mut marker =
        output_evidence_marker(omitted_bytes, omitted_lines, lagged_chunks, recovery_detail);
    for _ in 0..8 {
        let data_budget = cap.saturating_sub(marker.len());
        let stabilized_omitted =
            recorded_omitted_bytes.saturating_add(data.len().saturating_sub(data_budget));
        let retained_head = data.len().min(data_budget / 2);
        let retained_tail = data
            .len()
            .saturating_sub(retained_head)
            .min(data_budget.saturating_sub(data_budget / 2));
        let omitted_end = data.len().saturating_sub(retained_tail);
        let stabilized_omitted_lines = recorded_omitted_lines.saturating_add(count_line_breaks(
            &data[retained_head.min(omitted_end)..omitted_end],
        ));
        let stabilized_marker = output_evidence_marker(
            stabilized_omitted,
            stabilized_omitted_lines,
            lagged_chunks,
            recovery_detail,
        );
        if stabilized_omitted == omitted_bytes
            && stabilized_omitted_lines == omitted_lines
            && stabilized_marker.len() == marker.len()
        {
            marker = stabilized_marker;
            break;
        }
        omitted_bytes = stabilized_omitted;
        omitted_lines = stabilized_omitted_lines;
        marker = stabilized_marker;
    }

    if marker.len() >= cap {
        marker.truncate(cap);
        return marker;
    }

    let data_budget = cap.saturating_sub(marker.len());
    let head_budget = data_budget / 2;
    let tail_budget = data_budget.saturating_sub(head_budget);
    let head_len = data.len().min(head_budget);
    let tail_len = data.len().saturating_sub(head_len).min(tail_budget);
    let mut output = Vec::with_capacity(head_len + marker.len() + tail_len);
    output.extend_from_slice(&data[..head_len]);
    output.extend_from_slice(&marker);
    if tail_len > 0 {
        output.extend_from_slice(&data[data.len() - tail_len..]);
    }
    output
}

fn output_evidence_marker(
    omitted_bytes: usize,
    omitted_lines: usize,
    lagged_chunks: u64,
    recovery_detail: Option<&str>,
) -> Vec<u8> {
    let mut marker = match (omitted_bytes, lagged_chunks, recovery_detail) {
        (omitted_bytes, 0, None) => format!(
            "\n[output truncated: {omitted_bytes} byte(s) and {omitted_lines} line break(s) omitted]\n"
        ),
        (0, lagged_chunks, None) => format!(
            "\n[output unavailable: streaming receiver lagged by {lagged_chunks} chunk(s)]\n"
        ),
        _ => {
            let mut details = Vec::new();
            if omitted_bytes > 0 {
                details.push(format!(
                    "{omitted_bytes} byte(s) and {omitted_lines} line break(s) omitted from the middle by the output retention limit"
                ));
            }
            if lagged_chunks > 0 {
                details.push(format!(
                    "streaming receiver lagged by {lagged_chunks} chunk(s)"
                ));
            }
            if let Some(recovery_detail) = recovery_detail {
                details.push(recovery_detail.to_string());
            }
            format!("\n[output incomplete: {}]\n", details.join("; "))
        }
    }
    .into_bytes();
    if marker.len() > OUTPUT_EVIDENCE_MARKER_MAX_BYTES {
        marker = format!(
            "\n[output incomplete: {omitted_bytes} byte(s) and {omitted_lines} line break(s) omitted; {lagged_chunks} streaming chunk(s) unavailable]\n"
        )
        .into_bytes();
    }
    marker
}

fn count_line_breaks(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

#[cfg(test)]
#[path = "head_tail_buffer_tests.rs"]
mod tests;
