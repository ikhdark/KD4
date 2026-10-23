use super::input_queue::InputQueue;
use super::*;
use crate::agents_md_manager::AgentsMdManager;
use crate::config::ConstraintError;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::exec_env::create_env;
use crate::git_workspace::GitWorkspaceCache;
use crate::shell_snapshot::ShellSnapshot;
use crate::skills::SkillError;
use crate::startup_timing::StartupTimingState;
use crate::state::ActiveTurn;
use codex_extension_api::ExtensionDataInit;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_protocol::SessionId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentSelections;
use std::sync::OnceLock;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::watch;

#[cfg(test)]
pub(crate) struct SessionInitPersistenceTestHook {
    pub codex_home: PathBuf,
    pub ready: tokio::sync::oneshot::Sender<ThreadId>,
    pub release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
pub(crate) static SESSION_INIT_PERSISTENCE_TEST_HOOK: std::sync::Mutex<
    Option<SessionInitPersistenceTestHook>,
> = std::sync::Mutex::new(None);

const TOOL_HISTORY_JOURNAL_COMPACTION_RECORDS: u64 = 128;
const TOOL_HISTORY_JOURNAL_COMPACTION_BYTES: u64 = 1024 * 1024;

fn coordination_startup_result(
    source: &SessionSource,
    result: codex_agent_task_store::StoreResult<()>,
) -> anyhow::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if source.is_non_root_agent() => {
            Err(anyhow::anyhow!("failed to initialize child task coordination: {error}"))
        }
        Err(error) => {
            // Root analysis remains usable. Missing validation scope obtains an
            // uncertain/spent budget, and spawn still requires durable admission.
            warn!(%error, "task coordination unavailable; dependent operations remain fail-closed");
            Ok(false)
        }
    }
}

enum ToolHistoryPersistenceCommand {
    Mutation(Box<crate::tool_history::ToolHistoryMutation>),
    Checkpoint,
    ReplaceSnapshot {
        snapshot: Box<crate::tool_history::ToolHistoryState>,
        rollout_barrier: Option<LiveThread>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolHistoryPersistenceEnqueueError {
    SequenceOverflow,
    WorkerClosed,
}

impl std::fmt::Display for ToolHistoryPersistenceEnqueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SequenceOverflow => {
                formatter.write_str("tool-history persistence sequence overflowed")
            }
            Self::WorkerClosed => formatter.write_str("tool-history persistence worker is closed"),
        }
    }
}

impl std::error::Error for ToolHistoryPersistenceEnqueueError {}

struct PendingToolHistoryPersistence {
    sequence: u64,
    command: ToolHistoryPersistenceCommand,
    description: &'static str,
}

#[derive(Default)]
struct ToolHistoryPersistenceState {
    next_sequence: u64,
    pending: Vec<PendingToolHistoryPersistence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ToolHistoryPersistenceError {
    SequenceOverflow,
    WorkerClosed,
    Persistence(String),
}

impl std::fmt::Display for ToolHistoryPersistenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SequenceOverflow => {
                formatter.write_str("tool-history persistence sequence overflowed")
            }
            Self::WorkerClosed => formatter.write_str("tool-history persistence worker is closed"),
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ToolHistoryPersistenceError {}

impl From<ToolHistoryPersistenceEnqueueError> for ToolHistoryPersistenceError {
    fn from(error: ToolHistoryPersistenceEnqueueError) -> Self {
        match error {
            ToolHistoryPersistenceEnqueueError::SequenceOverflow => Self::SequenceOverflow,
            ToolHistoryPersistenceEnqueueError::WorkerClosed => Self::WorkerClosed,
        }
    }
}

#[derive(Clone, Default)]
struct ToolHistoryPersistenceProgress {
    completed_sequence: u64,
    failure: Option<(u64, ToolHistoryPersistenceError)>,
}

struct ToolHistoryPersistenceWorker {
    shutdown: tokio_util::sync::CancellationToken,
    stopped: tokio_util::sync::CancellationToken,
    // Retain the worker with the queue. Shutdown interrupts idle/gate waits, but
    // an active file write keeps its permit until the actual operation completes.
    _task: tokio::task::JoinHandle<()>,
}

impl Drop for ToolHistoryPersistenceWorker {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Clone)]
pub(super) struct ToolHistoryPersistenceQueue {
    state: Arc<tokio::sync::Mutex<ToolHistoryPersistenceState>>,
    wake_tx: mpsc::Sender<()>,
    progress: watch::Receiver<ToolHistoryPersistenceProgress>,
    mirror: Arc<tokio::sync::Mutex<crate::tool_history::ToolHistoryState>>,
    _worker: Arc<ToolHistoryPersistenceWorker>,
    #[cfg(test)]
    persisted_batch_count: Arc<std::sync::atomic::AtomicU64>,
}

pub(super) struct SessionStorageOwnership {
    _ledger: codex_file_system::AtomicWriteLock,
    _artifacts: Arc<std::fs::File>,
}

async fn acquire_session_storage_ownership(
    home: &std::path::Path,
    thread_id: ThreadId,
) -> std::io::Result<Arc<SessionStorageOwnership>> {
    let artifacts = crate::tools::command_output_artifact::acquire_artifact_residency(
        home,
        &thread_id.to_string(),
    )
    .await?;
    if tokio::fs::try_exists(
        home.join("tool-history")
            .join(format!("{thread_id}.retired")),
    )
    .await?
    {
        return Err(std::io::Error::other(
            "thread storage was permanently retired",
        ));
    }
    // Separate lifecycle ownership from the short checkpoint transaction lock.
    let owner_path = home.join("tool-history").join(format!("{thread_id}.owner"));
    let ledger = tokio::task::spawn_blocking(move || {
        codex_file_system::try_acquire_atomic_write_lock(&owner_path)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "completed-tool history has another live owner; retry after it closes",
            )
        })
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(Arc::new(SessionStorageOwnership {
        _ledger: ledger,
        _artifacts: artifacts,
    }))
}

#[cfg(test)]
mod durable_storage_ownership_tests {
    use super::*;

    #[tokio::test]
    async fn audit63_retired_thread_cannot_reacquire_session_storage() {
        let home = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        drop(
            acquire_session_storage_ownership(home.path(), id)
                .await
                .unwrap(),
        );
        std::fs::write(
            home.path()
                .join("tool-history")
                .join(format!("{id}.retired")),
            b"retired",
        )
        .unwrap();
        assert!(
            acquire_session_storage_ownership(home.path(), id)
                .await
                .is_err()
        );
        assert!(
            acquire_session_storage_ownership(home.path(), ThreadId::new())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn durable_audit_11_owner_survives_the_last_worker_clone() {
        let home = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        let owner = acquire_session_storage_ownership(home.path(), id)
            .await
            .unwrap();
        let worker = Arc::clone(&owner);
        assert!(
            acquire_session_storage_ownership(home.path(), id)
                .await
                .is_err()
        );
        drop(owner);
        assert!(
            acquire_session_storage_ownership(home.path(), id)
                .await
                .is_err()
        );
        assert!(
            acquire_session_storage_ownership(home.path(), ThreadId::new())
                .await
                .is_ok()
        );
        drop(worker);
        assert!(
            acquire_session_storage_ownership(home.path(), id)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn cancelled_queue_shutdown_retains_storage_until_writer_stops() {
        let home = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        let owner = acquire_session_storage_ownership(home.path(), id)
            .await
            .unwrap();
        let gate = Arc::new(Semaphore::new(1));
        let held = Arc::clone(&gate).acquire_owned().await.unwrap();
        let queue = ToolHistoryPersistenceQueue::new_with_owner(
            gate,
            home.path().to_path_buf(),
            id,
            crate::tool_history::ToolHistoryState::default(),
            Some(owner),
        );
        let mut shutdown = Box::pin(queue.shutdown());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), shutdown.as_mut())
                .await
                .is_err(),
            "shutdown cannot bypass a blocked ledger checkpoint"
        );
        drop(shutdown);
        assert!(
            acquire_session_storage_ownership(home.path(), id)
                .await
                .is_err(),
            "cancelling the observer must not release the live writer's lease"
        );
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), queue.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(
            acquire_session_storage_ownership(home.path(), id)
                .await
                .is_ok(),
            "a stopped queue must release ownership even while its handle survives"
        );
        assert!(queue._worker.stopped.is_cancelled());
    }
}

impl ToolHistoryPersistenceQueue {
    #[cfg(test)]
    pub(super) fn new(
        io_gate: Arc<Semaphore>,
        codex_home: PathBuf,
        thread_id: ThreadId,
        initial_state: crate::tool_history::ToolHistoryState,
    ) -> Self {
        Self::new_with_owner(io_gate, codex_home, thread_id, initial_state, None)
    }

