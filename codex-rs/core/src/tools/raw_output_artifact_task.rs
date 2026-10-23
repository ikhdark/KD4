use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use super::MAX_RAW_OUTPUT_ARTIFACT_BYTES;
use super::RawOutputArtifact;
use super::RawOutputArtifactWriter;
use super::STREAMING_ARTIFACT_BUFFER_BYTES;

const RAW_OUTPUT_ARTIFACT_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Persists the retained raw-output prefix without putting filesystem I/O on
/// the process reader's live-output path. The producer accepts at most one byte
/// beyond the artifact cap so the writer can preserve its truncation marker
/// while the in-memory queue remains bounded. Coalescing before enqueueing also
/// bounds message/allocator overhead when the producer emits tiny chunks and
/// storage stalls; no filesystem wait is added to the process reader.
#[derive(Debug)]
pub(crate) struct RawOutputArtifactTask {
    sender: Option<mpsc::UnboundedSender<Option<Vec<u8>>>>,
    task: JoinHandle<()>,
    state: Arc<Mutex<RawOutputArtifact>>,
    accepted_bytes: usize,
    pending_output: Vec<u8>,
    gap_recorded: bool,
}

/// Only original producer chunks enter this sink. Display decoding, cap markers
/// and cancellation/status notices never become canonical command bytes.
#[derive(Debug)]
pub(crate) struct LegacyRawCapture {
    task: Option<RawOutputArtifactTask>,
    inline: Option<Vec<u8>>,
    gap: bool,
    attached: bool,
}

pub(crate) type SharedLegacyRawCapture = Arc<std::sync::Mutex<LegacyRawCapture>>;

tokio::task_local! {
    static LEGACY_RAW_CAPTURE: SharedLegacyRawCapture;
}

pub(crate) fn current_legacy_capture() -> Option<SharedLegacyRawCapture> {
    LEGACY_RAW_CAPTURE
        .try_with(|capture| {
            capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .attached = true;
            Arc::clone(capture)
        })
        .ok()
}

impl LegacyRawCapture {
    pub(crate) fn write_chunk(&mut self, bytes: &[u8]) {
        if let Some(inline) = &mut self.inline {
            if inline.len().saturating_add(bytes.len())
                <= super::LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES
            {
                inline.extend_from_slice(bytes);
            } else {
                self.inline = None;
            }
        }
        if let Some(task) = &mut self.task {
            task.write_chunk(bytes);
        }
    }

    pub(crate) fn mark_output_gap(&mut self) {
        self.gap = true;
        if let Some(task) = &mut self.task {
            task.mark_output_gap();
        }
    }
}

#[derive(Clone)]
pub(crate) struct CapturedShellOutput {
    pub(crate) artifact: RawOutputArtifact,
    pub(crate) canonical: Option<codex_tools::CanonicalToolResult>,
}

pub(crate) async fn capture_legacy_output<F: std::future::Future>(
    home: &std::path::Path,
    thread_id: &str,
    future: F,
) -> (F::Output, CapturedShellOutput) {
    let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home, thread_id)));
    let capture = Arc::new(std::sync::Mutex::new(LegacyRawCapture {
        task: RawOutputArtifactTask::spawn(Some(Arc::clone(&state)), CancellationToken::new()),
        inline: Some(Vec::new()),
        gap: false,
        attached: false,
    }));
    let output = LEGACY_RAW_CAPTURE.scope(Arc::clone(&capture), future).await;
    // Never hold the reader's synchronous lock while waiting for storage.
    let (task, inline, gap, attached) = {
        let mut capture = capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            capture.task.take(),
            capture.inline.take(),
            capture.gap,
            capture.attached,
        )
    };
    if let Some(task) = task {
        task.finish().await;
    }
    let artifact = state.lock().await.clone();
    let canonical = match &artifact {
        RawOutputArtifact::Pending { .. } if attached => inline.map(|bytes| {
            let mut canonical = codex_tools::CanonicalToolResult::bytes(bytes);
            canonical.complete = !gap;
            canonical
        }),
        RawOutputArtifact::Stored { id, .. } => {
            super::load_tool_output_snapshot(home, thread_id, &id.to_string())
                .await
                .ok()
                .map(|snapshot| snapshot.canonical_result())
        }
        RawOutputArtifact::Pending { .. } | RawOutputArtifact::Failed { .. } => None,
    };
    (
        output,
        CapturedShellOutput {
            artifact,
            canonical,
        },
    )
}

