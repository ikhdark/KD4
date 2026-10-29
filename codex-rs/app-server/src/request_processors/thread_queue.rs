use super::*;

fn parse_thread_id_for_request(thread_id: &str) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::from_string(thread_id)
        .map_err(|err| invalid_request(format!("invalid thread id: {err}")))
}

use codex_app_server_protocol::QueuedSubmission;
use codex_app_server_protocol::ThreadQueueAddParams;
use codex_app_server_protocol::ThreadQueueAddResponse;
use codex_app_server_protocol::ThreadQueueChangedNotification;
use codex_app_server_protocol::ThreadQueueDeleteParams;
use codex_app_server_protocol::ThreadQueueDeleteResponse;
use codex_app_server_protocol::ThreadQueueListParams;
use codex_app_server_protocol::ThreadQueueListResponse;
use codex_app_server_protocol::ThreadQueueReorderParams;
use codex_app_server_protocol::ThreadQueueReorderResponse;
use codex_app_server_protocol::ThreadQueueStartParams;
use codex_app_server_protocol::ThreadQueueStartResponse;
use codex_app_server_protocol::ThreadQueueUpdateParams;
use codex_app_server_protocol::ThreadQueueUpdateResponse;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::fs::TryLockError;
use std::sync::Mutex as StdMutex;

const MAX_QUEUE_ENTRIES: usize = 100;
const MAX_QUEUE_BYTES: usize = 16 * 1024 * 1024;
// Retry identities survive consumption for the latest 100 accepted submissions,
// further bounded by MAX_QUEUE_BYTES. Pending submissions are never evicted.
const MAX_ACCEPTED_SUBMISSIONS: usize = 100;

/// Separate from the wire DTO: this payload is versioned and includes admission
/// recovery state. A pending start keeps the prompt until core accepts it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Queue {
    version: u32,
    revision: u64,
    submissions: Vec<QueuedSubmission>,
    paused: bool,
    pending_start: Option<PendingStart>,
    #[serde(default)]
    accepted: Vec<AcceptedSubmission>,
    /// Immutable execution inputs; submissions retain original UI/idempotency input.
    #[serde(default)]
    execution_inputs: HashMap<String, Vec<V2UserInput>>,
    /// Internal observations, not a change to the public queue/turn clock.
    /// Old persisted entries deliberately have unknown acceptance times.
    #[serde(default)]
    task_observations: HashMap<String, QueueTaskObservation>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueTaskObservation {
    version: u32,
    accepted_at_unix_ms: u64,
    #[serde(default)]
    execution_started_at_unix_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingStart {
    submission_id: String,
    turn_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcceptedSubmission {
    submission: QueuedSubmission,
    turn_id: String,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            submissions: Vec::new(),
            paused: false,
            pending_start: None,
            accepted: Vec::new(),
            execution_inputs: HashMap::new(),
            task_observations: HashMap::new(),
        }
    }
}

impl Queue {
    fn encode_with_budget(&mut self, max_bytes: usize) -> Result<String, JSONRPCErrorError> {
        let payload = serde_json::to_string(&*self).map_err(queue_error)?;
        let mut bytes = payload.len();
        let mut evicted = 0;
        for accepted in &self.accepted {
            if bytes <= max_bytes {
                break;
            }
            // Compact JSON contributions are exact, including escaped keys and
            // the comma removed from each non-singleton array/object.
            bytes -= serde_json::to_vec(accepted).map_err(queue_error)?.len()
                + usize::from(self.accepted.len() - evicted > 1);
            let observation_count = self.task_observations.len();
            if let Some(observation) = self.task_observations.remove(&accepted.submission.id) {
                bytes -= serde_json::to_vec(&accepted.submission.id)
                    .map_err(queue_error)?
                    .len()
                    + 1
                    + serde_json::to_vec(&observation).map_err(queue_error)?.len()
                    + usize::from(observation_count > 1);
            }
            evicted += 1;
        }
        self.accepted.drain(..evicted);
        if bytes > max_bytes {
            return Err(invalid_params("thread queue exceeds its storage limit"));
        }
        if evicted == 0 {
            Ok(payload)
        } else {
            // Pending prompts are encoded at most twice, not once per receipt.
            serde_json::to_string(self).map_err(queue_error)
        }
    }


    fn finish_sealed_add(
        &mut self,
        item: QueuedSubmission,
        added: bool,
        sealed: Result<Vec<V2UserInput>, JSONRPCErrorError>,
    ) -> Result<(QueuedSubmission, bool), JSONRPCErrorError> {
        if added && item.input.iter().any(|input| matches!(input, V2UserInput::LocalImage { .. })) {
            match sealed {
                Ok(input) => {
                    self.execution_inputs.insert(item.id.clone(), input);
                }
                Err(error) => {
                    self.submissions
                        .retain(|submission| submission.id != item.id);
                    self.task_observations.remove(&item.id);
                    return Err(error);
                }
            }
        }
        if added && let Some(observation) = self.task_observations.get_mut(&item.id) {
            // Seal preparation precedes durable acceptance, not task execution.
            observation.accepted_at_unix_ms = super::turn_processor::task_wall_time_ms();
        }
        Ok((item, added))
    }