    fn new_with_owner(
        io_gate: Arc<Semaphore>,
        codex_home: PathBuf,
        thread_id: ThreadId,
        initial_state: crate::tool_history::ToolHistoryState,
        owner: Option<Arc<SessionStorageOwnership>>,
    ) -> Self {
        let state = Arc::new(tokio::sync::Mutex::new(
            ToolHistoryPersistenceState::default(),
        ));
        let worker_state = Arc::clone(&state);
        let mirror = Arc::new(tokio::sync::Mutex::new(initial_state));
        let worker_mirror = Arc::clone(&mirror);
        let thread_id = thread_id.to_string();
        // Replay deduplicates sequence numbers per writer, so queue restarts must
        // not reuse an identity even when the clock repeats or moves backwards.
        let writer_id = uuid::Uuid::new_v4().to_string();
        // This channel only wakes the worker; state.pending owns every command.
        // Multiple enqueues can share a wakeup because the worker drains the batch.
        let (wake_tx, mut wake_rx) = mpsc::channel(/*buffer*/ 1);
        let (progress_tx, progress) = watch::channel(ToolHistoryPersistenceProgress::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let stopped = tokio_util::sync::CancellationToken::new();
        let worker_stopped = stopped.clone();
        #[cfg(test)]
        let persisted_batch_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(test)]
        let worker_persisted_batch_count = Arc::clone(&persisted_batch_count);
        let task = tokio::spawn(async move {
            // Declared before the owner so completion is signalled only after
            // the worker has released its storage lease, including on unwind.
            let _stopped = worker_stopped.drop_guard();
            // Includes an in-flight blocking checkpoint after Session/caller
            // cancellation. A successor must not load a stale private mirror.
            let _owner = owner;
            let mut journal_records = 0_u64;
            let mut journal_bytes = 0_u64;
            let mut journal_sequence = 0_u64;
            let journal_writer = Arc::new(std::sync::Mutex::new(
                crate::tool_history::ToolHistoryJournalWriter::default(),
            ));
            let mut checkpoint_required = false;
            // The durable ledger already equals the mirror and no journal
            // append awaits sync. Starts false: the loaded journal may be unsynced.
            let mut mirror_checkpointed = false;
            let mut cleanup_required = false;
            let mut rollout_barrier: Option<LiveThread> = None;
            let mut progress = ToolHistoryPersistenceProgress::default();
            loop {
                tokio::select! {
                    biased;
                    _ = worker_shutdown.cancelled() => return,
                    wake = wake_rx.recv() => if wake.is_none() { return; },
                }
                loop {
                    let requests = {
                        let mut state = worker_state.lock().await;
                        std::mem::take(&mut state.pending)
                    };
                    let Some(completed) = requests.last().map(|request| request.sequence) else {
                        break;
                    };
                    let permit = tokio::select! {
                        biased;
                        _ = worker_shutdown.cancelled() => {
                            progress.failure = Some((completed, ToolHistoryPersistenceError::WorkerClosed));
                            progress_tx.send_replace(progress);
                            return;
                        }
                        permit = Arc::clone(&io_gate).acquire_owned() => match permit {
                            Ok(permit) => permit,
                            Err(_) => {
                                progress.failure = Some((completed, ToolHistoryPersistenceError::WorkerClosed));
                                progress_tx.send_replace(progress);
                                return;
                            }
                        },
                    };
                    let mut mutations = Vec::new();
                    let mut description = "completed-tool history metadata";
                    {
                        let mut mirror = worker_mirror.lock().await;
                        for request in requests {
                            description = request.description;
                            match request.command {
                                ToolHistoryPersistenceCommand::Mutation(mutation) => {
                                    mutation.apply(&mut mirror);
                                    mutations.push(*mutation);
                                    mirror_checkpointed = false;
                                }
                                ToolHistoryPersistenceCommand::Checkpoint => {
                                    // Every turn terminal checkpoints; rewriting and
                                    // syncing an unchanged ledger would add nothing.
                                    checkpoint_required |= !mirror_checkpointed;
                                }
                                ToolHistoryPersistenceCommand::ReplaceSnapshot {
                                    snapshot,
                                    rollout_barrier: replacement_barrier,
                                } => {
                                    *mirror = *snapshot;
                                    // The latest live-thread barrier covers every accepted
                                    // ordered prefix, including superseded replacements.
                                    rollout_barrier = replacement_barrier;
                                    checkpoint_required = true;
                                    cleanup_required = true;
                                    mirror_checkpointed = false;
                                }
                            }
                        }
                    }
                    // A contracted ledger must not outlive its still-buffered rollout.
                    // Keep the accepted replacement and barrier for explicit retry; the
                    // old durable ledger/protections remain intact on flush failure.
                    let mut failure = None;
                    if let Some(barrier) = rollout_barrier.as_ref() {
                        match barrier.flush_durable().await {
                            Ok(()) => rollout_barrier = None,
                            Err(error) => {
                                failure = Some(ToolHistoryPersistenceError::Persistence(format!(
                                    "failed to flush compacted rollout before tool-history pruning: {error}",
                                )))
                            }
                        }
                    }
                    // Once a full checkpoint has failed, the mirror owns the
                    // unacknowledged mutations. Later deltas cannot bypass them.
                    if failure.is_none() && !checkpoint_required && !mutations.is_empty() {
                        let sequenced_mutations = mutations
                            .into_iter()
                            .enumerate()
                            .map(|(index, mutation)| {
                                (
                                    journal_sequence.saturating_add(
                                        u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                                    ),
                                    mutation,
                                )
                            })
                            .collect::<Vec<_>>();
                        match crate::tool_history::persist_tool_history_mutations_with_writer(
                            &codex_home,
                            &thread_id,
                            &writer_id,
                            &sequenced_mutations,
                            Arc::clone(&journal_writer),
                        )
                        .await
                        {
                            Ok(bytes) => {
                                journal_sequence = sequenced_mutations
                                    .last()
                                    .map(|(sequence, _)| *sequence)
                                    .unwrap_or(journal_sequence);
                                journal_records = journal_records.saturating_add(
                                    u64::try_from(sequenced_mutations.len()).unwrap_or(u64::MAX),
                                );
                                journal_bytes = journal_bytes.saturating_add(bytes);
                                #[cfg(test)]
                                worker_persisted_batch_count
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(err) => {
                                tracing::warn!("failed to persist {description}: {err}");
                                checkpoint_required = true;
                            }
                        }
                    }
                    if failure.is_none()
                        && (checkpoint_required
                            || journal_records >= TOOL_HISTORY_JOURNAL_COMPACTION_RECORDS
                            || journal_bytes >= TOOL_HISTORY_JOURNAL_COMPACTION_BYTES)
                    {
                        let mirror = worker_mirror.lock().await.clone();
                        match crate::tool_history::persist_tool_history_state_with_writer(
                            &codex_home,
                            &thread_id,
                            &mirror,
                            Arc::clone(&journal_writer),
                        )
                        .await
                        {
                            Ok(()) => {
                                checkpoint_required = false;
                                if cleanup_required {
                                    let references = mirror.artifact_references();
                                    match crate::tools::command_output_artifact::prune_active_tool_history_artifact_protection(
                                        &codex_home, &thread_id, &references,
                                    ).await {
                                        Ok(()) => cleanup_required = false,
                                        Err(error) => {
                                            checkpoint_required = true;
                                            failure = Some(ToolHistoryPersistenceError::Persistence(format!(
                                                "failed to finish tool-history artifact cleanup: {error}",
                                            )));
                                        }
                                    }
                                }
                                mirror_checkpointed = !checkpoint_required;
                                journal_records = 0;
                                journal_bytes = 0;
                                #[cfg(test)]
                                worker_persisted_batch_count
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(err) if checkpoint_required => {
                                failure = Some(ToolHistoryPersistenceError::Persistence(format!(
                                    "failed to checkpoint {description}: {err}",
                                )));
                            }
                            Err(err) => {
                                // The journal append is complete; optional
                                // compaction failure leaves terminal checkpointing responsible for durability.
                                tracing::warn!(
                                    "failed to compact completed-tool history journal: {err}"
                                );
                            }
                        }
                    }
                    // Never cancel an active persistence future: it may own a
                    // blocking writer. Release serialization only after it ends.
                    drop(permit);
                    if let Some(error) = failure {
                        progress.failure = Some((completed, error));
                    } else {
                        progress.completed_sequence = completed;
                        progress.failure = None;
                    }
                    progress_tx.send_replace(progress.clone());
                    if worker_shutdown.is_cancelled() {
                        return;
                    }
                }
            }
        });
        Self {
            state,
            wake_tx,
            progress,
            mirror,
            _worker: Arc::new(ToolHistoryPersistenceWorker {
                shutdown,
                stopped,
                _task: task,
            }),
            #[cfg(test)]
            persisted_batch_count,
        }
    }

    pub(super) async fn writer(&self) -> ToolHistoryPersistenceWriter<'_> {
        ToolHistoryPersistenceWriter {
            state: self.state.lock().await,
            wake_tx: &self.wake_tx,
        }
    }

    #[cfg(test)]
    pub(super) async fn enqueue_mutation(
        &self,
        mutation: crate::tool_history::ToolHistoryMutation,
        description: &'static str,
    ) -> Result<(), ToolHistoryPersistenceEnqueueError> {
        self.writer().await.enqueue_mutation(mutation, description)
    }

    pub(super) fn check_failure(&self) -> Result<(), ToolHistoryPersistenceError> {
        match &self.progress.borrow().failure {
            Some((_, error)) => Err(error.clone()),
            None if self.wake_tx.is_closed() => Err(ToolHistoryPersistenceError::WorkerClosed),
            None => Ok(()),
        }
    }

    pub(super) async fn drain(&self) -> Result<(), ToolHistoryPersistenceError> {
        if self.wake_tx.is_closed() {
            return Err(ToolHistoryPersistenceError::WorkerClosed);
        }
        let target_sequence = self.state.lock().await.next_sequence;
        self.wait_for_sequence(target_sequence).await
    }

    pub(super) async fn checkpoint(&self) -> Result<(), ToolHistoryPersistenceError> {
        let target_sequence = self.writer().await.enqueue_command(
            ToolHistoryPersistenceCommand::Checkpoint,
            "completed-tool history checkpoint",
        )?;
        self.wait_for_sequence(target_sequence).await
    }

    /// Called after turn/ordered-commit admission has drained. A cancelled
    /// shutdown observer must not cancel an in-flight blocking ledger write.
    pub(super) async fn shutdown(&self) -> Result<(), ToolHistoryPersistenceError> {
        if !self._worker.shutdown.is_cancelled() {
            self.checkpoint().await?;
            self._worker.shutdown.cancel();
        }
        self._worker.stopped.cancelled().await;
        Ok(())
    }

    async fn wait_for_sequence(
        &self,
        target_sequence: u64,
    ) -> Result<(), ToolHistoryPersistenceError> {
        let mut progress = self.progress.clone();
        loop {
            {
                let current = progress.borrow_and_update();
                if current.completed_sequence >= target_sequence {
                    return Ok(());
                }
                if let Some((failed_sequence, error)) = &current.failure
                    && *failed_sequence >= target_sequence
                {
                    return Err(error.clone());
                }
            }
            progress
                .changed()
                .await
                .map_err(|_| ToolHistoryPersistenceError::WorkerClosed)?;
        }
    }

    pub(super) async fn replace_mirror(&self, state: crate::tool_history::ToolHistoryState) {
        *self.mirror.lock().await = state;
    }

    #[cfg(test)]
    pub(super) fn persisted_batch_count(&self) -> u64 {
        self.persisted_batch_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn worker_handle_for_test(&self) -> tokio::task::AbortHandle {
        self._worker._task.abort_handle()
    }

    #[cfg(test)]
    pub(super) async fn wait_until_worker_closed_for_test(&self) {
        self.wake_tx.closed().await;
    }
}

/// Acquired before changing session history so publishing its matching queue
/// entry needs no await and cannot be separated from that change by cancellation.
pub(super) struct ToolHistoryPersistenceWriter<'a> {
    state: tokio::sync::MutexGuard<'a, ToolHistoryPersistenceState>,
    wake_tx: &'a mpsc::Sender<()>,
}

impl<'queue> ToolHistoryPersistenceWriter<'queue> {
    pub(super) fn enqueue_mutation(
        &mut self,
        mutation: crate::tool_history::ToolHistoryMutation,
        description: &'static str,
    ) -> Result<(), ToolHistoryPersistenceEnqueueError> {
        self.enqueue_command(
            ToolHistoryPersistenceCommand::Mutation(Box::new(mutation)),
            description,
        )
        .map(|_| ())
    }

    pub(super) fn reserve_snapshot<'writer>(
        &'writer mut self,
        snapshot: crate::tool_history::ToolHistoryState,
        rollout_barrier: Option<LiveThread>,
    ) -> Result<ToolHistorySnapshotReservation<'writer, 'queue>, ToolHistoryPersistenceEnqueueError>
    {
        let prior_sequence = self.state.next_sequence;
        let prior_pending_len = self.state.pending.len();
        let sequence = match self.enqueue_command(
            ToolHistoryPersistenceCommand::ReplaceSnapshot {
                snapshot: Box::new(snapshot),
                rollout_barrier,
            },
            "compacted completed-tool history checkpoint",
        ) {
            Ok(sequence) => sequence,
            Err(error) => {
                self.state.pending.truncate(prior_pending_len);
                self.state.next_sequence = prior_sequence;
                return Err(error);
            }
        };
        Ok(ToolHistorySnapshotReservation {
            writer: self,
            prior_sequence,
            sequence,
            committed: false,
        })
    }

