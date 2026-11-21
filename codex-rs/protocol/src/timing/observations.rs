//! Opt-in, content-free owner observations that can outlive a core turn.
//! Unlike the numeric client probe, this lane preserves exact string identities
//! and causal references. A receipt is not a provider or paint observation.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ROW_BYTES: usize = 1024 * 1024;
const MAX_SEGMENTS: u32 = 128;
const LANES: [&str; 4] = ["runtime", "mobile", "lifecycle", "request"];

struct Journal {
    lanes: [Option<Lane>; 4],
    sequence: u64,
    directory: PathBuf,
    rollover: bool,
}

struct Lane {
    file: File,
    sequence: u64,
    bytes: usize,
    stopped: bool,
    segment: u32,
    last_id: Option<String>,
    previous_segment_last_id: Option<String>,
}

fn journal() -> Option<&'static Mutex<Journal>> {
    static JOURNAL: OnceLock<Option<Mutex<Journal>>> = OnceLock::new();
    JOURNAL
        .get_or_init(|| {
            if std::env::var("CODEX_RUNTIME_OBSERVATIONS").as_deref() != Ok("1") {
                return None;
            }
            let directory = PathBuf::from(std::env::var_os("CODEX_RUNTIME_OBSERVATIONS_DIR")?);
            if !directory.is_absolute() || !directory.is_dir() {
                return None;
            }
            let open = |suffix: &str| {
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.join(format!(
                        "runtime-{}-{}{}.jsonl",
                        std::process::id(),
                        super::process_clock_domain_id(),
                        suffix
                    )))
                    .ok()?;
                Some(Lane {
                    file, sequence: 0, bytes: 0, stopped: false, segment: 0,
                    last_id: None, previous_segment_last_id: None,
                })
            };
            Some(Mutex::new(Journal {
                lanes: [Some(open("")?), open("-mobile"), open("-lifecycle"), open("-request")],
                sequence: 0,
                directory,
                rollover: std::env::var_os("CODEX_RUNTIME_OBSERVATIONS_ROLLOVER").is_some_and(|value| value == "1"),
            }))
        })
        .as_ref()
}

pub fn enabled() -> bool {
    journal().is_some()
}

#[derive(Clone, Debug)]
pub struct Receipt {
    pub id: String,
    pub at_ns: u64,
}

/// Call only at an owning boundary. `data` must contain identities, counts and
/// outcomes, never commands, environment values, message bodies or credentials.
/// Serialization, overflow and write failures return no receipt. No retry runs.
pub fn record(
    kind: &str,
    thread: &str,
    turn: &str,
    operation: &str,
    data: Value,
) -> Option<Receipt> {
    let journal = journal()?;
    let mut journal = journal.lock().ok()?;
    let journal = &mut *journal;
    // Frame detail must not exhaust the budget for request disposition or
    // socket lifecycle. Keep the retirement reserve independent of both.
    let requested_lane = if kind == "mobile_connection_retired" {
        2
    } else if kind.starts_with("mobile_rpc_") || kind.starts_with("mobile_socket_") {
        3
    } else if kind.starts_with("mobile_") {
        1
    } else {
        0
    };
    // A secondary file failure must not disable ordinary observations. Fall
    // back to the original shared budget and explicitly report lost isolation.
    let lane_index = if journal.lanes[requested_lane].is_some() { requested_lane } else { 0 };
    let lane_name = LANES[lane_index];
    let limit = match lane_index {
        2 => 64 * 1024,
        3 => 4 * 1024 * 1024,
        _ => MAX_BYTES,
    };
    let unavailable_lanes: Vec<_> = LANES.into_iter()
        .enumerate().filter_map(|(index, name)| journal.lanes[index].is_none().then_some(name)).collect();
    if journal.lanes[lane_index].as_ref()?.stopped {
        return None;
    }
    let sequence = journal.sequence.checked_add(1)?;
    let rollover = journal.rollover;
    let directory = &journal.directory;
    let lane = journal.lanes[lane_index].as_mut()?;
    let lane_sequence = lane.sequence.checked_add(1)?;
    let at_ns = u64::try_from(super::process_monotonic_ns()).ok()?;
    let id = format!("{}:{sequence}", super::process_clock_domain_id());
    let mut row = json!({"version":1, "pid":std::process::id(),
        "clock_id":super::process_clock_domain_id(), "seq":sequence, "id":id,
        "journal_lane":lane_name, "lane_seq":lane_sequence,
        "at_ns":at_ns, "kind":kind, "thread_id":thread, "turn_id":turn,
        "operation_id":operation, "data":data});
    if !unavailable_lanes.is_empty() {
        row["unavailable_journal_lanes"] = json!(unavailable_lanes);
    }
    if rollover {
        row["journal_segment"] = json!(lane.segment);
        row["previous_segment_last_id"] = json!(lane.previous_segment_last_id);
    }
    let mut bytes = serde_json::to_vec(&row).ok()?;
    let mut rollover_failed = false;
    if rollover && bytes.len() <= MAX_ROW_BYTES && lane.bytes + bytes.len() + 1024 > limit
        && lane.segment + 1 < MAX_SEGMENTS
    {
        let suffix = ["", "-mobile", "-lifecycle", "-request"][lane_index];
        let path = directory.join(format!(
            "runtime-{}-{}{suffix}-part{:06}.jsonl",
            std::process::id(), super::process_clock_domain_id(), lane.segment + 1
        ));
        if let Ok(file) = OpenOptions::new().write(true).create_new(true).open(path) {
            lane.file = file;
            lane.segment += 1;
            lane.bytes = 0;
            lane.sequence = 0;
            lane.previous_segment_last_id = lane.last_id.clone();
            row["lane_seq"] = json!(1);
            row["journal_segment"] = json!(lane.segment);
            row["previous_segment_last_id"] = json!(lane.previous_segment_last_id);
            bytes = serde_json::to_vec(&row).ok()?;
        } else {
            rollover_failed = true;
        }
    }
    if bytes.len() > MAX_ROW_BYTES || lane.bytes + bytes.len() + 1024 > limit {
        let reason = if bytes.len() > MAX_ROW_BYTES { "row_too_large" }
            else if rollover_failed { "rollover_create_failed" }
            else if rollover { "segment_limit" }
            else { "byte_limit" };
        row["kind"] = json!("capture_overflow");
        row["data"] = json!({"row_bytes":bytes.len(), "retained_bytes":lane.bytes,
            "reason":reason, "first_dropped_at_ns":at_ns, "last_retained_event_id":lane.last_id});
        bytes = serde_json::to_vec(&row).ok()?;
        lane.stopped = true;
    }
    bytes.push(b'\n');
    if lane.file.write_all(&bytes).is_err() {
        lane.stopped = true;
        return None;
    }
    lane.sequence += 1;
    lane.last_id = Some(id.clone());
    lane.bytes += bytes.len();
    let stopped = lane.stopped;
    journal.sequence = sequence;
    if stopped {
        None
    } else {
        Some(Receipt { id, at_ns })
    }
}

