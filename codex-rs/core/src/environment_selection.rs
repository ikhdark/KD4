use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use arc_swap::ArcSwap;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerError;
use codex_exec_server::ExecutorFileSystem;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use tokio_util::sync::CancellationToken;

use crate::session::turn_context::TurnEnvironment;
use crate::session::turn_context::TurnEnvironmentLifecycle;
use crate::shell::Shell;
use crate::shell_snapshot::ShellSnapshot;

pub(crate) fn default_thread_environment_selections(
    environment_manager: &EnvironmentManager,
    cwd: &AbsolutePathBuf,
) -> Vec<TurnEnvironmentSelection> {
    environment_manager
        .default_environment_ids()
        .into_iter()
        .map(|environment_id| TurnEnvironmentSelection {
            environment_id,
            cwd: PathUri::from_abs_path(cwd),
        })
        .collect()
}

type TurnEnvironmentResult = Result<TurnEnvironment, Arc<ExecServerError>>;
type TurnEnvironmentResolution = Shared<BoxFuture<'static, TurnEnvironmentResult>>;

#[derive(Clone)]
struct SelectedTurnEnvironment {
    selection: TurnEnvironmentSelection,
    resolution: TurnEnvironmentResolution,
    lifecycle: Option<Arc<TurnEnvironmentLifecycle>>,
}

#[derive(Clone)]
struct SelectedTurnEnvironmentState {
    generation: u64,
    environments: Vec<SelectedTurnEnvironment>,
}

#[derive(Clone)]
pub(crate) struct StartingTurnEnvironment {
    pub(crate) selection: TurnEnvironmentSelection,
    resolution: TurnEnvironmentResolution,
    _lifecycle: Option<Arc<TurnEnvironmentLifecycle>>,
}

impl fmt::Debug for StartingTurnEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StartingTurnEnvironment")
            .field("selection", &self.selection)
            .field("resolved", &self.resolution.peek().is_some())
            .finish_non_exhaustive()
    }
}

impl StartingTurnEnvironment {
    pub(crate) async fn wait_until_ready(&self) -> Result<(), Arc<ExecServerError>> {
        self.resolution.clone().await.map(|_| ())
    }
}

pub(crate) struct ThreadEnvironments {
    environment_manager: Arc<EnvironmentManager>,
    local_shell: Shell,
    shell_snapshot: ShellSnapshot,
    non_blocking_snapshots: bool,
    environments: ArcSwap<SelectedTurnEnvironmentState>,
    lifecycles: Mutex<Vec<Weak<TurnEnvironmentLifecycle>>>,
}

impl ThreadEnvironments {
    pub(crate) fn new(
        environment_manager: Arc<EnvironmentManager>,
        local_shell: Shell,
        shell_snapshot: ShellSnapshot,
        current: TurnEnvironmentSnapshot,
        non_blocking_snapshots: bool,
    ) -> Self {
        // Reuse only attached environments from the supplied snapshot; drop starting entries.
        let generation = current.generation;
        let environments: Vec<SelectedTurnEnvironment> = current
            .turn_environments
            .into_iter()
            .map(|environment| {
                let selection = environment.selection();
                let lifecycle = environment.lifecycle();
                let resolution: TurnEnvironmentResolution =
                    futures::future::ready(Ok(environment)).boxed().shared();
                SelectedTurnEnvironment {
                    selection,
                    resolution,
                    lifecycle,
                }
            })
            .collect();
        let lifecycles = environments
            .iter()
            .filter_map(|environment| environment.lifecycle.as_ref().map(Arc::downgrade))
            .collect();
        Self {
            environment_manager,
            local_shell,
            shell_snapshot,
            non_blocking_snapshots,
            environments: ArcSwap::from_pointee(SelectedTurnEnvironmentState {
                generation,
                environments,
            }),
            lifecycles: Mutex::new(lifecycles),
        }
    }

