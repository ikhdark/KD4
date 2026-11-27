//! Bounded ingress for the native overflow journal. Event reservation and
//! enqueue happen under the turn-state lock; only the blocking writer owns the
//! artifact sink. A stalled sink never holds a lock needed by a producer.
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use codex_tool_artifacts::RawOutputArtifact;
use codex_tool_artifacts::capture::RawOutputArtifactTask;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::runtime::Handle;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use super::{PROCESS_WAIT_ATTRIBUTION_SEALED, TurnTimingState, TurnTimingStateInner};

const MAX_PENDING_RECORDS: usize = 1024;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const TERMINAL_OBSERVATION: Duration = Duration::from_millis(100);
// An overflowing turn keeps an idle blocking worker until the journal seals.
static WRITER_SLOTS: Semaphore = Semaphore::const_new(16);

#[derive(Debug)]
pub(super) struct Archive {
    home: PathBuf,
    thread_id: String,
    turn_id: String,
    runtime: Option<Handle>,
    ingress: Mutex<Ingress>,
}

#[derive(Debug, Default)]
enum Ingress {
    #[default]
    Dormant,
    Unavailable(&'static str),
    Active {
        sender: Option<CloseIngress>,
        byte_budget: Arc<Semaphore>,
        sealed_expected: Arc<AtomicU64>,
        accepted: u64,
        finished: watch::Receiver<Option<Value>>,
    },
}

// Either turn custody ending or runtime shutdown closes the queue. Neither
// can leave an idle blocking worker holding the runtime open.
#[derive(Debug)]
struct CloseIngress(async_channel::Sender<Record>);

impl Drop for CloseIngress {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Debug)]
struct Record {
    line: Vec<u8>,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Serialize)]
struct JournalLine<'a> {
    schema_version: u8,
    sequence: u64,
    thread_id: &'a str,
    turn_id: &'a str,
    event: Value,
}

struct BoundedLine {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedLine {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("overflow journal record exceeds ingress byte budget"));
        }
        let len = self.bytes.len() + bytes.len();
        if len > self.bytes.capacity() {
            let capacity = len
                .max(self.bytes.capacity().saturating_mul(2))
                .max(64)
                .min(self.limit);
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(io::Error::other)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Archive {
    fn start(&self) -> Ingress {
        let Some(runtime) = self.runtime.clone().or_else(|| Handle::try_current().ok()) else {
            return Ingress::Unavailable("archive writer runtime unavailable");
        };
        let Ok(writer_slot) = WRITER_SLOTS.try_acquire() else {
            return Ingress::Unavailable("archive writer capacity exhausted");
        };
        let (sender, receiver) = async_channel::bounded(MAX_PENDING_RECORDS);
        let abort_ingress = CloseIngress(sender.clone());
        let (done, finished) = watch::channel(None);
        let sealed_expected = Arc::new(AtomicU64::new(0));
        let expected = Arc::clone(&sealed_expected);
        let home = self.home.clone();
        let thread_id = self.thread_id.clone();
        let turn_id = self.turn_id.clone();
        // The supervisor joins the actual sink owner, even when the terminal
        // observer is cancelled or times out. If runtime shutdown cancels
        // the supervisor, its guard closes ingress and wakes an idle writer.
        runtime.spawn(async move {
            let _abort_ingress = abort_ingress;
            let result = tokio::task::spawn_blocking(move || {
                let _writer_slot = writer_slot;
                run_writer(&home, &thread_id, &turn_id, receiver, &expected)
            })
            .await;
            let receipt = match result {
                Ok(receipt) => receipt,
                Err(error) => {
                    tracing::warn!(%error, "overflow journal writer failed");
                    return;
                }
            };
            done.send_replace(Some(receipt));
        });
        Ingress::Active {
            sender: Some(CloseIngress(sender)),
            byte_budget: Arc::new(Semaphore::new(MAX_PENDING_BYTES)),
            sealed_expected,
            accepted: 0,
            finished,
        }
    }

    fn enqueue(&self, sequence: u64, event: Value) {
        let mut ingress = self.ingress.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*ingress, Ingress::Dormant) {
            *ingress = self.start();
        }
        let Ingress::Active { sender: Some(sender), byte_budget, accepted, .. } = &mut *ingress else {
            return;
        };
        // All producers serialize under this guard and the writer only
        // releases budget. Do not encode a whole line when stalled storage
        // has left fewer bytes than the next record needs.
        let limit = byte_budget.available_permits();
        if limit == 0 || sender.0.is_closed() || sender.0.is_full() {
            return;
        }
        let event = JournalLine {
            schema_version: 1,
            sequence,
            thread_id: &self.thread_id,
            turn_id: &self.turn_id,
            event,
        };
        let mut line = BoundedLine {
            bytes: Vec::new(),
            limit,
        };
        if serde_json::to_writer(&mut line, &event).is_err() || line.write_all(b"\n").is_err() {
            return;
        }
        let Ok(capacity) = u32::try_from(line.bytes.capacity()) else {
            return;
        };
        let Ok(bytes) = Arc::clone(byte_budget).try_acquire_many_owned(capacity) else {
            return;
        };
        if sender.0.try_send(Record { line: line.bytes, _bytes: bytes }).is_ok() {
            *accepted = accepted.saturating_add(1);
        }
    }

    fn seal(&self, expected: u64) -> (Result<watch::Receiver<Option<Value>>, &'static str>, u64) {
        let mut ingress = self.ingress.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match &mut *ingress {
            Ingress::Active { sender, sealed_expected, accepted, finished, .. } => {
                // A reservation cannot be in flight after the turn snapshot:
                // producer enqueue and sequence assignment take the same lock.
                if let Some(sender) = sender.take() {
                    sealed_expected.store(expected, Ordering::Release);
                    drop(sender);
                }
                (Ok(finished.clone()), expected.saturating_sub(*accepted))
            }
            Ingress::Unavailable(reason) => (Err(*reason), expected),
            Ingress::Dormant => (Err("archive writer was not started"), expected),
        }
    }
}