    fn enqueue_command(
        &mut self,
        command: ToolHistoryPersistenceCommand,
        description: &'static str,
    ) -> Result<u64, ToolHistoryPersistenceEnqueueError> {
        if self.wake_tx.is_closed() {
            return Err(ToolHistoryPersistenceEnqueueError::WorkerClosed);
        }
        let sequence = next_tool_history_persistence_sequence(self.state.next_sequence)
            .ok_or(ToolHistoryPersistenceEnqueueError::SequenceOverflow)?;
        self.state.next_sequence = sequence;
        self.state.pending.push(PendingToolHistoryPersistence {
            sequence,
            command,
            description,
        });
        match self.wake_tx.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err(ToolHistoryPersistenceEnqueueError::WorkerClosed);
            }
        }
        Ok(sequence)
    }
}

/// A queued snapshot stays behind the exclusively held writer until its
/// matching rollout/live-history commit succeeds. Failed or canceled preparation
/// rolls it back before the worker can observe it.
pub(super) struct ToolHistorySnapshotReservation<'writer, 'queue> {
    writer: &'writer mut ToolHistoryPersistenceWriter<'queue>,
    prior_sequence: u64,
    sequence: u64,
    committed: bool,
}

impl ToolHistorySnapshotReservation<'_, '_> {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ToolHistorySnapshotReservation<'_, '_> {
    fn drop(&mut self) {
        if !self.committed {
            self.writer
                .state
                .pending
                .retain(|request| request.sequence != self.sequence);
            self.writer.state.next_sequence = self.prior_sequence;
        }
    }
}

pub(super) fn next_tool_history_persistence_sequence(current: u64) -> Option<u64> {
    current.checked_add(1)
}

/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    pub(crate) thread_id: ThreadId,
    pub(super) _storage_ownership: std::sync::Mutex<Option<Arc<SessionStorageOwnership>>>,
    pub(crate) installation_id: String,
    pub(super) tx_event: Sender<Event>,
    pub(super) agent_status: watch::Sender<AgentStatus>,
    /// Serializes ownership checks with watch publication, never across I/O.
    pub(super) agent_status_turn: std::sync::Mutex<Option<String>>,
    pub(super) state: Mutex<SessionState>,
    /// Serializes cancellation-shielded rollout/history commits.
    pub(super) durable_history_commit_gate: Semaphore,
    /// Counts cancellation-shielded ordered commits so terminal flush cannot overtake one.
    pub(super) durable_history_commits_in_flight: std::sync::atomic::AtomicUsize,
    pub(super) durable_history_commits_changed: tokio::sync::Notify,
    /// Serializes completed-tool ledger snapshots with artifact protection changes so
    /// compaction cannot remove a marker while a concurrent candidate is being persisted.
    pub(super) tool_history_io_gate: Arc<Semaphore>,
    /// Serializes live ledger mutations with reconciliation without keeping the
    /// broader session-state mutex locked across file I/O.
    pub(super) tool_history_reconciliation_gate: Semaphore,
    /// Keeps ledger durability ordered without delaying nested result relay.
    pub(super) tool_history_persistence: ToolHistoryPersistenceQueue,
    /// Serializes rebuild/apply cycles for the running proxy; each cycle
    /// rebuilds from the current SessionState while holding this lock.
    pub(super) managed_network_proxy_refresh_lock: Semaphore,
    #[cfg(test)]
    pub(super) managed_network_proxy_start_gate: Semaphore,
    #[cfg(test)]
    pub(super) managed_network_proxy_start_entered: tokio::sync::Notify,
    /// The set of enabled features should be invariant for the lifetime of the
    /// session.
    pub(super) features: ManagedFeatures,
    pub(super) multi_agent_version: OnceLock<MultiAgentVersion>,
    pub(super) pending_mcp_server_refresh_config: Mutex<Option<Arc<McpServerRefreshConfig>>>,
    /// Serializes turn replacement/startup and rollback history publication.
    /// Owned permits keep rollback admission protected after observer cancellation.
    pub(crate) task_start_gate: Arc<Semaphore>,
    /// At most one capacity waiter for this session's retained trigger mail.
    pub(crate) pending_work_retry: Arc<std::sync::atomic::AtomicBool>,
    /// Bounds ordinary submissions across channels and deferred preparation,
    /// without preventing controls from reaching a blocked preparation.
    pub(crate) submission_preparation_slots: Arc<Semaphore>,
    /// Stores reserved-ID admissions without keeping a mutex guard across startup awaits.
    pub(crate) task_start_state: Mutex<TaskStartState>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    /// Correlated pre-turn startup phases, frozen at the first model send.
    pub(crate) startup_timing: Arc<StartupTimingState>,
    pub(crate) startup_cancellation: CancellationToken,
    /// One-shot startup-prepared router handoff to the first pending-turn plan.
    pub(crate) startup_prepared_router: crate::session_startup_prewarm::StartupPreparedRouterCache,
    /// Stable request artifacts retained across turns and invalidated by their exact owners.
    pub(super) prepared_tool_router: super::turn::SessionPreparedRouterCache,
    pub(super) request_scaffold_cache: std::sync::Mutex<super::turn::RequestScaffoldCache>,
    pub(super) turn_config_cache: std::sync::Mutex<Option<Arc<Config>>>,
    /// Last tool-surface definition queued for this rollout. Repeated requests persist a compact
    /// reference while the terminal barrier remains the durability owner.
    pub(super) queued_tool_manifest_hash: std::sync::Mutex<Option<String>>,
    /// SHA-256 digests of freshness notices whose text a sampling boundary in this
    /// rollout already persisted. Later boundaries reference them by digest.
    pub(super) persisted_freshness_notice_digests:
        std::sync::Mutex<std::collections::HashSet<String>>,
    /// Owns terminal coordinators so request cancellation cannot strand a claimed turn.
    pub(crate) terminal_tasks: tokio_util::task::TaskTracker,
    /// Writer retirement can outlive the loop's bounded shutdown observation.
    pub(crate) persistence_tasks: tokio_util::task::TaskTracker,
    pub(crate) persistence_retirement_result: std::sync::Mutex<Option<Result<(), String>>>,
    /// Mandatory result, read only after session-loop termination. A successful
    /// shutdown barrier covers the already accepted terminal record as well.
    pub(crate) persistence_shutdown_result: std::sync::Mutex<Option<Result<(), String>>>,
    /// Only another real durable barrier may clear a failed checkpoint.
    pub(crate) rollout_durability_error: tokio::sync::Mutex<Option<String>>,
    /// Admission stays closed while the prior turn is between terminal claim and the
    /// interaction-released milestone, even if its raw active-turn slot is being detached.
    pub(crate) terminal_interaction_pending: std::sync::atomic::AtomicBool,
    /// Prevents terminal cleanup from waking queued work after shutdown begins.
    pub(crate) shutting_down: std::sync::atomic::AtomicBool,
    /// Whether any attached consumer asked for `RawResponseItem` events.
    ///
    /// Raw events mirror every recorded history item, so producing them clones each tool
    /// output and spends a bounded event-channel slot. Consumers opt in per thread and the
    /// flag is monotonic for the thread's lifetime, matching the app-server's own
    /// `experimental_raw_events` semantics.
    pub(crate) raw_response_items_requested: std::sync::atomic::AtomicBool,
    /// Internal history consumers need accepted-input provenance, not every
    /// potentially large tool result. This opt-in is also lifetime-monotonic.
    pub(crate) raw_user_turn_boundaries_requested: std::sync::atomic::AtomicBool,
    pub(crate) input_queue: Arc<InputQueue>,
    pub(crate) services: SessionServices,
    pub(super) next_internal_sub_id: AtomicU64,
}

#[derive(Default)]
pub(crate) struct TaskStartState {
    pub(crate) pending_start_only_admissions: HashMap<String, oneshot::Sender<CodexResult<()>>>,
}

#[derive(Clone)]
pub(crate) struct SessionConfiguration {
    /// Provider identifier ("openai", "openrouter", ...).
    pub(super) provider: ModelProviderInfo,

    pub(super) collaboration_mode: CollaborationMode,
    pub(super) model_reasoning_summary: Option<ReasoningSummaryConfig>,
    pub(super) service_tier: Option<String>,

    /// Developer instructions that supplement the base instructions.
    pub(super) developer_instructions: Option<String>,

    /// Personality preference for the model.
    pub(super) personality: Option<Personality>,

    /// Base instructions for the session.
    pub(super) base_instructions: String,

    /// Compact prompt override.
    pub(super) compact_prompt: Option<String>,

    /// When to escalate for approval for execution
    pub(super) approval_policy: Constrained<AskForApproval>,
    /// Permission profile state for the session. Keep the constrained profile,
    /// active profile id, and profile-defined workspace roots in sync by using
    /// the methods below instead of mutating the fields independently.
    pub(super) permission_profile_state: PermissionProfileState,
    pub(super) windows_sandbox_level: WindowsSandboxLevel,

    /// Sticky thread-level environment selections plus the legacy cwd used
    /// when a turn does not select an environment.
    pub(super) environments: TurnEnvironmentSelections,
    /// Thread-scoped runtime workspace roots for materializing symbolic
    /// workspace permissions at session runtime.
    pub(super) workspace_roots: Vec<AbsolutePathBuf>,
    /// Directory containing all Codex state for this session.
    pub(super) codex_home: AbsolutePathBuf,
    /// Optional user-facing name for the thread, updated during the session.
    pub(super) thread_name: Option<String>,

    // TODO(pakrym): Remove config from here
    pub(super) original_config_do_not_use: Arc<Config>,
    /// Optional service name tag for session metrics.
    pub(super) metrics_service_name: Option<String>,
    pub(super) app_server_client_name: Option<String>,
    pub(super) app_server_client_version: Option<String>,
    /// Source of the session (cli, vscode, exec, mcp, ...)
    pub(super) session_source: SessionSource,
    /// Persisted thread history contract selected when this thread was created.
    pub(super) history_mode: ThreadHistoryMode,
    /// Immediate history source copied into this thread, when this thread was forked.
    pub(super) forked_from_thread_id: Option<ThreadId>,
    /// Immediate control/spawn parent for this thread, when it has one.
    pub(super) parent_thread_id: Option<ThreadId>,
    /// Optional analytics source classification for this thread.
    pub(super) thread_source: Option<ThreadSource>,
    /// Effective originator used for this thread's Responses requests and analytics events.
    pub(super) originator: String,
    pub(super) dynamic_tools: Vec<DynamicToolSpec>,
    pub(super) user_shell_override: Option<shell::Shell>,
}

impl SessionConfiguration {
    pub(super) fn cwd(&self) -> &AbsolutePathBuf {
        &self.environments.legacy_fallback_cwd
    }

    pub(super) fn environment_selections(&self) -> &[TurnEnvironmentSelection] {
        &self.environments.environments
    }

    pub(crate) fn codex_home(&self) -> &AbsolutePathBuf {
        &self.codex_home
    }

    pub(super) fn permission_profile_state(&self) -> &PermissionProfileState {
        &self.permission_profile_state
    }

    pub(super) fn permission_profile(&self) -> PermissionProfile {
        self.permission_profile_state
            .permission_profile()
            .clone()
            .materialize_project_roots_with_workspace_roots(&self.workspace_roots)
    }

