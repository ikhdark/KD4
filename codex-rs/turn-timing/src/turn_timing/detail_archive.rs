//! Overflow event journal backed by the existing bounded-memory raw artifact sink.
//! Writers apply disk backpressure outside the turn-state lock. Sequence numbers
//! assigned under that lock preserve causality even when concurrent writes arrive
//! out of order; no unbounded write queue or new admission policy is added.
use std::sync::{Arc, Mutex};
use codex_tool_artifacts::RawOutputArtifact;
use codex_tool_artifacts::capture::RawOutputArtifactTask;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use super::{TurnTimingState, TurnTimingStateInner};

#[derive(Debug)]
pub(super) struct Capture {
    task: Option<RawOutputArtifactTask>,
    artifact: Arc<tokio::sync::Mutex<RawOutputArtifact>>,
    notify: Arc<Notify>,
    thread_id: String,
    turn_id: String,
    received: u64,
    bytes: u64,
    digest: Sha256,
    finishing: bool,
}

pub(super) struct PendingRecord {
    capture: Arc<Mutex<Capture>>,
    sequence: u64,
    event: Value,
}

impl PendingRecord {
    pub(super) fn write(self) {
        let mut capture = self.capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let encoded = serde_json::to_vec(&json!({
            "schema_version": 1, "sequence": self.sequence,
            "thread_id": capture.thread_id, "turn_id": capture.turn_id, "event": self.event,
        }));
        match encoded {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                if let Some(task) = capture.task.as_mut() { task.write_chunk(&bytes); }
                capture.bytes = capture.bytes.saturating_add(bytes.len() as u64);
                capture.digest.update(&bytes);
            }
            Err(_) => {
                if let Some(task) = capture.task.as_mut() { task.mark_output_gap(); }
            }
        }
        capture.received = capture.received.saturating_add(1);
        capture.notify.notify_waiters();
    }
}

impl Capture {
    fn receipt(&self, expected: u64) -> Value {
        let reference = self.artifact.try_lock().ok()
            .map(|artifact| RawOutputArtifact::output_reference(Some(&artifact)))
            .unwrap_or_else(|| json!({}));
        json!({
            "schema_version": 1, "format": "turn-timing-overflow-jsonl-v1",
            "artifact_id": reference.get("artifact_id"),
            "owner_thread_id": self.thread_id, "turn_id": self.turn_id,
            "sha256": hex::encode(self.digest.clone().finalize()),
            "bytes": self.bytes, "expected_records": expected, "received_records": self.received,
            "complete": self.received == expected && reference.get("capture_complete") == Some(&Value::Bool(true)),
            "output_reference": reference,
            "ordering": "sequence; disk order may differ",
        })
    }
}

impl TurnTimingStateInner {
    pub(super) fn archive_record(&mut self, event: Value, force: bool) -> Option<PendingRecord> {
        if self.completed_snapshot.is_some() || (!force && self.archive_sequence == 0) { return None; }
        let capture = self.detail_archive.as_ref()?.clone();
        self.archive_sequence = self.archive_sequence.saturating_add(1);
        Some(PendingRecord { capture, sequence: self.archive_sequence, event })
    }
}

pub(super) fn write(record: Option<PendingRecord>) {
    if let Some(record) = record { record.write(); }
}

impl TurnTimingState {
    pub fn bind_detail_archive(&self, home: &std::path::Path, thread_id: &str, turn_id: &str) {
        let mut state = self.state();
        if state.detail_archive.is_some() || state.completed_snapshot.is_some() { return; }
        let artifact = Arc::new(tokio::sync::Mutex::new(RawOutputArtifact::pending(home, thread_id)));
        let task = RawOutputArtifactTask::spawn(Some(artifact.clone()), Default::default());
        state.detail_archive = Some(Arc::new(Mutex::new(Capture {
            task, artifact, notify: Arc::new(Notify::new()), thread_id: thread_id.into(),
            turn_id: turn_id.into(), received: 0, bytes: 0, digest: Sha256::new(), finishing: false,
        })));
    }

    pub async fn retain_overflow_details(&self, timing: &mut codex_protocol::protocol::TurnTiming, ephemeral: bool) {
        let (capture, expected) = {
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
            (state.detail_archive.clone(), state.archive_sequence)
        };
        let Some(capture) = capture else { return; };
        let runtime = tokio::runtime::Handle::current();
        // Writers hold this mutex across filesystem I/O. Even acquiring it to
        // start finalization or read a receipt must stay off the async worker.
        // The blocking owner survives expiry of the terminal observer below.
        let finish = tokio::task::spawn_blocking(move || {
            let start = {
                let mut state = capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.finishing { false } else { state.finishing = true; true }
            };
            if start {
                let notify = capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner).notify.clone();
                let drained = runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_millis(100), async {
                        loop {
                            let changed = notify.notified();
                            tokio::pin!(changed);
                            changed.as_mut().enable();
                            if capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner).received >= expected { break; }
                            changed.await;
                        }
                    }).await.is_ok()
                });
                let task = capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner).task.take();
                if let Some(mut task) = task {
                    if !drained { task.mark_output_gap(); }
                    task.finish_blocking();
                }
            }
            capture.lock().unwrap_or_else(std::sync::PoisonError::into_inner).receipt(expected)
        });
        let receipt = match tokio::time::timeout(std::time::Duration::from_millis(100), finish).await {
            Ok(Ok(receipt)) => receipt,
            result => json!({
                "schema_version": 1, "format": "turn-timing-overflow-jsonl-v1",
                "complete": false, "capture_bound": true, "ephemeral": ephemeral,
                "expected_records": expected,
                "reason": if result.is_err() { "archive observation timed out" } else { "archive finalization worker failed" },
            }),
        };
        let evidence = timing.measurement_evidence.get_or_insert_with(|| json!({}));
        evidence["toolTimingOverflow"] = receipt;
    }
}