    pub(crate) fn update_selections(&self, environments: &[TurnEnvironmentSelection]) {
        let previous = self.environments.load();
        let mut seen_environment_ids = HashSet::with_capacity(environments.len());
        let mut next = Vec::with_capacity(environments.len());
        let mut replaced_resolution = false;
        for selected_environment in environments {
            if !seen_environment_ids.insert(selected_environment.environment_id.as_str()) {
                continue;
            }
            if let Some(environment) = previous
                .environments
                .iter()
                .find(|environment| environment.selection == *selected_environment)
                && !matches!(environment.resolution.clone().now_or_never(), Some(Err(_)))
            {
                next.push(environment.clone());
                continue;
            }

            let environment_id = &selected_environment.environment_id;
            let Some(environment) = self.environment_manager.get_environment(environment_id) else {
                tracing::warn!("skipping unknown turn environment `{environment_id}`");
                continue;
            };
            let lifecycle = Arc::new(TurnEnvironmentLifecycle::new());
            self.lifecycles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::downgrade(&lifecycle));
            let resolution = Self::resolve_environment(
                selected_environment.clone(),
                environment,
                self.local_shell.clone(),
                self.shell_snapshot.clone(),
                Arc::downgrade(&lifecycle),
                lifecycle.cancellation_token(),
            )
            .shared();
            if resolution.clone().now_or_never().is_none() {
                drop(tokio::spawn(resolution.clone()));
            }
            replaced_resolution = true;
            next.push(SelectedTurnEnvironment {
                selection: selected_environment.clone(),
                resolution,
                lifecycle: Some(lifecycle),
            });
        }
        let selections_changed = previous.environments.len() != next.len()
            || previous
                .environments
                .iter()
                .zip(&next)
                .any(|(previous, next)| previous.selection != next.selection);
        if !replaced_resolution && !selections_changed {
            return;
        }
        self.environments
            .store(Arc::new(SelectedTurnEnvironmentState {
                generation: previous.generation.saturating_add(1),
                environments: next,
            }));
    }

    fn resolve_environment(
        selection: TurnEnvironmentSelection,
        environment: Arc<Environment>,
        local_shell: Shell,
        shell_snapshot: ShellSnapshot,
        lifecycle: Weak<TurnEnvironmentLifecycle>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'static, TurnEnvironmentResult> {
        async move {
            let environment_id = &selection.environment_id;
            let ready = tokio::select! {
                _ = cancellation.cancelled() => return Err(cancelled_environment_resolution()),
                ready = environment.wait_until_ready() => ready,
            };
            if let Err(err) = ready {
                tracing::warn!("turn environment `{environment_id}` failed to start: {err}");
                return Err(Arc::new(err));
            }
            let shell = if environment.is_remote() {
                let info = tokio::select! {
                    _ = cancellation.cancelled() => {
                        return Err(cancelled_environment_resolution());
                    }
                    info = environment.info() => info,
                };
                let info = info.map_err(Arc::new)?;
                let shell = Shell::from_environment_shell_info(info.shell).map_err(|err| {
                    Arc::new(ExecServerError::Protocol(format!(
                        "invalid shell for environment `{environment_id}`: {err}"
                    )))
                })?;
                Some(shell)
            } else {
                Some(local_shell)
            };
            let Some(lifecycle) = lifecycle.upgrade() else {
                return Err(cancelled_environment_resolution());
            };
            let mut turn_environment =
                TurnEnvironment::new(selection.environment_id, environment, selection.cwd, shell);
            turn_environment.attach_lifecycle(lifecycle);
            let mut snapshot_environment = turn_environment.clone();
            snapshot_environment.detach_lifecycle();
            let snapshot_cancellation = cancellation.clone();
            let task = async move {
                tokio::select! {
                    _ = snapshot_cancellation.cancelled() => None,
                    snapshot = shell_snapshot.build(snapshot_environment) => snapshot,
                }
            }
            .boxed()
            .shared();
            drop(tokio::spawn(task.clone()));
            turn_environment.shell_snapshot = task;
            Ok(turn_environment)
        }
        .boxed()
    }

    pub(crate) async fn snapshot(&self) -> TurnEnvironmentSnapshot {
        self.snapshot_with_wait_policy(!self.non_blocking_snapshots)
            .await
    }

    async fn snapshot_with_wait_policy(&self, wait_for_ready: bool) -> TurnEnvironmentSnapshot {
        let current = self.environments.load_full();
        let mut turn_environments = Vec::with_capacity(current.environments.len());
        let mut starting = Vec::new();
        for environment in &current.environments {
            let resolved = if wait_for_ready {
                Some(environment.resolution.clone().await)
            } else {
                environment.resolution.clone().now_or_never()
            };
            match resolved {
                Some(Ok(turn_environment)) => turn_environments.push(turn_environment),
                Some(Err(err)) => tracing::debug!(
                    environment_id = %environment.selection.environment_id,
                    "skipping failed turn environment: {err}"
                ),
                None => starting.push(StartingTurnEnvironment {
                    selection: environment.selection.clone(),
                    resolution: environment.resolution.clone(),
                    _lifecycle: environment.lifecycle.clone(),
                }),
            }
        }
        TurnEnvironmentSnapshot {
            generation: current.generation,
            turn_environments,
            starting,
        }
    }

    pub(crate) fn environment_manager(&self) -> Arc<EnvironmentManager> {
        Arc::clone(&self.environment_manager)
    }

    pub(crate) fn shutdown(&self) {
        let mut lifecycles = self
            .lifecycles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycles.retain(|lifecycle| {
            if let Some(lifecycle) = lifecycle.upgrade() {
                lifecycle.cancel();
                true
            } else {
                false
            }
        });
    }
}

