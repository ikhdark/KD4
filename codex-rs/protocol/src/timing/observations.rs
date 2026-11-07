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

struct Journal {
    file: File,
    sequence: u64,
    bytes: usize,
    stopped: bool,
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
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(format!(
                    "runtime-{}-{}.jsonl",
                    std::process::id(),
                    super::process_clock_domain_id()
                )))
                .ok()?;
            Some(Mutex::new(Journal {
                file,
                sequence: 0,
                bytes: 0,
                stopped: false,
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
    if journal.stopped {
        return None;
    }
    let sequence = journal.sequence.checked_add(1)?;
    let at_ns = u64::try_from(super::process_monotonic_ns()).ok()?;
    let id = format!("{}:{sequence}", super::process_clock_domain_id());
    let mut row = json!({"version":1, "pid":std::process::id(),
        "clock_id":super::process_clock_domain_id(), "seq":sequence, "id":id,
        "at_ns":at_ns, "kind":kind, "thread_id":thread, "turn_id":turn,
        "operation_id":operation, "data":data});
    let mut bytes = serde_json::to_vec(&row).ok()?;
    if bytes.len() > MAX_ROW_BYTES || journal.bytes + bytes.len() + 1024 > MAX_BYTES {
        row["kind"] = json!("capture_overflow");
        row["data"] = json!({"row_bytes":bytes.len(), "retained_bytes":journal.bytes});
        bytes = serde_json::to_vec(&row).ok()?;
        journal.stopped = true;
    }
    bytes.push(b'\n');
    if journal.file.write_all(&bytes).is_err() {
        journal.stopped = true;
        return None;
    }
    journal.sequence = sequence;
    journal.bytes += bytes.len();
    if journal.stopped {
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
