mod snapshot;
mod types;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value as JsonValue;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) use self::types::CellEvent;
pub(crate) use self::types::CellId;
pub(crate) use self::types::CreateCellRequest;
pub(crate) use self::types::Error;
pub(crate) use self::types::ImageDetail;
use codex_code_mode_protocol::NestedCancellation;

pub(crate) use self::types::NestedToolCall;
pub(crate) use self::types::ObserveMode;
pub(crate) use self::types::OutputItem;
pub(crate) use self::types::SessionRuntimeDelegate;
pub(crate) use self::types::ToolDefinition;
pub(crate) use self::types::ToolKind;
pub(crate) use self::types::ToolName;
use crate::TaskFailureHandler;
use crate::cell_actor::CellActor;
use crate::cell_actor::CellError;
use crate::cell_actor::CellEventFuture;
use crate::cell_actor::CellHandle;
use crate::cell_actor::CellHost;
use crate::cell_actor::CellState;
use crate::cell_actor::CellToolCall;
use crate::cell_actor::CompletionCommit;
use crate::runtime::StoredValue;
use crate::runtime::stored_value_limit_message;
use crate::runtime::stored_values_with_writes_within_limits;

type RuntimeEventFuture = Pin<Box<dyn Future<Output = Result<CellEvent, Error>> + Send + 'static>>;
const TERMINAL_CELL_CACHE_CAPACITY: usize = 256;
/// Output retained for re-observing delivered terminal events, matching the
/// session's stored-value budget. Oversized events spill to owned temporary files.
const TERMINAL_CELL_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACTIVE_CELLS: usize = 8;
const CELL_INPUT_BYTES_PER_SLOT: usize = 2 * 1024 * 1024;

fn cell_admission_slots(input_bytes: usize, capacity: usize) -> u32 {
    // Charge known input pressure, not a model's claimed workload or an
    // estimate of JS execution time. Retain a hard isolate-count ceiling.
    (1 + input_bytes / CELL_INPUT_BYTES_PER_SLOT).min(capacity) as u32
}

#[derive(Default)]
struct TerminalCellCache {
    events: HashMap<CellId, (Arc<CachedCellEvent>, usize)>,
    order: VecDeque<CellId>,
    retained_bytes: usize,
}

enum CachedCellEvent {
    Inline(CellEvent),
    Spilled(tempfile::NamedTempFile),
}

