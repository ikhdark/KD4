use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::FsChangedNotification;
use codex_app_server_protocol::FsUnwatchParams;
use codex_app_server_protocol::FsUnwatchResponse;
use codex_app_server_protocol::FsWatchParams;
use codex_app_server_protocol::FsWatchResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::ThrottledWatchReceiver;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const FS_CHANGED_NOTIFICATION_DEBOUNCE: Duration = Duration::from_millis(200);
const FS_WATCH_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct FsWatchManager {
    outgoing: Arc<OutgoingMessageSender>,
    file_watcher: Option<Arc<FileWatcher>>,
    state: Arc<AsyncMutex<FsWatchState>>,
    setup_slots: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    watch_setup_count: Arc<AtomicUsize>,
    #[cfg(test)]
    setup_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct FsWatchState {
    entries: HashMap<WatchKey, WatchEntry>,
    next_reservation_id: u64,
}

enum WatchEntry {
    Pending { reservation_id: u64 },
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
    key: WatchKey,
    id: u64,
    armed: bool,
}

impl Drop for PendingReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let manager = self.manager.clone();
        let key = self.key.clone();
        let id = self.id;
        // This also covers cancellation while blocking setup is still running.
        drop(tokio::spawn(async move { manager.remove_pending_reservation(&key, id).await; }));
    }
}