    pub(super) fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.permission_profile_state.active_permission_profile()
    }

    pub(super) fn profile_workspace_roots(&self) -> &[AbsolutePathBuf] {
        self.permission_profile_state.profile_workspace_roots()
    }

    pub(super) fn apply_permission_profile_to_permissions(
        &self,
        permissions: &mut crate::config::Permissions,
    ) {
        permissions.set_permission_profile_state(self.permission_profile_state.clone());
    }

    #[cfg(test)]
    pub(super) fn set_permission_profile_for_tests(
        &mut self,
        permission_profile: PermissionProfile,
    ) -> ConstraintResult<()> {
        self.permission_profile_state
            .set_legacy_permission_profile(permission_profile)
    }

    pub(super) fn sandbox_policy(&self) -> SandboxPolicy {
        let permission_profile = self.permission_profile();
        codex_sandboxing::compatibility_sandbox_policy_for_permission_profile(
            &permission_profile,
            self.cwd(),
        )
    }

    pub(super) fn file_system_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        self.permission_profile().file_system_sandbox_policy()
    }

    pub(super) fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        self.permission_profile_state
            .permission_profile()
            .network_sandbox_policy()
    }

    pub(super) fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        ThreadConfigSnapshot {
            model: self.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            service_tier: self.service_tier.clone(),
            developer_instructions: self.developer_instructions.clone(),
            approval_policy: self.approval_policy.value(),
            permission_profile: self.permission_profile(),
            active_permission_profile: self.active_permission_profile(),
            windows_sandbox_level: self.windows_sandbox_level,
            environments: self.environments.clone(),
            workspace_roots: self.workspace_roots.clone(),
            profile_workspace_roots: self.profile_workspace_roots().to_vec(),
            ephemeral: self.original_config_do_not_use.ephemeral,
            reasoning_effort: self.collaboration_mode.reasoning_effort(),
            reasoning_summary: self.model_reasoning_summary,
            personality: self.personality,
            collaboration_mode: self.collaboration_mode.clone(),
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            forked_from_thread_id: self.forked_from_thread_id,
            parent_thread_id: self.parent_thread_id,
            thread_source: self.thread_source.clone(),
            originator: self.originator.clone(),
        }
    }

    pub(crate) async fn apply_async(
        &self,
        updates: &SessionSettingsUpdate,
    ) -> ConstraintResult<Self> {
        let configuration = self.clone();
        let updates = updates.clone();
        tokio::task::spawn_blocking(move || configuration.apply(&updates))
            .await
            .map_err(|error| codex_config::ConstraintError::UpdateRejected {
                reason: format!("session settings projection worker failed: {error}"),
            })?
    }

    pub(crate) fn apply(&self, updates: &SessionSettingsUpdate) -> ConstraintResult<Self> {
        let mut next_configuration = self.clone();
        let current_sandbox_policy = self.sandbox_policy();
        let current_file_system_sandbox_policy = self.file_system_sandbox_policy();
        let current_network_sandbox_policy = self.network_sandbox_policy();
        let legacy_file_system_projection =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                &current_sandbox_policy,
                self.cwd(),
                &current_file_system_sandbox_policy,
            );
        let file_system_policy_matches_legacy = current_file_system_sandbox_policy
            .is_semantically_equivalent_to(&legacy_file_system_projection, self.cwd());
        let file_system_policy_has_rebindable_project_root_write =
            current_file_system_sandbox_policy
                .entries
                .iter()
                .any(|entry| {
                    entry.access.can_write()
                        && matches!(
                            &entry.path,
                            FileSystemPath::Special {
                                value: FileSystemSpecialPath::ProjectRoots { subpath: None },
                            }
                        )
                });
        if let Some(collaboration_mode) = updates.collaboration_mode.clone() {
            next_configuration.collaboration_mode = collaboration_mode;
        }
        if let Some(summary) = updates.reasoning_summary {
            next_configuration.model_reasoning_summary = Some(summary);
        }
        if let Some(service_tier) = updates.service_tier.clone() {
            next_configuration.service_tier = match service_tier {
                Some(service_tier) => Some(
                    ServiceTier::from_request_value(&service_tier)
                        .map_or(service_tier, |service_tier| {
                            service_tier.request_value().to_string()
                        }),
                ),
                None => Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string()),
            };
        }
        if let Some(personality) = updates.personality {
            next_configuration.personality = Some(personality);
        }
        if let Some(approval_policy) = updates.approval_policy {
            next_configuration.approval_policy.set(approval_policy)?;
        }

        if let Some(windows_sandbox_level) = updates.windows_sandbox_level {
            next_configuration.windows_sandbox_level = windows_sandbox_level;
        }

        let current_cwd = self.cwd().clone();
        let next_environments = updates
            .environments
            .clone()
            .unwrap_or_else(|| self.environments.clone());
        let cwd_changed = next_environments.legacy_fallback_cwd.as_path() != current_cwd.as_path();
        next_configuration.environments = next_environments;
        if let Some(workspace_roots) = updates.workspace_roots.clone() {
            next_configuration.workspace_roots = workspace_roots;
        } else if cwd_changed && self.workspace_roots.contains(&current_cwd) {
            let mut retargeted_workspace_roots =
                Vec::with_capacity(next_configuration.workspace_roots.len());
            for root in &self.workspace_roots {
                let root = if root == &current_cwd {
                    next_configuration.cwd().clone()
                } else {
                    root.clone()
                };
                if !retargeted_workspace_roots.contains(&root) {
                    retargeted_workspace_roots.push(root);
                }
            }
            next_configuration.workspace_roots = retargeted_workspace_roots;
        }

        if let Some(permission_profile) = updates.permission_profile.clone() {
            let active_permission_profile =
                updates.active_permission_profile.clone().or_else(|| {
                    if permission_profile == self.permission_profile() {
                        self.active_permission_profile()
                    } else {
                        None
                    }
                });
            let profile_workspace_roots =
                updates.profile_workspace_roots.clone().unwrap_or_else(|| {
                    if permission_profile == self.permission_profile()
                        && active_permission_profile == self.active_permission_profile()
                    {
                        self.profile_workspace_roots().to_vec()
                    } else {
                        Vec::new()
                    }
                });
            next_configuration.set_permission_profile_projection(
                permission_profile,
                active_permission_profile,
                profile_workspace_roots,
                Some(&current_file_system_sandbox_policy),
            )?;
            if let Some(active_permission_profile) = next_configuration.active_permission_profile()
            {
                let mut config = (*next_configuration.original_config_do_not_use).clone();
                let permission_profile = next_configuration.permission_profile();
                config.permissions.network = config
                    .network_proxy_spec_for_active_permission_profile(
                        &active_permission_profile,
                        &permission_profile,
                    )
                    .map_err(|err| ConstraintError::InvalidValue {
                        field_name: "default_permissions",
                        candidate: active_permission_profile.id.clone(),
                        allowed: format!(
                            "configured permission profile with valid network policy ({err})"
                        ),
                        requirement_source: codex_config::RequirementSource::Unknown,
                    })?;
                config
                    .permissions
                    .set_permission_profile_from_session_snapshot(
                        PermissionProfileSnapshot::active(
                            permission_profile,
                            active_permission_profile,
                        ),
                    )?;
                next_configuration.original_config_do_not_use = Arc::new(config);
            }
        } else if let Some(sandbox_policy) = updates.sandbox_policy.clone() {
            let file_system_sandbox_policy =
                FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                    &sandbox_policy,
                    next_configuration.cwd(),
                    &current_file_system_sandbox_policy,
                );
            let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
            next_configuration
                .permission_profile_state
                .set_legacy_permission_profile(
                    PermissionProfile::from_runtime_permissions_with_enforcement(
                        SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
                        &file_system_sandbox_policy,
                        network_sandbox_policy,
                    ),
                )?;
        } else if cwd_changed
            && file_system_policy_matches_legacy
            && file_system_policy_has_rebindable_project_root_write
        {
            // Preserve richer split policies across cwd-only updates; only
            // rederive when the session is already using a structurally
            // cwd-bound legacy bridge.
            let file_system_sandbox_policy =
                FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                    &current_sandbox_policy,
                    next_configuration.cwd(),
                    &current_file_system_sandbox_policy,
                );
            next_configuration
                .permission_profile_state
                .set_legacy_permission_profile(
                    PermissionProfile::from_runtime_permissions_with_enforcement(
                        SandboxEnforcement::from_legacy_sandbox_policy(&current_sandbox_policy),
                        &file_system_sandbox_policy,
                        current_network_sandbox_policy,
                    ),
                )?;
        }
        if let Some(app_server_client_name) = updates.app_server_client_name.clone() {
            next_configuration.app_server_client_name = Some(app_server_client_name);
        }
        if let Some(app_server_client_version) = updates.app_server_client_version.clone() {
            next_configuration.app_server_client_version = Some(app_server_client_version);
        }
        Ok(next_configuration)
    }

    fn set_permission_profile_projection(
        &mut self,
        permission_profile: PermissionProfile,
        active_permission_profile: Option<ActivePermissionProfile>,
        profile_workspace_roots: Vec<AbsolutePathBuf>,
        preserve_deny_reads_from: Option<&FileSystemSandboxPolicy>,
    ) -> ConstraintResult<()> {
        let enforcement = permission_profile.enforcement();
        let (mut file_system_sandbox_policy, network_sandbox_policy) =
            permission_profile.to_runtime_permissions();
        if let Some(existing_file_system_policy) = preserve_deny_reads_from {
            file_system_sandbox_policy
                .preserve_deny_read_restrictions_from(existing_file_system_policy);
        }
        let effective_permission_profile =
            PermissionProfile::from_runtime_permissions_with_enforcement(
                enforcement,
                &file_system_sandbox_policy,
                network_sandbox_policy,
            );

        let permission_snapshot = match active_permission_profile {
            Some(active_permission_profile) => {
                PermissionProfileSnapshot::active_with_profile_workspace_roots(
                    effective_permission_profile,
                    active_permission_profile,
                    profile_workspace_roots,
                )
            }
            None => PermissionProfileSnapshot::legacy(effective_permission_profile),
        };

        self.permission_profile_state
            .set_permission_profile_snapshot(permission_snapshot)
    }
}

#[derive(Default, Clone)]
pub(crate) struct SessionSettingsUpdate {
    pub(crate) environments: Option<TurnEnvironmentSelections>,
    pub(crate) workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub(crate) profile_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) permission_profile: Option<PermissionProfile>,
    pub(crate) active_permission_profile: Option<ActivePermissionProfile>,
    pub(crate) windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub(crate) collaboration_mode: Option<CollaborationMode>,
    pub(crate) reasoning_summary: Option<ReasoningSummaryConfig>,
    pub(crate) service_tier: Option<Option<String>>,
    pub(crate) personality: Option<Personality>,
    pub(crate) app_server_client_name: Option<String>,
    pub(crate) app_server_client_version: Option<String>,
}

impl SessionSettingsUpdate {
    pub(crate) fn is_empty(&self) -> bool {
        self.environments.is_none()
            && self.workspace_roots.is_none()
            && self.profile_workspace_roots.is_none()
            && self.approval_policy.is_none()
            && self.sandbox_policy.is_none()
            && self.permission_profile.is_none()
            && self.active_permission_profile.is_none()
            && self.windows_sandbox_level.is_none()
            && self.collaboration_mode.is_none()
            && self.reasoning_summary.is_none()
            && self.service_tier.is_none()
            && self.personality.is_none()
            && self.app_server_client_name.is_none()
            && self.app_server_client_version.is_none()
    }
}

