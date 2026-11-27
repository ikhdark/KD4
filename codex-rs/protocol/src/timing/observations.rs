//! Opt-in, content-free owner observations that can outlive a core turn.
//! Unlike the numeric client probe, this lane preserves exact string identities
//! and causal references. A receipt is not a provider or paint observation.
//! Runtime-async receipts acknowledge admission only. Confirm evidence by
//! finding the exact event in the journal; don't count a missing event as an
//! observed outcome. Scope binding reports whether the writer was available.
//! A gap reports a lost tail when it can be written; I/O failure or process
//! exit can leave an unconfirmed tail without a gap.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;

use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ThreadId;
use crate::protocol::TurnAbortReason;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ROW_BYTES: usize = 1024 * 1024;
const MAX_SEGMENTS: u32 = 128;
const LANES: [&str; 4] = ["runtime", "mobile", "lifecycle", "request"];

struct Journal {
    lanes: [Option<Lane>; 4],
    directory: PathBuf,
    rollover: bool,
}

// Both writers share the receipt namespace, not a lock or a disk queue.
// File order is tracked independently by each lane's lane_seq.
fn next_sequence() -> Option<u64> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    SEQUENCE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |seq| seq.checked_add(1))
        .ok()
        .map(|previous| previous + 1)
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
            let lanes = [
                Some(Lane::open(&directory, "")?),
                Lane::open(&directory, "-mobile"),
                Lane::open(&directory, "-lifecycle"),
                Lane::open(&directory, "-request"),
            ];
            Some(Mutex::new(Journal {
                lanes,
                directory,
                rollover: rollover_enabled(),
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
    let sequence = next_sequence()?;
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
    let receipt = Receipt { id, at_ns };
    lane.write(
        row,
        &receipt,
        directory,
        ["", "-mobile", "-lifecycle", "-request"][lane_index],
        limit,
        rollover,
    )?;
    Some(receipt)
}

fn rollover_enabled() -> bool {
    std::env::var_os("CODEX_RUNTIME_OBSERVATIONS_ROLLOVER").is_some_and(|value| value == "1")
}

impl Lane {
    fn open(directory: &Path, suffix: &str) -> Option<Self> {
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
        Some(Self {
            file,
            sequence: 0,
            bytes: 0,
            stopped: false,
            segment: 0,
            last_id: None,
            previous_segment_last_id: None,
        })
    }

    fn write(
        &mut self,
        mut row: Value,
        receipt: &Receipt,
        directory: &Path,
        suffix: &str,
        limit: usize,
        rollover: bool,
    ) -> Option<()> {
        if rollover {
            row["journal_segment"] = json!(self.segment);
            row["previous_segment_last_id"] = json!(self.previous_segment_last_id);
        }
        // The terminal handoff marker is final and can use the overflow reserve.
        let reserve = if suffix == TERMINAL_SUFFIX && row["kind"] == "capture_overflow" {
            1 // newline
        } else {
            1024
        };
        let mut bytes = serde_json::to_vec(&row).ok()?;
        let mut rollover_failed = false;
        if rollover && bytes.len() <= MAX_ROW_BYTES && self.bytes + bytes.len() + reserve > limit
            && self.segment + 1 < MAX_SEGMENTS
        {
            let path = directory.join(format!(
                "runtime-{}-{}{suffix}-part{:06}.jsonl",
                std::process::id(), super::process_clock_domain_id(), self.segment + 1
            ));
            if let Ok(file) = OpenOptions::new().write(true).create_new(true).open(path) {
                self.file = file;
                self.segment += 1;
                self.bytes = 0;
                self.sequence = 0;
                self.previous_segment_last_id = self.last_id.clone();
                row["lane_seq"] = json!(1);
                row["journal_segment"] = json!(self.segment);
                row["previous_segment_last_id"] = json!(self.previous_segment_last_id);
                bytes = serde_json::to_vec(&row).ok()?;
            } else {
                rollover_failed = true;
            }
        }
        if bytes.len() > MAX_ROW_BYTES || self.bytes + bytes.len() + reserve > limit {
            let reason = if bytes.len() > MAX_ROW_BYTES { "row_too_large" }
                else if rollover_failed { "rollover_create_failed" }
                else if rollover { "segment_limit" }
                else { "byte_limit" };
            if suffix == RUNTIME_ASYNC_SUFFIX {
                // An admission ID never identifies a replacement gap record.
                let _ = self.write_runtime_async_gap(receipt.at_ns, reason, rollover);
                return None;
            }
            row["kind"] = json!("capture_overflow");
            row["data"] = json!({"row_bytes":bytes.len(), "retained_bytes":self.bytes,
                "reason":reason, "first_dropped_at_ns":receipt.at_ns, "last_retained_event_id":self.last_id});
            bytes = serde_json::to_vec(&row).ok()?;
            self.stopped = true;
        }
        self.append(bytes, receipt)?;
        (!self.stopped).then_some(())
    }

    fn append(&mut self, mut bytes: Vec<u8>, receipt: &Receipt) -> Option<()> {
        bytes.push(b'\n');
        if self.file.write_all(&bytes).is_err() {
            self.stopped = true;
            return None;
        }
        self.sequence += 1;
        self.last_id = Some(receipt.id.clone());
        self.bytes += bytes.len();
        Some(())
    }

    fn write_runtime_async_gap(
        &mut self,
        first_dropped_at_ns: u64,
        reason: &str,
        rollover: bool,
    ) -> Option<()> {
        self.stopped = true;
        let sequence = next_sequence()?;
        let receipt = runtime_async_receipt(sequence, runtime_async_now());
        let mut row = runtime_async_row(
            sequence,
            &receipt,
            self.sequence + 1,
            "capture_overflow",
            ["", "", ""],
            json!({
                "reason":reason,
                "first_dropped_at_ns":first_dropped_at_ns,
                "last_retained_event_id":self.last_id,
            }),
        );
        if rollover {
            row["journal_segment"] = json!(self.segment);
            row["previous_segment_last_id"] = json!(self.previous_segment_last_id);
        }
        let bytes = serde_json::to_vec(&row).ok()?;
        // Mark the end of the retained segment in the existing reserve.
        // Opening another segment could itself be the cause of this gap.
        if self.bytes + bytes.len() + 1 > MAX_BYTES {
            return None;
        }
        self.append(bytes, &receipt)
    }
}

const RUNTIME_ASYNC_LANE: &str = "runtime-async";
const RUNTIME_ASYNC_SUFFIX: &str = "-async";
const RUNTIME_ASYNC_QUEUE_CAPACITY: usize = 256;
const MAX_RUNTIME_ASYNC_ROW_BYTES: usize = 64 * 1024;
const MAX_RUNTIME_ASYNC_DEPTH: usize = 32;
const NO_RUNTIME_ASYNC_LOSS: u64 = u64::MAX;
static RUNTIME_ASYNC: OnceLock<Option<RuntimeAsyncSink>> = OnceLock::new();

struct RuntimeAsyncSink {
    sender: mpsc::SyncSender<RuntimeAsyncRow>,
    state: Arc<RuntimeAsyncState>,
    worker: thread::Thread,
}

struct RuntimeAsyncState {
    // Serializes admission order with receipt allocation, not with disk work.
    admission: Mutex<()>,
    first_loss_ns: AtomicU64,
    writer_stopped: AtomicBool,
}

struct RuntimeAsyncRow {
    sequence: u64,
    receipt: Receipt,
    kind: String,
    scope: [String; 3],
    data: Value,
}

/// Prepare at startup or scope binding, outside a hot observer. Returns false
/// when disabled, setup failed, or admission already stopped. A true result
/// means the writer has been launched; it does not prove a file open or append.
pub fn prepare_runtime_async_lane() -> bool {
    RUNTIME_ASYNC
        .get_or_init(|| {
            if std::env::var("CODEX_RUNTIME_OBSERVATIONS").as_deref() != Ok("1") {
                return None;
            }
            let directory = PathBuf::from(std::env::var_os("CODEX_RUNTIME_OBSERVATIONS_DIR")?);
            if !directory.is_absolute() {
                return None;
            }
            let _ = super::process_clock_domain_id();
            let rollover = rollover_enabled();
            let (sender, receiver) = mpsc::sync_channel(RUNTIME_ASYNC_QUEUE_CAPACITY);
            let state = Arc::new(RuntimeAsyncState {
                admission: Mutex::new(()),
                first_loss_ns: AtomicU64::new(NO_RUNTIME_ASYNC_LOSS),
                writer_stopped: AtomicBool::new(false),
            });
            let worker_state = Arc::clone(&state);
            let worker = thread::Builder::new()
                .name("runtime-async-observations".into())
                .spawn(move || {
                    if let Some(lane) = Lane::open(&directory, RUNTIME_ASYNC_SUFFIX) {
                        write_runtime_async_lane(
                            lane,
                            &directory,
                            rollover,
                            &receiver,
                            &worker_state,
                        );
                    }
                    worker_state.writer_stopped.store(true, Ordering::Release);
                })
                .ok()?;
            Some(RuntimeAsyncSink {
                sender,
                state,
                worker: worker.thread().clone(),
            })
        })
        .as_ref()
        .is_some_and(RuntimeAsyncSink::available)
}

fn runtime_async_sink() -> Option<&'static RuntimeAsyncSink> {
    RUNTIME_ASYNC.get()?.as_ref().filter(|sink| sink.available())
}

/// Check the prepared lane without initializing it or waiting for the writer.
pub fn runtime_async_enabled() -> bool {
    runtime_async_sink().is_some()
}

/// Hand off content-free owner observations without waiting for capacity, a
/// producer lock, or the writer. This receipt is not confirmation of append.
/// The first admission loss closes the FIFO prefix after any admission
/// already in flight; there is no synchronous fallback or retry.
pub fn enqueue_runtime(
    kind: &str,
    thread: &str,
    turn: &str,
    operation: &str,
    data: Value,
) -> Option<Receipt> {
    let sink = runtime_async_sink()?;
    let mut budget = MAX_RUNTIME_ASYNC_ROW_BYTES - 512; // fixed row envelope
    if kind == "capture_overflow"
        || [kind, thread, turn, operation]
            .into_iter()
            .try_for_each(|value| charge_runtime_async_string(&mut budget, value))
            .is_none()
        || charge_runtime_async_value(&mut budget, &data, 0).is_none()
    {
        sink.note_loss(runtime_async_now());
        return None;
    }
    let kind = kind.to_owned();
    let scope = [thread.to_owned(), turn.to_owned(), operation.to_owned()];
    let Ok(admission) = sink.state.admission.try_lock() else {
        sink.note_loss(runtime_async_now());
        return None;
    };
    let at_ns = runtime_async_now();
    if !sink.available() {
        return None;
    }
    // The availability check orders this in-flight admission before any
    // racing loss. The writer settles it before deciding the prefix ended.
    let Some(sequence) = next_sequence() else {
        sink.note_loss(at_ns);
        return None;
    };
    let receipt = runtime_async_receipt(sequence, at_ns);
    if let Err(_rejected) = sink.sender.try_send(RuntimeAsyncRow {
        sequence,
        receipt: receipt.clone(),
        kind,
        scope,
        data,
    }) {
        sink.note_loss(at_ns);
        return None;
    }
    drop(admission);
    sink.worker.unpark();
    Some(receipt)
}

impl RuntimeAsyncSink {
    fn available(&self) -> bool {
        self.state.first_loss_ns.load(Ordering::Acquire) == NO_RUNTIME_ASYNC_LOSS
            && !self.state.writer_stopped.load(Ordering::Acquire)
    }

    fn note_loss(&self, at_ns: u64) {
        if self
            .state
            .first_loss_ns
            .compare_exchange(
                NO_RUNTIME_ASYNC_LOSS,
                at_ns,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.worker.unpark();
        }
    }
}

fn charge_runtime_async_bytes(budget: &mut usize, bytes: usize) -> Option<()> {
    *budget = budget.checked_sub(bytes)?;
    Some(())
}

fn charge_runtime_async_string(budget: &mut usize, value: &str) -> Option<()> {
    // Worst-case JSON escaping, without walking or serializing the string.
    charge_runtime_async_bytes(budget, value.len().checked_mul(6)?)
}

fn charge_runtime_async_value(budget: &mut usize, value: &Value, depth: usize) -> Option<()> {
    if depth >= MAX_RUNTIME_ASYNC_DEPTH {
        return None;
    }
    // Covers quotes, delimiters, and scalar literals, and also bounds the
    // number of tiny values that can be walked or retained in one queue row.
    charge_runtime_async_bytes(budget, 16)?;
    match value {
        Value::String(value) => charge_runtime_async_string(budget, value)?,
        // The workspace enables arbitrary_precision, so account for the
        // stored numeric string, not just the usual machine number widths.
        Value::Number(value) => charge_runtime_async_bytes(budget, value.as_str().len())?,
        Value::Array(values) => {
            for value in values {
                charge_runtime_async_value(budget, value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                charge_runtime_async_string(budget, key)?;
                charge_runtime_async_value(budget, value, depth + 1)?;
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
    Some(())
}

fn runtime_async_now() -> u64 {
    u64::try_from(super::process_monotonic_ns())
        .unwrap_or(u64::MAX)
        .min(NO_RUNTIME_ASYNC_LOSS - 1)
}

fn runtime_async_receipt(sequence: u64, at_ns: u64) -> Receipt {
    Receipt {
        id: format!("{}:{sequence}", super::process_clock_domain_id()),
        at_ns,
    }
}

fn write_runtime_async_lane(
    mut lane: Lane,
    directory: &Path,
    rollover: bool,
    receiver: &mpsc::Receiver<RuntimeAsyncRow>,
    state: &RuntimeAsyncState,
) {
    let mut first_loss = None;
    loop {
        if first_loss.is_none() {
            let loss = state.first_loss_ns.load(Ordering::Acquire);
            if loss != NO_RUNTIME_ASYNC_LOSS {
                // Wait only on the writer, once, for the producer already
                // inside admission. No later producer can extend the prefix.
                let _admission = state
                    .admission
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                first_loss = Some(loss);
            }
        }
        match receiver.try_recv() {
            Ok(pending) => {
                let row = runtime_async_row(
                    pending.sequence,
                    &pending.receipt,
                    lane.sequence + 1,
                    &pending.kind,
                    pending.scope.each_ref().map(String::as_str),
                    pending.data,
                );
                if lane
                    .write(
                        row,
                        &pending.receipt,
                        directory,
                        RUNTIME_ASYNC_SUFFIX,
                        MAX_BYTES,
                        rollover,
                    )
                    .is_none()
                {
                    if !lane.stopped {
                        let _ = lane.write_runtime_async_gap(
                            pending.receipt.at_ns,
                            "row_serialization_failed",
                            rollover,
                        );
                    }
                    break;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                if let Some(first_dropped_at_ns) = first_loss {
                    let _ = lane.write_runtime_async_gap(
                        first_dropped_at_ns,
                        "runtime_async_handoff_loss",
                        rollover,
                    );
                    break;
                }
                thread::park();
            }
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
}

fn runtime_async_row(
    sequence: u64,
    receipt: &Receipt,
    lane_sequence: u64,
    kind: &str,
    [thread, turn, operation]: [&str; 3],
    data: Value,
) -> Value {
    let mut row = json!({"version":1, "pid":std::process::id(),
        "clock_id":super::process_clock_domain_id(), "seq":sequence, "id":receipt.id,
        "journal_lane":RUNTIME_ASYNC_LANE, "lane_seq":lane_sequence,
        "at_ns":receipt.at_ns, "kind":kind, "thread_id":thread, "turn_id":turn,
        "operation_id":operation});
    row["data"] = data;
    row
}

const TERMINAL_LANE: &str = "terminal";
const TERMINAL_SUFFIX: &str = "-terminal";
const TERMINAL_QUEUE_CAPACITY: usize = 256;
const MAX_TERMINAL_ID_BYTES: usize = 4096;
const NO_TERMINAL_LOSS: u64 = u64::MAX;
static TERMINAL: OnceLock<Option<TerminalSink>> = OnceLock::new();

struct TerminalSink {
    sender: mpsc::SyncSender<TerminalRow>,
    state: Arc<TerminalSinkState>,
    worker: thread::Thread,
}

struct TerminalSinkState {
    // Only producers own this on the live path; the writer uses it once to
    // settle the admitted prefix after loss. A producer never waits for it.
    admission: Mutex<()>,
    first_loss_ns: AtomicU64,
    writer_stopped: AtomicBool,
}

struct TerminalRow {
    sequence: u64,
    receipt: Receipt,
    thread: String,
    turn: String,
    operation: String,
    observation: TerminalObservation,
}

/// Start the detached writer at startup or scope binding, never at a durability
/// boundary. True means a handoff is expected, not that a row persisted.
pub fn prepare_terminal_lane() -> bool {
    TERMINAL
        .get_or_init(|| {
            if std::env::var("CODEX_RUNTIME_OBSERVATIONS").as_deref() != Ok("1") {
                return None;
            }
            let directory = PathBuf::from(std::env::var_os("CODEX_RUNTIME_OBSERVATIONS_DIR")?);
            if !directory.is_absolute() {
                return None;
            }
            // Even the first producer can read the initialized process clock.
            let _ = super::process_clock_domain_id();
            let rollover = rollover_enabled();
            let (sender, receiver) = mpsc::sync_channel(TERMINAL_QUEUE_CAPACITY);
            let state = Arc::new(TerminalSinkState {
                admission: Mutex::new(()),
                first_loss_ns: AtomicU64::new(NO_TERMINAL_LOSS),
                writer_stopped: AtomicBool::new(false),
            });
            let worker_state = Arc::clone(&state);
            let worker = thread::Builder::new()
                .name("terminal-observations".into())
                .spawn(move || {
                    let _ = write_terminal_lane(directory, rollover, receiver, &worker_state);
                    worker_state.writer_stopped.store(true, Ordering::Release);
                })
                .ok()?;
            Some(TerminalSink {
                sender,
                state,
                worker: worker.thread().clone(),
            })
        })
        .as_ref()
        .is_some_and(TerminalSink::available)
}

fn terminal_sink() -> Option<&'static TerminalSink> {
    TERMINAL.get()?.as_ref().filter(|sink| sink.available())
}

pub fn terminal_enabled() -> bool {
    terminal_sink().is_some()
}

impl TerminalSink {
    fn available(&self) -> bool {
        self.state.first_loss_ns.load(Ordering::Acquire) == NO_TERMINAL_LOSS
            && !self.state.writer_stopped.load(Ordering::Acquire)
    }

    fn note_loss(&self, at_ns: u64) {
        self.state.first_loss_ns.fetch_min(at_ns, Ordering::AcqRel);
        self.worker.unpark();
    }

    fn note_loss_now(&self) {
        if let Ok(at_ns) = u64::try_from(super::process_monotonic_ns()) {
            self.note_loss(at_ns);
        }
    }

    fn enqueue(
        &self,
        thread: &str,
        turn: &str,
        operation: &str,
        observation: TerminalObservation,
    ) -> Option<Receipt> {
        let Ok(_admission) = self.state.admission.try_lock() else {
            self.note_loss_now();
            return None;
        };
        if !self.available() {
            return None;
        }
        let at_ns = u64::try_from(super::process_monotonic_ns()).ok()?;
        if [thread, turn, operation]
            .into_iter()
            .any(|id| id.len() > MAX_TERMINAL_ID_BYTES)
        {
            self.note_loss(at_ns);
            return None;
        }
        let sequence = next_sequence()?;
        let receipt = Receipt {
            id: format!("{}:{sequence}", super::process_clock_domain_id()),
            at_ns,
        };
        if self
            .sender
            .try_send(TerminalRow {
                sequence,
                receipt: receipt.clone(),
                thread: thread.into(),
                turn: turn.into(),
                operation: operation.into(),
                observation,
            })
            .is_err()
        {
            self.note_loss(at_ns);
            return None;
        }
        self.worker.unpark();
        Some(receipt)
    }
}

/// A terminal outcome could not acquire its exact scope without waiting.
pub fn note_terminal_scope_loss() {
    if let Some(sink) = terminal_sink() {
        sink.note_loss_now();
    }
}

fn write_terminal_lane(
    directory: PathBuf,
    rollover: bool,
    mut receiver: mpsc::Receiver<TerminalRow>,
    state: &TerminalSinkState,
) -> Option<()> {
    let mut lane = Lane::open(&directory, TERMINAL_SUFFIX)?;
    loop {
        // On loss, write only the already admitted FIFO prefix, then one gap.
        // Checking the queue under admission once on this path prevents a
        // producer already inside enqueue from being overtaken by the gap.
        let first_loss_ns = state.first_loss_ns.load(Ordering::Acquire);
        let pending = if first_loss_ns == NO_TERMINAL_LOSS {
            receiver.try_recv()
        } else {
            let _admission = state.admission.lock().ok()?;
            receiver.try_recv()
        };
        match pending {
            Ok(pending) => {
                let (kind, data) = pending.observation.into_parts();
                let row = terminal_row(
                    pending.sequence,
                    &pending.receipt,
                    lane.sequence + 1,
                    kind,
                    (&pending.thread, &pending.turn, &pending.operation),
                    data,
                );
                // If a start cannot be retained, the writer cannot advance to
                // its finish. No retry or later row can bridge a file failure.
                lane.write(
                    row,
                    &pending.receipt,
                    &directory,
                    TERMINAL_SUFFIX,
                    MAX_BYTES,
                    rollover,
                )?;
            }
            Err(mpsc::TryRecvError::Empty) if first_loss_ns != NO_TERMINAL_LOSS => {
                let sequence = next_sequence()?;
                let receipt = Receipt {
                    id: format!("{}:{sequence}", super::process_clock_domain_id()),
                    at_ns: u64::try_from(super::process_monotonic_ns()).ok()?,
                };
                let row = terminal_row(
                    sequence,
                    &receipt,
                    lane.sequence + 1,
                    "capture_overflow",
                    ("", "", ""),
                    json!({
                        "reason":"terminal_handoff_loss",
                        "first_dropped_at_ns":state.first_loss_ns.load(Ordering::Acquire),
                        "last_retained_event_id":lane.last_id,
                    }),
                );
                return lane.write(row, &receipt, &directory, TERMINAL_SUFFIX, MAX_BYTES, rollover);
            }
            Err(mpsc::TryRecvError::Empty) => thread::park(),
            Err(mpsc::TryRecvError::Disconnected) => return None,
        }
    }
}

fn terminal_row(
    sequence: u64,
    receipt: &Receipt,
    lane_sequence: u64,
    kind: &str,
    scope: (&str, &str, &str),
    data: Value,
) -> Value {
    let (thread, turn, operation) = scope;
    json!({"version":1, "pid":std::process::id(),
        "clock_id":super::process_clock_domain_id(), "seq":sequence, "id":receipt.id,
        "journal_lane":TERMINAL_LANE, "lane_seq":lane_sequence,
        "at_ns":receipt.at_ns, "kind":kind, "thread_id":thread, "turn_id":turn,
        "operation_id":operation, "data":data})
}

pub enum CoreTerminalOutcome {
    Success,
    Failed,
    Cancelled(TurnAbortReason),
}

pub fn record_core_terminal(scope: &str, turn: &str, outcome: CoreTerminalOutcome, persist: bool) {
    if let Some(sink) = terminal_sink() {
        let _ = sink.enqueue("", scope, turn, TerminalObservation::Core { outcome, persist });
    }
}

/// Terminal state of an exact assignment attempt observed by its selected-child wait.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentJoinState {
    Completed,
    NeedsMain,
    Violated,
    Abandoned,
}

/// Content-free facts for the selected-child join observation.
#[derive(Serialize)]
pub struct AgentJoinObservation {
    pub child_thread_id: ThreadId,
    pub assignment_id: Uuid,
    pub attempt_id: Uuid,
    pub state: AgentJoinState,
    pub receipt_available: bool,
}

/// Hand off a selected-child join to the prepared lane; never wait for the writer.
pub fn record_agent_joined(scope: &str, operation: &str, join: AgentJoinObservation) {
    if let Some(sink) = terminal_sink() {
        let _ = sink.enqueue("", scope, operation, TerminalObservation::AgentJoined(join));
    }
}

#[derive(Clone, Copy)]
pub enum TerminalPhase {
    RolloutAppend,
    RolloutBarrier,
}

impl TerminalPhase {
    fn kinds_and_ack(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::RolloutAppend => (
                "terminal_rollout_append_started",
                "terminal_rollout_append_finished",
                "append_acknowledged",
            ),
            Self::RolloutBarrier => (
                "terminal_rollout_barrier_started",
                "terminal_rollout_barrier_finished",
                "barrier_acknowledged",
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum TerminalSpanKind {
    Core(TerminalPhase),
    DurableRollout,
}

impl TerminalSpanKind {
    fn operation(self, turn: &str) -> &str {
        match self {
            Self::Core(_) => turn,
            Self::DurableRollout => "writer_barrier",
        }
    }

    fn started(self) -> (&'static str, Value) {
        match self {
            Self::Core(phase) => (phase.kinds_and_ack().0, Value::Null),
            Self::DurableRollout => (
                "durable_rollout_started",
                json!({"scope":"ordered_rollout_and_payload_sync_not_a_state_db_or_client_ack"}),
            ),
        }
    }

    fn finished_kind(self) -> &'static str {
        match self {
            Self::Core(phase) => phase.kinds_and_ack().1,
            Self::DurableRollout => "durable_rollout_finished",
        }
    }
}

enum TerminalEnd {
    OwnerOutcome(&'static str),
    RolloutSynced {
        materialized: bool,
        pending_items: usize,
        namespace_sync_pending: bool,
    },
}

impl TerminalEnd {
    fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::OwnerOutcome(outcome) => (outcome, Value::Null),
            Self::RolloutSynced {
                materialized,
                pending_items,
                namespace_sync_pending,
            } => {
                let reason = if materialized {
                    "exact_index_revision_unavailable_at_sync"
                } else {
                    "index_not_materialized"
                };
                (
                    "synced",
                    json!({
                        "checkpoint":{"available":false, "reason":reason},
                        "materialized":materialized,
                        "pending_items":pending_items,
                        "namespace_sync_pending":namespace_sync_pending,
                    }),
                )
            }
        }
    }
}

enum TerminalObservation {
    Core {
        outcome: CoreTerminalOutcome,
        persist: bool,
    },
    AgentJoined(AgentJoinObservation),
    Start(TerminalSpanKind),
    Finish {
        kind: TerminalSpanKind,
        start: Receipt,
        end: TerminalEnd,
    },
}

impl TerminalObservation {
    fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::Core { outcome, persist } => (
                "core_terminal",
                match outcome {
                    CoreTerminalOutcome::Success => json!({"outcome":"success", "persist":persist}),
                    CoreTerminalOutcome::Failed => json!({"outcome":"failed", "persist":persist}),
                    CoreTerminalOutcome::Cancelled(reason) => {
                        json!({"outcome":"cancelled", "reason":reason, "persist":persist})
                    }
                },
            ),
            Self::AgentJoined(join) => ("agent_joined", json!(join)),
            Self::Start(kind) => kind.started(),
            Self::Finish { kind, start, end } => {
                let (outcome, facts) = end.into_parts();
                (
                    kind.finished_kind(),
                    json!({"start_event_id":start.id, "start_ns":start.at_ns, "outcome":outcome, "facts":facts}),
                )
            }
        }
    }
}

/// Terminal-only span. Queue admission is not a receipt of journal persistence.
pub struct TerminalSpan {
    span: QueuedTerminalSpan,
    phase: TerminalPhase,
}

pub struct DurableRolloutSpan(QueuedTerminalSpan);

struct QueuedTerminalSpan {
    start: Option<Receipt>,
    kind: TerminalSpanKind,
    thread: String,
    turn: String,
}

impl TerminalSpan {
    pub fn start(phase: TerminalPhase, thread: &str, turn: &str) -> Option<Self> {
        Some(Self {
            span: QueuedTerminalSpan::start(TerminalSpanKind::Core(phase), thread, turn)?,
            phase,
        })
    }

    pub fn finish(mut self, acknowledged: bool) {
        let outcome = if acknowledged {
            self.phase.kinds_and_ack().2
        } else {
            "error"
        };
        self.span.close(TerminalEnd::OwnerOutcome(outcome));
    }

    /// Observe the actual recorder barrier. This does not prepare or wait for the lane.
    pub fn start_durable_rollout(thread: &str) -> Option<DurableRolloutSpan> {
        QueuedTerminalSpan::start(TerminalSpanKind::DurableRollout, thread, "")
            .map(DurableRolloutSpan)
    }
}

impl DurableRolloutSpan {
    pub fn finish(
        mut self,
        materialized: bool,
        pending_items: usize,
        namespace_sync_pending: bool,
    ) {
        self.0.close(TerminalEnd::RolloutSynced {
            materialized,
            pending_items,
            namespace_sync_pending,
        });
    }
}

impl QueuedTerminalSpan {
    fn start(kind: TerminalSpanKind, thread: &str, turn: &str) -> Option<Self> {
        let start = terminal_sink()?.enqueue(
            thread,
            turn,
            kind.operation(turn),
            TerminalObservation::Start(kind),
        )?;
        Some(Self {
            start: Some(start),
            kind,
            thread: thread.into(),
            turn: turn.into(),
        })
    }

    fn close(&mut self, end: TerminalEnd) {
        if let Some(start) = self.start.take()
            && let Some(sink) = terminal_sink()
        {
            let _ = sink.enqueue(
                &self.thread,
                &self.turn,
                self.kind.operation(&self.turn),
                TerminalObservation::Finish {
                    kind: self.kind,
                    start,
                    end,
                },
            );
        }
    }
}

impl Drop for QueuedTerminalSpan {
    fn drop(&mut self) {
        self.close(TerminalEnd::OwnerOutcome("abandoned"));
    }
}

/// A dropped scope closes as abandoned: cancellation, an early error and
/// unwinding cannot be distinguished by Drop alone.
/// This guard owns no task, permission, lock, cancellation token or retry.
pub struct Span {
    start: Option<Receipt>,
    record_event: RecordSpanEvent,
    kind: String,
    thread: String,
    turn: String,
    operation: String,
}

type RecordSpanEvent = fn(&str, &str, &str, &str, Value) -> Option<Receipt>;

impl Span {
    pub fn start(kind: &str, thread: &str, turn: &str, operation: &str, data: Value) -> Self {
        Self::start_with(record, kind, thread, turn, operation, data)
    }

    /// Start, finish, and abandon on the prepared runtime-async lane. A finish
    /// relates to an admitted start, not necessarily to a retained start.
    pub fn start_queued(kind: &str, thread: &str, turn: &str, operation: &str, data: Value) -> Self {
        Self::start_with(enqueue_runtime, kind, thread, turn, operation, data)
    }

    fn start_with(
        record_event: RecordSpanEvent,
        kind: &str,
        thread: &str,
        turn: &str,
        operation: &str,
        data: Value,
    ) -> Self {
        let start = record_event(&format!("{kind}_started"), thread, turn, operation, data);
        if start.is_none() {
            return Self {
                start: None,
                record_event,
                kind: String::new(),
                thread: String::new(),
                turn: String::new(),
                operation: String::new(),
            };
        }
        Self {
            start,
            record_event,
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
            let facts = Value::Object(serde_json::Map::from_iter([
                ("start_event_id".into(), start.id.into()),
                ("start_ns".into(), start.at_ns.into()),
                ("outcome".into(), outcome.into()),
                ("facts".into(), data),
            ]));
            let _ = (self.record_event)(
                &format!("{}_finished", self.kind),
                &self.thread,
                &self.turn,
                &self.operation,
                facts,
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