// serde_json emits many small writes. Flush explicitly so an I/O error cannot
// be hidden by BufWriter::drop before a receipt or snapshot is published.
fn write_buffered_json(writer: impl Write, value: &impl serde::Serialize) -> Result<(), String> {
    let mut writer = BufWriter::new(writer);
    serde_json::to_writer(&mut writer, value).map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

impl CachedCellEvent {
    // Called off the async runtime and outside the cell registry/cache locks.
    fn new(event: CellEvent) -> (Arc<Self>, usize) {
        let bytes = cell_event_bytes(&event);
        if bytes > TERMINAL_CELL_CACHE_MAX_BYTES {
            let spilled = (|| -> Result<tempfile::NamedTempFile, String> {
                let mut file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
                write_buffered_json(file.as_file_mut(), &event)?;
                Ok(file)
            })();
            match spilled {
                Ok(file) => return (Arc::new(Self::Spilled(file)), 0),
                Err(error) => {
                    // Preserve the evidence on storage failure rather than
                    // silently turning a completed cell into an unknown one.
                    tracing::warn!(%error, bytes, "terminal cache spill failed; retaining inline");
                }
            }
        }
        (Arc::new(Self::Inline(event)), bytes)
    }

    fn read(&self) -> Result<CellEvent, Error> {
        match self {
            Self::Inline(event) => Ok(event.clone()),
            Self::Spilled(file) => file.reopen()
                .map_err(|error| Error::Runtime(format!(
                    "terminal cell result is unavailable: {error}; the cell completed; do not replay its effects"
                )))
                .and_then(|file| serde_json::from_reader(BufReader::new(file)).map_err(|error| Error::Runtime(
                    format!("terminal cell result cannot be decoded: {error}; do not replay its effects")
                ))),
        }
    }
}

impl TerminalCellCache {
    #[cfg(test)]
    fn get(&self, cell_id: &CellId) -> Option<CellEvent> {
        self.entry(cell_id).and_then(|event| event.read().ok())
    }

    fn entry(&self, cell_id: &CellId) -> Option<Arc<CachedCellEvent>> {
        self.events.get(cell_id).map(|(event, _)| Arc::clone(event))
    }

    #[cfg(test)]
    fn insert(&mut self, cell_id: CellId, event: CellEvent) {
        self.insert_cached(cell_id, CachedCellEvent::new(event));
    }

    fn insert_cached(&mut self, cell_id: CellId, (event, bytes): (Arc<CachedCellEvent>, usize)) {
        if let Some((_, replaced)) = self.events.insert(cell_id.clone(), (event, bytes)) {
            self.retained_bytes = self.retained_bytes.saturating_sub(replaced);
        } else {
            self.order.push_back(cell_id);
        }
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        while self.order.len() > TERMINAL_CELL_CACHE_CAPACITY
            || (self.order.len() > 1 && self.retained_bytes > TERMINAL_CELL_CACHE_MAX_BYTES)
        {
            let Some(expired) = self.order.pop_front() else {
                break;
            };
            if let Some((_, expired_bytes)) = self.events.remove(&expired) {
                self.retained_bytes = self.retained_bytes.saturating_sub(expired_bytes);
            }
        }
    }
}

fn cell_event_bytes(event: &CellEvent) -> usize {
    let (content_items, error_text) = match event {
        CellEvent::Yielded { content_items }
        | CellEvent::ExplicitYield { content_items }
        | CellEvent::Terminated { content_items } => (content_items, None),
        CellEvent::Completed {
            content_items,
            error_text,
            ..
        } => (content_items, error_text.as_deref()),
    };
    content_items
        .iter()
        .map(|item| match item {
            OutputItem::Text { text } => text.len(),
            OutputItem::Image { image_url, .. } => image_url.len(),
        })
        .fold(error_text.map_or(0, str::len), usize::saturating_add)
}

/// Owns all cells and shared state for one transport-neutral code-mode session.
pub(crate) struct SessionRuntime<D: SessionRuntimeDelegate> {
    inner: Arc<Inner<D>>,
}

struct Inner<D: SessionRuntimeDelegate> {
    // Cells snapshot keys but share immutable payloads; later commits cannot change their view.
    stored_values: Mutex<HashMap<String, StoredValue>>,
    durable_state: tokio::sync::OnceCell<Arc<snapshot::DurableState>>,
    // Initialization excludes only snapshot/permit admission, never native
    // actor startup or execution. This closes the restore-versus-start race.
    state_admission: tokio::sync::RwLock<()>,
    cells: Mutex<HashMap<CellId, CellHandle>>,
    terminal_cells: StdMutex<TerminalCellCache>,
    active_cell_permits: Arc<Semaphore>,
    active_cell_capacity: usize,
    cell_tasks: TaskTracker,
    shutdown_token: CancellationToken,
    delegate: Arc<D>,
    task_failure_handler: Option<TaskFailureHandler>,
    next_cell_id: AtomicU64,
}

impl<D: SessionRuntimeDelegate> SessionRuntime<D> {
    pub(crate) fn new(delegate: Arc<D>) -> Self {
        Self::new_with_task_failure_handler(delegate, /*task_failure_handler*/ None)
    }

    pub(crate) fn new_with_task_failure_handler(
        delegate: Arc<D>,
        task_failure_handler: Option<TaskFailureHandler>,
    ) -> Self {
        crate::runtime::prewarm_runtime();
        let active_cell_capacity = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .clamp(2, MAX_ACTIVE_CELLS);
        Self {
            inner: Arc::new(Inner {
                stored_values: Mutex::new(HashMap::new()),
                durable_state: tokio::sync::OnceCell::new(),
                state_admission: tokio::sync::RwLock::new(()),
                cells: Mutex::new(HashMap::new()),
                terminal_cells: StdMutex::new(TerminalCellCache::default()),
                active_cell_permits: Arc::new(Semaphore::new(active_cell_capacity)),
                active_cell_capacity,
                cell_tasks: TaskTracker::new(),
                shutdown_token: CancellationToken::new(),
                delegate,
                task_failure_handler,
                next_cell_id: AtomicU64::new(1),
            }),
        }
    }

    pub(crate) async fn execute(
        &self,
        mut request: CreateCellRequest,
        initial_observe_mode: ObserveMode,
    ) -> Result<StartedCell, Error> {
        if self.inner.shutdown_token.is_cancelled() {
            return Err(Error::ShuttingDown);
        }
        let persistence_notice = self.restore_durable_state(request.state_path.as_ref()).await?;
        if persistence_notice.is_some() {
            request.state_path = None;
        }
        let admission = self.inner.state_admission.read().await;
        let cell_id = self.allocate_cell_id()?;
        let initial_event = self
            .start_cell(cell_id.clone(), request, initial_observe_mode, admission)
            .await?;
        let initial_event: RuntimeEventFuture = Box::pin(async move {
            let mut event = initial_event.await?;
            if let Some(notice) = persistence_notice {
                let (CellEvent::Yielded { content_items }
                    | CellEvent::ExplicitYield { content_items }
                    | CellEvent::Completed { content_items, .. }
                    | CellEvent::Terminated { content_items }) = &mut event;
                content_items.insert(0, OutputItem::Text { text: notice.to_string() });
            }
            Ok(event)
        });
        Ok(StartedCell {
            cell_id,
            initial_event,
        })
    }

    pub(crate) async fn begin_observe(
        &self,
        cell_id: &CellId,
        mode: ObserveMode,
    ) -> Result<PendingEvent, Error> {
        let handle = self.inner.cells.lock().await.get(cell_id).cloned();
        let Some(handle) = handle else {
            let event = self.cached_terminal_event(cell_id).await?;
            return Ok(PendingEvent {
                event: Box::pin(async move { Ok(event) }),
            });
        };
        Ok(PendingEvent {
            event: map_actor_event(cell_id.clone(), handle.observe(mode)),
        })
    }

    pub(crate) async fn terminate(&self, cell_id: &CellId) -> Result<CellEvent, Error> {
        let handle = self.inner.cells.lock().await.get(cell_id).cloned();
        let Some(handle) = handle else {
            return self.cached_terminal_event(cell_id).await;
        };
        handle
            .terminate()
            .await
            .map_err(|error| actor_error(cell_id, error))
    }

    pub(crate) async fn shutdown(&self) -> Result<(), Error> {
        self.begin_shutdown();
        // Taking the registry lock ensures every cell that passed the shutdown
        // check has registered its actor with the tracker before we wait.
        let cells = self.inner.cells.lock().await;
        self.inner.cell_tasks.close();
        drop(cells);
        self.inner.cell_tasks.wait().await;
        Ok(())
    }

    async fn cached_terminal_event(&self, cell_id: &CellId) -> Result<CellEvent, Error> {
        let event = self.inner.terminal_cells.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(cell_id)
            .ok_or_else(|| Error::MissingCell(cell_id.clone()))?;
        tokio::task::spawn_blocking(move || event.read()).await
            .map_err(|error| Error::Runtime(format!(
                "terminal cell recovery failed: {error}; do not replay its effects"
            )))?
    }

    fn allocate_cell_id(&self) -> Result<CellId, Error> {
        self.inner
            .next_cell_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next_cell_id| {
                if self.inner.durable_state.get()
                    .is_some_and(|state| next_cell_id >= state.cell_id_limit)
                {
                    return None;
                }
                next_cell_id.checked_add(1)
            })
            .map(|cell_id| CellId::new(cell_id.to_string()))
            .map_err(|_| Error::CellIdSpaceExhausted)
    }

    async fn restore_durable_state(
        &self,
        path: Option<&std::path::PathBuf>,
    ) -> Result<Option<&'static str>, Error> {
        let Some(path) = path else { return Ok(None) };
        let selected_path = path.clone();
        let _admission = if self.inner.durable_state.get().is_none() {
            Some(self.inner.state_admission.write().await)
        } else {
            None
        };
        // Check under exclusive admission before opening (and reserving IDs in)
        // a durable snapshot. Never replace values or active cells to opt in late.
        if self.inner.durable_state.get().is_none()
            && (!self.inner.stored_values.lock().await.is_empty()
                || self.inner.active_cell_permits.available_permits() != self.inner.active_cell_capacity)
        {
            return Ok(Some("Named-state persistence was not enabled: existing values or active cells must be preserved. This cell runs with in-memory state only; it is not durable across restart."));
        }
        let state = self.inner.durable_state.get_or_try_init(|| async {
            let (state, restored, completed) = tokio::task::spawn_blocking(move || {
                let (state, restored) = snapshot::DurableState::open(selected_path)?;
                let completed = state.completed_cells().into_iter().map(|(id, event)| {
                    (id, CachedCellEvent::new(event))
                }).collect::<Vec<_>>();
                Ok::<_, String>((state, restored, completed))
            }).await.map_err(|error| Error::Runtime(error.to_string()))?
                .map_err(Error::Runtime)?;
            let mut values = self.inner.stored_values.lock().await;
            let next_id = state.first_cell_id;
            let mut terminal = self.inner.terminal_cells.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let allocated_before = self.inner.next_cell_id.load(Ordering::Acquire);
            if completed.iter().any(|(id, _)| {
                id.parse::<u64>().is_ok_and(|id| id < allocated_before)
            }) {
                return Err(Error::Runtime(
                    "durable cell IDs overlap cells already observed in this runtime; enable persistence before starting cells; no live state was replaced".into()
                ));
            }
            for (id, event) in completed {
                terminal.insert_cached(CellId::new(id), event);
            }
            self.inner.next_cell_id.fetch_max(next_id, Ordering::AcqRel);
            *values = restored;
            Ok(Arc::new(state))
        }).await?;
        if &state.path != path {
            return Err(Error::Runtime("a runtime cannot switch named-state snapshot paths".into()));
        }
        Ok(None)
    }

    async fn start_cell(
        &self,
        cell_id: CellId,
        request: CreateCellRequest,
        initial_observe_mode: ObserveMode,
        admission: tokio::sync::RwLockReadGuard<'_, ()>,
    ) -> Result<RuntimeEventFuture, Error> {
        // Yielded cells retain their permits until they finish. Waiting here can
        // prevent the caller from ever issuing the wait/terminate that frees one.
        let stored_values = self.inner.stored_values.lock().await.clone();
        let input_bytes = stored_values.values().fold(request.source.len(), |bytes, value| {
            bytes.saturating_add(value.bytes)
        });
        let slots = cell_admission_slots(input_bytes, self.inner.active_cell_capacity);
        let cell_permit = match Arc::clone(&self.inner.active_cell_permits)
            .try_acquire_many_owned(slots)
        {
            Ok(cell_permit) => cell_permit,
            Err(tokio::sync::TryAcquireError::Closed) => return Err(Error::ShuttingDown),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                let mut active = self
                    .inner
                    .cells
                    .lock()
                    .await
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                active.sort_by_key(|cell_id| cell_id.as_str().parse::<u64>().unwrap_or(u64::MAX));
                return Err(Error::ActiveCellLimit(active));
            }
        };
        let host = Arc::new(RuntimeCellHost {
            cell_id: cell_id.clone(),
            parent_tool_call_id: request.tool_call_id.clone(),
            snapshot: stored_values.clone(),
            inner: Arc::clone(&self.inner),
            cell_permit: Mutex::new(Some(cell_permit)),
        });
        drop(admission);
        let cell_state = Arc::new(CellState::new(self.inner.shutdown_token.child_token()));
        // Prepare outside the registry lock: native startup can take seconds and
        // must not block observing, terminating, or closing other cells.
        let (handle, initial_event, task) = tokio::select! {
            biased;
            _ = self.inner.shutdown_token.cancelled() => return Err(Error::ShuttingDown),
            prepared = CellActor::prepare(
                request,
                stored_values,
                host,
                initial_observe_mode,
                cell_state,
                self.inner.task_failure_handler.clone(),
            ) => prepared.map_err(Error::Runtime)?,
        };
        // Shutdown cancels its token before taking this lock, so a cell published
        // here is tracked before shutdown waits. Otherwise dropping the unspawned
        // task terminates its runtime and releases the permit.
        let mut cells = self.inner.cells.lock().await;
        if self.inner.shutdown_token.is_cancelled() {
            return Err(Error::ShuttingDown);
        }
        if cells.contains_key(&cell_id) {
            return Err(Error::DuplicateCell(cell_id));
        }
        cells.insert(cell_id.clone(), handle);
        let task = self.inner.cell_tasks.spawn(task);
        if let Some(task_failure_handler) = self.inner.task_failure_handler.clone() {
            let failed_cell_id = cell_id.clone();
            let _failure_watcher = self.inner.cell_tasks.spawn(async move {
                if let Err(err) = task.await {
                    task_failure_handler(format!(
                        "code-mode cell {failed_cell_id} task failed: {err}"
                    ));
                }
            });
        }
        drop(cells);
        Ok(map_actor_event(cell_id, initial_event))
    }

    fn begin_shutdown(&self) {
        self.inner.shutdown_token.cancel();
        self.inner.cell_tasks.close();
    }
}