/// Owns cancellation outside the unpublished session/task Arc cycle. Dropping
/// construction leaves one explicit cleanup task, never unowned speculation.
#[derive(Default)]
struct SessionRuntimeInitGuard(Option<Arc<Session>>);

impl SessionRuntimeInitGuard {
    fn cleanup(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let session = self.0.take()?;
        session.startup_cancellation.cancel();
        session
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        session.services.turn_environments.shutdown();
        Some(tokio::spawn(async move {
            super::handlers::shutdown_unpublished_session(&session).await;
        }))
    }

    async fn rollback(&mut self) {
        if let Some(mut cleanup) = self.cleanup() {
            // The task remains the owner if observation expires; dropping the
            // JoinHandle must not cancel accepted storage or host cleanup.
            match tokio::time::timeout(std::time::Duration::from_secs(10), &mut cleanup).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "unpublished session cleanup task failed"),
                Err(_) => tracing::warn!("unpublished session cleanup remains pending"),
            }
        }
    }
}

impl Drop for SessionRuntimeInitGuard {
    fn drop(&mut self) {
        drop(self.cleanup());
    }
}

#[cfg(test)]
mod initialization_ownership_tests {
    use super::*;

    #[tokio::test]
    async fn abandoned_constructor_breaks_a_pending_prewarm_session_cycle() {
        struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let (session, _) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let weak = Arc::downgrade(&session);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let retained = Arc::clone(&session);
        let task = tokio::spawn(async move {
            let _session = retained;
            let _drop = OnDrop(Some(dropped_tx));
            let _ = entered_tx.send(());
            futures::future::pending::<CodexResult<Option<crate::client::ModelClientSession>>>()
                .await
        });
        session
            .set_session_startup_prewarm(
                crate::session_startup_prewarm::SessionStartupPrewarmHandle::new(
                    task,
                    std::time::Instant::now(),
                ),
            )
            .await;
        entered_rx.await.unwrap();
        let cancellation = session.startup_cancellation.clone();
        let guard = SessionRuntimeInitGuard(Some(Arc::clone(&session)));
        drop(session);
        drop(guard);
        assert!(cancellation.is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup must release the unpublished session");
    }
}

pub(crate) struct AppServerClientMetadata {
    pub(crate) client_name: Option<String>,
    pub(crate) client_version: Option<String>,
}

async fn warm_plugins_and_skills_for_session_init(
    config: Arc<Config>,
    plugins_manager: Arc<PluginsManager>,
    skills_service: Arc<SkillsService>,
    turn_environments: &TurnEnvironmentSnapshot,
) -> Vec<SkillError> {
    let fs = turn_environments.primary_filesystem();
    let plugins_input = config.plugins_config_input();
    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
    let effective_skill_roots = plugin_outcome.effective_plugin_skill_roots();
    let plugin_skill_snapshots = plugins_manager.plugin_skill_snapshots_for_config(&plugins_input);
    let skills_input = skills_load_input_from_config(config.as_ref(), effective_skill_roots)
        .with_plugin_skill_snapshots(plugin_skill_snapshots);
    skills_service
        .snapshot_for_config(&skills_input, fs)
        .await
        .outcome()
        .errors
        .clone()
}

impl Session {
    /// Returns the concrete identity for this thread.
    pub(crate) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// Returns the identity shared by the root thread and all descendant threads.
    pub(crate) fn session_id(&self) -> SessionId {
        self.services.agent_control.session_id()
    }

    pub(crate) async fn originator(&self) -> String {
        let state = self.state.lock().await;
        state.session_configuration.originator.clone()
    }

    /// The service tier as configured now, including settings updates applied
    /// after the current turn was constructed.
    pub(crate) async fn configured_service_tier(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .session_configuration
            .service_tier
            .clone()
    }