impl WatchEntry {
    async fn stop(self) {
        if let Self::Active(entry) = self {
            entry.cancellation.cancel();
            if tokio::time::timeout(FS_WATCH_SHUTDOWN_GRACE, entry.done_rx)
                .await
                .is_err()
            {
                entry.abort_handle.abort();
            }
            entry.native.stop().await;
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
        let file_watcher = match FileWatcher::new() {
            Ok(file_watcher) => Some(Arc::new(file_watcher)),
            Err(err) => {
                warn!("filesystem watch manager unavailable: {err}");
                None
            }
        };
        Self {
            outgoing,
            file_watcher,
            state: Arc::new(AsyncMutex::new(FsWatchState::default())),
            setup_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            #[cfg(test)]
            watch_setup_count: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            setup_hook: None,
        }
    }

    #[cfg(test)]
    fn new_with_file_watcher(
        outgoing: Arc<OutgoingMessageSender>,
        file_watcher: Arc<FileWatcher>,
    ) -> Self {
        Self {
            outgoing,
            file_watcher: Some(file_watcher),
            state: Arc::new(AsyncMutex::new(FsWatchState::default())),
            setup_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            watch_setup_count: Arc::new(AtomicUsize::new(0)),
            setup_hook: None,
        }
    }

    pub(crate) async fn watch_with_gate(
        &self,
        connection_id: ConnectionId,
        params: FsWatchParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<FsWatchResponse, JSONRPCErrorError> {
        let watch_id = params.watch_id;
        let watch_key = WatchKey {
            connection_id,
            watch_id: watch_id.clone(),
        };

        let reservation_id = {
            let mut state = self.state.lock().await;
            let reservation_id = state.next_reservation_id;
            let reservation =
                rpc_gate.try_commit(|| match state.entries.entry(watch_key.clone()) {
                    Entry::Occupied(_) => Err(invalid_request(format!(
                        "watchId already exists: {watch_id}"
                    ))),
                    Entry::Vacant(entry) => {
                        entry.insert(WatchEntry::Pending { reservation_id });
                        Ok(reservation_id)
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
            key: watch_key.clone(),
            id: reservation_id,
            armed: true,
        };
        let cancellation = rpc_gate.cancellation_token().child_token();
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

        #[cfg(test)]
        self.watch_setup_count.fetch_add(1, Ordering::AcqRel);
        let setup_result = tokio::task::spawn_blocking(move || {
            let _setup_permit = setup_permit;
            #[cfg(test)]
            if let Some(hook) = &setup_manager.setup_hook { hook(); }
            if cancellation.is_cancelled() {
                return Err(invalid_request("watch registration was canceled"));
            }
            let file_watcher = setup_manager
                .file_watcher
                .as_ref()
                .ok_or_else(|| internal_error("filesystem watching is unavailable"))?;
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
                        reservation_id: current
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

    async fn remove_pending_reservation(&self, watch_key: &WatchKey, reservation_id: u64) {
        let mut state = self.state.lock().await;
        if matches!(
            state.entries.get(watch_key),
            Some(WatchEntry::Pending {
                reservation_id: current
            }) if *current == reservation_id
        ) {
            state.entries.remove(watch_key);
        }
    }

    #[cfg(test)]
    fn watch_setup_count(&self) -> usize {
        self.watch_setup_count.load(Ordering::Acquire)
    }

    #[cfg(test)]
    async fn watch(
        &self,
        connection_id: ConnectionId,
        params: FsWatchParams,
    ) -> Result<FsWatchResponse, JSONRPCErrorError> {
        self.watch_with_gate(connection_id, params, &ConnectionRpcGate::new())
            .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[tokio::test]
    async fn canceled_blocking_setup_releases_its_reservation_without_stalling_executor() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("watched");
        std::fs::write(&path, "value").unwrap();
        let mut manager = manager_with_noop_watcher();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let worker_entered = entered.clone();
        let worker_release = release.clone();
        let executor_thread = std::thread::current().id();
        manager.setup_hook = Some(Arc::new(move || {
            assert_ne!(std::thread::current().id(), executor_thread);
            worker_entered.notify_one();
            let (lock, changed) = &*worker_release;
            drop(changed.wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(5), |released| !*released).unwrap());
        }));
        let task_manager = manager.clone();
        let params = FsWatchParams { watch_id: "canceled".into(), path: absolute_path(path) };
        let task_params = params.clone();
        let task = tokio::spawn(async move { task_manager.watch(ConnectionId(1), task_params).await });
        entered.notified().await;
        // This current-thread executor must continue to run while the native
        // setup hook deliberately occupies its blocking worker.
        tokio::time::sleep(Duration::from_millis(10)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !manager.state.lock().await.entries.is_empty() {
                tokio::task::yield_now().await;
            }
        }).await.expect("canceled setup must relinquish its watch ID");
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        tokio::time::timeout(Duration::from_secs(1), async {
            while manager.setup_slots.available_permits() != 4 {
                tokio::task::yield_now().await;
            }
        }).await.expect("late setup must release native admission");
        manager.setup_hook = None;
        manager.watch(ConnectionId(1), params).await.expect("canceled watch ID can be reused");
        manager.connection_closed(ConnectionId(1)).await;
        assert!(manager.state.lock().await.entries.is_empty());
    }

    #[tokio::test]
    async fn backpressure_completion_retires_the_exact_watch_id() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("watched");
        std::fs::write(&file, "A").unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        assert!(
            outgoing.try_send_server_notification(ServerNotification::SkillsChanged(
                codex_app_server_protocol::SkillsChangedNotification {}
            ))
        );
        let manager =
            FsWatchManager::new_with_file_watcher(outgoing, Arc::new(FileWatcher::new().unwrap()));
        let key = WatchKey {
            connection_id: ConnectionId(1),
            watch_id: "same-id".into(),
        };
        let params = FsWatchParams {
            watch_id: key.watch_id.clone(),
            path: absolute_path(file.clone()),
        };
        manager
            .watch(key.connection_id, params.clone())
            .await
            .unwrap();
        std::fs::write(&file, "B").unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            while manager.state.lock().await.entries.contains_key(&key) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a timed-out worker must not retain an Active ID");
        manager
            .watch(key.connection_id, params)
            .await
            .expect("retired ID can be reused");
        manager.connection_closed(key.connection_id).await;
    }

    fn absolute_path(path: PathBuf) -> AbsolutePathBuf {
        assert!(
            path.is_absolute(),
            "path must be absolute: {}",
            path.display()
        );
        AbsolutePathBuf::try_from(path).expect("path should be absolute")
    }

    fn manager_with_noop_watcher() -> FsWatchManager {
        const OUTGOING_BUFFER: usize = 1;
        let (tx, _rx) = mpsc::channel(OUTGOING_BUFFER);
        FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(
                tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            )),
            Arc::new(FileWatcher::noop()),
        )
    }

    fn manager_without_watcher() -> FsWatchManager {
        const OUTGOING_BUFFER: usize = 1;
        let (tx, _rx) = mpsc::channel(OUTGOING_BUFFER);
        FsWatchManager {
            outgoing: Arc::new(OutgoingMessageSender::new(
                tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            )),
            file_watcher: None,
            state: Arc::new(AsyncMutex::new(FsWatchState::default())),
            setup_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            watch_setup_count: Arc::new(AtomicUsize::new(0)),
            setup_hook: None,
        }
    }

    #[tokio::test]
    async fn watch_fails_when_the_core_watcher_is_unavailable() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");

        let manager = manager_without_watcher();
        let error = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    watch_id: "watch-head".to_string(),
                    path: absolute_path(head_path),
                },
            )
            .await
            .expect_err("watch should fail");

        assert_eq!(error.message, "filesystem watching is unavailable");
        assert!(manager.state.lock().await.entries.is_empty());
        assert_eq!(manager.watch_setup_count(), 1);
    }