impl RawOutputArtifactTask {
    pub(crate) fn spawn(
        state: Option<Arc<Mutex<RawOutputArtifact>>>,
        ready: CancellationToken,
    ) -> Option<Self> {
        let state = state?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        let task_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            let ready_on_exit = ready.clone().drop_guard();
            let mut writer = RawOutputArtifactWriter::open(Some(&task_state)).await;
            while let Some(output) = receiver.recv().await {
                if let Some(writer) = writer.as_mut() {
                    if let Some(output) = output {
                        writer.write_chunk(Some(&task_state), &output).await;
                    } else {
                        writer.mark_output_gap(Some(&task_state)).await;
                    }
                }
            }
            if let Some(writer) = writer.as_mut() {
                writer.flush_pending(Some(&task_state)).await;
            }
            // Readiness means finalized: consumers snapshot the state right
            // after it and report capture completeness from `finalized`, and
            // readers stay locked out until finish. Finish adds no data sync,
            // and the abort drop guard still releases waiters.
            if let Some(writer) = writer.as_mut() {
                writer.finish(Some(&task_state)).await;
            }
            ready.cancel();
            drop(ready_on_exit);
        });
        Some(Self {
            sender: Some(sender),
            task,
            state,
            accepted_bytes: 0,
            pending_output: Vec::new(),
            gap_recorded: false,
        })
    }

    pub(crate) fn write_chunk(&mut self, output: &[u8]) {
        if output.is_empty() {
            return;
        }
        let queue_limit = MAX_RAW_OUTPUT_ARTIFACT_BYTES.saturating_add(1);
        let remaining = queue_limit.saturating_sub(self.accepted_bytes);
        if remaining == 0 {
            return;
        }
        if self.sender.is_none() {
            return;
        }
        let mut retained = &output[..output.len().min(remaining)];
        while !retained.is_empty() {
            let count = retained
                .len()
                .min(STREAMING_ARTIFACT_BUFFER_BYTES - self.pending_output.len());
            self.pending_output.extend_from_slice(&retained[..count]);
            self.accepted_bytes += count;
            retained = &retained[count..];
            if self.pending_output.len() == STREAMING_ARTIFACT_BUFFER_BYTES {
                self.flush_pending_output();
                if self.sender.is_none() {
                    break;
                }
            }
        }
    }

    fn flush_pending_output(&mut self) {
        if self.pending_output.is_empty() {
            return;
        }
        let output = std::mem::take(&mut self.pending_output);
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.send(Some(output)).is_err())
        {
            self.sender = None;
        }
    }

    pub(crate) async fn finish(mut self) {
        self.flush_pending_output();
        self.sender.take();
        if matches!(
            tokio::time::timeout(RAW_OUTPUT_ARTIFACT_FINALIZATION_TIMEOUT, &mut self.task).await,
            Ok(Ok(()))
        ) {
            return;
        }

        self.task.abort();
        let mut artifact = self.state.lock().await;
        let failed = match &*artifact {
            RawOutputArtifact::Pending { .. } => {
                RawOutputArtifact::unavailable("raw output artifact finalization timed out")
            }
            RawOutputArtifact::Stored {
                id, path, bytes, ..
            } => RawOutputArtifact::Failed {
                id: Some(*id),
                message: "raw output artifact finalization timed out".to_string(),
                owned_path: Some(path.clone()),
                bytes: *bytes,
            },
            RawOutputArtifact::Failed { .. } => return,
        };
        *artifact = failed;
    }

    pub(crate) fn mark_output_gap(&mut self) {
        if self.gap_recorded {
            return;
        }
        self.gap_recorded = true;
        // Keep the producer gap between its preceding and following bytes.
        self.flush_pending_output();
        if let Some(sender) = &self.sender {
            let _ = sender.send(None);
        }
    }
}

impl Drop for RawOutputArtifactTask {
    fn drop(&mut self) {
        if !self.pending_output.is_empty() {
            // Cancellation/unwinding must not discard coalesced bytes while the
            // detached writer certifies the already-queued prefix as complete.
            // This only queues data and a gap marker; Drop never waits on disk.
            self.mark_output_gap();
            self.flush_pending_output();
        }
    }
}

#[cfg(test)]
mod raw_artifact_queue_tests {
    use super::*;