    #[instrument(name = "session_init", level = "info", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        mut session_configuration: SessionConfiguration,
        session_reasoning_effort: Option<ReasoningEffortConfig>,
        config: Arc<Config>,
        user_instructions: Option<codex_extension_api::UserInstructions>,
        installation_id: String,
        auth_manager: Arc<AuthManager>,
        models_manager: SharedModelsManager,
        exec_policy: Arc<ExecPolicyManager>,
        tx_event: Sender<Event>,
        agent_status: watch::Sender<AgentStatus>,
        initial_history: InitialHistory,
        session_source: SessionSource,
        skills_service: Arc<SkillsService>,
        plugins_manager: Arc<PluginsManager>,
        mcp_manager: Arc<McpManager>,
        code_mode_session_provider: Arc<dyn codex_code_mode::CodeModeSessionProvider>,
        extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
        mut thread_extension_init: ExtensionDataInit,
        supports_openai_form_elicitation: bool,
        agent_control: AgentControl,
        environment_manager: Arc<EnvironmentManager>,
        inherited_environments: Option<TurnEnvironmentSnapshot>,
        analytics_events_client: Option<AnalyticsEventsClient>,
        thread_store: Arc<dyn ThreadStore>,
        parent_rollout_thread_trace: ThreadTraceContext,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
        external_time_provider: Option<Arc<dyn TimeProvider>>,
        multi_agent_version: Option<MultiAgentVersion>,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            "Configuring session: model={}; provider={:?}",
            session_configuration.collaboration_mode.model(),
            session_configuration.provider
        );
        let forked_from_id = session_configuration
            .forked_from_thread_id
            .or_else(|| initial_history.forked_from_id());
        session_configuration.forked_from_thread_id = forked_from_id;
        let tool_history_fork_source = matches!(&initial_history, InitialHistory::Forked(_))
            .then_some(forked_from_id)
            .flatten();
        let restores_initial_tool_history = matches!(
            &initial_history,
            InitialHistory::Resumed(_) | InitialHistory::Forked(_)
        );
        let parent_thread_id = session_configuration
            .parent_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id());
        session_configuration.parent_thread_id = parent_thread_id;
        let multi_agent_version = multi_agent_version.map(OnceLock::from).unwrap_or_default();
        let initial_multi_agent_version = multi_agent_version.get().copied();

        let thread_id = match &initial_history {
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                ThreadId::default()
            }
            InitialHistory::Resumed(resumed_history) => resumed_history.conversation_id,
        };
        // One live canonical ledger owner, matching local rollout writer
        // admission. This also fences mutating startup recovery before loading.
        let storage_ownership =
            acquire_session_storage_ownership(config.codex_home.as_path(), thread_id).await?;
        let rollback_created_persistence_on_error = !config.ephemeral
            && matches!(
                &initial_history,
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_)
            );
        let startup_timing = StartupTimingState::new(thread_id.to_string());
        let resumed_session_id = match &initial_history {
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.session_id),
                    _ => None,
                })
            }
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        // Legacy subagent rollouts synthesize session_id from their own thread id.
        let resumed_session_id = resumed_session_id.filter(|session_id| {
            !session_configuration.session_source.is_non_root_agent()
                || *session_id != SessionId::from(thread_id)
        });
        let session_id = resumed_session_id.unwrap_or_else(|| {
            if session_configuration.session_source.is_non_root_agent() {
                agent_control.session_id()
            } else {
                SessionId::from(thread_id)
            }
        });
        let initial_auto_compact_window_ids = AutoCompactWindowIds::new_initial();
        let agent_control = agent_control.with_session_id(
            session_id,
            config
                .effective_agent_max_threads(MultiAgentVersion::V2)
                .unwrap_or(usize::MAX),
        );
        let time_provider = crate::current_time::resolve_time_provider(
            config.current_time_reminder.as_ref(),
            external_time_provider,
        )?;
        let selected_capability_roots =
            match thread_extension_init.get::<Vec<SelectedCapabilityRoot>>() {
                Some(roots) => roots.as_ref().clone(),
                None => {
                    let roots = initial_history.get_selected_capability_roots();
                    if !roots.is_empty() {
                        thread_extension_init.insert(roots.clone());
                    }
                    roots
                }
            };
        let mcp_thread_init = thread_extension_init.clone();
        let thread_extension_data = codex_extension_api::ExtensionData::new_with_init(
            thread_id.to_string(),
            thread_extension_init,
        );
        // Kick off independent async setup tasks in parallel to reduce startup latency.
        //
        // - initialize thread persistence with new or resumed session info
        // Host shell discovery is lazy: remote-only/empty selections do not need it.
        // - load history metadata (skipped for subagents)
        #[cfg(test)]
        let (persistence_ready, startup_release) = {
            let mut hook = SESSION_INIT_PERSISTENCE_TEST_HOOK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if hook
                .as_ref()
                .is_some_and(|hook| hook.codex_home == config.codex_home.as_path())
            {
                let hook = hook.take().expect("matching startup hook");
                (Some(hook.ready), Some(hook.release))
            } else {
                (None, None)
            }
        };
        let thread_persistence_fut = async {
            if config.ephemeral {
                Ok::<_, anyhow::Error>(LiveThreadInitGuard::new(None))
            } else {
                let live_thread = match &initial_history {
                    InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                        let params = CreateThreadParams {
                            session_id,
                            thread_id,
                            extra_config: config.extra_config.clone(),
                            forked_from_id,
                            parent_thread_id,
                            source: session_source,
                            thread_source: session_configuration.thread_source.clone(),
                            originator: session_configuration.originator.clone(),
                            base_instructions: BaseInstructions {
                                text: session_configuration.base_instructions.clone(),
                            },
                            dynamic_tools: session_configuration.dynamic_tools.clone(),
                            selected_capability_roots: selected_capability_roots.clone(),
                            multi_agent_version: initial_multi_agent_version,
                            history_mode: session_configuration.history_mode,
                            initial_window_id: initial_auto_compact_window_ids
                                .window_id
                                .to_string(),
                            metadata: ThreadPersistenceMetadata {
                                cwd: Some(config.cwd.to_path_buf()),
                                model_provider: config.model_provider_id.clone(),
                            },
                        };
                        LiveThread::create(Arc::clone(&thread_store), params).await?
                    }
                    InitialHistory::Resumed(resumed_history) => {
                        let params = ResumeThreadParams {
                            thread_id: resumed_history.conversation_id,
                            rollout_path: resumed_history.rollout_path.clone(),
                            history: Some(resumed_history.history.clone()),
                            include_archived: true,
                            metadata: ThreadPersistenceMetadata {
                                cwd: Some(config.cwd.to_path_buf()),
                                model_provider: config.model_provider_id.clone(),
                            },
                        };
                        LiveThread::resume(
                            Arc::clone(&thread_store),
                            session_configuration.history_mode,
                            params,
                        )
                        .await?
                    }
                };
                // The other joined branches may still be pending when this one
                // completes. Keep persistence guarded while join! owns its result.
                let guarded = LiveThreadInitGuard::new(Some(live_thread));
                #[cfg(test)]
                if let Some(ready) = persistence_ready {
                    let _ = ready.send(thread_id);
                }
                Ok(guarded)
            }
        }
        .instrument(info_span!(
            "session_init.thread_persistence",
            otel.name = "session_init.thread_persistence",
            session_init.ephemeral = config.ephemeral,
        ));
        let state_db_fut = async {
            if config.ephemeral {
                None
            } else if let Some(local_store) =
                thread_store.as_any().downcast_ref::<LocalThreadStore>()
            {
                local_store.state_db().await
            } else {
                None
            }
        }
        .instrument(info_span!(
            "session_init.state_db",
            otel.name = "session_init.state_db",
            session_init.ephemeral = config.ephemeral,
        ));

        let default_shell =
            shell::LocalShell::new(session_configuration.user_shell_override.clone());
        let tool_history_fut = async {
            if !config.completed_tool_history_projection || !restores_initial_tool_history {
                return None;
            }
            Some(if let Some(source_thread_id) = tool_history_fork_source {
                crate::tool_history::load_tool_history_state_for_fork(
                    config.codex_home.as_path(),
                    &source_thread_id.to_string(),
                )
                .await
            } else {
                crate::tool_history::load_tool_history_state_for_initialization(
                    config.codex_home.as_path(),
                    &thread_id.to_string(),
                )
                .await
            })
        };

        let auth_manager_clone = Arc::clone(&auth_manager);
        let config_for_mcp = Arc::clone(&config);
        let mcp_manager_for_mcp = Arc::clone(&mcp_manager);
        let mcp_thread_init_for_startup = &mcp_thread_init;
        let thread_extension_data_for_mcp = &thread_extension_data;
        let mcp_originator = session_configuration.originator.clone();
        let mcp_runtime_cwd = session_configuration
            .environment_selections()
            .iter()
            .find(|selection| {
                environment_manager
                    .get_environment(&selection.environment_id)
                    .is_some_and(|environment| !environment.is_remote())
            })
            .and_then(|environment| environment.cwd.to_abs_path().ok())
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| config.codex_home.to_path_buf())
            });
        let mcp_runtime_context =
            McpRuntimeContext::new(Arc::clone(&environment_manager), mcp_runtime_cwd);
        let mcp_runtime_context_for_auth = mcp_runtime_context.clone();
        let auth_and_mcp_fut = async move {
            #[cfg(test)]
            if let Some(release) = startup_release {
                let _ = release.await;
            }
            let auth = auth_manager_clone.auth().await;
            let mcp_projection = mcp_manager_for_mcp
                .runtime_config_for_step(
                    &config_for_mcp,
                    mcp_thread_init_for_startup,
                    thread_extension_data_for_mcp,
                    &mcp_originator,
                    /*available_environment_ids*/ &[],
                )
                .await;
            let mcp_config = &mcp_projection.config;
            let mcp_servers = codex_mcp::effective_mcp_servers(mcp_config, auth.as_ref());
            let tool_plugin_provenance = codex_mcp::tool_plugin_provenance(mcp_config);
            let auth_statuses = codex_mcp::compute_cached_auth_statuses(
                mcp_servers.iter(),
                &config_for_mcp.codex_home,
                config_for_mcp.mcp_oauth_credentials_store_mode,
                config_for_mcp.auth_keyring_backend_kind(),
                auth.as_ref(),
                &mcp_runtime_context_for_auth,
            )
            .await;
            (
                auth,
                mcp_projection,
                mcp_servers,
                auth_statuses,
                tool_plugin_provenance,
            )
        }
        .instrument(info_span!(
            "session_init.auth_mcp",
            otel.name = "session_init.auth_mcp",
        ));

        // Ledger loading can checkpoint/quarantine during recovery. It is not
        // independent of target persistence: acquire the existing writer lease
        // first, retaining its initialization guard through history installation.
        let persistence_and_history_fut = async {
            let persistence = thread_persistence_fut.await;
            let history = if persistence.is_ok() {
                tool_history_fut.await
            } else {
                None
            };
            (persistence, history)
        };
        let (
            (thread_persistence_result, initial_tool_history),
            state_db_ctx,
            (auth, mcp_projection, mcp_servers, auth_statuses, tool_plugin_provenance),
            config_validation,
        ) = tokio::join!(
            persistence_and_history_fut,
            state_db_fut,
            auth_and_mcp_fut,
            validate_config_lock_if_configured(&session_configuration),
        );

        let mut live_thread_init = thread_persistence_result.map_err(|e| {
            error!("failed to initialize thread persistence: {e:#}");
            e
        })?;
        if let Some(state_runtime) = state_db_ctx.as_ref() {
            // One shared owner performs historical recovery/export off session
            // startup. New jobs are fenced by their current runner identity.
            crate::tools::handlers::agent_jobs::start_orphaned_agent_job_reconciler(Arc::clone(
                state_runtime,
            ));
        }
        let mut runtime_init = SessionRuntimeInitGuard::default();
        let session_result: anyhow::Result<Arc<Self>> = async {
            let mut coordination_ready = true;
            if initial_multi_agent_version == Some(MultiAgentVersion::V2)
                && let Some(state_runtime) = state_db_ctx.as_ref()
            {
                coordination_ready = coordination_startup_result(
                    &session_configuration.session_source,
                    agent_control
                        .task_coordinator()
                        .initialize(Arc::clone(state_runtime), session_id.to_string())
                        .await,
                )?;
            }
            if config.features.enabled(Feature::Kd4Runtime) && coordination_ready {
                let coordinator = agent_control.task_coordinator();
                let validation_init: codex_agent_task_store::StoreResult<()> = async {
                    coordinator
                        .initialize_for_workspace_coordination(
                            state_db_ctx.clone(),
                            config.sqlite_home.clone(),
                            config.model_provider_id.clone(),
                            session_id.to_string(),
                        )
                        .await?;
                    let parent = parent_thread_id
                        .or(session_configuration.forked_from_thread_id)
                        .map(|id| id.to_string());
                    let store = coordinator.store().ok_or_else(|| {
                        codex_agent_task_store::StoreError::CorruptData(
                            "validation task store is unavailable".into(),
                        )
                    })?;
                    if let Some((task, grant)) =
                        agent_control.inherited_validation_task_scope.as_ref()
                    {
                        store
                            .inherit_validation_task_scope(&thread_id.to_string(), task, grant)
                            .await?;
                    } else {
                        store
                            .initialize_validation_task(
                                &thread_id.to_string(),
                                parent.as_deref(),
                                matches!(&initial_history, InitialHistory::New) && parent.is_none(),
                            )
                            .await?;
                    }
                    Ok(())
                }
                .await;
                coordination_startup_result(
                    &session_configuration.session_source,
                    validation_init,
                )?;
            }
            let rollout_path = if let Some(live_thread) = live_thread_init.as_ref() {
                live_thread.local_rollout_path().await?
            } else {
                None
            };
            if let Some(path) = rollout_path.as_ref() {
                crate::tools::command_output_artifact::retain_external_rollout_ownership(
                    config.codex_home.as_path(),
                    &thread_id.to_string(),
                    path,
                )
                .await?;
            }
            let trace_agent_path = session_configuration
                .session_source
                .get_agent_path()
                .unwrap_or_else(codex_protocol::AgentPath::root);
            let trace_task_name =
                (!trace_agent_path.is_root()).then(|| trace_agent_path.name().to_string());
            let trace_metadata = ThreadStartedTraceMetadata {
                thread_id: thread_id.to_string(),
                agent_path: trace_agent_path.to_string(),
                task_name: trace_task_name,
                nickname: session_configuration.session_source.get_nickname(),
                agent_role: session_configuration.session_source.get_agent_role(),
                session_source: session_configuration.session_source.clone(),
                cwd: session_configuration.cwd().to_path_buf(),
                rollout_path: rollout_path.clone(),
                model: session_configuration.collaboration_mode.model().to_string(),
                provider_name: config.model_provider_id.clone(),
                approval_policy: session_configuration.approval_policy.value().to_string(),
                sandbox_policy: format!("{:?}", session_configuration.sandbox_policy()),
            };
            let rollout_thread_trace = if matches!(
                session_configuration.session_source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
            ) {
                // Spawned child threads are part of their root rollout tree. If the
                // parent had no trace bundle, do not create an orphan child bundle
                // that looks like an independent rollout.
                if matches!(&initial_history, InitialHistory::Resumed(_)) {
                    parent_rollout_thread_trace
                        .resume_child_thread_trace_or_disabled(trace_metadata)
                } else {
                    parent_rollout_thread_trace.start_child_thread_trace_or_disabled(trace_metadata)
                }
            } else {
                ThreadTraceContext::start_root_or_disabled(trace_metadata)
            };

            let mut post_session_configured_events = Vec::<Event>::new();

            for usage in config.features.legacy_feature_usages() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::DeprecationNotice(
                        codex_protocol::protocol::DeprecationNoticeEvent {
                            summary: usage.summary.clone(),
                            details: usage.details.clone(),
                        },
                    ),
                });
            }
            for message in &config.startup_warnings {
                post_session_configured_events.push(Event {
                    id: "".to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: message.clone(),
                    }),
                });
            }
            let config_path = config.codex_home.join(CONFIG_TOML_FILE);
            // Attribute each flagged key to the layer that set it. A key
            // injected by a managed or bundled layer is not in the user's
            // config.toml, so advising them to edit that file cannot help.
            let feature_origins = config.config_layer_stack.origins();
            let user_config_file = config_path.display().to_string();
            if let Some(event) = codex_features::unknown_features_warning_event_with_origins(
                config
                    .config_layer_stack
                    .effective_config()
                    .get("features")
                    .and_then(TomlValue::as_table),
                &user_config_file,
                |key| {
                    let metadata = feature_origins.get(&format!("features.{key}"))?;
                    if matches!(metadata.name, codex_config::ConfigLayerSource::User { .. }) {
                        return None;
                    }
                    Some(codex_config::format_config_layer_source(
                        &metadata.name,
                        CONFIG_TOML_FILE,
                    ))
                },
            ) {
                post_session_configured_events.push(event);
            }
            if let Some(event) = unstable_features_warning_event(
                config
                    .config_layer_stack
                    .effective_config()
                    .get("features")
                    .and_then(TomlValue::as_table),
                config.suppress_unstable_features_warning,
                &config.features,
                &config_path.display().to_string(),
            ) {
                post_session_configured_events.push(event);
            }
            let auth = auth.as_ref();
            let auth_mode = auth.map(CodexAuth::auth_mode).map(TelemetryAuthMode::from);
            let account_id = auth.and_then(CodexAuth::get_account_id);
            let account_email = auth.and_then(CodexAuth::get_account_email);
            let originator = session_configuration.originator.clone();
            let terminal_type = user_agent();
            let session_model = session_configuration.collaboration_mode.model().to_string();
            let auth_env_telemetry = collect_auth_env_telemetry(
                &session_configuration.provider,
                auth_manager.codex_api_key_env_enabled(),
            );
            let mut session_telemetry = SessionTelemetry::new(
                thread_id,
                session_model.as_str(),
                session_model.as_str(),
                account_id.clone(),
                account_email.clone(),
                auth_mode,
                originator.clone(),
                config.otel.log_user_prompt,
                terminal_type.clone(),
                session_configuration.session_source.clone(),
            )
            .with_auth_env(auth_env_telemetry.to_otel_metadata());
            if let Some(service_name) = session_configuration.metrics_service_name.as_deref() {
                session_telemetry = session_telemetry.with_metrics_service_name(service_name);
            }
            let network_proxy_audit_metadata = NetworkProxyAuditMetadata {
                conversation_id: Some(thread_id.to_string()),
                app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                user_account_id: account_id,
                auth_mode: auth_mode.map(|mode| mode.to_string()),
                originator: Some(originator),
                user_email: account_email,
                terminal_type: Some(terminal_type),
                model: Some(session_model.clone()),
                slug: Some(session_model),
            };
            config.features.emit_metrics(&session_telemetry);
            session_telemetry.conversation_starts(
                config.model_provider.name.as_str(),
                session_configuration.collaboration_mode.reasoning_effort(),
                config
                    .model_reasoning_summary
                    .unwrap_or(ReasoningSummaryConfig::Auto),
                config.model_context_window,
                config.model_auto_compact_token_limit,
                config.permissions.approval_policy.value(),
                config
                    .permissions
                    .legacy_sandbox_policy(session_configuration.cwd().as_path()),
                mcp_servers.keys().map(String::as_str).collect(),
            );

            let shell_snapshot = if config.features.enabled(Feature::ShellSnapshot) {
                ShellSnapshot::new(
                    config.codex_home.clone(),
                    thread_id,
                    session_telemetry.clone(),
                    state_db_ctx.clone(),
                    create_env(
                        &config.permissions.shell_environment_policy,
                        Some(thread_id),
                    ),
                    config.permissions.shell_environment_policy.clone(),
                )
            } else {
                ShellSnapshot::disabled()
            };
            // Host shell discovery is lazy only so remote-only, empty and inherited
            // selections skip it. A newly selected local environment demands it:
            // finish discovery before that selection is first resolved, so the
            // environment attaches at creation (instruction sources, Git, skills)
            // instead of racing its background resolution. A discovery failure is
            // reported by that environment's own resolution.
            let inherited_selections = inherited_environments
                .as_ref()
                .map(TurnEnvironmentSnapshot::to_selections)
                .unwrap_or_default();
            if session_configuration
                .environment_selections()
                .iter()
                .any(|selection| {
                    !inherited_selections.contains(selection)
                        && environment_manager
                            .get_environment(&selection.environment_id)
                            .is_some_and(|environment| !environment.is_remote())
                })
            {
                let _ = default_shell.resolve().await;
            }
            let turn_environments = Arc::new(ThreadEnvironments::new(
                environment_manager,
                default_shell.clone(),
                shell_snapshot,
                inherited_environments.unwrap_or_default(),
                config.features.enabled(Feature::DeferredExecutor),
            ));
            turn_environments.update_selections(session_configuration.environment_selections());
            // `snapshot()` honors the DeferredExecutor policy configured above: normal
            // sessions still wait for their selected executor, while deferred sessions
            // retain a `starting` entry and finish initializing without blocking on it.
            let (resolved_environments, thread_name) = tokio::join!(
                turn_environments.snapshot_for_startup(Arc::clone(&startup_timing)),
                thread_title_from_thread_store(live_thread_init.as_ref(), &thread_store, thread_id)
                    .instrument(info_span!(
                        "session_init.thread_name_lookup",
                        otel.name = "session_init.thread_name_lookup",
                    )),
            );
            let git_workspace = GitWorkspaceCache::new();
            let initial_git_workspace = git_workspace
                .snapshot_with_project_discovery(
                    &resolved_environments,
                    config.config_layer_stack.project_discovery(),
                )
                .await;
            session_telemetry.counter(
                THREAD_STARTED_METRIC,
                /*inc*/ 1,
                &[(
                    "is_git",
                    if initial_git_workspace.primary_is_git() == Some(true) {
                        "true"
                    } else {
                        "false"
                    },
                )],
            );
            let agents_md_manager = Arc::new(
                AgentsMdManager::new(user_instructions).with_thread_id(thread_id.to_string()),
            );
            let plugin_skill_warmup = warm_plugins_and_skills_for_session_init(
                Arc::clone(&config),
                Arc::clone(&plugins_manager),
                Arc::clone(&skills_service),
                &resolved_environments,
            )
            .instrument(info_span!(
                "session_init.plugin_skill_warmup",
                otel.name = "session_init.plugin_skill_warmup",
            ));
            let ((), plugin_skill_errors) = tokio::join!(
                agents_md_manager.refresh(config.as_ref(), &resolved_environments),
                plugin_skill_warmup,
            );
            for err in &plugin_skill_errors {
                error!(
                    "failed to load skill {}: {}",
                    err.path.display(),
                    err.message
                );
            }
            session_configuration.thread_name = thread_name.clone();
            config_validation?;
            export_config_lock_if_configured(&session_configuration, thread_id).await?;
            let mut state = SessionState::new_with_auto_compact_window_ids(
                session_configuration.clone(),
                initial_auto_compact_window_ids,
            );
            if let Some(tool_history) = initial_tool_history {
                let (tool_history, ledger_warning) = tool_history.into_state_and_warning();
                if let Some(message) = ledger_warning {
                    tracing::warn!("{message}");
                    post_session_configured_events.push(Event {
                        id: INITIAL_SUBMIT_ID.to_owned(),
                        msg: EventMsg::Warning(WarningEvent { message }),
                    });
                }
                state.set_tool_history_state(tool_history);
            }
            if thread_store
                .as_any()
                .downcast_ref::<LocalThreadStore>()
                .is_some()
            {
                // Local rollouts decide whether another thread can still read
                // its retained artifacts.
                crate::tools::command_output_artifact::spawn_unresumable_thread_artifact_reclaim(
                    config.codex_home.to_path_buf(),
                    thread_id.to_string(),
                );
            }
            crate::tools::command_output_artifact::spawn_retention_index_prewarm(
                config.codex_home.as_path(),
            );
            let managed_network_requirements_configured = config
                .config_layer_stack
                .requirements_toml()
                .network
                .is_some();
            let managed_network_requirements_enabled =
                config.managed_network_requirements_enabled();
            let network_approval = Arc::new(NetworkApprovalService::default());
            // The managed proxy can call back into core for allowlist-miss decisions.
            let network_policy_decider_session = if managed_network_requirements_configured {
                config
                    .permissions
                    .network
                    .as_ref()
                    .map(|_| Arc::new(RwLock::new(std::sync::Weak::<Session>::new())))
            } else {
                None
            };
            let blocked_request_observer = if managed_network_requirements_configured {
                config
                    .permissions
                    .network
                    .as_ref()
                    .map(|_| build_blocked_request_observer(Arc::clone(&network_approval)))
            } else {
                None
            };
            let network_policy_decider =
                network_policy_decider_session
                    .as_ref()
                    .map(|network_policy_decider_session| {
                        build_network_policy_decider(
                            Arc::clone(&network_approval),
                            Arc::clone(network_policy_decider_session),
                        )
                    });
            let (network_proxy, session_network_proxy) =
                if let Some(spec) = config.permissions.network.as_ref() {
                    let current_exec_policy = exec_policy.current();
                    let (network_proxy, session_network_proxy) = Self::start_managed_network_proxy(
                        spec,
                        config.codex_home.as_path(),
                        current_exec_policy.as_ref(),
                        config.permissions.permission_profile(),
                        network_policy_decider.as_ref().map(Arc::clone),
                        blocked_request_observer.as_ref().map(Arc::clone),
                        managed_network_requirements_configured,
                        network_proxy_audit_metadata.clone(),
                    )
                    .instrument(info_span!(
                        "session_init.network_proxy",
                        otel.name = "session_init.network_proxy",
                        session_init.managed_network_requirements_enabled =
                            managed_network_requirements_enabled,
                    ))
                    .await?;
                    (Some(network_proxy), Some(session_network_proxy))
                } else {
                    (None, None)
                };

            let plugin_outcome = plugins_manager
                .plugins_for_config(&config.plugins_config_input())
                .await;
            for warning in plugin_outcome.load_warnings() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: warning.clone(),
                    }),
                });
            }
            let hooks = build_hooks_for_config(
                &config,
                plugins_manager.as_ref(),
                resolved_environments.single_local_environment(),
            )
            .await;
            for warning in hooks.startup_warnings() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: warning.clone(),
                    }),
                });
            }

            let analytics_events_client = analytics_events_client.unwrap_or_else(|| {
                AnalyticsEventsClient::new(
                    Arc::clone(&auth_manager),
                    config.chatgpt_base_url.clone(),
                    config.analytics_enabled,
                    config.http_client_factory(),
                )
            });
            let mcp_runtime: Arc<arc_swap::ArcSwapOption<McpRuntimeSnapshot>> =
                Arc::new(arc_swap::ArcSwapOption::empty());
            let resource_mcp_runtime = Arc::clone(&mcp_runtime);
            let session_extension_data =
                codex_extension_api::ExtensionData::new(session_id.to_string());
            session_extension_data.insert(McpResourceClient::new(move || {
                resource_mcp_runtime.load_full().map(|runtime| {
                    let manager = runtime.manager_arc();
                    (runtime, manager)
                })
            }));
            for contributor in extensions.thread_lifecycle_contributors() {
                contributor
                    .on_thread_start(codex_extension_api::ThreadStartInput {
                        config: config.as_ref(),
                        session_source: &session_configuration.session_source,
                        persistent_thread_state_available: state_db_ctx.is_some(),
                        environments: session_configuration.environment_selections(),
                        session_store: &session_extension_data,
                        thread_store: &thread_extension_data,
                    })
                    .await;
            }
            let unified_exec_manager =
                UnifiedExecProcessManager::new(config.background_terminal_max_timeout);
            let command_execution =
                crate::tools::command_execution::CommandExecutionLedger::load_or_new(
                    config.codex_home.to_path_buf(),
                    thread_id.to_string(),
                    session_configuration.cwd().as_path(),
                )
                .await
                .with_workspace_cache(Arc::clone(&git_workspace));
            let shared_model_http_clients = agent_control.model_http_clients();
            let services = SessionServices {
                // Initialize the MCP connection manager with an uninitialized
                // instance. It will be replaced with one created via
                // McpConnectionManager::new() once all its constructor args are
                // available. This also ensures `SessionConfigured` is emitted
                // before any MCP-related events. It is reasonable to consider
                // changing this to use Option or OnceCell, though the current
                // setup is straightforward enough and performs well.
                mcp_runtime,
                mcp_shutdown_managers: std::sync::Mutex::new(Vec::new()),
                planning_generation: std::sync::atomic::AtomicU64::new(0),
                mcp_projection_lock: Semaphore::new(1),
                mcp_startup_cancellation_token: Mutex::new(CancellationToken::new()),
                unified_exec_manager,
                command_execution,
                retained_patches: Default::default(),
                path_replays: Default::default(),
                validation_uncertainty: Default::default(),
                plan_store: crate::plan_store::PlanStore::default(),
                elicitations: crate::elicitation::ElicitationService::new(),
                analytics_events_client,
                hooks: arc_swap::ArcSwap::from_pointee(hooks),
                rollout_thread_trace,
                user_shell: default_shell,
                show_raw_agent_reasoning: config.show_raw_agent_reasoning,
                exec_policy,
                auth_manager: Arc::clone(&auth_manager),
                session_telemetry,
                models_manager: Arc::clone(&models_manager),
                tool_approvals: Mutex::new(ApprovalStore::default()),

                skills_service,
                agents_md_manager,
                plugins_manager: Arc::clone(&plugins_manager),
                mcp_manager: Arc::clone(&mcp_manager),
                extensions,
                // TODO(jif): extract session to share between sub-agents
                session_extension_data,
                thread_extension_data,
                selected_capability_roots,
                mcp_thread_init,
                supports_openai_form_elicitation: std::sync::atomic::AtomicBool::new(
                    supports_openai_form_elicitation,
                ),
                agent_control,
                network_proxy: arc_swap::ArcSwapOption::from(network_proxy.map(Arc::new)),
                network_proxy_audit_metadata,
                managed_network_requirements_configured,
                network_approval: Arc::clone(&network_approval),
                network_policy_decider: network_policy_decider.clone(),
                state_db: state_db_ctx.clone(),
                live_thread: live_thread_init.as_ref().cloned(),
                thread_store: Arc::clone(&thread_store),
                attestation_provider: attestation_provider.clone(),
                time_provider,
                model_client: ModelClient::new(
                    Some(Arc::clone(&auth_manager)),
                    if config.features.enabled(Feature::UseAgentIdentity) {
                        AgentIdentityAuthPolicy::ChatGptAuth
                    } else {
                        AgentIdentityAuthPolicy::JwtOnly
                    },
                    thread_id,
                    session_configuration.provider.clone(),
                    session_configuration.session_source.clone(),
                    session_configuration.originator.clone(),
                    config.model_verbosity,
                    config.features.enabled(Feature::EnableRequestCompression),
                    config.features.enabled(Feature::RuntimeMetrics),
                    Self::build_model_client_beta_features_header(config.as_ref()),
                    /*concurrent_reasoning_summaries_enabled*/
                    config
                        .features
                        .enabled(Feature::ConcurrentReasoningSummaries),
                    attestation_provider,
                    config.http_client_factory(),
                )
                .with_shared_http_clients(shared_model_http_clients.as_deref())
                .with_startup_timing(Arc::clone(&startup_timing)),
                code_mode_service: crate::tools::code_mode::CodeModeService::new(Arc::clone(
                    &code_mode_session_provider,
                ))
                .with_recovery_path(
                    config
                        .codex_home
                        .join("code-mode-state")
                        .join(format!("{thread_id}.json")),
                ),
                tool_search_handler_cache: Default::default(),
                turn_environments: Arc::clone(&turn_environments),
                git_workspace,
            };
            let tool_history_io_gate = Arc::new(Semaphore::new(/*permits*/ 1));
            let tool_history_persistence = ToolHistoryPersistenceQueue::new_with_owner(
                Arc::clone(&tool_history_io_gate),
                config.codex_home.to_path_buf(),
                thread_id,
                state.tool_history_state(),
                Some(Arc::clone(&storage_ownership)),
            );
            let sess = Arc::new(Session {
                thread_id,
                _storage_ownership: std::sync::Mutex::new(Some(storage_ownership)),
                installation_id,
                tx_event: tx_event.clone(),
                agent_status,
                agent_status_turn: std::sync::Mutex::new(None),
                state: Mutex::new(state),
                durable_history_commit_gate: Semaphore::new(1),
                durable_history_commits_in_flight: std::sync::atomic::AtomicUsize::new(0),
                durable_history_commits_changed: tokio::sync::Notify::new(),
                tool_history_io_gate,
                tool_history_reconciliation_gate: Semaphore::new(/*permits*/ 1),
                tool_history_persistence,
                managed_network_proxy_refresh_lock: Semaphore::new(/*permits*/ 1),
                #[cfg(test)]
                managed_network_proxy_start_gate: Semaphore::new(/*permits*/ 1),
                #[cfg(test)]
                managed_network_proxy_start_entered: tokio::sync::Notify::new(),
                features: config.features.clone(),
                multi_agent_version,
                pending_mcp_server_refresh_config: Mutex::new(None),
                task_start_gate: Arc::new(Semaphore::new(/*permits*/ 1)),
                pending_work_retry: Default::default(),
                submission_preparation_slots: Arc::new(Semaphore::new(SUBMISSION_CHANNEL_CAPACITY)),
                task_start_state: Mutex::new(TaskStartState::default()),
                active_turn: Mutex::new(None),
                startup_timing: Arc::clone(&startup_timing),
                startup_cancellation: CancellationToken::new(),
                startup_prepared_router: Default::default(),
                prepared_tool_router: Default::default(),
                request_scaffold_cache: Default::default(),
                turn_config_cache: Default::default(),
                queued_tool_manifest_hash: Default::default(),
                persisted_freshness_notice_digests: Default::default(),
                terminal_tasks: tokio_util::task::TaskTracker::new(),
                persistence_tasks: tokio_util::task::TaskTracker::new(),
                persistence_retirement_result: Default::default(),
                persistence_shutdown_result: Default::default(),
                rollout_durability_error: Default::default(),
                terminal_interaction_pending: std::sync::atomic::AtomicBool::new(false),
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                raw_response_items_requested: std::sync::atomic::AtomicBool::new(false),
                raw_user_turn_boundaries_requested: std::sync::atomic::AtomicBool::new(false),
                input_queue: services
                    .agent_control
                    .retained_input_queue(thread_id)
                    .unwrap_or_else(|| Arc::new(InputQueue::new())),
                services,
                next_internal_sub_id: AtomicU64::new(0),
            });
            runtime_init.0 = Some(Arc::clone(&sess));
            if let Some(network_policy_decider_session) = network_policy_decider_session {
                let mut guard = network_policy_decider_session.write().await;
                *guard = Arc::downgrade(&sess);
            }
            sess.schedule_startup_transport_preconnect().await;
            // Dispatch the SessionConfiguredEvent first and then report any errors.
            // If resuming, include converted initial messages in the payload so UIs can render them immediately.
            let mut startup_rollout_facts = StartupRolloutFacts::reduce(&initial_history);
            sess.input_queue
                .seed_seen_mailbox_communication_ids_from_ids(std::mem::take(
                    &mut startup_rollout_facts.mailbox_communication_ids,
                ))
                .await;
            let initial_messages = startup_rollout_facts.initial_messages.take();
            let events = std::iter::once(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                    session_id,
                    thread_id,
                    forked_from_id,
                    parent_thread_id,
                    thread_source: session_configuration.thread_source.clone(),
                    thread_name: session_configuration.thread_name.clone(),
                    model: session_configuration.collaboration_mode.model().to_string(),
                    model_provider_id: config.model_provider_id.clone(),
                    service_tier: session_configuration.service_tier.clone(),
                    approval_policy: session_configuration.approval_policy.value(),
                    permission_profile: session_configuration.permission_profile(),
                    active_permission_profile: session_configuration.active_permission_profile(),
                    cwd: session_configuration.cwd().clone(),
                    reasoning_effort: session_reasoning_effort,
                    initial_messages,
                    network_proxy: session_network_proxy.filter(|_| {
                        Self::managed_network_proxy_active_for_permission_profile(
                            session_configuration
                                .permission_profile_state()
                                .permission_profile(),
                        )
                    }),
                    rollout_path,
                }),
            })
            .chain(post_session_configured_events.into_iter());
            for event in events {
                // Startup metadata must be visible immediately without turning an otherwise
                // empty thread into a persisted rollout. Resumed/forked rollouts already exist,
                // so this helper continues to persist their startup events in place.
                sess.send_event_raw_without_materializing_rollout(event)
                    .await;
            }

            let mcp_startup_cancellation_token = {
                let mut cancel_guard = sess.services.mcp_startup_cancellation_token.lock().await;
                cancel_guard.cancel();
                let cancel_token = CancellationToken::new();
                *cancel_guard = cancel_token.clone();
                cancel_token
            };
            let codex_apps_auth_manager =
                codex_mcp::host_owned_codex_apps_enabled(&mcp_projection.config, auth)
                    .then(|| Arc::clone(&sess.services.auth_manager));
            let elicitation_router = codex_mcp::ElicitationRequestRouter::default();
            let _startup_route = elicitation_router.startup_route(thread_id);
            let mcp_connection_manager = McpConnectionManager::new(
                &mcp_servers,
                config.mcp_oauth_credentials_store_mode,
                config.auth_keyring_backend_kind(),
                auth_statuses,
                &session_configuration.approval_policy,
                INITIAL_SUBMIT_ID.to_owned(),
                tx_event.clone(),
                mcp_startup_cancellation_token,
                session_configuration.permission_profile(),
                mcp_runtime_context.clone(),
                config.codex_home.to_path_buf(),
                sess.services.mcp_manager.codex_apps_tools_cache(),
                codex_apps_tools_cache_key(
                    auth,
                    &mcp_projection.config.chatgpt_base_url,
                    mcp_projection.config.apps_mcp_product_sku.as_deref(),
                ),
                config.prefix_mcp_tool_names(),
                mcp_projection.config.client_elicitation_capability.clone(),
                sess.services
                    .supports_openai_form_elicitation
                    .load(std::sync::atomic::Ordering::Relaxed),
                tool_plugin_provenance,
                auth,
                codex_apps_auth_manager,
                Some(sess.mcp_elicitation_lifecycle()),
                elicitation_router,
                /*previous_manager*/ None,
            )
            .instrument(info_span!(
                "session_init.mcp_manager_init",
                otel.name = "session_init.mcp_manager_init",
            ))
            .await;
            sess.services
                .install_mcp_connection_manager(
                    Arc::new(mcp_projection.config),
                    mcp_projection.plugins_available,
                    mcp_runtime_context,
                    /*available_environment_ids*/ Vec::new(),
                    mcp_connection_manager,
                )
                .await?;
            sess.schedule_startup_prewarm(session_configuration.base_instructions.clone())
                .await;
            let session_start_source = match &initial_history {
                InitialHistory::Resumed(_) => codex_hooks::SessionStartSource::Resume,
                InitialHistory::New | InitialHistory::Forked(_) => {
                    codex_hooks::SessionStartSource::Startup
                }
                InitialHistory::Cleared => codex_hooks::SessionStartSource::Clear,
            };

            // record_initial_history can emit events. We record only after the SessionConfiguredEvent is emitted.
            Box::pin(
                sess.record_initial_history_with_facts(initial_history, startup_rollout_facts),
            )
            .await?;
            if config.completed_tool_history_projection && restores_initial_tool_history {
                sess.finalize_initial_tool_history_state(
                    config.codex_home.as_path(),
                    tool_history_fork_source,
                )
                .await?;
            }
            {
                let mut state = sess.state.lock().await;
                state.queue_pending_session_start_source(session_start_source);
            }
            startup_timing.finish_session_initialization();
            Ok(sess)
        }
        .await;
        match session_result {
            Ok(sess) => {
                runtime_init.0 = None;
                live_thread_init.commit();
                Ok(sess)
            }
            Err(err) => {
                runtime_init.rollback().await;
                live_thread_init.discard().await;
                if rollback_created_persistence_on_error {
                    crate::thread_manager::rollback_created_thread_persistence(
                        &thread_store,
                        thread_id,
                    )
                    .await;
                }
                Err(err)
            }
        }
    }
}