    fn existing_submission(
        &self,
        params: &ThreadQueueAddParams,
    ) -> Result<Option<QueuedSubmission>, JSONRPCErrorError> {
        if let Some(client_id) = &params.client_user_message_id
            && let Some(existing) = self
                .submissions
                .iter()
                .chain(self.accepted.iter().map(|accepted| &accepted.submission))
                .find(|item| item.client_user_message_id.as_ref() == Some(client_id))
        {
            if existing.input != params.input {
                return Err(invalid_params(
                    "clientUserMessageId already has different queued input",
                ));
            }
            return Ok(Some(existing.clone()));
        }
        Ok(None)
    }

    fn add(
        &mut self,
        params: ThreadQueueAddParams,
    ) -> Result<(QueuedSubmission, bool), JSONRPCErrorError> {
        if let Some(existing) = self.existing_submission(&params)? {
            return Ok((existing, false));
        }
        if self.submissions.len() >= MAX_QUEUE_ENTRIES {
            return Err(invalid_params("thread queue is full"));
        }
        let submission = QueuedSubmission {
            id: uuid::Uuid::new_v4().to_string(),
            client_user_message_id: params.client_user_message_id,
            input: params.input,
        };
        self.submissions.push(submission.clone());
        self.task_observations.insert(
            submission.id.clone(),
            QueueTaskObservation {
                version: 1,
                accepted_at_unix_ms: super::turn_processor::task_wall_time_ms(),
                execution_started_at_unix_ms: None,
            },
        );
        Ok((submission, true))
    }

    fn reorder(&mut self, ids: &[String]) -> Result<bool, JSONRPCErrorError> {
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id) || !self.submissions.iter().any(|item| &item.id == id) {
                return Err(invalid_params(
                    "queuedSubmissionIds contains duplicate or unknown ids",
                ));
            }
        }
        let before = self
            .submissions
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        self.submissions.sort_by_key(|item| {
            ids.iter()
                .position(|id| id == &item.id)
                .unwrap_or(ids.len())
        });
        Ok(self
            .submissions
            .iter()
            .map(|item| &item.id)
            .ne(before.iter()))
    }

    fn page(
        &self,
        params: &ThreadQueueListParams,
    ) -> Result<ThreadQueueListResponse, JSONRPCErrorError> {
        let offset = match params.cursor.as_deref() {
            None => 0,
            Some(cursor) => {
                let (revision, offset) = cursor
                    .split_once(':')
                    .ok_or_else(|| invalid_params("invalid queue cursor"))?;
                let revision = revision
                    .parse::<u64>()
                    .map_err(|_| invalid_params("invalid queue cursor"))?;
                let offset = offset
                    .parse::<usize>()
                    .map_err(|_| invalid_params("invalid queue cursor"))?;
                if revision != self.revision || offset > self.submissions.len() {
                    return Err(invalid_params("queue changed; restart pagination"));
                }
                offset
            }
        };
        let limit = params.limit.unwrap_or(100).clamp(1, 100) as usize;
        let end = offset.saturating_add(limit).min(self.submissions.len());
        Ok(ThreadQueueListResponse {
            data: self.submissions[offset..end].to_vec(),
            next_cursor: (end < self.submissions.len()).then(|| format!("{}:{end}", self.revision)),
            snapshot: Some(codex_app_server_protocol::ThreadQueueSnapshot {
                revision: self.revision,
                paused: self.paused,
                pending_start: self.pending_start.as_ref().map(|pending| {
                    codex_app_server_protocol::ThreadQueuePendingStart {
                        queued_submission_id: pending.submission_id.clone(),
                        turn_id: pending.turn_id.clone(),
                    }
                }),
                accepted_submissions: self
                    .accepted
                    .iter()
                    .map(
                        |accepted| codex_app_server_protocol::ThreadQueueAcceptedSubmission {
                            queued_submission_id: accepted.submission.id.clone(),
                            turn_id: accepted.turn_id.clone(),
                        },
                    )
                    .collect(),
            }),
        })
    }
}

#[derive(Clone)]
pub(crate) struct QueueOrigin {
    pub(crate) request_id: ConnectionRequestId,
    pub(crate) client_name: Option<String>,
    pub(crate) client_version: Option<String>,
    pub(crate) supports_openai_form_elicitation: bool,
}

#[derive(Clone)]
pub(crate) struct ThreadQueueRequestProcessor {
    state_db: Option<StateDbHandle>,
    codex_home: PathBuf,
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
    thread_state_manager: ThreadStateManager,
    outgoing: Arc<OutgoingMessageSender>,
    turn_processor: TurnRequestProcessor,
    background_tasks: TaskTracker,
    // Connection capabilities are never persisted or reused across restarts.
    origins: Arc<StdMutex<HashMap<ThreadId, HashMap<String, QueueOrigin>>>>,
    notification_revisions: Arc<StdMutex<HashMap<ThreadId, std::sync::Weak<Mutex<u64>>>>>,
}

