//! Native accepted-tool lifecycle interchange. Events are recorded at their
//! owners, not reconstructed/sorted from rounded terminal timing rows. This
//! deliberately certifies no client delivery, OS execution or terminal emission.
use super::*;
use serde_json::{Value, json};

const MAX_EVENTS: usize = 8192;

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

    fn close(
        &mut self,
        at_ns: u128,
        thread: &str,
        turn: &str,
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
        let packet = json!({"version":1, "thread_id":thread, "complete":complete,
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
        self.closed = Some(packet.clone());
        Some(packet)
    }
}

impl TurnTimingState {
    /// Close only the native accepted-tool observation window. This is NOT a
    /// server terminal-emission, cancel acknowledgment or client render receipt.
    pub fn close_native_app_server_trace(&self, thread: &str, turn: &str) -> Option<Value> {
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
            accepted,
            ungated,
            inventory_complete,
        )
    }
}
