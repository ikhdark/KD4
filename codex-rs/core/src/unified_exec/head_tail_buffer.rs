use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

const OUTPUT_EVIDENCE_MARKER_MAX_BYTES: usize = 256;

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
    tail_budget: usize,
    head: VecDeque<Vec<u8>>,
    tail: VecDeque<Vec<u8>>,
    head_bytes: usize,
    tail_bytes: usize,
    omitted_bytes: usize,
    unreported_omitted_bytes: usize,
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
            tail_budget,
            head: VecDeque::new(),
            tail: VecDeque::new(),
            head_bytes: 0,
            tail_bytes: 0,
            omitted_bytes: 0,
            unreported_omitted_bytes: 0,
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
        self.record_omitted_bytes(omitted);
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes currently retained by the buffer (head + tail).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.head_bytes.saturating_add(self.tail_bytes)
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
            self.record_omitted_bytes(chunk.len());
            return;
        }

        // Fill the head budget first, then keep a capped tail.
        if self.head_bytes < self.head_budget {
            let remaining_head = self.head_budget.saturating_sub(self.head_bytes);
            if chunk.len() <= remaining_head {
                self.head_bytes = self.head_bytes.saturating_add(chunk.len());
                self.head.push_back(chunk);
                return;
            }

            // Split the chunk: part goes to head, remainder goes to tail.
            let (head_part, tail_part) = chunk.split_at(remaining_head);
            if !head_part.is_empty() {
                self.head_bytes = self.head_bytes.saturating_add(head_part.len());
                self.head.push_back(head_part.to_vec());
            }
            self.push_to_tail(tail_part.to_vec());
            return;
        }

        self.push_to_tail(chunk);
    }

    /// Snapshot the retained output as a list of chunks.
    ///
    /// The returned chunks are ordered as: head chunks first, then tail chunks.
    /// Omitted bytes are not represented in the snapshot.
    pub(crate) fn snapshot_chunks(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        out.extend(self.head.iter().cloned());
        out.extend(self.tail.iter().cloned());
        out
    }

    /// Return the retained output as a single byte vector.
    ///
    /// The output is formed by concatenating head chunks, then tail chunks.
    /// Omitted bytes are not represented in the returned value.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        for chunk in self.head.iter() {
            out.extend_from_slice(chunk);
        }
        for chunk in self.tail.iter() {
            out.extend_from_slice(chunk);
        }
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
        let mut out: Vec<Vec<u8>> = self.head.drain(..).collect();
        out.extend(self.tail.drain(..));
        self.head_bytes = 0;
        self.tail_bytes = 0;
        out
    }

    fn push_to_tail(&mut self, chunk: Vec<u8>) {
        if self.tail_budget == 0 {
            self.record_omitted_bytes(chunk.len());
            return;
        }

        if chunk.len() >= self.tail_budget {
            // This single chunk is larger than the whole tail budget. Keep only the last
            // tail_budget bytes and drop everything else.
            let start = chunk.len().saturating_sub(self.tail_budget);
            let kept = chunk[start..].to_vec();
            let dropped = chunk.len().saturating_sub(kept.len());
            self.record_omitted_bytes(self.tail_bytes.saturating_add(dropped));
            self.tail.clear();
            self.tail_bytes = kept.len();
            self.tail.push_back(kept);
            return;
        }

        self.tail_bytes = self.tail_bytes.saturating_add(chunk.len());
        self.tail.push_back(chunk);
        self.trim_tail_to_budget();
    }

    fn trim_tail_to_budget(&mut self) {
        let mut excess = self.tail_bytes.saturating_sub(self.tail_budget);
        while excess > 0 {
            let (omitted, done) = match self.tail.front_mut() {
                Some(front) if excess >= front.len() => {
                    let front_len = front.len();
                    excess -= front_len;
                    self.tail_bytes = self.tail_bytes.saturating_sub(front_len);
                    self.tail.pop_front();
                    (front_len, false)
                }
                Some(front) => {
                    let omitted = excess;
                    front.drain(..excess);
                    self.tail_bytes = self.tail_bytes.saturating_sub(excess);
                    (omitted, true)
                }
                None => break,
            };
            self.record_omitted_bytes(omitted);
            if done {
                break;
            }
        }
    }

    fn record_omitted_bytes(&mut self, omitted: usize) {
        self.omitted_bytes = self.omitted_bytes.saturating_add(omitted);
        self.unreported_omitted_bytes = self.unreported_omitted_bytes.saturating_add(omitted);
    }
}

fn render_capped_output(
    data: &[u8],
    cap: usize,
    recorded_omitted_bytes: usize,
    lagged_chunks: u64,
    recovery_detail: Option<&str>,
) -> Vec<u8> {
    if cap == 0 {
        return Vec::new();
    }

    let base_omitted_bytes = recorded_omitted_bytes.saturating_add(data.len().saturating_sub(cap));
    if base_omitted_bytes == 0 && lagged_chunks == 0 && recovery_detail.is_none() {
        return data.to_vec();
    }

    let mut omitted_bytes = base_omitted_bytes;
    let mut marker = output_evidence_marker(omitted_bytes, lagged_chunks, recovery_detail);
    for _ in 0..8 {
        let data_budget = cap.saturating_sub(marker.len());
        let stabilized_omitted =
            recorded_omitted_bytes.saturating_add(data.len().saturating_sub(data_budget));
        let stabilized_marker =
            output_evidence_marker(stabilized_omitted, lagged_chunks, recovery_detail);
        if stabilized_omitted == omitted_bytes && stabilized_marker.len() == marker.len() {
            marker = stabilized_marker;
            break;
        }
        omitted_bytes = stabilized_omitted;
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
    lagged_chunks: u64,
    recovery_detail: Option<&str>,
) -> Vec<u8> {
    let mut marker = match (omitted_bytes, lagged_chunks, recovery_detail) {
        (omitted_bytes, 0, None) => format!(
            "\n[output truncated: {omitted_bytes} byte(s) omitted from the middle by the output retention limit]\n"
        ),
        (0, lagged_chunks, None) => format!(
            "\n[output unavailable: streaming receiver lagged by {lagged_chunks} chunk(s)]\n"
        ),
        _ => {
            let mut details = Vec::new();
            if omitted_bytes > 0 {
                details.push(format!(
                    "{omitted_bytes} byte(s) omitted from the middle by the output retention limit"
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
        marker = b"\n[output incomplete]\n".to_vec();
    }
    marker
}

#[cfg(test)]
#[path = "head_tail_buffer_tests.rs"]
mod tests;