impl ThreadQueueRequestProcessor {
    pub(crate) fn new(
        state_db: Option<StateDbHandle>,
        thread_processor: &ThreadRequestProcessor,
        turn_processor: TurnRequestProcessor,
    ) -> Self {
        Self {
            state_db,
            codex_home: thread_processor.config.codex_home.to_path_buf(),
            thread_manager: thread_processor.thread_manager.clone(),
            thread_store: thread_processor.thread_store.clone(),
            thread_state_manager: thread_processor.thread_state_manager.clone(),
            outgoing: thread_processor.outgoing.clone(),
            turn_processor,
            background_tasks: thread_processor.background_tasks.clone(),
            origins: Arc::default(),
            notification_revisions: Arc::default(),
        }
    }

    fn db(&self) -> Result<&StateDbHandle, JSONRPCErrorError> {
        self.state_db
            .as_ref()
            .ok_or_else(|| internal_error("thread queue requires the state database"))
    }

    fn origin(&self, id: ThreadId, submission_id: &str) -> Option<QueueOrigin> {
        self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)?.get(submission_id).cloned()
    }

    async fn insert_origin(&self, id: ThreadId, submission_id: String, origin: QueueOrigin) -> bool {
        self.thread_state_manager.with_live_connection(origin.request_id.connection_id, || {
            self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(id).or_default().insert(submission_id, origin);
        }).await.is_some()
    }

    fn remove_origin(&self, id: ThreadId, submission_id: &str) {
        let mut origins = self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(submissions) = origins.get_mut(&id) {
            submissions.remove(submission_id);
            if submissions.is_empty() { origins.remove(&id); }
        }
    }

    fn has_origins(&self, id: ThreadId) -> bool {
        self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(&id)
    }

    /// Called only after the runtime is unregistered. The existing queue lock
    /// joins any in-flight origin publication before retiring its capabilities;
    /// durable pending prompts are left intact for explicit recovery.
    pub(super) async fn retire_thread(&self, id: ThreadId) {
        if self.state_db.is_none() { return; }
        let _guard = loop {
            match self.lock(id).await {
                Ok(guard) => break guard,
                Err(error) => {
                    tracing::warn!(%id, error = %error.message, "queue retirement lock failed; retaining cleanup owner");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
    }

    pub(super) async fn retire_thread_connection(&self, id: ThreadId, connection_id: ConnectionId) {
        let _guard = self.lock(id).await.ok();
        let mut origins = self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(submissions) = origins.get_mut(&id) {
            submissions.retain(|_, origin| origin.request_id.connection_id != connection_id);
            if submissions.is_empty() { origins.remove(&id); }
        }
    }

    pub(super) async fn retire_connection(&self, connection_id: ConnectionId) {
        self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, submissions| {
                submissions.retain(|_, origin| origin.request_id.connection_id != connection_id);
                !submissions.is_empty()
            });
    }

    async fn load(&self, id: ThreadId) -> Result<Queue, JSONRPCErrorError> {
        let db = self.db()?;
        let metadata = db.get_thread(id).await.map_err(queue_error)?;
        if metadata
            .as_ref()
            .is_some_and(|metadata| metadata.archived_at.is_some())
        {
            return Err(invalid_params("thread is archived"));
        }
        if metadata.is_none() {
            let thread = self
                .thread_manager
                .get_thread(id)
                .await
                .map_err(|_| invalid_params("thread not found"))?;
            if thread.rollout_path().is_none() {
                return Err(invalid_params("ephemeral threads do not support queues"));
            }
        }
        let queue: Queue = match db.read_thread_queue(id).await.map_err(queue_error)? {
            Some(payload) => serde_json::from_str(&payload).map_err(queue_error)?,
            None => Queue::default(),
        };
        if queue.version != 1 {
            return Err(internal_error("unsupported persisted thread queue version"));
        }
        Ok(queue)
    }

    async fn save(&self, id: ThreadId, queue: &mut Queue) -> Result<(), JSONRPCErrorError> {
        queue
            .execution_inputs
            .retain(|id, _| queue.submissions.iter().any(|item| &item.id == id));
        queue.task_observations.retain(|id, _| {
            queue.submissions.iter().any(|item| &item.id == id)
                || queue.accepted.iter().any(|item| &item.submission.id == id)
        });
        // A pause protects the prompts queued when a turn was interrupted; it
        // must not outlive them and silently disable later automatic follow-ups.
        if queue.submissions.is_empty() && queue.pending_start.is_none() {
            queue.paused = false;
        }
        queue.revision = queue
            .revision
            .checked_add(1)
            .ok_or_else(|| internal_error("queue revision overflow"))?;
        let payload = queue.encode_with_budget(MAX_QUEUE_BYTES)?;
        let db = self.db()?;
        if db.get_thread(id).await.map_err(queue_error)?.is_none() {
            let thread = self
                .thread_manager
                .get_thread(id)
                .await
                .map_err(queue_error)?;
            thread.ensure_rollout_materialized().await;
            thread.flush_rollout().await.map_err(queue_error)?;
        }
        db.write_thread_queue(id, &payload)
            .await
            .map_err(queue_error)
    }

    /// File locks span SQLite writes *and* core admission without holding a DB
    /// transaction while core itself writes state. OS release on exit also lets
    /// another process safely reconcile an interrupted admission.
    async fn lock(&self, id: ThreadId) -> Result<(File, Arc<Mutex<u64>>), JSONRPCErrorError> {
        // Register publication lifetime before storage admission. A delayed
        // older operation must share the gate retained by a newer publisher.
        let publication = self.publication(id);
        let directory = self.codex_home.join("thread-queue-locks");
        let file = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(directory.join(format!("{id}.lock")))
        })
        .await
        .map_err(queue_error)?
        .map_err(queue_error)?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok((file, publication)),
                    Err(TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(TryLockError::Error(error)) => return Err(queue_error(error)),
                }
            }
        })
        .await
        .map_err(|_| invalid_request("thread queue is busy; retry the request"))?
    }

    fn publication(&self, id: ThreadId) -> Arc<Mutex<u64>> {
        let mut publications = self
            .notification_revisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publications.retain(|_, gate| gate.strong_count() > 0);
        if let Some(publication) = publications.get(&id).and_then(std::sync::Weak::upgrade) {
            return publication;
        }
        let publication = Arc::new(Mutex::new(0));
        publications.insert(id, Arc::downgrade(&publication));
        publication
    }

    async fn changed(&self, id: ThreadId, revision: u64, publication: Arc<Mutex<u64>>) {
        publish_queue_change(
            &self.thread_state_manager,
            &self.outgoing,
            id,
            revision,
            publication,
        )
        .await;
    }

    async fn recover(&self, id: ThreadId, queue: &mut Queue) -> Result<(), JSONRPCErrorError> {
        let Some(pending) = &queue.pending_start else {
            return Ok(());
        };
        let thread = self.thread_manager.get_thread(id).await.ok();
        let live_receipt = match &thread {
            Some(thread) => thread.durable_input_recorded(&pending.turn_id).await.map_err(queue_error)?,
            None => None,
        };
        let accepted = if let Some(recorded) = live_receipt {
            recorded
        } else {
            if let Some(thread) = &thread {
                thread.flush_rollout().await.map_err(queue_error)?;
            }
            let history = self
            .thread_store
            .read_thread(codex_thread_store::ReadThreadParams {
                thread_id: id,
                include_archived: false,
                include_history: true,
            })
            .await
            .map_err(queue_error)?;
            history
            .history
            .as_ref()
            .is_some_and(|history| pending_input_recorded(&history.items, &pending.turn_id))
        };
        // Accepted input belongs to history even while the turn is running.
        // Without that evidence, worker installation is not enough to release
        // custody: it may still be starting or recording the prompt.
        if !accepted {
            let live = self.thread_state_manager.thread_state(id).await;
            if live.lock().await.in_progress_turn_id() == Some(pending.turn_id.as_str()) {
                return Ok(());
            }
            if let Some(thread) = &thread
                && matches!(thread.agent_status().await, AgentStatus::Running)
            {
                return Ok(());
            }
        }
        let origin_present = self.origin(id, &pending.submission_id).is_some();
        let pending = queue.pending_start.take().ok_or_else(|| {
            internal_error("pending thread queue start disappeared during recovery")
        })?;
        if accepted
            && let Some(index) = queue
                .submissions
                .iter()
                .position(|item| item.id == pending.submission_id)
        {
            let submission = queue.submissions.remove(index);
            queue.accepted.push(AcceptedSubmission {
                submission,
                turn_id: pending.turn_id,
            });
            if queue.accepted.len() > MAX_ACCEPTED_SUBMISSIONS {
                queue.accepted.remove(0);
            }
        }
        // Never silently replay an unaccepted start or resume after process death.
        queue.paused |= !accepted || !origin_present;
        self.save(id, queue).await?;
        if accepted {
            self.remove_origin(id, &pending.submission_id);
        }
        let processor = self.clone();
        let revision = queue.revision;
        let publication = self.publication(id);
        // Publication is owned independently of the request and never extends
        // the file-lock critical section with a transport wait. A later
        // revision can overtake it; changed() coalesces that stale invalidation.
        self.background_tasks.spawn(async move {
            processor.changed(id, revision, publication).await;
        });
        Ok(())
    }

    async fn mutate<R>(
        &self,
        id: ThreadId,
        mutation: impl FnOnce(&mut Queue) -> Result<(R, bool), JSONRPCErrorError>,
    ) -> Result<R, JSONRPCErrorError> {
        let admission = self.lock(id).await?;
        self.mutate_locked(id, admission, mutation).await
    }

    async fn mutate_locked<R>(
        &self,
        id: ThreadId,
        admission: (File, Arc<Mutex<u64>>),
        mutation: impl FnOnce(&mut Queue) -> Result<(R, bool), JSONRPCErrorError>,
    ) -> Result<R, JSONRPCErrorError> {
        let (guard, publication) = admission;
        let mut queue = self.load(id).await?;
        self.recover(id, &mut queue).await?;
        let (result, changed) = mutation(&mut queue)?;
        if changed {
            self.save(id, &mut queue).await?;
        }
        drop(guard);
        if changed {
            self.changed(id, queue.revision, publication).await;
        }
        Ok(result)
    }

    pub(crate) async fn list(
        &self,
        params: ThreadQueueListParams,
    ) -> Result<ThreadQueueListResponse, JSONRPCErrorError> {
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let admission = self.lock(id).await?;
        let processor = self.clone();
        // Recovery can commit too. Keep durable state, capability bookkeeping
        // and change publication together when the requesting connection closes.
        self.background_tasks
            .spawn(async move { processor.list_owned(params, id, admission).await })
            .await
            .map_err(queue_error)?
    }

    async fn list_owned(
        &self,
        params: ThreadQueueListParams,
        id: ThreadId,
        _admission: (File, Arc<Mutex<u64>>),
    ) -> Result<ThreadQueueListResponse, JSONRPCErrorError> {
        let mut queue = self.load(id).await?;
        self.recover(id, &mut queue).await?;
        queue.page(&params)
    }

    pub(crate) async fn add(
        &self,
        params: ThreadQueueAddParams,
        origin: QueueOrigin,
    ) -> Result<(), JSONRPCErrorError> {
        TurnRequestProcessor::validate_queue_input(&params.input)?;
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let mut admission = self.lock(id).await?;
        let mut sealed = None;
        let mut prepared_queue = None;
        if params.input.iter().any(|input| matches!(input, V2UserInput::LocalImage { .. })) {
            let processor = self.clone();
            // Recovery may commit, but mutable image preparation must remain
            // cancellable. Return ownership only after recovery has settled.
            let (returned_admission, queue) = self.background_tasks
                .spawn(async move {
                    let mut queue = processor.load(id).await?;
                    processor.recover(id, &mut queue).await?;
                    Ok::<_, JSONRPCErrorError>((admission, queue))
                })
                .await
                .map_err(queue_error)??;
            admission = returned_admission;
            if queue.existing_submission(&params)?.is_none() {
                if queue.submissions.len() >= MAX_QUEUE_ENTRIES {
                    return Err(invalid_params("thread queue is full"));
                }
                drop(admission);
                drop(queue);
                // A failed seal is retained until the locked duplicate check:
                // a winning retry still owns its original immutable snapshot.
                sealed = Some(seal_queue_input(params.input.clone()).await);
                admission = self.lock(id).await?;
            } else {
                prepared_queue = Some(queue);
            }
        }
        let processor = self.clone();
        // Recovery can commit too. Keep durable state, capability bookkeeping
        // and change publication together when the requesting connection closes.
        self.background_tasks
            .spawn(async move {
                processor.add_owned(params, origin, id, admission, sealed, prepared_queue).await
            })
            .await
            .map_err(queue_error)?
    }

    async fn add_owned(
        &self,
        params: ThreadQueueAddParams,
        origin: QueueOrigin,
        id: ThreadId,
        admission: (File, Arc<Mutex<u64>>),
        sealed: Option<Result<Vec<V2UserInput>, JSONRPCErrorError>>,
        prepared_queue: Option<Queue>,
    ) -> Result<(), JSONRPCErrorError> {
        let (guard, publication) = admission;
        let mut queue = match prepared_queue {
            Some(queue) => queue,
            None => {
                let mut queue = self.load(id).await?;
                self.recover(id, &mut queue).await?;
                queue
            }
        };
        // Duplicate accepted IDs are resolved before reading mutable source paths.
        let (queued_submission, added) = queue.add(params)?;
        let (queued_submission, added) = if queued_submission.input.iter().any(|input| matches!(input, V2UserInput::LocalImage { .. })) {
            queue.finish_sealed_add(queued_submission, added, sealed.unwrap_or_else(|| {
                Err(internal_error("queued image preparation is missing"))
            }))?
        } else { (queued_submission, added) };
        if added {
            self.save(id, &mut queue).await?;
        }
        if let Some(observation) = queue.task_observations.get(&queued_submission.id) {
            super::turn_processor::observe_task_boundary(
                id,
                &queued_submission.id,
                None,
                "queue_accepted",
                observation.accepted_at_unix_ms,
                None,
            );
        }
        if added {
            // Publish under the queue lock: whole-thread retirement joins this
            // owner before clearing its in-memory connection capabilities.
            self.insert_origin(id, queued_submission.id.clone(), origin.clone()).await;
        }
        drop(guard);
        if added {
            self.changed(id, queue.revision, publication).await;
        }
        // The client must see acceptance before a fast automatic start removes the entry.
        self.outgoing
            .send_response(
                origin.request_id,
                ThreadQueueAddResponse { queued_submission },
            )
            .await;
        if let Ok(thread) = self.thread_manager.get_thread(id).await
            && !matches!(thread.agent_status().await, AgentStatus::Running)
        {
            self.kick(id);
        }
        Ok(())
    }

    pub(crate) async fn update(
        &self,
        params: ThreadQueueUpdateParams,
    ) -> Result<ThreadQueueUpdateResponse, JSONRPCErrorError> {
        TurnRequestProcessor::validate_queue_input(&params.input)?;
        let execution_input = if params
            .input
            .iter()
            .any(|input| matches!(input, V2UserInput::LocalImage { .. }))
        {
            Some(seal_queue_input(params.input.clone()).await?)
        } else {
            None
        };
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let admission = self.lock(id).await?;
        let processor = self.clone();
        self.background_tasks
            .spawn(async move {
                processor.update_owned(params, execution_input, id, admission).await
            })
            .await
            .map_err(queue_error)?
    }

    async fn update_owned(
        &self,
        params: ThreadQueueUpdateParams,
        execution_input: Option<Vec<V2UserInput>>,
        id: ThreadId,
        admission: (File, Arc<Mutex<u64>>),
    ) -> Result<ThreadQueueUpdateResponse, JSONRPCErrorError> {
        let queued_submission = self
            .mutate_locked(id, admission, |queue| {
                if queue
                    .pending_start
                    .as_ref()
                    .is_some_and(|pending| pending.submission_id == params.queued_submission_id)
                {
                    return Err(invalid_request(
                        "queued submission is starting; input is still owned by the queue",
                    ));
                }
                let item = queue
                    .submissions
                    .iter_mut()
                    .find(|item| item.id == params.queued_submission_id)
                    .ok_or_else(|| invalid_params("queued submission not found"))?;
                let changed = item.input != params.input
                    || queue.execution_inputs.get(&item.id) != execution_input.as_ref();
                if let Some(input) = execution_input {
                    queue.execution_inputs.insert(item.id.clone(), input);
                } else {
                    queue.execution_inputs.remove(&item.id);
                }
                item.input = params.input;
                Ok((item.clone(), changed))
            })
            .await?;
        Ok(ThreadQueueUpdateResponse { queued_submission })
    }

    pub(crate) async fn delete(
        &self,
        params: ThreadQueueDeleteParams,
    ) -> Result<ThreadQueueDeleteResponse, JSONRPCErrorError> {
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let admission = self.lock(id).await?;
        let processor = self.clone();
        // Recovery can commit too. Keep durable state, capability bookkeeping
        // and change publication together when the requesting connection closes.
        self.background_tasks
            .spawn(async move { processor.delete_owned(params, id, admission).await })
            .await
            .map_err(queue_error)?
    }

    async fn delete_owned(
        &self,
        params: ThreadQueueDeleteParams,
        id: ThreadId,
        admission: (File, Arc<Mutex<u64>>),
    ) -> Result<ThreadQueueDeleteResponse, JSONRPCErrorError> {
        let deleted = self
            .mutate_locked(id, admission, |queue| {
                if queue
                    .pending_start
                    .as_ref()
                    .is_some_and(|pending| pending.submission_id == params.queued_submission_id)
                {
                    return Err(invalid_request(
                        "queued submission is starting; input is still owned by the queue",
                    ));
                }
                let before = queue.submissions.len();
                queue
                    .submissions
                    .retain(|item| item.id != params.queued_submission_id);
                let deleted = before != queue.submissions.len();
                Ok((deleted, deleted))
            })
            .await?;
        if deleted {
            self.remove_origin(id, &params.queued_submission_id);
        }
        Ok(ThreadQueueDeleteResponse { deleted })
    }

    pub(crate) async fn reorder(
        &self,
        params: ThreadQueueReorderParams,
    ) -> Result<ThreadQueueReorderResponse, JSONRPCErrorError> {
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let admission = self.lock(id).await?;
        let processor = self.clone();
        // Recovery can commit too. Keep durable state, capability bookkeeping
        // and change publication together when the requesting connection closes.
        self.background_tasks
            .spawn(async move { processor.reorder_owned(params, id, admission).await })
            .await
            .map_err(queue_error)?
    }

    async fn reorder_owned(
        &self,
        params: ThreadQueueReorderParams,
        id: ThreadId,
        admission: (File, Arc<Mutex<u64>>),
    ) -> Result<ThreadQueueReorderResponse, JSONRPCErrorError> {
        self.mutate_locked(id, admission, |queue| {
            queue
                .reorder(&params.queued_submission_ids)
                .map(|changed| ((), changed))
        })
        .await?;
        Ok(ThreadQueueReorderResponse {})
    }

    pub(crate) async fn start(
        &self,
        params: ThreadQueueStartParams,
        origin: QueueOrigin,
    ) -> Result<ThreadQueueStartResponse, JSONRPCErrorError> {
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let processor = self.clone();
        // Own the entire durable claim -> submit -> consume sequence on disconnect.
        self.background_tasks
            .spawn(async move {
                processor
                    .start_inner(id, Some(&params.queued_submission_id), Some(origin))
                    .await?
                    .ok_or_else(|| invalid_params("queued submission not found"))
            })
            .await
            .map_err(queue_error)?
    }

    async fn start_inner(
        &self,
        id: ThreadId,
        selected: Option<&str>,
        origin: Option<QueueOrigin>,
    ) -> Result<Option<ThreadQueueStartResponse>, JSONRPCErrorError> {
        let (guard, publication) = self.lock(id).await?;
        let mut queue = self.load(id).await?;
        self.recover(id, &mut queue).await?;
        if queue.pending_start.is_some() {
            return Err(invalid_request("a queued submission is still starting"));
        }
        if selected.is_none() && queue.paused {
            return Ok(None);
        }
        let item = match selected {
            Some(selected) => queue.submissions.iter().find(|item| item.id == selected),
            None => queue.submissions.first(),
        };
        let Some(item) = item.cloned() else {
            return Ok(None);
        };
        // An explicit starter owns only this submission. Automatic starts use
        // its original connection, never the last client to touch the thread.
        let mut origin = match origin.or_else(|| self.origin(id, &item.id)) {
            Some(origin) => origin,
            None => return Ok(None),
        };
        let connections = self
            .thread_state_manager
            .subscribed_connection_ids(id)
            .await;
        if selected.is_none() && !connections.contains(&origin.request_id.connection_id) {
            return Ok(None);
        }
        if !self.insert_origin(id, item.id.clone(), origin.clone()).await {
            return Err(invalid_request("queue origin connection is closed"));
        }
        if selected.is_none() {
            origin.request_id.request_id =
                RequestId::String(format!("thread-queue-{}", uuid::Uuid::new_v4()));
        }
        let thread = self
            .thread_manager
            .get_thread(id)
            .await
            .map_err(queue_error)?;
        let turn_id = thread.reserve_turn_id();
        let execution_input = match queue.execution_inputs.get(&item.id) {
            Some(input) => input.clone(),
            None if item
                .input
                .iter()
                .any(|input| matches!(input, V2UserInput::LocalImage { .. })) =>
            {
                return Err(invalid_request(
                    "queued local image predates immutable snapshots; update the submission before starting it",
                ));
            }
            None => item.input.clone(),
        };
        queue.paused = false;
        if let Some(observation) = queue.task_observations.get_mut(&item.id) {
            observation
                .execution_started_at_unix_ms
                .get_or_insert_with(super::turn_processor::task_wall_time_ms);
        }
        queue.pending_start = Some(PendingStart {
            submission_id: item.id.clone(),
            turn_id: turn_id.clone(),
        });
        self.save(id, &mut queue).await?;
        if let Some(observation) = queue.task_observations.get(&item.id) {
            super::turn_processor::observe_task_boundary(
                id,
                &item.id,
                Some(&turn_id),
                "queue_execution_start",
                observation
                    .execution_started_at_unix_ms
                    .unwrap_or(observation.accepted_at_unix_ms),
                None,
            );
        }
        let result = self
            .turn_processor
            .start_queued_turn(
                origin.request_id,
                TurnStartParams {
                    thread_id: id.to_string(),
                    client_user_message_id: item.client_user_message_id,
                    input: execution_input,
                    ..Default::default()
                },
                origin.client_name,
                origin.client_version,
                origin.supports_openai_form_elicitation,
                turn_id,
            )
            .await;
        match result {
            Ok(response) => {
                // Keep pending_start and its input until persisted history takes
                // custody. The input event reconciles that handoff; completion
                // and later queue requests remain recovery paths.
                drop(guard);
                self.changed(id, queue.revision, publication).await;
                Ok(Some(ThreadQueueStartResponse {
                    turn: response.turn,
                }))
            }
            Err(error) => {
                // The acknowledgement can fail after installation. Recovery,
                // not a blind retry, decides whether input was actually recorded.
                Err(error)
            }
        }
    }

    pub(super) async fn pause(&self, id: ThreadId) {
        let Some(db) = self.state_db.as_ref() else {
            return;
        };
        if let Ok(thread) = self.thread_manager.get_thread(id).await
            && thread.rollout_path().is_none()
        {
            return;
        }
        self.origins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        // This runs before the listener forwards every interrupted or failed
        // turn. Without queued prompts there is nothing to protect: skip the
        // lock file, queue write, and change notification.
        let has_queued = match db.read_thread_queue(id).await {
            Ok(Some(payload)) => match serde_json::from_str::<Queue>(&payload) {
                Ok(queue) => !queue.submissions.is_empty() || queue.pending_start.is_some(),
                // Let the locked path report an unreadable queue.
                Err(_) => true,
            },
            Ok(None) => false,
            Err(_) => true,
        };
        if !has_queued {
            return;
        }
        if let Err(error) = self
            .mutate(id, |queue| {
                let changed = !queue.paused;
                queue.paused = true;
                Ok(((), changed))
            })
            .await
        {
            tracing::warn!(%id, error = %error.message, "failed to pause thread queue");
        }
    }

    pub(super) fn input_recorded(&self, id: ThreadId) {
        if !self.has_origins(id) {
            return;
        }
        let processor = self.clone();
        // Never wait for the queue lock or storage in the event listener: a
        // start can still hold the lock while core is publishing this event.
        // Keep the tracked task and lock alive through persistence. Timing out
        // a queue write could release the lock before SQLite finishes it and
        // let that late write overwrite a newer queue mutation.
        self.background_tasks.spawn(async move {
            if let Err(error) = processor.mutate(id, |_| Ok(((), false))).await {
                tracing::warn!(%id, error = %error.message, "queued input handoff failed");
            }
        });
    }

    pub(super) fn kick(&self, id: ThreadId) {
        if !self.has_origins(id) {
            return;
        }
        let processor = self.clone();
        self.background_tasks.spawn(async move {
            let Ok(thread) = processor.thread_manager.get_thread(id).await else {
                return;
            };
            let mut status = thread.subscribe_status();
            // Core emits completion before publishing its final agent status.
            // Observe that existing watch rather than polling or starting in the listener.
            let ready = {
                let result = tokio::time::timeout(
                    Duration::from_secs(10),
                    status.wait_for(|state| !matches!(state, AgentStatus::Running)),
                )
                .await;
                matches!(result, Ok(Ok(_)))
            };
            if !ready {
                return;
            }
            if matches!(
                thread.agent_status().await,
                AgentStatus::Shutdown | AgentStatus::Errored(_) | AgentStatus::Interrupted
            ) {
                return;
            }
            if let Err(error) = processor.start_inner(id, None, None).await {
                tracing::warn!(%id, error = %error.message, "queued follow-up was not started");
            }
        });
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "Serialize queue-change publication so revisions are delivered in increasing order"
)]
async fn publish_queue_change(
    thread_state_manager: &ThreadStateManager,
    outgoing: &OutgoingMessageSender,
    id: ThreadId,
    revision: u64,
    publication: Arc<Mutex<u64>>,
) {
    let mut published_revision = publication.lock().await;
    if revision <= *published_revision {
        return;
    }
    let connections = thread_state_manager.subscribed_connection_ids(id).await;
    // Recovery publication outlives its RPC and may run after the last
    // subscriber disconnects. An empty target list means broadcast to outgoing.
    if !connections.is_empty() {
        outgoing
            .send_server_notification_to_connections(
                &connections,
                ServerNotification::ThreadQueueChanged(ThreadQueueChangedNotification {
                    thread_id: id.to_string(),
                    revision: Some(revision),
                }),
            )
            .await;
    }
    *published_revision = revision;
}

