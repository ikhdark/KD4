use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::error_code::FS_WATCH_CAPACITY_ERROR_CODE;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol_types::FsChangedNotification;
use codex_app_server_protocol_types::FsUnwatchParams;
use codex_app_server_protocol_types::FsUnwatchResponse;
use codex_app_server_protocol_types::FsWatchParams;
use codex_app_server_protocol_types::FsWatchResponse;
use codex_app_server_protocol_types::JSONRPCErrorError;
use codex_app_server_protocol_types::ServerNotification;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::ThrottledWatchReceiver;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const FS_CHANGED_NOTIFICATION_DEBOUNCE: Duration = Duration::from_millis(200);
const FS_WATCH_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const FS_WATCH_INITIALIZATION_COOLDOWN: Duration = Duration::from_secs(1);
// Both pending and live watches count. Bound retained metadata as well as tasks.
const FS_WATCHES_PER_CONNECTION: usize = 128;
const FS_WATCHES_PER_SERVER: usize = 1024;
const FS_WATCH_ID_MAX_BYTES: usize = 1024;
const FS_WATCH_PATH_MAX_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub(crate) struct FsWatchManager {
    outgoing: Arc<OutgoingMessageSender>,
    file_watcher: Arc<std::sync::OnceLock<Arc<FileWatcher>>>,
    initialization_failure: Arc<std::sync::Mutex<Option<Instant>>>,
    state: Arc<AsyncMutex<FsWatchState>>,
    setup_slots: Arc<tokio::sync::Semaphore>,
}

#[derive(Default)]
struct FsWatchState {
    entries: HashMap<WatchKey, WatchEntry>,
    next_reservation_id: u64,
}

impl FsWatchState {
    fn remove_pending_reservation(&mut self, key: &WatchKey, id: u64) {
        if matches!(
            self.entries.get(key),
            Some(WatchEntry::Pending { reservation_id, .. }) if *reservation_id == id
        ) {
            self.entries.remove(key);
        }
    }

    fn remove_cancelled_pending(&mut self) {
        self.entries.retain(|_, entry| {
            !matches!(
                entry,
                WatchEntry::Pending { cancellation, .. } if cancellation.is_cancelled()
            )
        });
    }
}

enum WatchEntry {
    Pending { reservation_id: u64, cancellation: CancellationToken },
    Active(ActiveWatchEntry),
}

struct ActiveWatchEntry {
    reservation_id: u64,
    cancellation: CancellationToken,
    abort_handle: AbortHandle,
    done_rx: oneshot::Receiver<()>,
    native: NativeWatchHandles,
}

struct NativeWatchHandles(Option<(FileWatcherSubscriber, WatchRegistration)>);

impl NativeWatchHandles {
    async fn stop(mut self) {
        if let Some(handles) = self.0.take() {
            let _ = tokio::task::spawn_blocking(move || drop(handles)).await;
        }
    }
}

impl Drop for NativeWatchHandles {
    fn drop(&mut self) {
        if let Some(handles) = self.0.take() {
            // A setup result may outlive its canceled RPC. Retain ownership of
            // late native teardown even when no caller can await its cleanup.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                drop(runtime.spawn_blocking(move || drop(handles)));
            } else {
                drop(handles);
            }
        }
    }
}

struct PendingReservation {
    manager: FsWatchManager,
    cancellation: CancellationToken,
    key: WatchKey,
    id: u64,
    armed: bool,
}

impl Drop for PendingReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Publish cancellation synchronously, even if the owning runtime is
        // stopping. The next admission removes abandoned pending entries
        // before counting capacity or rejecting a reused key.
        self.cancellation.cancel();
        if let Ok(mut state) = self.manager.state.try_lock() {
            state.remove_pending_reservation(&self.key, self.id);
        }
    }
}