impl<D: SessionRuntimeDelegate> Drop for SessionRuntime<D> {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

/// A cell admitted by [`SessionRuntime::execute`].
pub(crate) struct StartedCell {
    pub(crate) cell_id: CellId,
    initial_event: RuntimeEventFuture,
}

impl StartedCell {
    pub(crate) async fn initial_event(self) -> Result<CellEvent, Error> {
        self.initial_event.await
    }
}

/// An admitted observation that has not reached its requested frontier yet.
pub(crate) struct PendingEvent {
    event: RuntimeEventFuture,
}

impl PendingEvent {
    pub(crate) async fn event(self) -> Result<CellEvent, Error> {
        self.event.await
    }
}

struct RuntimeCellHost<D: SessionRuntimeDelegate> {
    cell_id: CellId,
    parent_tool_call_id: String,
    snapshot: HashMap<String, StoredValue>,
    inner: Arc<Inner<D>>,
    cell_permit: Mutex<Option<OwnedSemaphorePermit>>,
}

impl<D: SessionRuntimeDelegate> CellHost for RuntimeCellHost<D> {
    async fn invoke_tool(
        &self,
        invocation: CellToolCall,
        cancellation: NestedCancellation,
    ) -> Result<JsonValue, String> {
        self.inner
            .delegate
            .invoke_tool(
                NestedToolCall {
                    cell_id: self.cell_id.clone(),
                    parent_tool_call_id: self.parent_tool_call_id.clone(),
                    runtime_tool_call_id: invocation.id,
                    tool_name: invocation.name,
                    tool_kind: invocation.kind,
                    input: invocation.input,
                    nested_deadline: invocation.deadline,
                },
                cancellation,
            )
            .await
    }