// A TurnStarted record alone says nothing about recoverable input ownership.
fn pending_input_recorded(items: &[RolloutItem], turn_id: &str) -> bool {
    let mut in_turn = false;
    for item in items {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                in_turn = event.turn_id == turn_id
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) if in_turn => return true,
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                if event.turn_id == turn_id
                    && matches!(&event.item, codex_protocol::items::TurnItem::UserMessage(_)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn queue_error(error: impl std::fmt::Display) -> JSONRPCErrorError {
    internal_error(format!("thread queue: {error}"))
}

async fn seal_queue_input(input: Vec<V2UserInput>) -> Result<Vec<V2UserInput>, JSONRPCErrorError> {
    if !input
        .iter()
        .any(|item| matches!(item, V2UserInput::LocalImage { .. }))
    {
        return Ok(input);
    }
    tokio::task::spawn_blocking(move || {
        use codex_protocol::models::ContentItem;
        use std::io::Read;
        let mut sealed = Vec::new();
        let mut remaining = MAX_QUEUE_BYTES;
        for item in input {
            if let V2UserInput::LocalImage { path, detail } = item {
                if !std::fs::metadata(&path).map_err(queue_error)?.is_file() {
                    return Err(invalid_params("queued image must be a regular file"));
                }
                let file = File::open(&path).map_err(queue_error)?;
                if !file.metadata().map_err(queue_error)?.is_file() {
                    return Err(invalid_params("queued image must be a regular file"));
                }
                let mut bytes = Vec::new();
                file.take(remaining as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(queue_error)?;
                if bytes.len() > remaining {
                    return Err(invalid_params("queued images exceed their storage limit"));
                }
                remaining -= bytes.len();
                let prepared = codex_protocol::models::local_image_content_items_with_label_number(
                    &path,
                    bytes,
                    None,
                    detail.unwrap_or(codex_protocol::models::DEFAULT_IMAGE_DETAIL),
                );
                if !prepared
                    .iter()
                    .any(|item| matches!(item, ContentItem::InputImage { .. }))
                {
                    return Err(invalid_params("queued local image could not be decoded"));
                }
                for item in prepared {
                    match item {
                        ContentItem::InputImage { image_url, detail } => {
                            sealed.push(V2UserInput::Image {
                                url: image_url,
                                detail,
                            })
                        }
                        ContentItem::InputText { text } => sealed.push(V2UserInput::Text {
                            text,
                            text_elements: Vec::new(),
                        }),
                        _ => {}
                    }
                }
            } else {
                sealed.push(item);
            }
        }
        if serde_json::to_vec(&sealed).map_err(queue_error)?.len() > MAX_QUEUE_BYTES {
            return Err(invalid_params(
                "sealed queue input exceeds its storage limit",
            ));
        }
        Ok(sealed)
    })
    .await
    .map_err(queue_error)?
}

