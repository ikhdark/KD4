//! Call-scoped filtered observations of the existing process owner. No launcher,
//! background task, output acknowledgement, or persistent subscription registry.
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::Instant;

use super::super::UnifiedExecProcessManager;
use super::super::head_tail_buffer::RetainedStream;
use super::UnifiedExecProcess;

const MAX_FILTERS: usize = 16;
const MAX_LITERAL_BYTES: usize = 256;
const CURSOR_BEYOND_OUTPUT: &str = "event_filter.after exceeds the output this process incarnation produced; reuse output_cursor from the same session and incarnation. No subscription was created.";

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CursorKind {
    Observed,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamOutputCursor {
    stream: EventStream,
    observed_bytes: usize,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessOutputCursor {
    kind: CursorKind,
    session_id: u32,
    incarnation: uuid::Uuid,
    stdout: StreamOutputCursor,
    stderr: StreamOutputCursor,
}

impl ProcessOutputCursor {
    fn validate_owner(&self, session_id: u32, incarnation: uuid::Uuid) -> Result<(), String> {
        let CursorKind::Observed = self.kind;
        if self.session_id != session_id || self.incarnation != incarnation {
            return Err("event_filter.after cursor belongs to another process incarnation; no subscription created".into());
        }
        if self.stdout.stream != EventStream::Stdout || self.stderr.stream != EventStream::Stderr {
            return Err("event_filter.after cursor stream identity does not match its position; no subscription created".into());
        }
        Ok(())
    }

    fn positions(self) -> [usize; 2] {
        [self.stdout.observed_bytes, self.stderr.observed_bytes]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventStream {
    #[default]
    Any,
    Stdout,
    Stderr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessEventFilter {
    pub(crate) contains: Vec<String>,
    #[serde(default)]
    stream: EventStream,
    pub(crate) timeout_ms: u64,
    #[serde(default)]
    after: Option<ProcessOutputCursor>,
}

impl ProcessEventFilter {
    pub(crate) fn exit_only(timeout_ms: u64) -> Self {
        Self {
            contains: Vec::new(),
            stream: EventStream::Any,
            timeout_ms,
            after: None,
        }
    }

    pub(crate) fn validate(&self, max_timeout_ms: u64) -> Result<(), String> {
        if self.contains.len() > MAX_FILTERS
            || self
                .contains
                .iter()
                .any(|s| s.is_empty() || s.len() > MAX_LITERAL_BYTES)
        {
            return Err("event_filter.contains accepts at most 16 nonempty UTF-8 literals of at most 256 bytes each".into());
        }
        if !(1..=max_timeout_ms).contains(&self.timeout_ms) {
            return Err(format!(
                "event_filter.timeout_ms must be between 1 and {max_timeout_ms}"
            ));
        }
        Ok(())
    }

    /// Reuse lifecycle evidence without opening a subscription or pretending
    /// retained receipt metadata contains the bytes needed for a literal scan.
    pub(crate) fn retained_observation(
        &self,
        process_id: u32,
        incarnation: uuid::Uuid,
        receipt: &Value,
    ) -> Result<Value, String> {
        if let Some(after) = self.after {
            after.validate_owner(process_id, incarnation)?;
        }
        let mut matcher = Matcher::new(self);
        if !self.contains.is_empty() {
            matcher.gap("unknown", "history_unavailable");
        }
        let exited = receipt["process_exited"] == true;
        matcher.end(
            !self.contains.is_empty(), exited,
            receipt["exit_code"].as_i64().and_then(|code| i32::try_from(code).ok()),
            receipt["error"].as_str().or_else(|| (!exited).then_some("retained process outcome is unresolved")),
            false,
        );
        let mut result = matcher.finish(process_id, incarnation);
        result["completion_provenance"] = receipt["completion_provenance"].clone();
        result["output_reference"] = receipt["output_reference"].clone();
        result["output_drained"] = receipt["output_drained"].clone();
        Ok(result)
    }

    fn selects(&self, stream: &str) -> bool {
        matches!(self.stream, EventStream::Any)
            || matches!(
                (self.stream, stream),
                (EventStream::Stdout, "stdout") | (EventStream::Stderr, "stderr")
            )
    }
}

struct Matcher {
    literals: Vec<String>,
    matched: Vec<bool>,
    overlap: usize,
    events: Vec<Value>,
    history_complete: bool,
    cursors: [usize; 2],
}

impl Matcher {
    fn new(filter: &ProcessEventFilter) -> Self {
        let mut literals = Vec::new();
        for literal in &filter.contains {
            if !literals.contains(literal) {
                literals.push(literal.clone());
            }
        }
        Self {
            matched: vec![false; literals.len()],
            overlap: literals
                .iter()
                .map(|s| s.len().saturating_sub(1))
                .max()
                .unwrap_or(0),
            literals,
            events: Vec::new(),
            history_complete: true,
            cursors: filter
                .after
                .map_or([0, 0], ProcessOutputCursor::positions),
        }
    }

    fn gap(&mut self, stream: &str, reason: &str) {
        self.history_complete = false;
        if !self
            .events
            .iter()
            .any(|event| event["kind"] == "output_gap" && event["stream"] == stream)
        {
            self.events
                .push(json!({"kind":"output_gap", "stream":stream, "reason":reason}));
        }
    }

    fn feed(&mut self, stream: &str, tail: &mut Vec<u8>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Copy only the bounded seam, not each accumulated output snapshot.
        let mut seam = tail.clone();
        seam.extend_from_slice(&bytes[..bytes.len().min(self.overlap)]);
        for (literal, matched) in self.literals.iter().zip(&mut self.matched) {
            if !*matched
                && (memchr::memmem::find(bytes, literal.as_bytes()).is_some()
                    || memchr::memmem::find(
                        &seam[tail.len().saturating_sub(literal.len() - 1)..],
                        literal.as_bytes(),
                    )
                    .is_some())
            {
                *matched = true;
                self.events
                    .push(json!({"kind":"output_match", "stream":stream, "contains":literal}));
            }
        }
        if bytes.len() >= self.overlap {
            tail.clear();
            tail.extend_from_slice(&bytes[bytes.len() - self.overlap..]);
        } else {
            tail.extend_from_slice(bytes);
            let excess = tail.len().saturating_sub(self.overlap);
            tail.drain(..excess);
        }
    }

    fn feed_since(
        &mut self,
        stream: &str,
        tail: &mut Vec<u8>,
        start: usize,
        bytes: &[u8],
        after: usize,
    ) {
        // Seed only the bounded lookbehind. A match must end after the cursor,
        // including a literal split across two separate subscriptions.
        let prefix = after.saturating_sub(start).min(bytes.len());
        tail.extend_from_slice(&bytes[..prefix]);
        self.feed(stream, tail, &bytes[prefix..]);
    }

    fn scan(&mut self, stream: &str, index: usize, retained: &RetainedStream<'_>, closed: bool) {
        let after = self.cursors[index];
        let from = after.saturating_sub(self.overlap);
        // A new literal may begin before `after`. Losing that lookbehind is
        // still a gap even when every byte after the cursor remains retained.
        if retained.lost_since(after, closed)
            || (retained.total() > after && retained.lost_since(from, false))
        {
            self.gap(stream, "output_unavailable");
        }
        // Missing producer bytes have no count; never manufacture a match
        // across an omission or lag, including in the lookbehind.
        let mut tail = Vec::new();
        let total = retained.visit_since(from, |start, bytes, joined| {
            if !joined {
                tail.clear();
            }
            self.feed_since(stream, &mut tail, start, bytes, after);
        });
        self.cursors[index] = after.max(total);
    }

    fn end(
        &mut self,
        literal: bool,
        exited: bool,
        exit_code: Option<i32>,
        failure: Option<&str>,
        closed: bool,
    ) {
        // A failure stops the reader or loses exit observation, so a literal
        // wait may have missed bytes. Ending on exit or failure while output
        // is still open misses later bytes, even for an exit-only wait.
        if (failure.is_some() && literal) || ((exited || failure.is_some()) && !closed) {
            self.history_complete = false;
        }
        if let Some(message) = failure {
            self.events.push(json!({"kind":"failed", "message":message.chars().take(1024).collect::<String>(), "message_truncated":message.chars().count() > 1024}));
        }
        if exited {
            self.events
                .push(json!({"kind":"exited", "exit_code":exit_code}));
        }
    }

    fn finish(self, process_id: u32, incarnation: uuid::Uuid) -> Value {
        let exit_status = if self.events.iter().any(|event| event["kind"] == "exited") {
            "exited"
        } else if self.events.iter().any(|event| {
            event["kind"] == "failed"
                || (event["kind"] == "output_gap" && event["reason"] == "history_expired")
        }) {
            "unavailable"
        } else if self.events.is_empty() {
            "timed_out"
        } else {
            "not_observed"
        };
        json!({
            "session_id":process_id, "incarnation":incarnation,
            "exit_status":exit_status,
            "subscription_closed":true,
            "subscription_end":if self.events.is_empty() { "timeout" } else { "events" },
            "history_complete":self.history_complete, "events":self.events,
            "output_cursor":{
                "kind":"observed", "session_id":process_id, "incarnation":incarnation,
                "stdout":{"stream":"stdout", "observed_bytes":self.cursors[0]},
                "stderr":{"stream":"stderr", "observed_bytes":self.cursors[1]},
            },
        })
    }
}

impl UnifiedExecProcess {
    async fn filtered_events(
        &self,
        filter: &ProcessEventFilter,
        deadline: Instant,
    ) -> Result<Matcher, String> {
        if let Some(after) = filter.after {
            for (buffer, position) in [
                (&self.stdout_buffer, after.stdout.observed_bytes),
                (&self.stderr_buffer, after.stderr.observed_bytes),
            ] {
                let buffer = buffer.lock().await;
                if position > buffer.retained_bytes().saturating_add(buffer.omitted_bytes()) {
                    return Err(CURSOR_BEYOND_OUTPUT.into());
                }
            }
        }
        let mut matcher = Matcher::new(filter);
        let mut state = self.state_rx.clone();
        loop {
            // Arm before reading producer state. Each buffer lock is held alone;
            // no interaction/store lock, blocking send, or new task is needed.
            let output = self.output_notify.notified();
            let closed = self.output_closed_notify.notified();
            tokio::pin!(output, closed);
            // Exit-only subscriptions use the lifecycle watch owner. Output
            // notifications must not wake them or register another log reader.
            if !filter.contains.is_empty() {
                output.as_mut().enable();
                closed.as_mut().enable();
            }
            // Root exit is not output closure. Only closure observed before this
            // scan proves that the scan covered every captured byte.
            let drained = self.output_closed.load(Ordering::Acquire);
            for (index, (stream, buffer)) in [
                ("stdout", &self.stdout_buffer),
                ("stderr", &self.stderr_buffer),
            ]
            .into_iter()
            .enumerate()
            {
                // An exit-only wait examines no bytes, so it returns `after`
                // unchanged; a later literal wait from it still reports loss.
                if filter.contains.is_empty() || !filter.selects(stream) {
                    continue;
                }
                let buffer = buffer.lock().await;
                matcher.scan(stream, index, &buffer.retained_stream(), drained);
            }
            let current = state.borrow_and_update().clone();
            // A literal wait ends on exit only after scanning closed output;
            // otherwise later bytes (including inherited pipes) may still match.
            let exit_ends_wait =
                current.has_exited && (drained || filter.contains.is_empty());
            if !matcher.events.is_empty()
                || current.failure_message.is_some()
                || exit_ends_wait
                || Instant::now() >= deadline
            {
                matcher.end(
                    !filter.contains.is_empty(),
                    current.has_exited,
                    current.exit_code,
                    current.failure_message.as_deref(),
                    drained,
                );
                return Ok(matcher);
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {},
                changed = state.changed() => {
                    if changed.is_err() {
                        matcher.end(
                            !filter.contains.is_empty(),
                            /*exited*/ false,
                            /*exit_code*/ None,
                            Some("process state observation closed"),
                            drained,
                        );
                        return Ok(matcher);
                    }
                },
                _ = output, if !filter.contains.is_empty() => {},
                _ = closed, if !drained && !filter.contains.is_empty() => {},
            }
        }
    }
}

impl UnifiedExecProcessManager {
    /// `bound` is the latest instant a nested caller can still receive a result.
    pub(crate) async fn filtered_process_events(
        &self,
        process_id: u32,
        incarnation: uuid::Uuid,
        filter: &ProcessEventFilter,
        bound: Option<Instant>,
    ) -> Result<Value, String> {
        filter.validate(self.max_write_stdin_yield_time_ms)?;
        if let Some(after) = filter.after {
            after.validate_owner(process_id, incarnation)?;
        }
        let requested = Instant::now() + Duration::from_millis(filter.timeout_ms);
        let deadline = bound.map_or(requested, |bound| bound.min(requested));
        let process = {
            let store = self.process_store.lock().await;
            if let Some(entry) = store.processes.get(&process_id) {
                if entry.process.incarnation() != incarnation {
                    return Err("stale process incarnation; no subscription created".into());
                }
                Arc::clone(&entry.process)
            } else {
                if store.used_process_ids.get(&process_id).copied().flatten() != Some(incarnation) {
                    return Err("stale process incarnation; no subscription created".into());
                }
                let Some((_, result, totals)) = store.finished.iter().find(|(id, _, _)| *id == process_id) else {
                    // The identity tombstone outlives retention, not the output
                    // or exit evidence. Preserve the cursor and expose the gap.
                    drop(store);
                    let mut matcher = Matcher::new(filter);
                    matcher.gap("unknown", "history_expired");
                    return Ok(matcher.finish(process_id, incarnation));
                };
                let literal = !filter.contains.is_empty();
                if filter.after.is_some_and(|after| totals.as_ref().is_some_and(|totals| {
                    after.stdout.observed_bytes as u64 > totals[0]
                        || after.stderr.observed_bytes as u64 > totals[1]
                }))
                {
                    return Err(CURSOR_BEYOND_OUTPUT.into());
                }
                let closed = result.process_output.as_ref().is_some_and(|snapshot| snapshot.output_closed);
                // A settled exit-only observation needs lifecycle facts, not a
                // copy of retained logs under the shared process-store lock.
                let snapshot = if literal { result.process_output.clone() } else { None };
                let failure = result.error.clone();
                let exited = result.process_exited;
                let exit_code = result.exit_code;
                drop(store);
                let mut matcher = Matcher::new(filter);
                // The frozen streams rescan exactly as the live owner did.
                if let Some(snapshot) = &snapshot {
                    let streams = [
                        ("stdout", RetainedStream::frozen(&snapshot.stdout, &snapshot.stdout_layout)),
                        ("stderr", RetainedStream::frozen(&snapshot.stderr, &snapshot.stderr_layout)),
                    ];
                    for (index, (stream, retained)) in streams.iter().enumerate() {
                        // As on the live path, an exit-only wait keeps `after`.
                        if literal && filter.selects(stream) {
                            matcher.scan(stream, index, retained, snapshot.output_closed);
                        }
                    }
                } else if literal {
                    matcher.gap("unknown", "history_unavailable");
                }
                let failure = failure.as_deref().or_else(|| {
                    (!literal && !exited).then_some("retired process owner has no confirmed exit observation")
                });
                matcher.end(literal, exited, exit_code, failure, closed);
                return Ok(matcher.finish(process_id, incarnation));
            }
        };
        Ok(process
            .filtered_events(filter, deadline)
            .await?
            .finish(process_id, incarnation))
    }
}