fn cancelled_environment_resolution() -> Arc<ExecServerError> {
    Arc::new(ExecServerError::Disconnected(
        "turn environment resolution was cancelled".to_string(),
    ))
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TurnEnvironmentSnapshot {
    pub(crate) generation: u64,
    pub(crate) turn_environments: Vec<TurnEnvironment>,
    pub(crate) starting: Vec<StartingTurnEnvironment>,
}

impl TurnEnvironmentSnapshot {
    /// Maps each captured environment to its exact ready handle, or `None` when it was starting.
    pub(crate) fn captured_environments(&self) -> HashMap<String, Option<Arc<Environment>>> {
        self.turn_environments
            .iter()
            .map(|environment| {
                (
                    environment.environment_id.clone(),
                    Some(Arc::clone(&environment.environment)),
                )
            })
            .chain(
                self.starting
                    .iter()
                    .map(|environment| (environment.selection.environment_id.clone(), None)),
            )
            .collect()
    }

    pub(crate) fn primary(&self) -> Option<&TurnEnvironment> {
        self.turn_environments.first()
    }

    #[cfg(test)]
    pub(crate) fn primary_environment(&self) -> Option<Arc<codex_exec_server::Environment>> {
        self.primary()
            .map(|environment| Arc::clone(&environment.environment))
    }

    pub(crate) fn to_selections(&self) -> Vec<TurnEnvironmentSelection> {
        self.turn_environments
            .iter()
            .map(TurnEnvironment::selection)
            .collect()
    }

    pub(crate) fn primary_filesystem(&self) -> Option<Arc<dyn ExecutorFileSystem>> {
        self.primary()
            .map(|environment| environment.environment.get_filesystem())
    }

    pub(crate) fn single_local_environment(&self) -> Option<&TurnEnvironment> {
        if !self.starting.is_empty() {
            return None;
        }
        let [environment] = self.turn_environments.as_slice() else {
            return None;
        };

        (!environment.environment.is_remote()).then_some(environment)
    }

    pub(crate) fn single_local_environment_cwd(&self) -> Option<AbsolutePathBuf> {
        // TODO(anp): Migrate local-environment consumers to PathUri so this compatibility
        // conversion can be removed.
        self.single_local_environment()?.cwd().to_abs_path().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use codex_exec_server::Environment;
    use codex_exec_server::ExecServerRuntimePaths;
    use codex_exec_server::LOCAL_ENVIRONMENT_ID;
    use codex_exec_server::REMOTE_ENVIRONMENT_ID;
    use codex_protocol::protocol::TurnEnvironmentSelection;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use futures::SinkExt;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::time::timeout;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    async fn resolve_turn_environments(
        environment_manager: Arc<EnvironmentManager>,
        selections: &[TurnEnvironmentSelection],
    ) -> Arc<ThreadEnvironments> {
        let turn_environments = Arc::new(ThreadEnvironments::new(
            environment_manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ false,
        ));
        turn_environments.update_selections(selections);
        turn_environments.snapshot().await;
        turn_environments
    }

    fn test_runtime_paths() -> ExecServerRuntimePaths {
        ExecServerRuntimePaths::new(std::env::current_exe().expect("current exe"))
            .expect("runtime paths")
    }

    async fn read_websocket_json(websocket: &mut WebSocketStream<TcpStream>) -> Value {
        loop {
            match timeout(std::time::Duration::from_secs(5), websocket.next())
                .await
                .expect("websocket read should not time out")
                .expect("websocket should stay open")
                .expect("websocket frame should read")
            {
                Message::Text(text) => {
                    return serde_json::from_str(text.as_ref()).expect("valid JSON-RPC message");
                }
                Message::Binary(bytes) => {
                    return serde_json::from_slice(bytes.as_ref()).expect("valid JSON-RPC message");
                }
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("expected JSON-RPC message, got {other:?}"),
            }
        }
    }

    async fn serve_environment_info(listener: TcpListener) {
        let (stream, _) = listener.accept().await.expect("connection");
        let mut websocket = accept_async(stream).await.expect("websocket handshake");

        let initialize = read_websocket_json(&mut websocket).await;
        assert_eq!(initialize["method"], "initialize");
        websocket
            .send(Message::Text(
                serde_json::json!({
                    "id": initialize["id"],
                    "result": { "sessionId": "test-session" }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("initialize response");
        let initialized = read_websocket_json(&mut websocket).await;
        assert_eq!(initialized["method"], "initialized");

        let info = read_websocket_json(&mut websocket).await;
        assert_eq!(info["method"], "environment/info");
        websocket
            .send(Message::Text(
                serde_json::json!({
                    "id": info["id"],
                    "result": {
                        "operatingSystem": "windows",
                        "shell": {
                            "name": "powershell",
                            "path": "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
                        },
                        "cwd": "file:///C:/workspace"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("environment info response");
    }

    #[tokio::test]
    async fn default_thread_environment_selections_use_manager_default_id() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let cwd_uri = PathUri::from_abs_path(&cwd);
        let manager = EnvironmentManager::create_for_tests(
            Some("ws://127.0.0.1:8765".to_string()),
            Some(test_runtime_paths()),
        )
        .await;

        assert_eq!(
            default_thread_environment_selections(&manager, &cwd),
            vec![TurnEnvironmentSelection {
                environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
                cwd: cwd_uri,
            }]
        );
    }

    #[tokio::test]
    async fn toml_default_thread_environment_selections_include_local_and_remote() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp_dir.path().join("environments.toml"),
            r#"
[[environments]]
id = "remote"
url = "ws://127.0.0.1:8765"
"#,
        )
        .expect("write environments.toml");
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let cwd_uri = PathUri::from_abs_path(&cwd);
        let manager =
            EnvironmentManager::from_codex_home(temp_dir.path(), Some(test_runtime_paths()))
                .await
                .expect("environment manager");

        assert_eq!(
            default_thread_environment_selections(&manager, &cwd),
            vec![
                TurnEnvironmentSelection {
                    environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                    cwd: cwd_uri.clone(),
                },
                TurnEnvironmentSelection {
                    environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
                    cwd: cwd_uri,
                },
            ]
        );
    }

    #[tokio::test]
    async fn default_thread_environment_selections_empty_when_default_disabled() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let manager = EnvironmentManager::without_environments();

        assert_eq!(
            default_thread_environment_selections(&manager, &cwd),
            Vec::<TurnEnvironmentSelection>::new()
        );
    }

    #[tokio::test]
    async fn local_environment_uses_configured_shell() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let local_shell = Shell {
            shell_type: crate::shell::ShellType::Zsh,
            shell_path: std::path::PathBuf::from("/configured/zsh"),
        };
        let turn_environments = ThreadEnvironments::new(
            Arc::new(EnvironmentManager::default_for_tests()),
            local_shell.clone(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ false,
        );
        turn_environments.update_selections(&[TurnEnvironmentSelection {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&cwd),
        }]);

        let snapshot = turn_environments.snapshot().await;

        assert_eq!(
            snapshot
                .primary()
                .and_then(|environment| environment.shell.as_ref()),
            Some(&local_shell)
        );
    }

    #[tokio::test]
    async fn resolve_environment_selections_keeps_first_duplicate_id() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let cwd_uri = PathUri::from_abs_path(&cwd);
        let manager = Arc::new(EnvironmentManager::default_for_tests());
        let first = TurnEnvironmentSelection {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: cwd_uri.clone(),
        };

        let resolved = resolve_turn_environments(
            manager,
            &[
                first.clone(),
                TurnEnvironmentSelection {
                    environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                    cwd: cwd_uri.join("other").expect("other cwd URI"),
                },
            ],
        )
        .await;

        assert_eq!(resolved.snapshot().await.to_selections(), vec![first]);
    }

    #[tokio::test]
    async fn resolved_environment_selections_use_first_selection_as_primary() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let selected_cwd = cwd.join("selected");
        let selected_cwd_uri = PathUri::from_abs_path(&selected_cwd);
        let manager = Arc::new(EnvironmentManager::default_for_tests());

        let resolved = resolve_turn_environments(
            Arc::clone(&manager),
            &[TurnEnvironmentSelection {
                environment_id: "local".to_string(),
                cwd: selected_cwd_uri,
            }],
        )
        .await;

        let resolved = resolved.snapshot().await;
        assert_eq!(
            resolved
                .primary()
                .expect("primary environment")
                .environment_id,
            "local"
        );
        assert_eq!(
            resolved.primary().expect("primary environment").shell,
            Some(
                Shell::from_environment_shell_info(
                    manager
                        .get_environment("local")
                        .expect("local environment")
                        .info()
                        .await
                        .expect("local environment info")
                        .shell
                )
                .expect("resolved shell")
            )
        );
    }

    #[tokio::test]
    async fn unresolved_environment_selections_are_skipped() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let cwd_uri = PathUri::from_abs_path(&cwd);
        let manager = Arc::new(EnvironmentManager::default_for_tests());
        let local = TurnEnvironmentSelection {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: cwd_uri.clone(),
        };

        let resolved = resolve_turn_environments(
            manager,
            &[
                TurnEnvironmentSelection {
                    environment_id: "missing".to_string(),
                    cwd: cwd_uri,
                },
                local.clone(),
            ],
        )
        .await;

        assert_eq!(resolved.snapshot().await.to_selections(), vec![local]);
    }

    #[tokio::test]
    async fn blocking_snapshot_waits_for_starting_environment() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket listener");
        let manager = Arc::new(
            EnvironmentManager::create_for_tests(
                Some(format!(
                    "ws://{}",
                    listener.local_addr().expect("listener address")
                )),
                Some(test_runtime_paths()),
            )
            .await,
        );
        let selection = TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&AbsolutePathBuf::current_dir().expect("cwd")),
        };
        let environments = Arc::new(ThreadEnvironments::new(
            manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ false,
        ));
        environments.update_selections(std::slice::from_ref(&selection));
        let snapshot_task = tokio::spawn({
            let environments = Arc::clone(&environments);
            async move { environments.snapshot().await }
        });
        tokio::task::yield_now().await;
        assert!(!snapshot_task.is_finished());

        let server = tokio::spawn(serve_environment_info(listener));
        let snapshot = timeout(Duration::from_secs(5), snapshot_task)
            .await
            .expect("snapshot should finish after the environment starts")
            .expect("snapshot task");

        assert!(snapshot.starting.is_empty());
        assert_eq!(snapshot.to_selections(), vec![selection]);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn snapshot_keeps_starting_environment_until_it_can_be_attached() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket listener");
        let manager = Arc::new(
            EnvironmentManager::create_for_tests_with_local(
                Some(format!(
                    "ws://{}",
                    listener.local_addr().expect("listener address")
                )),
                test_runtime_paths(),
            )
            .await,
        );
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let cwd = PathUri::from_abs_path(&cwd);
        let remote = TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: cwd.clone(),
        };
        let local = TurnEnvironmentSelection {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd,
        };
        let turn_environments = ThreadEnvironments::new(
            manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ true,
        );
        turn_environments.update_selections(&[remote.clone(), local.clone()]);

        let starting = turn_environments.snapshot().await;
        assert_eq!(
            starting
                .turn_environments
                .iter()
                .map(TurnEnvironment::selection)
                .collect::<Vec<_>>(),
            vec![local.clone()]
        );
        assert_eq!(
            starting
                .starting
                .iter()
                .map(|environment| environment.selection.clone())
                .collect::<Vec<_>>(),
            vec![remote.clone()]
        );
        assert_eq!(starting.to_selections(), vec![local.clone()]);
        assert!(starting.single_local_environment().is_none());

        let server = tokio::spawn(serve_environment_info(listener));
        timeout(
            std::time::Duration::from_secs(5),
            starting.starting[0].resolution.clone(),
        )
        .await
        .expect("environment resolution should finish")
        .expect("environment resolution should succeed");
        let attached = turn_environments.snapshot().await;

        assert!(attached.starting.is_empty());
        assert_eq!(
            attached
                .turn_environments
                .iter()
                .map(TurnEnvironment::selection)
                .collect::<Vec<_>>(),
            vec![remote.clone(), local.clone()]
        );
        assert_eq!(attached.to_selections(), vec![remote, local]);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn superseded_resolution_is_cancelled_after_captured_snapshot_is_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket listener");
        let manager = Arc::new(
            EnvironmentManager::create_for_tests(
                Some(format!(
                    "ws://{}",
                    listener.local_addr().expect("listener address")
                )),
                Some(test_runtime_paths()),
            )
            .await,
        );
        let selection = TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&AbsolutePathBuf::current_dir().expect("cwd")),
        };
        let turn_environments = ThreadEnvironments::new(
            manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ true,
        );
        turn_environments.update_selections(std::slice::from_ref(&selection));
        let cancellation = {
            let current = turn_environments.environments.load();
            current.environments[0]
                .lifecycle
                .as_ref()
                .expect("resolution lifecycle")
                .cancellation_token()
        };
        let snapshot = turn_environments.snapshot().await;
        assert_eq!(snapshot.starting.len(), 1);

        turn_environments.update_selections(&[]);
        assert!(
            !cancellation.is_cancelled(),
            "a captured turn must retain its starting environment"
        );

        drop(snapshot);
        tokio::task::yield_now().await;
        assert!(cancellation.is_cancelled());
        drop(listener);
    }

    #[tokio::test]
    async fn shutdown_cancels_resolution_even_when_snapshot_is_captured() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket listener");
        let manager = Arc::new(
            EnvironmentManager::create_for_tests(
                Some(format!(
                    "ws://{}",
                    listener.local_addr().expect("listener address")
                )),
                Some(test_runtime_paths()),
            )
            .await,
        );
        let selection = TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&AbsolutePathBuf::current_dir().expect("cwd")),
        };
        let turn_environments = ThreadEnvironments::new(
            manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ true,
        );
        turn_environments.update_selections(std::slice::from_ref(&selection));
        let cancellation = {
            let current = turn_environments.environments.load();
            current.environments[0]
                .lifecycle
                .as_ref()
                .expect("resolution lifecycle")
                .cancellation_token()
        };
        let snapshot = turn_environments.snapshot().await;
        assert_eq!(snapshot.starting.len(), 1);

        turn_environments.update_selections(&[]);
        assert!(!cancellation.is_cancelled());
        turn_environments.shutdown();

        assert!(cancellation.is_cancelled());
        drop(snapshot);
        drop(listener);
    }

    #[tokio::test]
    async fn failed_resolution_is_replaced_from_the_environment_manager() {
        let manager = Arc::new(
            EnvironmentManager::create_for_tests(
                Some("http://example.com".to_string()),
                Some(test_runtime_paths()),
            )
            .await,
        );
        let selection = TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&AbsolutePathBuf::current_dir().expect("cwd")),
        };
        let environments = ThreadEnvironments::new(
            Arc::clone(&manager),
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ true,
        );
        environments.update_selections(std::slice::from_ref(&selection));
        let failed_resolution = environments.environments.load().environments[0]
            .resolution
            .clone();
        assert!(failed_resolution.clone().await.is_err());

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replacement listener");
        manager
            .upsert_environment(
                REMOTE_ENVIRONMENT_ID.to_string(),
                format!("ws://{}", listener.local_addr().expect("listener address")),
                /*connect_timeout*/ None,
            )
            .expect("replacement environment");
        environments.update_selections(std::slice::from_ref(&selection));

        let replacement = environments.snapshot().await;
        let [replacement] = replacement.starting.as_slice() else {
            panic!("expected the replacement environment to be starting");
        };
        assert_eq!(replacement.selection, selection);
        assert!(!failed_resolution.ptr_eq(&replacement.resolution));
    }

    #[tokio::test]
    async fn matching_environment_id_and_cwd_reuse_resolution() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let first_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind first listener");
        let manager = Arc::new(
            EnvironmentManager::create_for_tests(
                Some(format!(
                    "ws://{}",
                    first_listener.local_addr().expect("first listener address")
                )),
                Some(test_runtime_paths()),
            )
            .await,
        );
        let selection = TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&cwd),
        };
        let environments = ThreadEnvironments::new(
            Arc::clone(&manager),
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ true,
        );
        environments.update_selections(std::slice::from_ref(&selection));
        let initial_snapshot = environments.snapshot().await;
        let second_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind second listener");
        manager
            .upsert_environment(
                REMOTE_ENVIRONMENT_ID.to_string(),
                format!(
                    "ws://{}",
                    second_listener
                        .local_addr()
                        .expect("second listener address")
                ),
                /*connect_timeout*/ None,
            )
            .expect("replace environment");

        environments.update_selections(std::slice::from_ref(&selection));
        let reused_snapshot = environments.snapshot().await;
        environments.update_selections(&[TurnEnvironmentSelection {
            cwd: PathUri::from_abs_path(&cwd.join("changed")),
            ..selection
        }]);
        let changed_snapshot = environments.snapshot().await;

        let initial = initial_snapshot
            .starting
            .first()
            .expect("initial environment");
        let reused = reused_snapshot
            .starting
            .first()
            .expect("reused environment");
        let changed = changed_snapshot
            .starting
            .first()
            .expect("changed environment");
        assert!(initial.resolution.ptr_eq(&reused.resolution));
        assert!(!reused.resolution.ptr_eq(&changed.resolution));
    }

    #[tokio::test]
    async fn inherited_environment_reuses_parent_handle() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let selection = TurnEnvironmentSelection {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&cwd),
        };
        let inherited_environment = Arc::new(
            Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                .expect("inherited environment"),
        );
        let inherited = TurnEnvironment::new(
            selection.environment_id.clone(),
            Arc::clone(&inherited_environment),
            selection.cwd.clone(),
            /*shell*/ None,
        );
        let manager = Arc::new(EnvironmentManager::without_environments());
        manager
            .upsert_environment(
                REMOTE_ENVIRONMENT_ID.to_string(),
                "ws://127.0.0.1:9876".to_string(),
                /*connect_timeout*/ None,
            )
            .expect("replacement environment");
        let environments = ThreadEnvironments::new(
            manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot {
                generation: 0,
                turn_environments: vec![inherited],
                starting: Vec::new(),
            },
            /*non_blocking_snapshots*/ false,
        );

        environments.update_selections(std::slice::from_ref(&selection));
        let snapshot = environments.snapshot().await;

        assert!(Arc::ptr_eq(
            &snapshot
                .primary()
                .expect("inherited environment")
                .environment,
            &inherited_environment,
        ));
    }

    #[tokio::test]
    async fn single_local_environment_cwd_requires_exactly_one_local_environment() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let cwd_uri = PathUri::from_abs_path(&cwd);
        let local_manager = Arc::new(EnvironmentManager::default_for_tests());
        let local = resolve_turn_environments(
            Arc::clone(&local_manager),
            &[TurnEnvironmentSelection {
                environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                cwd: cwd_uri.clone(),
            }],
        )
        .await;
        let local = local.snapshot().await;
        let remote_environment = Arc::new(
            Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                .expect("remote environment"),
        );
        let remote = TurnEnvironmentSnapshot {
            generation: 0,
            turn_environments: vec![TurnEnvironment::new(
                REMOTE_ENVIRONMENT_ID.to_string(),
                remote_environment.clone(),
                cwd_uri.clone(),
                /*shell*/ None,
            )],
            starting: Vec::new(),
        };
        let multiple = TurnEnvironmentSnapshot {
            generation: 0,
            turn_environments: vec![
                local.primary().expect("local environment").clone(),
                TurnEnvironment::new(
                    REMOTE_ENVIRONMENT_ID.to_string(),
                    remote_environment,
                    cwd_uri,
                    /*shell*/ None,
                ),
            ],
            starting: Vec::new(),
        };

        assert_eq!(local.single_local_environment_cwd(), Some(cwd));
        assert_eq!(remote.single_local_environment_cwd(), None);
        assert_eq!(multiple.single_local_environment_cwd(), None);
    }
}