fn run_writer(
    home: &Path,
    thread_id: &str,
    turn_id: &str,
    receiver: async_channel::Receiver<Record>,
    sealed_expected: &AtomicU64,
) -> Value {
    let artifact = Arc::new(tokio::sync::Mutex::new(RawOutputArtifact::pending(home, thread_id)));
    let mut task = RawOutputArtifactTask::spawn(Some(Arc::clone(&artifact)), Default::default());
    let mut received = 0_u64;
    let mut bytes = 0_u64;
    let mut digest = Sha256::new();
    let mut size_limit = false;
    while let Ok(record) = receiver.recv_blocking() {
        let total = bytes.saturating_add(record.line.len() as u64);
        // Keep whole JSONL records and a stable retained prefix at the ceiling.
        if !size_limit && total <= codex_tool_artifacts::MAX_COMMAND_LOG_BYTES {
            if let Some(task) = task.as_mut() {
                task.write_chunk(&record.line);
            }
            bytes = total;
            digest.update(&record.line);
        } else {
            size_limit = true;
        }
        received = received.saturating_add(1);
    }

    let expected = sealed_expected.load(Ordering::Acquire);
    if let Some(mut task) = task {
        if expected == 0 || received != expected || size_limit {
            task.mark_output_gap();
        }
        task.finish_blocking();
    }
    let reference = RawOutputArtifact::output_reference(Some(&*artifact.blocking_lock()));
    let mut receipt = json!({
        "schema_version": 1, "format": "turn-timing-overflow-jsonl-v1",
        "artifact_id": reference.get("artifact_id"),
        "owner_thread_id": thread_id, "turn_id": turn_id,
        "bytes": reference.get("retained_bytes"),
        "expected_records": expected, "received_records": received,
        "complete": expected != 0 && received == expected
            && reference.get("capture_complete") == Some(&Value::Bool(true)),
        "output_reference": reference,
        "ordering": "sequence; disk order may differ",
    });
    if reference["retrieval_availability"] == "available"
        && reference["retained_bytes"].as_u64() == Some(bytes)
    {
        receipt["sha256"] = hex::encode(digest.finalize()).into();
    }
    if received != expected && expected != 0 {
        receipt["missing_records"] = expected.saturating_sub(received).into();
    }
    if let Some(reason) = if expected == 0 {
        Some("archive owner ended before terminal seal")
    } else if received != expected {
        Some("archive ingress lost reserved records")
    } else if size_limit {
        Some("journal reached its size limit; later records were not retained")
    } else if receipt["complete"] != true {
        Some("archive sink did not retain a complete journal")
    } else {
        None
    } {
        receipt["reason"] = reason.into();
    }
    receipt
}