impl WatchEntry {
    async fn stop(self) {
        match self {
            Self::Pending { cancellation, .. } => cancellation.cancel(),
            Self::Active(entry) => {
                entry.cancellation.cancel();
                let abort_handle = entry.abort_handle;
                // Bound forwarding and native teardown together. On timeout,
                // the blocking task still owns the native handles; dropping
                // its join handle does not cancel or abandon native teardown.
                if tokio::time::timeout(FS_WATCH_SHUTDOWN_GRACE, async move {
                    let _ = entry.done_rx.await;
                    entry.native.stop().await;
                }).await.is_err() {
                    abort_handle.abort();
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WatchKey {
    connection_id: ConnectionId,
    watch_id: String,
}

impl FsWatchManager {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>) -> Self {
        Self {
            outgoing,
            file_watcher: Arc::new(std::sync::OnceLock::new()),
            initialization_failure: Arc::new(std::sync::Mutex::new(None)),
            state: Arc::new(AsyncMutex::new(FsWatchState::default())),
            setup_slots: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }


    pub(crate) async fn watch_with_gate(
        &self,
        connection_id: ConnectionId,
        params: FsWatchParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<FsWatchResponse, JSONRPCErrorError> {
        let watch_id = params.watch_id;
        if watch_id.len() > FS_WATCH_ID_MAX_BYTES
            || params.path.as_os_str().len() > FS_WATCH_PATH_MAX_BYTES
        {
            return Err(invalid_params("filesystem watch ID or path exceeds its size limit"));
        }
        let watch_key = WatchKey {
            connection_id,
            watch_id: watch_id.clone(),
        };

        let cancellation = rpc_gate.cancellation_token().child_token();
        let reservation_id = {
            let mut state = self.state.lock().await;
            state.remove_cancelled_pending();
            let reservation_id = state.next_reservation_id;
            let reservation = rpc_gate.try_commit(|| {
                let limit = if state.entries.len() >= FS_WATCHES_PER_SERVER {
                    Some("server")
                } else if state.entries.keys()
                    .filter(|key| key.connection_id == connection_id)
                    .count() >= FS_WATCHES_PER_CONNECTION
                {
                    Some("connection")
                } else {
                    None
                };
                match state.entries.entry(watch_key.clone()) {
                    Entry::Occupied(_) => Err(invalid_request(format!(
                        "watchId already exists: {watch_id}"
                    ))),
                    Entry::Vacant(entry) => {
                        if let Some(scope) = limit {
                            return Err(JSONRPCErrorError {
                                code: FS_WATCH_CAPACITY_ERROR_CODE,
                                message: "filesystem watch capacity reached; unwatch an existing subscription before retrying".into(),
                                data: Some(serde_json::json!({
                                    "reason": "fsWatchCapacity",
                                    "scope": scope,
                                    "retryable": false,
                                })),
                            });
                        }
                        entry.insert(WatchEntry::Pending {
                            reservation_id,
                            cancellation: cancellation.clone(),
                        });
                        Ok(reservation_id)
                    }
                }
            });
            match reservation {
                None => return Err(invalid_request("connection is closed")),
                Some(Err(err)) => return Err(err),
                Some(Ok(reservation_id)) => {
                    state.next_reservation_id = state.next_reservation_id.wrapping_add(1);
                    reservation_id
                }
            }
        };

        let mut reservation = PendingReservation {
            manager: self.clone(),
            cancellation: cancellation.clone(),
            key: watch_key.clone(),
            id: reservation_id,
            armed: true,
        };
        let setup_guard = cancellation.clone().drop_guard();
        let setup_permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(invalid_request("connection is closed")),
            permit = self.setup_slots.clone().acquire_owned() => permit
                .map_err(|_| internal_error("filesystem watch setup is unavailable"))?,
        };
        let setup_manager = self.clone();
        let setup_path = params.path.clone();
        let setup_key = watch_key.clone();

        let setup_result = tokio::task::spawn_blocking(move || {
            let _setup_permit = setup_permit;
            if cancellation.is_cancelled() {
                return Err(invalid_request("watch registration was canceled"));
            }
            let file_watcher = setup_manager.file_watcher()?;
            if cancellation.is_cancelled() {
                return Err(invalid_request("watch registration was canceled"));
            }
            let outgoing = setup_manager.outgoing.clone();
            let (subscriber, rx) = file_watcher.add_subscriber();
            let watch_root = setup_path.clone();
            let registration = subscriber
                .register_paths(vec![WatchPath {
                    path: setup_path.to_path_buf(),
                    recursive: false,
                }])
                .map_err(|err| {
                    internal_error(format!("failed to register filesystem watch: {err}"))
                })?;
            let task_cancellation = cancellation.clone();
            let task_watch_id = watch_id.clone();
            let task_state = Arc::downgrade(&setup_manager.state);
            let task_key = setup_key;
            let (start_tx, start_rx) = oneshot::channel();
            let (done_tx, done_rx) = oneshot::channel();
            let task = tokio::spawn(async move {
                if start_rx.await.is_ok() {
                    let mut rx = ThrottledWatchReceiver::new(rx, FS_CHANGED_NOTIFICATION_DEBOUNCE);
                    loop {
                        let event = tokio::select! {
                            biased;
                            _ = task_cancellation.cancelled() => break,
                            event = rx.recv() => match event {
                                Some(event) => event,
                                None => {
                                    outgoing.fail_connection_delivery(connection_id).await;
                                    break;
                                },
                            },
                        };
                        let mut changed_paths = event
                            .paths
                            .into_iter()
                            .map(|path| watch_root.join(path))
                            .collect::<Vec<_>>();
                        if event.rescan_required {
                            changed_paths.push(watch_root.clone());
                        }
                        changed_paths.sort_by(|left, right| left.as_path().cmp(right.as_path()));
                        changed_paths.dedup();
                        if !changed_paths.is_empty()
                            && !outgoing
                                .send_server_notification_to_connection_bounded(
                                    connection_id,
                                    ServerNotification::FsChanged(FsChangedNotification {
                                        watch_id: task_watch_id.clone(),
                                        changed_paths,
                                    }),
                                    &task_cancellation,
                                )
                                .await
                        {
                            break;
                        }
                    }
                }
                // Completion belongs to this reservation only. An old worker
                // must never remove a later reuse of the connection/watch ID.
                if let Some(state) = task_state.upgrade() {
                    let retired = {
                        let mut state = state.lock().await;
                        if matches!(state.entries.get(&task_key),
                            Some(WatchEntry::Active(entry)) if entry.reservation_id == reservation_id)
                        {
                            state.entries.remove(&task_key)
                        } else {
                            None
                        }
                    };
                    if let Some(WatchEntry::Active(retired)) = retired {
                        retired.native.stop().await;
                    }
                }
                let _ = done_tx.send(());
            });
            let entry = ActiveWatchEntry {
                reservation_id,
                cancellation,
                abort_handle: task.abort_handle(),
                done_rx,
                native: NativeWatchHandles(Some((subscriber, registration))),
            };
            drop(task);
            Ok::<_, JSONRPCErrorError>((entry, start_tx))
        }).await.map_err(|err| internal_error(format!("filesystem watch setup worker failed: {err}")))?;
        let (entry, start_tx) = match setup_result {
            Ok(setup) => setup,
            Err(err) => {
                self.remove_pending_reservation(&watch_key, reservation_id)
                    .await;
                reservation.armed = false;
                return Err(err);
            }
        };
        let mut pending_entry = Some(entry);
        let commit_result = {
            let mut state = self.state.lock().await;
            rpc_gate.try_commit(|| {
                let Some(entry) = state.entries.get_mut(&watch_key) else {
                    return Err(invalid_request("watch registration was canceled"));
                };
                if !matches!(
                    entry,
                    WatchEntry::Pending {
                        reservation_id: current, ..
                    } if *current == reservation_id
                ) {
                    return Err(internal_error(
                        "watch reservation changed during registration",
                    ));
                }
                let Some(pending_entry) = pending_entry.take() else {
                    return Err(internal_error("watch entry missing during registration"));
                };
                *entry = WatchEntry::Active(pending_entry);
                Ok(())
            })
        };
        if commit_result.as_ref().is_some_and(Result::is_ok) {
            reservation.armed = false;
            setup_guard.disarm();
            let _ = start_tx.send(());
        } else {
            drop(start_tx);
        }
        if let Some(entry) = pending_entry {
            WatchEntry::Active(entry).stop().await;
        }
        self.remove_pending_reservation(&watch_key, reservation_id)
            .await;
        reservation.armed = false;
        commit_result.ok_or_else(|| invalid_request("connection is closed"))??;

        Ok(FsWatchResponse { path: params.path })
    }

    fn file_watcher(&self) -> Result<&Arc<FileWatcher>, JSONRPCErrorError> {
        if let Some(watcher) = self.file_watcher.get() {
            return Ok(watcher);
        }
        // Construction runs only in blocking setup workers. Publish success once,
        // but allow an explicit later request to retry a transient native failure.
        let mut failure = self
            .initialization_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.file_watcher.get().is_none() {
            if failure.is_some_and(|when| when.elapsed() < FS_WATCH_INITIALIZATION_COOLDOWN) {
                return Err(internal_error("filesystem watching is unavailable"));
            }
            match FileWatcher::new() {
                Ok(watcher) => {
                    let _ = self.file_watcher.set(Arc::new(watcher));
                    *failure = None;
                }
                Err(err) => {
                    *failure = Some(Instant::now());
                    warn!("filesystem watch manager unavailable: {err}");
                    return Err(internal_error("filesystem watching is unavailable"));
                }
            }
        }
        self.file_watcher
            .get()
            .ok_or_else(|| {
                internal_error("filesystem watch initialization did not publish a watcher")
            })
    }

    async fn remove_pending_reservation(&self, watch_key: &WatchKey, reservation_id: u64) {
        self.state
            .lock()
            .await
            .remove_pending_reservation(watch_key, reservation_id);
    }



    pub(crate) async fn unwatch(
        &self,
        connection_id: ConnectionId,
        params: FsUnwatchParams,
    ) -> Result<FsUnwatchResponse, JSONRPCErrorError> {
        let watch_key = WatchKey {
            connection_id,
            watch_id: params.watch_id,
        };
        let entry = self.state.lock().await.entries.remove(&watch_key);
        if let Some(entry) = entry {
            entry.stop().await;
        }
        Ok(FsUnwatchResponse {})
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let entries = self
            .state
            .lock()
            .await
            .entries
            .extract_if(|key, _| key.connection_id == connection_id)
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        futures::future::join_all(entries.into_iter().map(WatchEntry::stop)).await;
    }
}