    async fn notify(
        &self,
        call_id: String,
        text: String,
        cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        self.inner
            .delegate
            .notify(call_id, self.cell_id.clone(), text, cancellation_token)
            .await
    }

    async fn commit_completion(
        &self,
        mut stored_value_writes: HashMap<String, StoredValue>,
        event: CellEvent,
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        cell_state: Arc<CellState>,
    ) -> CompletionCommit {
        let cancellation_token = cell_state.cancellation_token();
        let mut stored_values = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                return CompletionCommit::Rejected(event);
            }
            stored_values = self.inner.stored_values.lock() => stored_values,
        };
        let writes_fit =
            stored_values_with_writes_within_limits(&stored_values, &stored_value_writes);
        let conflicting_write = stored_value_writes.keys().any(|key| {
            // Payload identity is a per-key revision: snapshots retain the Arc,
            // and every committed store allocates a new one. Comparing values
            // would miss ABA writes and two increments producing the same value.
            match (stored_values.get(key), self.snapshot.get(key)) {
                (None, None) => false,
                (Some(current), Some(original)) => !Arc::ptr_eq(&current.value, &original.value),
                _ => true,
            }
        });
        let changed = |key: &String| match (stored_values.get(key), self.snapshot.get(key)) {
            (None, None) => false,
            (Some(current), Some(original)) => !Arc::ptr_eq(&current.value, &original.value),
            _ => true,
        };
        // All writes from a cell share its captured read set. Read-only cells
        // retain snapshot semantics; cells committing state reject stale reads.
        let conflicting_read = stored_value_writes.values().next().is_some_and(|write| {
            match &write.read_dependencies {
                Some(reads) => reads.iter().any(&changed),
                None => stored_values.keys().chain(self.snapshot.keys()).any(&changed),
            }
        });
        let event = if conflicting_write || conflicting_read {
            storage_failure_event(event, "code-mode store conflict: another cell changed a read or written key; no stored values from this cell were committed. Nested tool effects may already have occurred; do not replay the cell blindly.".into())
        } else if writes_fit {
            event
        } else {
            storage_rejected_event(event)
        };
        let durable = self.inner.durable_state.get().cloned();
        let staged = if let Some(durable) = durable.clone() {
            let mut values = stored_values.clone();
            if writes_fit && !conflicting_write && !conflicting_read {
                values.extend(stored_value_writes.clone());
            }
            // A rejected transaction is still a completed cell with a
            // recovery receipt. Retain it without committing its writes.
            let call_id = self.parent_tool_call_id.clone();
            let cell_id = self.cell_id.to_string();
            let completed = event.clone();
            match tokio::task::spawn_blocking(move || durable.stage(call_id, values, cell_id, completed)).await {
                Ok(result) => result.map(Some),
                Err(error) => Err(error.to_string()),
            }
        } else {
            Ok(None)
        };
        cell_state.commit_completion_with_event(event, pending_initial_yield_items, |event| {
            let staged = match staged {
                Ok(staged) => staged,
                Err(error) => return storage_failure_event(event, format!("snapshot staging failed: {error}")),
            };
            if let (Some(durable), Some(staged)) = (durable, staged)
                && let Err(error) = durable.publish(staged)
            {
                return storage_failure_event_with_status(event, format!("snapshot publication failed: {error}"), "unknown");
            }
            if writes_fit && !conflicting_write && !conflicting_read {
                // Read sets belong to this transaction, not retained values.
                for value in stored_value_writes.values_mut() {
                    value.read_dependencies = None;
                }
                stored_values.extend(stored_value_writes);
            }
            event
        })
    }

    async fn closed(&self, event: Option<CellEvent>) {
        let cached = match event {
            Some(event) => match tokio::task::spawn_blocking(move || CachedCellEvent::new(event)).await {
                Ok(cached) => Some(cached),
                Err(error) => Some(CachedCellEvent::new(CellEvent::Completed {
                    content_items: Vec::new(),
                    error_text: Some(format!(
                        "terminal result retention failed: {error}; the cell completed; do not replay its effects"
                    )),
                    output_loss: None,
                })),
            },
            None => None,
        };
        {
            let mut cells = self.inner.cells.lock().await;
            let removed = cells.remove(&self.cell_id);
            if removed.is_none() {
                return;
            }
            if let Some(cached) = cached {
                self.inner
                    .terminal_cells
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert_cached(self.cell_id.clone(), cached);
            }
        }
        self.cell_permit.lock().await.take();
        self.inner.delegate.cell_closed(&self.cell_id);
    }
}