    #[tokio::test]
    async fn closed_connection_gate_rejects_watch_registration() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");
        let manager = manager_with_noop_watcher();
        let gate = ConnectionRpcGate::new();
        gate.close().await;

        let error = manager
            .watch_with_gate(
                ConnectionId(1),
                FsWatchParams {
                    watch_id: "watch-head".to_string(),
                    path: absolute_path(head_path),
                },
                &gate,
            )
            .await
            .expect_err("closed connection must reject watch registration");

        assert_eq!(error.message, "connection is closed");
        assert!(manager.state.lock().await.entries.is_empty());
        assert_eq!(manager.watch_setup_count(), 0);
    }

    #[tokio::test]
    async fn watch_and_unwatch_preserve_connection_ownership() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");

        let manager = manager_with_noop_watcher();
        let path = absolute_path(head_path);
        let watch_id = "watch-head".to_string();
        let response = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    watch_id: watch_id.clone(),
                    path: path.clone(),
                },
            )
            .await
            .expect("watch should succeed");

        assert_eq!(response.path, path);

        let watch_key = WatchKey {
            connection_id: ConnectionId(1),
            watch_id: watch_id.clone(),
        };
        assert_eq!(
            manager
                .state
                .lock()
                .await
                .entries
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
            HashSet::from([watch_key.clone()])
        );

        manager
            .unwatch(
                ConnectionId(2),
                FsUnwatchParams {
                    watch_id: "watch-head".to_string(),
                },
            )
            .await
            .expect("foreign unwatch should be a no-op");
        assert!(matches!(
            manager.state.lock().await.entries.get(&watch_key),
            Some(WatchEntry::Active(_))
        ));

        manager
            .unwatch(
                ConnectionId(1),
                FsUnwatchParams {
                    watch_id: "watch-head".to_string(),
                },
            )
            .await
            .expect("owner unwatch should succeed");
        assert!(!manager.state.lock().await.entries.contains_key(&watch_key));
    }

    #[tokio::test]
    async fn watch_rejects_duplicate_id_for_the_same_connection() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        let fetch_head_path = temp_dir.path().join("FETCH_HEAD");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");
        std::fs::write(&fetch_head_path, "old-fetch\n").expect("write FETCH_HEAD");

        let manager = manager_with_noop_watcher();
        manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    watch_id: "watch-head".to_string(),
                    path: absolute_path(head_path),
                },
            )
            .await
            .expect("first watch should succeed");

        let error = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    watch_id: "watch-head".to_string(),
                    path: absolute_path(fetch_head_path),
                },
            )
            .await
            .expect_err("duplicate watch should fail");

        assert_eq!(error.message, "watchId already exists: watch-head");
        assert_eq!(manager.state.lock().await.entries.len(), 1);
        assert_eq!(manager.watch_setup_count(), 1);
    }

    #[tokio::test]
    async fn connection_closed_removes_only_that_connections_watches() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        let fetch_head_path = temp_dir.path().join("FETCH_HEAD");
        let packed_refs_path = temp_dir.path().join("packed-refs");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");
        std::fs::write(&fetch_head_path, "old-fetch\n").expect("write FETCH_HEAD");
        std::fs::write(&packed_refs_path, "refs\n").expect("write packed-refs");

        let manager = manager_with_noop_watcher();
        let response = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    watch_id: "watch-head".to_string(),
                    path: absolute_path(head_path.clone()),
                },
            )
            .await
            .expect("first watch should succeed");
        manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    watch_id: "watch-fetch-head".to_string(),
                    path: absolute_path(fetch_head_path),
                },
            )
            .await
            .expect("second watch should succeed");
        manager
            .watch(
                ConnectionId(2),
                FsWatchParams {
                    watch_id: "watch-packed-refs".to_string(),
                    path: absolute_path(packed_refs_path),
                },
            )
            .await
            .expect("third watch should succeed");

        manager.connection_closed(ConnectionId(1)).await;

        assert_eq!(
            manager
                .state
                .lock()
                .await
                .entries
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
            HashSet::from([WatchKey {
                connection_id: ConnectionId(2),
                watch_id: "watch-packed-refs".to_string(),
            }])
        );
        assert_eq!(response.path, absolute_path(head_path));
    }

    #[tokio::test]
    async fn file_change_is_delivered_to_the_watch_owner() {
        use crate::outgoing_message::OutgoingEnvelope;
        use crate::outgoing_message::OutgoingMessage;

        let temp_dir = TempDir::new().expect("temp dir");
        let path = absolute_path(temp_dir.path().join("HEAD"));
        std::fs::write(&path, "before").unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let manager = FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(
                tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            )),
            Arc::new(FileWatcher::new().expect("filesystem watcher")),
        );
        manager
            .watch(
                ConnectionId(7),
                FsWatchParams {
                    watch_id: "head".to_string(),
                    path: path.clone(),
                },
            )
            .await
            .unwrap();

        std::fs::write(&path, "after").unwrap();

        let delivered = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("file change must reach the connection")
            .unwrap();
        match delivered {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message:
                    OutgoingMessage::AppServerNotification(ServerNotification::FsChanged(notification)),
                ..
            } => {
                assert_eq!(connection_id, ConnectionId(7));
                assert_eq!(notification.watch_id, "head");
                assert_eq!(notification.changed_paths, vec![path]);
            }
            other => panic!("unexpected notification: {other:?}"),
        }
        manager.connection_closed(ConnectionId(7)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn connection_closed_bounds_the_total_grace_for_stalled_watches() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("HEAD");
        std::fs::write(&path, "before").unwrap();
        let manager = manager_with_noop_watcher();
        for watch_id in ["first", "second"] {
            manager
                .watch(
                    ConnectionId(1),
                    FsWatchParams {
                        watch_id: watch_id.to_string(),
                        path: absolute_path(path.clone()),
                    },
                )
                .await
                .unwrap();
        }
        let mut stalled_tasks = Vec::new();
        let mut done_senders = Vec::new();
        {
            let mut state = manager.state.lock().await;
            for entry in state.entries.values_mut() {
                let WatchEntry::Active(entry) = entry else {
                    panic!("watch must be active")
                };
                let (done_tx, done_rx) = oneshot::channel();
                let task = tokio::spawn(std::future::pending::<()>());
                entry.done_rx = done_rx;
                entry.abort_handle = task.abort_handle();
                done_senders.push(done_tx);
                stalled_tasks.push(task);
            }
        }

        let started = tokio::time::Instant::now();
        manager.connection_closed(ConnectionId(1)).await;

        assert_eq!(started.elapsed(), FS_WATCH_SHUTDOWN_GRACE);
        assert!(manager.state.lock().await.entries.is_empty());
        for task in stalled_tasks {
            assert!(task.await.unwrap_err().is_cancelled());
        }
        drop(done_senders);
    }
}
