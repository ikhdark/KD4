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
use codex_file_watcher::DebouncedWatchReceiver;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::Hash;
use std::sync::Arc;
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
}

#[derive(Default)]
struct FsWatchState {
    entries: HashMap<WatchKey, WatchEntry>,
}

struct WatchEntry {
    cancellation: CancellationToken,
    abort_handle: AbortHandle,
    done_rx: oneshot::Receiver<()>,
    _subscriber: FileWatcherSubscriber,
    _registration: WatchRegistration,
}

impl WatchEntry {
    async fn stop(self) {
        self.cancellation.cancel();
        if tokio::time::timeout(FS_WATCH_SHUTDOWN_GRACE, self.done_rx)
            .await
            .is_err()
        {
            self.abort_handle.abort();
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
        let file_watcher = self
            .file_watcher
            .as_ref()
            .ok_or_else(|| internal_error("filesystem watching is unavailable"))?;
        let outgoing = self.outgoing.clone();
        let (subscriber, rx) = file_watcher.add_subscriber();
        let watch_root = params.path.clone();
        let registration = subscriber
            .register_paths(vec![WatchPath {
                path: params.path.to_path_buf(),
                recursive: false,
            }])
            .map_err(|err| internal_error(format!("failed to register filesystem watch: {err}")))?;
        let cancellation = rpc_gate.cancellation_token().child_token();
        let task_cancellation = cancellation.clone();
        let task_watch_id = watch_id.clone();
        let (start_tx, start_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            if start_rx.await.is_ok() {
                let mut rx = DebouncedWatchReceiver::new(rx, FS_CHANGED_NOTIFICATION_DEBOUNCE);
                loop {
                    let event = tokio::select! {
                        biased;
                        _ = task_cancellation.cancelled() => break,
                        event = rx.recv() => match event {
                            Some(event) => event,
                            None => break,
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
            let _ = done_tx.send(());
        });
        let mut pending_entry = Some(WatchEntry {
            cancellation,
            abort_handle: task.abort_handle(),
            done_rx,
            _subscriber: subscriber,
            _registration: registration,
        });
        drop(task);

        let mut state = self.state.lock().await;
        let commit_result = rpc_gate.try_commit(|| match state.entries.entry(watch_key) {
            Entry::Occupied(_) => Err(invalid_request(format!(
                "watchId already exists: {watch_id}"
            ))),
            Entry::Vacant(entry) => {
                entry.insert(pending_entry.take().expect("watch entry must be available"));
                Ok(())
            }
        });
        drop(state);
        if commit_result.as_ref().is_some_and(Result::is_ok) {
            let _ = start_tx.send(());
        } else {
            drop(start_tx);
        }
        if let Some(entry) = pending_entry {
            entry.stop().await;
        }
        commit_result.ok_or_else(|| invalid_request("connection is closed"))??;

        Ok(FsWatchResponse { path: params.path })
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
        for entry in entries {
            entry.stop().await;
        }
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
    }

    #[tokio::test]
    async fn watch_uses_client_id_and_tracks_the_owner_scoped_entry() {
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

        let state = manager.state.lock().await;
        assert_eq!(
            state.entries.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([WatchKey {
                connection_id: ConnectionId(1),
                watch_id,
            }])
        );
    }

    #[tokio::test]
    async fn unwatch_is_scoped_to_the_connection_that_created_the_watch() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");

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
            .expect("watch should succeed");
        let watch_key = WatchKey {
            connection_id: ConnectionId(1),
            watch_id: "watch-head".to_string(),
        };

        manager
            .unwatch(
                ConnectionId(2),
                FsUnwatchParams {
                    watch_id: "watch-head".to_string(),
                },
            )
            .await
            .expect("foreign unwatch should be a no-op");
        assert!(manager.state.lock().await.entries.contains_key(&watch_key));

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
}