fn storage_rejected_event(event: CellEvent) -> CellEvent {
    storage_failure_event(event, stored_value_limit_message())
}

fn storage_failure_event(event: CellEvent, limit_error: String) -> CellEvent {
    storage_failure_event_with_status(event, limit_error, "rejected")
}

fn storage_failure_event_with_status(event: CellEvent, limit_error: String, status: &str) -> CellEvent {
    match event {
        CellEvent::Completed {
            content_items,
            error_text,
            output_loss,
        } => {
            // This is a rejected local transaction, not rollback of nested
            // tools. Preserve the distinction across both session transports.
            let error_text = Some(serde_json::json!({
                "kind": "code_mode_store_commit_failure",
                "version": 1,
                "store_commit": status,
                "message": limit_error,
                "script_error": error_text,
                "external_effects_rolled_back": false,
                "automatic_replay_allowed": false,
                "recovery": "inspect retained nested call receipts; recompute only the rejected named-state transaction",
            }).to_string());
            CellEvent::Completed {
                content_items,
                error_text,
                output_loss,
            }
        }
        event => event,
    }
}

fn map_actor_event(cell_id: CellId, event: CellEventFuture) -> RuntimeEventFuture {
    Box::pin(async move { event.await.map_err(|error| actor_error(&cell_id, error)) })
}

fn actor_error(cell_id: &CellId, error: CellError) -> Error {
    match error {
        CellError::Busy => Error::BusyObserver(cell_id.clone()),
        CellError::AlreadyTerminating => Error::AlreadyTerminating(cell_id.clone()),
        CellError::Closed => Error::ClosedCell(cell_id.clone()),
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