    #[tokio::test]
    async fn dropping_capture_flushes_the_buffered_tail_as_incomplete() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut artifact = RawOutputArtifactTask {
            sender: Some(sender),
            task: tokio::spawn(async {}),
            state: Arc::new(Mutex::new(RawOutputArtifact::unavailable("stalled sink"))),
            accepted_bytes: 0,
            pending_output: Vec::new(),
            gap_recorded: false,
        };
        artifact.write_chunk(b"buffered tail");
        drop(artifact);
        assert_eq!(
            receiver.try_recv().unwrap(),
            Some(b"buffered tail".to_vec())
        );
        assert_eq!(receiver.try_recv().unwrap(), None);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn coalesced_capture_recovers_unicode_crlf_binary_and_the_partial_tail() {
        let home = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(RawOutputArtifact::pending(
            home.path(),
            "coalesced",
        )));
        let ready = CancellationToken::new();
        let mut task =
            RawOutputArtifactTask::spawn(Some(Arc::clone(&state)), ready.clone()).unwrap();
        let mut bytes = "é😀\r\n".repeat(9_000).into_bytes();
        bytes.push(0xff);
        for chunk in bytes.chunks(3) {
            task.write_chunk(chunk);
        }
        task.finish().await;
        assert!(ready.is_cancelled());
        let artifact = state.lock().await.clone();
        assert!(artifact.capture_finalized_complete());
        let id = artifact.artifact_id().unwrap().to_string();
        let snapshot = super::super::load_tool_output_snapshot(home.path(), "coalesced", &id)
            .await
            .unwrap();
        assert_eq!(snapshot.historical_authenticity(), "authenticated");
        assert_eq!(snapshot.canonical_result().bytes, bytes);
    }

    #[tokio::test]
    async fn tiny_artifact_chunks_bound_queue_overhead_with_a_stalled_sink() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut artifact = RawOutputArtifactTask {
            sender: Some(sender),
            task: tokio::spawn(async {}),
            state: Arc::new(Mutex::new(RawOutputArtifact::unavailable("stalled sink"))),
            accepted_bytes: 0,
            pending_output: Vec::new(),
            gap_recorded: false,
        };
        let bytes = 2 * super::super::STREAMING_ARTIFACT_BUFFER_BYTES + 3;
        for _ in 0..bytes {
            artifact.write_chunk(b"x");
        }
        assert!(
            receiver.len() <= 2,
            "tiny chunks must not allocate one queued message each"
        );
        artifact.finish().await;
        let mut recovered = Vec::new();
        let mut messages = 0;
        while let Ok(chunk) = receiver.try_recv() {
            recovered.extend(chunk.expect("no producer gap"));
            messages += 1;
        }
        assert_eq!(messages, 3);
        assert_eq!(recovered, vec![b'x'; bytes]);
    }

    #[tokio::test]
    async fn coalesced_capture_keeps_gap_order_and_flushes_the_final_tail() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut artifact = RawOutputArtifactTask {
            sender: Some(sender),
            task: tokio::spawn(async {}),
            state: Arc::new(Mutex::new(RawOutputArtifact::unavailable("stalled sink"))),
            accepted_bytes: 0,
            pending_output: Vec::new(),
            gap_recorded: false,
        };
        artifact.write_chunk(b"before");
        artifact.mark_output_gap();
        artifact.mark_output_gap();
        artifact.write_chunk(b"after");
        artifact.finish().await;
        assert_eq!(receiver.try_recv().unwrap(), Some(b"before".to_vec()));
        assert_eq!(receiver.try_recv().unwrap(), None);
        assert_eq!(receiver.try_recv().unwrap(), Some(b"after".to_vec()));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn empty_artifact_chunks_do_not_queue_messages_or_consume_byte_capacity() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut artifact = RawOutputArtifactTask {
            sender: Some(sender),
            task: tokio::spawn(async {}),
            state: Arc::new(Mutex::new(RawOutputArtifact::unavailable("test sink"))),
            accepted_bytes: 0,
            pending_output: Vec::new(),
            gap_recorded: false,
        };
        for _ in 0..100 {
            artifact.write_chunk(b"");
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(artifact.accepted_bytes, 0);
        artifact.write_chunk(b"retained output");
        assert_eq!(artifact.accepted_bytes, 15);
        artifact.finish().await;
        assert_eq!(
            receiver.try_recv().expect("nonempty output queued"),
            Some(b"retained output".to_vec())
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    /// Audit 145 (A145-07): `raw_output_artifact()` snapshots the shared state
    /// as soon as readiness fires, so readiness must follow finalization. A gap
    /// still releases the waiter, but its capture is never complete.
    #[tokio::test]
    async fn audit145_claude_readiness_follows_capture_finalization() {
        let home = tempfile::tempdir().unwrap();
        for gap in [false, true] {
            let state = Arc::new(Mutex::new(RawOutputArtifact::pending(home.path(), "ready")));
            let ready = CancellationToken::new();
            let mut task =
                RawOutputArtifactTask::spawn(Some(Arc::clone(&state)), ready.clone()).unwrap();
            task.write_chunk(&vec![
                b'x';
                super::super::LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES + 1
            ]);
            if gap {
                task.mark_output_gap();
            }
            let finishing = tokio::spawn(task.finish());
            ready.cancelled().await;
            let snapshot = state.lock().await.clone();
            assert!(snapshot.artifact_id().is_some(), "gap={gap}");
            assert_eq!(snapshot.capture_finalized_complete(), !gap, "gap={gap}");
            finishing.await.unwrap();
        }
    }
}