/// A dropped scope closes as abandoned: cancellation, an early error and
/// unwinding cannot be distinguished by Drop alone.
/// This guard owns no task, permission, lock, cancellation token or retry.
pub struct Span {
    start: Option<Receipt>,
    kind: String,
    thread: String,
    turn: String,
    operation: String,
}

impl Span {
    pub fn start(kind: &str, thread: &str, turn: &str, operation: &str, data: Value) -> Self {
        let start = record(&format!("{kind}_started"), thread, turn, operation, data);
        if start.is_none() {
            return Self {
                start: None,
                kind: String::new(),
                thread: String::new(),
                turn: String::new(),
                operation: String::new(),
            };
        }
        Self {
            start,
            kind: kind.into(),
            thread: thread.into(),
            turn: turn.into(),
            operation: operation.into(),
        }
    }

    pub fn finish(mut self, outcome: &str, data: Value) {
        self.close(outcome, data);
    }

    fn close(&mut self, outcome: &str, data: Value) {
        if let Some(start) = self.start.take() {
            let _ = record(
                &format!("{}_finished", self.kind),
                &self.thread,
                &self.turn,
                &self.operation,
                json!({"start_event_id":start.id, "start_ns":start.at_ns, "outcome":outcome, "facts":data}),
            );
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.close("abandoned", Value::Null);
    }
}

/// Transfer this token with the exact output to its frontend. Call `scheduled`
/// at actual UI dequeue and `painted` only after that output revision is painted.
/// All endpoints use this process clock; cross-process timestamps are not mixed.
#[derive(Debug)]
pub struct ClientRenderReceipt {
    receipt: Receipt,
    thread: String,
    turn: String,
    output: String,
    revision: String,
    scheduled: Option<Receipt>,
}

impl ClientRenderReceipt {
    pub fn received(thread: &str, turn: &str, output: &str, revision: &str) -> Option<Self> {
        if !enabled() || output.is_empty() || revision.is_empty() {
            return None;
        }
        let receipt = record(
            "client_output_received",
            thread,
            turn,
            output,
            json!({"revision":revision}),
        )?;
        Some(Self {
            receipt,
            thread: thread.into(),
            turn: turn.into(),
            output: output.into(),
            revision: revision.into(),
            scheduled: None,
        })
    }

    pub fn scheduled(&mut self, ui: &str, active_sessions: u32) {
        if self.scheduled.is_none() {
            self.scheduled = record(
                "client_output_scheduled",
                &self.thread,
                &self.turn,
                &self.output,
                json!({"receipt_id":self.receipt.id, "revision":self.revision,
                    "ui_id":ui, "active_sessions":active_sessions}),
            );
        }
    }

    pub fn dequeued(&self) {
        let _ = record(
            "client_output_dequeued",
            &self.thread,
            &self.turn,
            &self.output,
            json!({"receipt_id":self.receipt.id, "revision":self.revision}),
        );
    }

    pub fn painted(self) {
        let displayed_thread = self.thread.clone();
        self.painted_in(&displayed_thread);
    }

    /// Prefer this when the frontend has an independently observed target view.
    pub fn painted_in(self, displayed_thread_id: &str) {
        let _ = record(
            "client_output_painted",
            &self.thread,
            &self.turn,
            &self.output,
            json!({"receipt_id":self.receipt.id, "revision":self.revision,
                "scheduled_id":self.scheduled.as_ref().map(|receipt| &receipt.id),
                "displayed_thread_id":displayed_thread_id, "source":"paint_ack"}),
        );
    }
}