impl TurnTimingStateInner {
    pub(super) fn archive_record(&mut self, event: Value, force: bool) {
        if self.completed_snapshot.is_some() || (!force && self.archive_sequence == 0) {
            return;
        }
        let Some(archive) = self.detail_archive.as_ref() else { return; };
        self.archive_sequence = self.archive_sequence.saturating_add(1);
        // A refused enqueue never rewinds expected_records. That gap is
        // observable both in the journal receipt and in native raw recovery.
        archive.enqueue(self.archive_sequence, event);
    }
}

impl TurnTimingState {
    pub fn bind_detail_archive(&self, home: &Path, thread_id: &str, turn_id: &str) {
        let mut state = self.state();
        if state.detail_archive.is_some() || state.completed_snapshot.is_some() {
            return;
        }
        state.detail_archive = Some(Arc::new(Archive {
            home: home.into(),
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            runtime: Handle::try_current().ok(),
            ingress: Mutex::default(),
        }));
    }

    pub async fn retain_overflow_details(
        &self,
        timing: &mut codex_protocol::protocol::TurnTiming,
        ephemeral: bool,
    ) {
        let skipped = self.missed_process_wait_attributions.load(Ordering::Acquire)
            & !PROCESS_WAIT_ATTRIBUTION_SEALED;
        if skipped > 0 {
            let evidence = timing.measurement_evidence.get_or_insert_with(|| json!({}));
            evidence["processWaitAttribution"] = json!({"complete": false, "collectorContention": skipped});
        }
        let ((finished, ingress_missing), expected) = {
            let state = self.state();
            if state.archive_sequence == 0 {
                if timing.tool_call_timing_overflow > 0 {
                    let evidence = timing.measurement_evidence.get_or_insert_with(|| json!({}));
                    evidence["toolTimingOverflow"] = json!({
                        "schema_version": 1, "format": "turn-timing-overflow-jsonl-v1",
                        "complete": false, "capture_bound": state.detail_archive.is_some(),
                        "ephemeral": ephemeral,
                        "reason": "overflow calls have no recorded journal events",
                        "unrecorded_calls": timing.tool_call_timing_overflow,
                    });
                }
                return;
            }
            let Some(archive) = state.detail_archive.as_ref() else { return; };
            (archive.seal(state.archive_sequence), state.archive_sequence)
        };
        let receipt = match finished {
            Ok(mut finished) => match tokio::time::timeout(
                TERMINAL_OBSERVATION,
                finished.wait_for(Option::is_some),
            )
            .await
            {
                Ok(Ok(receipt)) => Ok(receipt.as_ref().expect("matched a writer receipt").clone()),
                Ok(Err(_)) => Err("archive finalization worker failed"),
                Err(_) => Err("archive observation timed out"),
            },
            Err(reason) => Err(reason),
        }
        .unwrap_or_else(|reason| {
            let mut receipt = json!({
                "schema_version": 1, "format": "turn-timing-overflow-jsonl-v1",
                "complete": false, "capture_bound": true, "ephemeral": ephemeral,
                "expected_records": expected, "reason": reason,
            });
            // A stalled writer cannot report final persistence, but rejected
            // ingress is already known and will never be recovered.
            if ingress_missing != 0 {
                receipt["ingress_missing_records"] = ingress_missing.into();
            }
            receipt
        });
        let evidence = timing.measurement_evidence.get_or_insert_with(|| json!({}));
        evidence["toolTimingOverflow"] = receipt;
    }
}
