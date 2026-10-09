use super::thread_goal_processor::parse_thread_id_for_request;
use super::*;
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
        }
    }
}

impl Queue {
    fn add(&mut self, params: ThreadQueueAddParams) -> Result<(QueuedSubmission, bool), JSONRPCErrorError> {
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
            return Ok((existing.clone(), false));
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
        let before = self.submissions.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        self.submissions.sort_by_key(|item| {
            ids.iter()
                .position(|id| id == &item.id)
                .unwrap_or(ids.len())
        });
        Ok(self.submissions.iter().map(|item| &item.id).ne(before.iter()))
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
    origins: Arc<StdMutex<HashMap<(ThreadId, String), QueueOrigin>>>,
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
        }
    }

    fn db(&self) -> Result<&StateDbHandle, JSONRPCErrorError> {
        self.state_db
            .as_ref()
            .ok_or_else(|| internal_error("thread queue requires the state database"))
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
        // A pause protects the prompts queued when a turn was interrupted; it
        // must not outlive them and silently disable later automatic follow-ups.
        if queue.submissions.is_empty() && queue.pending_start.is_none() {
            queue.paused = false;
        }
        queue.revision = queue
            .revision
            .checked_add(1)
            .ok_or_else(|| internal_error("queue revision overflow"))?;
        let mut payload = serde_json::to_string(queue).map_err(queue_error)?;
        while payload.len() > MAX_QUEUE_BYTES && !queue.accepted.is_empty() {
            queue.accepted.remove(0);
            payload = serde_json::to_string(queue).map_err(queue_error)?;
        }
        if payload.len() > MAX_QUEUE_BYTES {
            return Err(invalid_params("thread queue exceeds its storage limit"));
        }
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
    async fn lock(&self, id: ThreadId) -> Result<File, JSONRPCErrorError> {
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
                    Ok(()) => return Ok(file),
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

    async fn changed(&self, id: ThreadId) {
        let connections = self
            .thread_state_manager
            .subscribed_connection_ids(id)
            .await;
        self.outgoing
            .send_server_notification_to_connections(
                &connections,
                ServerNotification::ThreadQueueChanged(ThreadQueueChangedNotification {
                    thread_id: id.to_string(),
                }),
            )
            .await;
    }

    async fn recover(&self, id: ThreadId, queue: &mut Queue) -> Result<(), JSONRPCErrorError> {
        let Some(pending) = &queue.pending_start else { return Ok(()) };
        let thread = self.thread_manager.get_thread(id).await.ok();
        if let Some(thread) = &thread {
            thread.flush_rollout().await.map_err(queue_error)?;
        }
        let history = self.thread_store.read_thread(codex_thread_store::ReadThreadParams {
            thread_id: id, include_archived: false, include_history: true,
        }).await.map_err(queue_error)?;
        let accepted = history.history.as_ref().is_some_and(|history|
            pending_input_recorded(&history.items, &pending.turn_id));
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
        let origin_present = self.origins.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&(id, pending.submission_id.clone()));
        let pending = queue.pending_start.take().ok_or_else(|| {
            internal_error("pending thread queue start disappeared during recovery")
        })?;
        if accepted {
            if let Some(index) = queue.submissions.iter().position(|item| item.id == pending.submission_id) {
                let submission = queue.submissions.remove(index);
                queue.accepted.push(AcceptedSubmission { submission, turn_id: pending.turn_id });
                if queue.accepted.len() > MAX_ACCEPTED_SUBMISSIONS {
                    queue.accepted.remove(0);
                }
            }
        }
        // Never silently replay an unaccepted start or resume after process death.
        queue.paused |= !accepted || !origin_present;
        self.save(id, queue).await?;
        if accepted {
            self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&(id, pending.submission_id));
        }
        self.changed(id).await;
        Ok(())
    }

    async fn mutate<R>(
        &self,
        id: ThreadId,
        mutation: impl FnOnce(&mut Queue) -> Result<(R, bool), JSONRPCErrorError>,
    ) -> Result<R, JSONRPCErrorError> {
        let guard = self.lock(id).await?;
        let mut queue = self.load(id).await?;
        self.recover(id, &mut queue).await?;
        let (result, changed) = mutation(&mut queue)?;
        if changed { self.save(id, &mut queue).await?; }
        drop(guard);
        if changed { self.changed(id).await; }
        Ok(result)
    }

    pub(crate) async fn list(
        &self,
        params: ThreadQueueListParams,
    ) -> Result<ThreadQueueListResponse, JSONRPCErrorError> {
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let _guard = self.lock(id).await?;
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
        let (queued_submission, added) = self.mutate(id, |queue| {
            queue.add(params).map(|(item, changed)| ((item, changed), changed))
        }).await?;
        if added {
            self.origins
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((id, queued_submission.id.clone()), origin.clone());
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
        let id = parse_thread_id_for_request(&params.thread_id)?;
        let queued_submission = self
            .mutate(id, |queue| {
                if queue.pending_start.as_ref().is_some_and(|pending| pending.submission_id == params.queued_submission_id) {
                    return Err(invalid_request("queued submission is starting; input is still owned by the queue"));
                }
                let item = queue
                    .submissions
                    .iter_mut()
                    .find(|item| item.id == params.queued_submission_id)
                    .ok_or_else(|| invalid_params("queued submission not found"))?;
                let changed = item.input != params.input;
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
        let deleted = self
            .mutate(id, |queue| {
                if queue.pending_start.as_ref().is_some_and(|pending| pending.submission_id == params.queued_submission_id) {
                    return Err(invalid_request("queued submission is starting; input is still owned by the queue"));
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
            self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&(id, params.queued_submission_id));
        }
        Ok(ThreadQueueDeleteResponse { deleted })
    }

    pub(crate) async fn reorder(
        &self,
        params: ThreadQueueReorderParams,
    ) -> Result<ThreadQueueReorderResponse, JSONRPCErrorError> {
        let id = parse_thread_id_for_request(&params.thread_id)?;
        self.mutate(id, |queue| queue.reorder(&params.queued_submission_ids).map(|changed| ((), changed)))
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
        let guard = self.lock(id).await?;
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
        let mut origin = match origin.or_else(|| self.origins.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(id, item.id.clone())).cloned()) {
            Some(origin) => origin,
            None => return Ok(None),
        };
        let connections = self.thread_state_manager.subscribed_connection_ids(id).await;
        if selected.is_none() && !connections.contains(&origin.request_id.connection_id) { return Ok(None); }
        self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((id, item.id.clone()), origin.clone());
        if selected.is_none() {
            origin.request_id.request_id = RequestId::String(format!("thread-queue-{}", uuid::Uuid::new_v4()));
        }
        let thread = self
            .thread_manager
            .get_thread(id)
            .await
            .map_err(queue_error)?;
        let turn_id = thread.reserve_turn_id();
        queue.paused = false;
        queue.pending_start = Some(PendingStart {
            submission_id: item.id.clone(),
            turn_id: turn_id.clone(),
        });
        self.save(id, &mut queue).await?;
        let result = self
            .turn_processor
            .start_queued_turn(
                origin.request_id,
                TurnStartParams {
                    thread_id: id.to_string(),
                    client_user_message_id: item.client_user_message_id,
                    input: item.input,
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
                self.changed(id).await;
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
            .retain(|(thread_id, _), _| *thread_id != id);
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
        if !self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys().any(|(thread_id, _)| *thread_id == id) { return; }
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
        if !self.origins.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys().any(|(thread_id, _)| *thread_id == id) { return; }
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

// A TurnStarted record alone says nothing about recoverable input ownership.
fn pending_input_recorded(items: &[RolloutItem], turn_id: &str) -> bool {
    let mut in_turn = false;
    for item in items {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => in_turn = event.turn_id == turn_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str) -> Vec<V2UserInput> {
        vec![V2UserInput::Text {
            text: text.to_string(),
            text_elements: vec![],
        }]
    }

    fn add(queue: &mut Queue, text: &str) -> QueuedSubmission {
        queue
            .add(ThreadQueueAddParams {
                thread_id: "unused".to_string(),
                client_user_message_id: Some(text.to_string()),
                input: input(text),
            })
            .expect("queue add").0
    }

    #[test]
    fn thread_queue_preserves_input_and_deduplicates_client_retries() {
        let mut queue = Queue::default();
        let first = add(&mut queue, "first");
        let params = ThreadQueueAddParams {
            thread_id: "unused".into(), client_user_message_id: Some("first".into()), input: input("first"),
        };
        let (retry, changed) = queue.add(params.clone()).unwrap();
        assert_eq!(retry, first);
        assert!(!changed);
        assert!(queue.add(ThreadQueueAddParams { input: input("different"), ..params.clone() }).is_err());
        assert_eq!(queue.submissions, vec![first.clone()]);
        queue.accepted.push(AcceptedSubmission {
            submission: queue.submissions.remove(0), turn_id: "accepted-turn".into(),
        });
        let mut restored: Queue = serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
        let (retry, changed) = restored.add(params.clone()).unwrap();
        assert_eq!(retry, first);
        assert!(!changed);
        assert!(restored.submissions.is_empty());
        assert!(restored.add(ThreadQueueAddParams { input: input("different"), ..params }).is_err());
    }

    #[test]
    fn queue_ownership_requires_input_not_just_worker_start() {
        let start = |id: &str| RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: id.into(), trace_id: None, started_at: None,
                model_context_window: None, collaboration_mode_kind: Default::default(),
            }
        ));
        let input = RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent { message: "prompt".into(), ..Default::default() }
        ));
        assert!(!pending_input_recorded(&[start("turn")], "turn"));
        assert!(!pending_input_recorded(&[start("turn"), start("other"), input.clone()], "turn"));
        assert!(pending_input_recorded(&[start("turn"), input], "turn"));
    }

    #[test]
    fn queue_ownership_recognizes_paginated_input_only_for_its_turn() {
        use codex_protocol::items::{TurnItem, UserMessageItem};
        use codex_protocol::protocol::{ItemCompletedEvent, ItemStartedEvent};

        let item = TurnItem::UserMessage(UserMessageItem::new(&[]));
        let thread_id = ThreadId::new();
        let started = RolloutItem::EventMsg(EventMsg::ItemStarted(ItemStartedEvent {
            thread_id, turn_id: "turn".into(), item: item.clone(), started_at_ms: 0,
        }));
        let completed = RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id, turn_id: "turn".into(), item, completed_at_ms: 0,
        }));
        assert!(!pending_input_recorded(&[started], "turn"));
        assert!(!pending_input_recorded(std::slice::from_ref(&completed), "other"));
        assert!(pending_input_recorded(&[completed], "turn"));
    }

    #[test]
    fn thread_queue_reorder_is_atomic_and_preserves_omitted_items() {
        let mut queue = Queue::default();
        let first = add(&mut queue, "first");
        let second = add(&mut queue, "second");
        let third = add(&mut queue, "third");
        queue
            .reorder(std::slice::from_ref(&third.id))
            .expect("partial reorder");
        let expected = vec![third.clone(), first, second];
        assert_eq!(queue.submissions, expected);
        assert!(queue.reorder(&[third.id.clone(), third.id]).is_err());
        assert!(queue.reorder(&["unknown".to_string()]).is_err());
        assert_eq!(queue.submissions, expected);
    }

    #[test]
    fn thread_queue_pagination_accepts_noops_and_rejects_stale_or_invalid_cursors() {
        let mut queue = Queue::default();
        let first = add(&mut queue, "first");
        let second = add(&mut queue, "second");
        let mut params = ThreadQueueListParams {
            thread_id: "unused".to_string(),
            cursor: None,
            limit: Some(1),
        };
        let page = queue.page(&params).expect("first page");
        assert_eq!(page.data, vec![first.clone()]);
        params.cursor = page.next_cursor;
        assert_eq!(params.cursor.as_deref(), Some("0:1"));
        let (_, changed) = queue.add(ThreadQueueAddParams {
            thread_id: "unused".into(), client_user_message_id: Some("first".into()), input: input("first"),
        }).unwrap();
        assert!(!changed);
        assert!(!queue.reorder(&[first.id]).unwrap());
        let page = queue.page(&params).expect("last page");
        assert_eq!(page.data, vec![second]);
        assert_eq!(page.next_cursor, None);
        queue.revision += 1;
        assert!(queue.page(&params).is_err());
        for cursor in ["garbage", "x:1", "1:x", "1:3"] {
            params.cursor = Some(cursor.to_string());
            assert!(queue.page(&params).is_err(), "cursor={cursor}");
        }
    }

    #[test]
    fn thread_queue_rejects_empty_and_remote_image_input() {
        assert!(TurnRequestProcessor::validate_queue_input(&[]).is_err());
        let input: Vec<V2UserInput> = serde_json::from_value(serde_json::json!([
            {"type":"image", "url":"https://example.com/image.png"}
        ]))
        .expect("image input");
        assert!(TurnRequestProcessor::validate_queue_input(&input).is_err());
    }
}
