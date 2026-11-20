//! Native accepted-tool lifecycle and opt-in model-stream receive evidence.
//! Neither captures frontend delivery, OS execution or RPC terminal emission.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::num::NonZeroU64;
use std::sync::OnceLock;

const MAX_EVENTS: usize = 8192;
const MAX_TEXT_EVENTS: usize = 512;
const MAX_TEXT_BYTES_PER_BOUNDARY: usize = 4 * 1024 * 1024;
const TEXT_CONSUMER: &str = "kd4-harness-model-stream";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TextDelivery {
    active_streams: usize,
    budget_exhausted: bool,
    produced_bytes: usize,
    received_bytes: usize,
    events: Vec<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Capture {
    clock: Option<String>,
    capture_id: String,
    events: Vec<Value>,
    requested: BTreeSet<String>,
    returned: BTreeSet<String>,
    sequence: u64,
    dropped: u64,
    invalid: bool,
    started: bool,
    closed: Option<Value>,
    text_delivery: TextDelivery,
}

impl Capture {
    fn event(&mut self, at_ns: u128, kind: &str, call: Option<&str>) {
        if self.events.len() >= MAX_EVENTS || self.sequence == u64::MAX {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let at_ms = at_ns as f64 / 1e6;
        if !self.started
            || !at_ms.is_finite()
            || at_ms >= (1_u64 << 53) as f64
            || self
                .events
                .last()
                .and_then(|row| row["at_ms"].as_f64())
                .is_some_and(|previous| previous > at_ms)
        {
            self.invalid = true;
            return;
        }
        self.sequence += 1;
        let mut row = json!({"id":format!("{}:{}", self.capture_id, self.sequence),
            "seq":self.sequence, "at_ms":at_ms, "kind":kind});
        if let Some(call) = call {
            // Execution identities disambiguate wire call IDs reused by later
            // generations. No arguments, output bodies or filesystem paths.
            row["call_id"] = json!(call);
            if kind == "tool_requested" {
                row["human_wait"] = json!(false);
            }
        }
        self.events.push(row);
    }

    pub(super) fn start(&mut self, at_ns: u128, clock: Option<&str>) {
        if self.started {
            self.invalid = true;
            return;
        }
        self.clock = clock.map(str::to_owned);
        self.capture_id = uuid::Uuid::new_v4().to_string();
        self.started = true;
        self.event(at_ns, "turn_started", None);
    }

    pub(super) fn request(&mut self, execution: &str, at_ns: u128) {
        if self.closed.is_some() {
            return;
        } // Outside this closed capture window.
        if execution.is_empty() || execution.len() > 256 {
            self.invalid = true;
            return;
        }
        if self.requested.len() >= MAX_EVENTS {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        if self.requested.insert(execution.to_owned()) {
            self.event(at_ns, "tool_requested", Some(execution));
        }
    }

    pub(super) fn result(&mut self, execution: &str, at_ns: u128) {
        if self.closed.is_some() {
            return;
        }
        if !self.requested.contains(execution) {
            // An unbound result may be a capture/version/admission mismatch;
            // do not certify an exhaustive inventory from that observation.
            self.invalid = true;
            return;
        }
        if self.returned.insert(execution.to_owned()) {
            self.event(at_ns, "tool_result", Some(execution));
        }
    }

    fn text_chunk(
        &mut self,
        received_tag: Option<NonZeroU64>,
        text: &str,
        at_ns: u128,
    ) -> Option<NonZeroU64> {
        if self.closed.is_some() || self.text_delivery.budget_exhausted {
            return None;
        }
        let bytes = if received_tag.is_some() {
            &mut self.text_delivery.received_bytes
        } else {
            &mut self.text_delivery.produced_bytes
        };
        if self.text_delivery.events.len() >= MAX_TEXT_EVENTS
            || bytes.saturating_add(text.len()) > MAX_TEXT_BYTES_PER_BOUNDARY
            || self.sequence >= u64::MAX - 2
        {
            self.text_delivery.budget_exhausted = true;
            return None;
        }
        *bytes += text.len();
        self.sequence += 1;
        let tag = received_tag.or_else(|| NonZeroU64::new(self.sequence))?;
        let kind = if received_tag.is_some() {
            "chunk_received"
        } else {
            "chunk_produced"
        };
        let mut event = json!({
            "id": format!("{}:{}", self.capture_id, self.sequence),
            "seq": self.sequence,
            "at_ms": at_ns as f64 / 1e6,
            "kind": kind,
            "chunk_id": tag.to_string(),
            "content_sha256": hex::encode(Sha256::digest(text.as_bytes())),
            "content_bytes": text.len(),
        });
        if received_tag.is_some() {
            event["consumer_id"] = json!(TEXT_CONSUMER);
            event["boundary"] = json!("received");
            event["client_paused"] = json!(false);
            event["intentional_batch"] = json!(false);
        }
        self.text_delivery.events.push(event);
        Some(tag)
    }

    fn close(
        &mut self,
        at_ns: u128,
        thread: &str,
        turn: &str,
        outcome: &str,
        accepted: usize,
        ungated: bool,
        inventory_complete: bool,
    ) -> Option<Value> {
        if let Some(closed) = &self.closed {
            return Some(closed.clone());
        }
        if !self.started
            || self.clock.is_none()
            || thread.is_empty()
            || thread.len() > 256
            || turn.is_empty()
            || turn.len() > 256
        {
            return None;
        }
        // Scope the extra facts to observed, drained streams. Do not infer
        // loss from a cancelled reader, an open stream or a capture limit.
        let export_text = self.text_delivery.active_streams == 0
            && !self.text_delivery.budget_exhausted
            && !self.text_delivery.events.is_empty()
            && self.events.len() + self.text_delivery.events.len() + 2 <= MAX_EVENTS;
        let terminal_seq = self.sequence + u64::from(export_text);
        if export_text {
            self.sequence = terminal_seq;
        }
        self.event(at_ns, "turn_closed", None);
        let complete = inventory_complete
            && !self.invalid
            && self.dropped == 0
            && self.requested.len() == accepted;
        let mut events = self.events.clone();
        for event in &mut events {
            event["turn_id"] = json!(turn);
        }
        let mut capabilities = vec!["AS-06"];
        // A per-call pending-human-gate inventory is not currently captured.
        // Omit the orphan check whenever this turn observed interactive waits.
        if ungated {
            capabilities.push("AS-05");
        }
        let mut packet = json!({"version":1, "thread_id":thread, "complete":complete,
            "clock_id":self.clock, "sequence_domain":self.capture_id,
            "turn_inventory_complete":complete, "capabilities":capabilities, "events":events,
            "source":"kd4_native_accepted_tool_lifecycle",
            "call_identity_basis":"native_execution_id_not_reused_wire_call_id",
            "source_inventory_complete":inventory_complete,
            "accepted_native_call_count":accepted, "recorded_request_count":self.requested.len(),
            "recorded_result_count":self.returned.len(), "capture_invalid":self.invalid,
            "dropped_events":self.dropped});
        if serde_json::to_vec(&packet).ok()?.len() > 2 * 1024 * 1024 {
            return None;
        }
        if complete && export_text {
            let mut candidate = packet.clone();
            let mut events = events;
            let mut delivery_events = std::mem::take(&mut self.text_delivery.events);
            for event in &mut delivery_events {
                event["turn_id"] = json!(turn);
                if event["kind"] == "chunk_received" {
                    event["received_thread"] = json!(thread);
                }
            }
            events.extend(delivery_events);
            events.push(json!({
                "id": format!("{}:{}", self.capture_id, terminal_seq), "seq": terminal_seq,
                "at_ms": at_ns as f64 / 1e6, "kind": "turn_terminal", "turn_id": turn,
                "outcome": outcome,
            }));
            events.sort_unstable_by_key(|event| event["seq"].as_u64());
            candidate["events"] = json!(events);
            candidate["chunk_inventory_complete"] = json!(true);
            candidate["chunk_delivery"] = json!("at_most_once");
            candidate["delivery_contract"] = json!({
                "version": 1, "consumer_id": TEXT_CONSUMER, "consumer_kind": "client",
                "boundary": "received", "representation": "kd4-mapped-assistant-text-utf8-v1",
                "ordered": true, "inventory_complete": true,
            });
            if serde_json::to_vec(&candidate).ok()?.len() <= 2 * 1024 * 1024 {
                packet = candidate;
            }
        }
        self.closed = Some(packet.clone());
        Some(packet)
    }
}

impl TurnTimingState {
    pub fn begin_text_delivery_stream(&self) -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        if !*ENABLED
            .get_or_init(|| std::env::var_os("CODEX_KDR_TEXT_DELIVERY").is_some_and(|v| v == "1"))
        {
            return false;
        }
        let mut state = self.state();
        let capture = &mut state.measurements.app_server;
        if !capture.started || capture.closed.is_some() || capture.clock.is_none() {
            return false;
        }
        capture.text_delivery.active_streams += 1;
        true
    }

    pub fn record_text_produced(&self, text: &str) -> Option<NonZeroU64> {
        let mut state = self.state();
        let at_ns = self.clock.sample().time.monotonic_ns;
        state.measurements.app_server.text_chunk(None, text, at_ns)
    }

    pub fn record_text_received(&self, tag: Option<NonZeroU64>, text: &str) {
        let mut state = self.state();
        let at_ns = self.clock.sample().time.monotonic_ns;
        let capture = &mut state.measurements.app_server;
        if tag.is_some() {
            capture.text_chunk(tag, text, at_ns);
        } else {
            // Missing queue identity is not an attributable receipt.
            capture.text_delivery.budget_exhausted = true;
        }
    }

    pub fn finish_text_delivery_stream(&self) {
        let mut state = self.state();
        let active = &mut state.measurements.app_server.text_delivery.active_streams;
        *active = active.saturating_sub(1);
    }

    /// Close only the native accepted-tool observation window. This is NOT a
    /// server terminal-emission, cancel acknowledgment or client render receipt.
    pub fn close_native_app_server_trace(
        &self,
        thread: &str,
        turn: &str,
        outcome: &str,
    ) -> Option<Value> {
        let mut state = self.state();
        let at_ns = self.clock.sample().time.monotonic_ns;
        let accepted = state.tool_closure.entries.len();
        let ungated = state.unions.interactive_wait_ns == 0;
        // Early suppression/admission failure can produce model-visible
        // responses without a native accepted execution. Do not claim an
        // exhaustive turn inventory when those source requests are unaccounted
        // for, or when the independent source-side request history was lost.
        let direct = state
            .tool_closure
            .entries
            .values()
            .filter(|entry| entry.identity.source == TurnTimingToolCallSource::Direct)
            .count() as u64;
        let emitted = state
            .model_requests
            .iter()
            .map(|request| u64::from(request.model_emitted_tool_call_count))
            .sum::<u64>();
        let inventory_complete = !state.model_request_history_truncated
            && state.counters.saturation_count == 0
            && state.counters.clock_regression_count == 0
            && state.counters.invalid_transition_count == 0
            && state
                .model_requests
                .iter()
                .all(|request| request.model_emitted_tool_call_count != u32::MAX)
            && emitted == direct;
        state.measurements.app_server.close(
            at_ns,
            thread,
            turn,
            outcome,
            accepted,
            ungated,
            inventory_complete,
        )
    }
}
