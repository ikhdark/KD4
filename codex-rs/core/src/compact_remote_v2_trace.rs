//! Bounded, session-owned ordering for optional remote-compaction diagnostics.

use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::models::ResponseItem;
use codex_rollout_trace::CompactionCheckpointTracePayload;
use codex_rollout_trace::CompactionResponseReceipt;
use codex_rollout_trace::CompactionTraceAttempt;
use codex_rollout_trace::CompactionTraceContext;
use codex_rollout_trace::TraceOccurrence;
use tokio::sync::mpsc;
use tokio_util::task::TaskTracker;

type TraceJob = Box<dyn FnOnce() + Send + 'static>;

// Backpressure bounds retained histories during slow/blocked storage. A request
// reserves its terminal slot before dispatch, so Drop never needs to spawn an
// unbounded waiter or discard an accepted cancellation record.
const TRACE_QUEUE_CAPACITY: usize = 8;

#[derive(Clone)]
pub(super) struct AsyncCompactionTraceContext {
    context: CompactionTraceContext,
    sender: Option<mpsc::Sender<TraceJob>>,
}

impl AsyncCompactionTraceContext {
    pub(super) fn new(tasks: &TaskTracker, context: CompactionTraceContext) -> Self {
        let sender = context.is_enabled().then(|| {
            let (sender, mut receiver) = mpsc::channel::<TraceJob>(TRACE_QUEUE_CAPACITY);
            let writes = tasks.clone();
            tasks.spawn(async move {
                while let Some(job) = receiver.recv().await {
                    if let Err(error) = writes.spawn_blocking(job).await {
                        tracing::warn!(%error, "compaction trace recording task failed");
                    }
                }
            });
            sender
        });
        Self { context, sender }
    }

    #[cfg(test)]
    pub(super) fn disabled() -> Self {
        Self {
            context: CompactionTraceContext::disabled(),
            sender: None,
        }
    }

    pub(super) fn with_context(&self, context: CompactionTraceContext) -> Self {
        Self {
            context,
            sender: self.sender.clone(),
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.context.is_enabled()
    }

    pub(super) async fn start_attempt(
        &self,
        model: String,
        instructions: String,
        input: Arc<[ResponseItem]>,
        parallel_tool_calls: bool,
    ) -> Option<RemoteCompactionTraceOwner> {
        let sender = self.sender.as_ref()?;
        let terminal = match sender.clone().reserve_owned().await {
            Ok(permit) => permit,
            Err(error) => {
                tracing::warn!(%error, "compaction trace queue closed before start");
                return None;
            }
        };
        let occurrence = TraceOccurrence::now();
        let slot = Arc::new(Mutex::new(None));
        let started = Arc::clone(&slot);
        let context = self.context.clone();
        if sender
            .send(Box::new(move || {
                let attempt = context.start_attempt_at(
                    &super::RemoteCompactionV2TraceRequest {
                        model: &model,
                        instructions: &instructions,
                        input: &input,
                        parallel_tool_calls,
                    },
                    occurrence,
                );
                *started
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(attempt);
            }))
            .await
            .is_err()
        {
            tracing::warn!("compaction trace queue closed while accepting start");
            return None;
        }
        Some(RemoteCompactionTraceOwner {
            slot,
            terminal: Some(terminal),
        })
    }

    pub(super) async fn record_installed(
        &self,
        input_history: Vec<ResponseItem>,
        replacement_history: Vec<ResponseItem>,
    ) {
        let Some(sender) = &self.sender else { return };
        let context = self.context.clone();
        if sender
            .send(Box::new(move || {
                context.record_installed(&CompactionCheckpointTracePayload {
                    input_history: &input_history,
                    replacement_history: &replacement_history,
                });
            }))
            .await
            .is_err()
        {
            tracing::warn!("compaction trace queue closed while accepting checkpoint");
        }
    }
}

pub(super) struct RemoteCompactionTraceOwner {
    slot: Arc<Mutex<Option<CompactionTraceAttempt>>>,
    terminal: Option<mpsc::OwnedPermit<TraceJob>>,
}

impl RemoteCompactionTraceOwner {
    pub(super) fn record_result(
        mut self,
        result: Result<Vec<ResponseItem>, String>,
        receipt: Option<CompactionResponseReceipt>,
        cancelled: bool,
    ) {
        let occurrence = TraceOccurrence::now();
        let slot = Arc::clone(&self.slot);
        if let Some(terminal) = self.terminal.take() {
            terminal.send(Box::new(move || {
                let attempt = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(attempt) = attempt {
                    let attempt = attempt.at_occurrence(occurrence);
                    if cancelled {
                        attempt.record_cancelled("compaction cancelled");
                    } else {
                        attempt.record_result_with_receipt(result.as_deref(), receipt.as_ref());
                    }
                }
            }));
        }
    }
}

impl Drop for RemoteCompactionTraceOwner {
    fn drop(&mut self) {
        let occurrence = TraceOccurrence::now();
        let slot = Arc::clone(&self.slot);
        if let Some(terminal) = self.terminal.take() {
            terminal.send(Box::new(move || {
                let attempt = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(attempt) = attempt {
                    attempt
                        .at_occurrence(occurrence)
                        .record_cancelled("compaction owner dropped");
                }
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn dropped_attempt_keeps_reserved_terminal_order_while_writer_is_blocked()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let writer = Arc::new(codex_rollout_trace::TraceWriter::create(
            dir.path(),
            "trace".into(),
            "rollout".into(),
            "root".into(),
        )?);
        let tasks = TaskTracker::new();
        let context = AsyncCompactionTraceContext::new(
            &tasks,
            CompactionTraceContext::enabled(
                writer,
                "root".into(),
                "turn".into(),
                "compact".into(),
                "model".into(),
                "provider".into(),
            ),
        );
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        context
            .sender
            .as_ref()
            .unwrap()
            .send(Box::new(move || {
                let _ = entered.send(());
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }))
            .await
            .map_err(|_| anyhow::anyhow!("queue closed"))?;
        entered_rx.await?;
        let owner = context
            .start_attempt("model".into(), "instructions".into(), Arc::from([]), false)
            .now_or_never()
            .expect("trace admission must not wait for storage")
            .unwrap();
        drop(owner); // Uses its reserved slot, never a detached Drop task.
        context
            .record_installed(vec![], vec![])
            .now_or_never()
            .expect("checkpoint admission must not wait for storage");
        // Saturation has bounded backpressure rather than unbounded tasks.
        while context
            .sender
            .as_ref()
            .unwrap()
            .try_send(Box::new(|| {}))
            .is_ok()
        {}
        assert!(
            context
                .start_attempt("model".into(), "full".into(), Arc::from([]), false)
                .now_or_never()
                .is_none()
        );
        drop(context);
        release.send(())?;
        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.wait()).await?;
        let kinds = std::fs::read_to_string(dir.path().join("trace.jsonl"))?
            .lines()
            .map(serde_json::from_str::<codex_rollout_trace::RawTraceEvent>)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|event| match event.payload {
                codex_rollout_trace::RawTraceEventPayload::CompactionRequestStarted { .. } => {
                    Some("start")
                }
                codex_rollout_trace::RawTraceEventPayload::CompactionRequestCancelled {
                    ..
                } => Some("cancel"),
                codex_rollout_trace::RawTraceEventPayload::CompactionInstalled { .. } => {
                    Some("installed")
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(kinds, ["start", "cancel", "installed"]);
        assert!(tasks.is_empty());
        Ok(())
    }
}
