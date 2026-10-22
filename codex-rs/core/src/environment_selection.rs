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
    environment: Arc<Environment>,
    resolution: TurnEnvironmentResolution,
    /// Runtime incarnation of `environment` when this resolution started or was
    /// inherited; `None` while no instance existed.
    incarnation_at_start: Option<String>,
    lifecycle: Option<Arc<TurnEnvironmentLifecycle>>,
    owns_resolution: bool,
    snapshot_cleanup_tasks: Option<tokio_util::task::TaskTracker>,
}

impl SelectedTurnEnvironment {
    /// Whether `current`, the unchanged registration, now has a ready runtime
    /// instance other than the one it had when this resolution started. For a
    /// failed resolution that instance comes from a reconnect outside this
    /// thread's snapshots (another selector's retry or a restart); checking
    /// opens no connection.
    fn has_newer_ready_runtime(&self, current: &Environment) -> bool {
        current.is_ready()
            && current.cached_runtime().is_some_and(|runtime| {
                self.incarnation_at_start.as_deref() != Some(runtime.incarnation.as_str())
            })
    }
}

fn runtime_incarnation(environment: &Environment) -> Option<String> {
    environment
        .cached_runtime()
        .map(|runtime| runtime.incarnation)
}

#[derive(Clone)]
struct SelectedTurnEnvironmentState {
    generation: u64,
    environments: Vec<SelectedTurnEnvironment>,
}

#[derive(Clone)]
pub(crate) struct StartingTurnEnvironment {
    pub(crate) selection: TurnEnvironmentSelection,
    environment: Arc<Environment>,
    pub(crate) selection_index: usize,
    resolution: TurnEnvironmentResolution,
    _lifecycle: Option<Arc<TurnEnvironmentLifecycle>>,
    snapshot_cleanup_tasks: Option<tokio_util::task::TaskTracker>,
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
    #[cfg(test)]
    pub(crate) fn for_test(
        environment: TurnEnvironment,
        selection_index: usize,
        release: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            selection: environment.selection(),
            environment: Arc::clone(&environment.environment),
            selection_index,
            resolution: async move {
                release.notified().await;
                Ok(environment)
            }
            .boxed()
            .shared(),
            _lifecycle: None,
            snapshot_cleanup_tasks: None,
        }
    }

    pub(crate) fn has_failed(&self) -> bool {
        self.resolution.peek().is_some_and(Result::is_err)
    }

    pub(crate) async fn wait_until_ready(&self) -> TurnEnvironmentResult {
        let _readiness = crate::tools::tool_dispatch_trace::active_tool_dispatch_timing()
            .and_then(|timing| timing.turn_timing_state())
            .map(|timing| {
                timing.begin_local_phase(crate::turn_timing::TurnLocalPhase::ExecutorReadinessWait)
            });
        self.resolution.clone().await
    }
}

pub(crate) struct ThreadEnvironments {
    environment_manager: Arc<EnvironmentManager>,
    local_shell: crate::shell::LocalShell,
    shell_snapshot: ShellSnapshot,
    non_blocking_snapshots: bool,
    environments: ArcSwap<SelectedTurnEnvironmentState>,
    update_lock: Mutex<()>,
    shutdown: std::sync::atomic::AtomicBool,
    owned_lifecycles: Mutex<Vec<Weak<TurnEnvironmentLifecycle>>>,
    snapshot_tasks: Vec<tokio_util::task::TaskTracker>,
}

impl ThreadEnvironments {
    pub(crate) fn new(
        environment_manager: Arc<EnvironmentManager>,
        local_shell: impl Into<crate::shell::LocalShell>,
        shell_snapshot: ShellSnapshot,
        current: TurnEnvironmentSnapshot,
        non_blocking_snapshots: bool,
    ) -> Self {
        // Preserve the selected order and shared startup ownership when inheriting.
        let local_shell = local_shell.into();
        let selections = current.to_selections();
        let generation = current.generation;
        // Retain cleanup-only trackers even if selections are later replaced.
        // Inherited builders belong to their parent and must not delay this session.
        let snapshot_tasks = std::iter::once(shell_snapshot.cleanup_tasks())
            .chain(
                current
                    .turn_environments
                    .iter()
                    .filter_map(|environment| environment.shell_snapshot_tasks.clone()),
            )
            .chain(
                current
                    .starting
                    .iter()
                    .filter_map(|environment| environment.snapshot_cleanup_tasks.clone()),
            )
            .collect();
        let mut environments: Vec<SelectedTurnEnvironment> = current
            .turn_environments
            .into_iter()
            .map(|mut environment| {
                shell_snapshot.restrict_inherited(&mut environment, local_shell.explicit());
                let selection = environment.selection();
                let registration = Arc::clone(&environment.environment);
                let lifecycle = environment.lifecycle();
                let snapshot_cleanup_tasks = environment.shell_snapshot_tasks.clone();
                let resolution: TurnEnvironmentResolution =
                    futures::future::ready(Ok(environment)).boxed().shared();
                SelectedTurnEnvironment {
                    selection,
                    incarnation_at_start: runtime_incarnation(&registration),
                    environment: registration,
                    resolution,
                    lifecycle,
                    owns_resolution: false,
                    snapshot_cleanup_tasks,
                }
            })
            .chain(current.starting.into_iter().map(|environment| {
                let registration = Arc::clone(&environment.environment);
                let snapshot_policy = shell_snapshot.clone();
                let inherited_shell = local_shell.clone();
                let resolution = async move {
                    let mut resolved = environment.resolution.await?;
                    snapshot_policy.restrict_inherited(&mut resolved, inherited_shell.explicit());
                    Ok(resolved)
                }
                .boxed()
                .shared();
                SelectedTurnEnvironment {
                    selection: environment.selection,
                    incarnation_at_start: runtime_incarnation(&registration),
                    environment: registration,
                    resolution,
                    lifecycle: environment._lifecycle,
                    owns_resolution: false,
                    snapshot_cleanup_tasks: environment.snapshot_cleanup_tasks,
                }
            }))
            .collect();
        environments.sort_by_key(|environment| {
            selections
                .iter()
                .position(|selection| selection == &environment.selection)
        });
        Self {
            environment_manager,
            local_shell,
            shell_snapshot,
            non_blocking_snapshots,
            environments: ArcSwap::from_pointee(SelectedTurnEnvironmentState {
                generation,
                environments,
            }),
            owned_lifecycles: Mutex::new(Vec::new()),
            update_lock: Mutex::new(()),
            shutdown: std::sync::atomic::AtomicBool::new(false),
            snapshot_tasks,
        }
    }

    pub(crate) fn update_selections(&self, environments: &[TurnEnvironmentSelection]) {
        let _guard = self
            .update_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.update_selections_locked(environments, /*retry_failed*/ true);
    }

    /// `retry_failed` is true for an explicit selection update, which retries a
    /// failed startup even when the selection is unchanged. Snapshot
    /// reconciliation passes false: a failed startup stays selected with its
    /// original error until the selection is explicitly updated, the manager
    /// replaces the registration (a different approval scope), or the
    /// registration reports a ready instance newer than the failed attempt
    /// (recovered elsewhere). A snapshot never starts that recovery itself.
    fn update_selections_locked(
        &self,
        environments: &[TurnEnvironmentSelection],
        retry_failed: bool,
    ) {
        if self.shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let previous = self.environments.load();
        let mut seen_environment_ids = HashSet::with_capacity(environments.len());
        let mut next = Vec::with_capacity(environments.len());
        let mut replaced_resolution = false;
        for selected_environment in environments {
            if !seen_environment_ids.insert(selected_environment.environment_id.as_str()) {
                continue;
            }
            // Inherited handles are an accepted delegation lease. Keep its
            // logical session through transport recovery; never retarget it.
            if let Some(inherited) = previous.environments.iter().find(|environment| {
                !environment.owns_resolution && environment.selection == *selected_environment
            }) {
                let resolved = inherited.resolution.clone().now_or_never();
                let current = self
                    .environment_manager
                    .get_environment(&selected_environment.environment_id);
                let replaced = current.as_ref().is_none_or(|current|
                    current.approval_scope_id() != inherited.environment.approval_scope_id()
                        || matches!(&resolved, Some(Ok(previous)) if previous.runtime.as_ref().is_some_and(|previous|
                            current.cached_runtime().is_some_and(|current| previous.incarnation != current.incarnation))));
                if replaced {
                    let mut retired = inherited.clone();
                    if !matches!(resolved, Some(Err(_))) {
                        retired.resolution = futures::future::ready(Err(Arc::new(ExecServerError::Protocol(
                            format!("inherited environment `{}` was replaced; the delegated workspace was not retargeted", selected_environment.environment_id),
                        )))).boxed().shared();
                        replaced_resolution = true;
                    }
                    next.push(retired);
                    continue;
                }
                if matches!(&resolved, Some(Ok(previous)) if previous.runtime.is_some()) {
                    next.push(inherited.clone());
                    continue;
                }
            }
            if let Some(environment) = previous
                .environments
                .iter()
                .find(|environment| environment.selection == *selected_environment)
                && self
                    .environment_manager
                    .get_environment(&selected_environment.environment_id)
                    .is_some_and(|current| {
                        current.approval_scope_id() == environment.environment.approval_scope_id()
                    })
                && match environment.resolution.clone().now_or_never() {
                    None => true,
                    Some(Err(_)) => {
                        !retry_failed
                            && !self
                                .environment_manager
                                .get_environment(&selected_environment.environment_id)
                                .is_some_and(|current| {
                                    environment.has_newer_ready_runtime(&current)
                                })
                    }
                    Some(Ok(resolved)) => self
                        .environment_manager
                        .get_environment(&selected_environment.environment_id)
                        .is_some_and(|current| {
                            !current.is_remote()
                                || (current.is_ready()
                                    && current.cached_runtime().is_some_and(|runtime| {
                                        resolved.runtime.as_ref().is_some_and(|old| {
                                            old.incarnation == runtime.incarnation
                                        })
                                    }))
                        }),
                }
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
            self.owned_lifecycles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::downgrade(&lifecycle));
            // Read before the first poll below, which may join a startup attempt.
            let incarnation_at_start = runtime_incarnation(&environment);
            let resolution = Self::resolve_environment(
                selected_environment.clone(),
                Arc::clone(&environment),
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
                environment,
                resolution,
                incarnation_at_start,
                lifecycle: Some(lifecycle),
                owns_resolution: true,
                snapshot_cleanup_tasks: Some(self.shell_snapshot.cleanup_tasks()),
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
        local_shell: crate::shell::LocalShell,
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
            let (shell, operating_system) = if environment.is_remote() {
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
                (Some(shell), info.operating_system)
            } else {
                let shell = tokio::select! {
                    _ = cancellation.cancelled() => return Err(cancelled_environment_resolution()),
                    shell = local_shell.resolve() => shell,
                }
                .map_err(|error| Arc::new(ExecServerError::Protocol(error.to_string())))?;
                (
                    Some(shell.as_ref().clone()),
                    Some(std::env::consts::OS.to_string()),
                )
            };
            let Some(lifecycle) = lifecycle.upgrade() else {
                return Err(cancelled_environment_resolution());
            };
            let mut turn_environment =
                TurnEnvironment::new(selection.environment_id, environment, selection.cwd, shell);
            turn_environment.operating_system = operating_system;
            tokio::select! {
                _ = cancellation.cancelled() => return Err(cancelled_environment_resolution()),
                result = turn_environment.refresh_runtime() => result.map_err(Arc::new)?,
            }
            turn_environment.attach_lifecycle(lifecycle);
            turn_environment.shell_snapshot_origin = shell_snapshot.origin(&turn_environment);
            let builder_tasks = shell_snapshot.builder_tasks();
            turn_environment.shell_snapshot_tasks = Some(shell_snapshot.cleanup_tasks());
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
            drop(builder_tasks.spawn(task.clone()));
            turn_environment.shell_snapshot = task;
            Ok(turn_environment)
        }
        .boxed()
    }

    pub(crate) async fn snapshot(&self) -> TurnEnvironmentSnapshot {
        self.snapshot_with_wait_policy(!self.non_blocking_snapshots)
            .await
    }

    pub(crate) async fn snapshot_now(&self) -> TurnEnvironmentSnapshot {
        self.snapshot_with_wait_policy(false).await
    }

    async fn snapshot_with_wait_policy(&self, wait_for_ready: bool) -> TurnEnvironmentSnapshot {
        // Reconcile under the same short, synchronous lock as settings updates.
        // Never hold it while waiting for a backend or snapshot builder. A failed
        // startup is never restarted here; it keeps its original error unless an
        // instance recovered elsewhere is already ready.
        {
            let _guard = self
                .update_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let selections = self
                .environments
                .load()
                .environments
                .iter()
                .map(|environment| environment.selection.clone())
                .collect::<Vec<_>>();
            self.update_selections_locked(&selections, /*retry_failed*/ false);
        }
        let current = self.environments.load_full();
        let mut turn_environments = Vec::with_capacity(current.environments.len());
        let mut starting = Vec::new();
        for (selection_index, environment) in current.environments.iter().enumerate() {
            let resolved = if wait_for_ready {
                Some(environment.resolution.clone().await)
            } else {
                environment.resolution.clone().now_or_never()
            };
            let resolved = match resolved {
                Some(Ok(mut turn_environment)) => turn_environment
                    .refresh_cached_runtime()
                    .then_some(Ok(turn_environment)),
                other => other,
            };
            match resolved {
                Some(Ok(turn_environment)) => turn_environments.push(turn_environment),
                // Retain failures as resolved startup futures too: a failed primary
                // must report its failure, not redirect an implicit operation.
                Some(Err(_)) | None => starting.push(StartingTurnEnvironment {
                    selection: environment.selection.clone(),
                    environment: Arc::clone(&environment.environment),
                    selection_index,
                    resolution: environment.resolution.clone(),
                    _lifecycle: environment.lifecycle.clone(),
                    snapshot_cleanup_tasks: environment.snapshot_cleanup_tasks.clone(),
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

    /// Time the selected-environment snapshot, honoring deferred startup. No
    /// observer task keeps an unbounded readiness guard alive after handoff.
    pub(crate) async fn snapshot_for_startup(
        &self,
        timing: Arc<crate::startup_timing::StartupTimingState>,
    ) -> TurnEnvironmentSnapshot {
        let _guard = timing.begin_phase(crate::startup_timing::StartupPhase::ExecutorReadiness);
        timing.record_executor_status("pending");
        let snapshot = self.snapshot().await;
        let failed = snapshot.starting.iter().any(|environment| {
            matches!(environment.resolution.clone().now_or_never(), Some(Err(_)))
        });
        timing.record_executor_status(if failed {
            "failed"
        } else if !snapshot.starting.is_empty() {
            "deferred"
        } else if snapshot.turn_environments.is_empty() {
            "not_selected"
        } else {
            "ready"
        });
        snapshot
    }

    pub(crate) fn shutdown(&self) {
        let _guard = self
            .update_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        let mut lifecycles = self
            .owned_lifecycles
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

    /// Release this session's snapshot ownership before waiting for deletions.
    /// A live inheriting session keeps its shared file and drains the same tracker
    /// when it releases the final owner; no cleanup task waits for that owner here.
    pub(crate) async fn finish_shutdown(&self) {
        let current = self.environments.load_full();
        for environment in &current.environments {
            if !environment.owns_resolution {
                continue;
            }
            if let Ok(environment) = environment.resolution.clone().await {
                // Cancellation must retire a pending builder before its tracker is
                // checked, including a file published just before validation.
                drop(environment.shell_snapshot.clone().await);
            }
        }
        self.environments
            .store(Arc::new(SelectedTurnEnvironmentState {
                generation: current.generation,
                environments: Vec::new(),
            }));
        drop(current);
        // Owned builders include accepted remote writes, which retain cleanup
        // ownership until the write is acknowledged even after cancellation.
        let builders = self.shell_snapshot.builder_tasks();
        builders.close();
        builders.wait().await;
        for tasks in &self.snapshot_tasks {
            tasks.close();
            tasks.wait().await;
        }
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
        if self
            .starting
            .iter()
            .any(|environment| environment.selection_index == 0)
        {
            return None;
        }
        self.turn_environments.first()
    }

    #[cfg(test)]
    pub(crate) fn primary_environment(&self) -> Option<Arc<codex_exec_server::Environment>> {
        self.primary()
            .map(|environment| Arc::clone(&environment.environment))
    }

    pub(crate) fn to_selections(&self) -> Vec<TurnEnvironmentSelection> {
        let mut selections: Vec<_> = self
            .turn_environments
            .iter()
            .map(TurnEnvironment::selection)
            .collect();
        for environment in &self.starting {
            selections.insert(environment.selection_index, environment.selection.clone());
        }
        selections
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
#[path = "environment_comprehension_tests.rs"]
mod comprehension_tests;

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

    async fn serve_environment_info(listener: TcpListener) -> WebSocketStream<TcpStream> {
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
                            "name": "pwsh",
                            "path": "C:\\Program Files\\PowerShell\\7\\pwsh.exe"
                        },
                        "cwd": "file:///C:/workspace"
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("environment info response");
        // The JoinHandle retains the peer until the caller has observed
        // readiness; EOF must not race the client's pending response owner.
        websocket
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

    async fn delayed_startup_fixture(
        deferred: bool,
    ) -> (
        ThreadEnvironments,
        tokio::sync::oneshot::Sender<TurnEnvironmentResult>,
        TurnEnvironment,
    ) {
        let mut environments = ThreadEnvironments::new(
            Arc::new(EnvironmentManager::default_for_tests()),
            Shell {
                shell_type: crate::shell::ShellType::Zsh,
                shell_path: "/configured/zsh".into(),
            },
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            false,
        );
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        environments.update_selections(&[TurnEnvironmentSelection {
            environment_id: LOCAL_ENVIRONMENT_ID.into(),
            cwd: PathUri::from_abs_path(&cwd),
        }]);
        let ready = environments.snapshot().await.turn_environments.remove(0);
        let current = environments.environments.load_full();
        let (release, receiver) = tokio::sync::oneshot::channel();
        let mut selected = current.environments[0].clone();
        selected.resolution = async move { receiver.await.expect("release readiness") }
            .boxed()
            .shared();
        environments
            .environments
            .store(Arc::new(SelectedTurnEnvironmentState {
                generation: current.generation,
                environments: vec![selected],
            }));
        environments.non_blocking_snapshots = deferred;
        (environments, release, ready)
    }

    #[tokio::test]
    async fn startup_readiness_times_the_blocking_snapshot() {
        let (environments, release, ready) = delayed_startup_fixture(false).await;
        let timing = crate::startup_timing::StartupTimingState::new("blocking".into());
        let mut snapshot = Box::pin(environments.snapshot_for_startup(Arc::clone(&timing)));
        assert!(futures::poll!(snapshot.as_mut()).is_pending());
        release.send(Ok(ready)).expect("release readiness");
        assert!(snapshot.await.primary().is_some());
        timing.finish_session_initialization();
        let profile = timing.complete_snapshot();
        assert!(profile.profile_valid);
        assert_eq!(profile.executor_status, "ready");
        assert!(profile.phases.executor_readiness_ns > 0);
    }

    #[tokio::test]
    async fn deferred_startup_is_explicit_and_later_wait_retains_primary_failure() {
        let (environments, release, _) = delayed_startup_fixture(true).await;
        let timing = crate::startup_timing::StartupTimingState::new("deferred".into());
        let snapshot = environments.snapshot_for_startup(Arc::clone(&timing)).await;
        assert!(snapshot.primary().is_none());
        timing.finish_session_initialization();
        let frozen = timing.complete_snapshot();
        assert!(frozen.profile_valid);
        assert_eq!(frozen.executor_status, "deferred");
        let mut wait = Box::pin(snapshot.starting[0].wait_until_ready());
        assert!(futures::poll!(wait.as_mut()).is_pending());
        release
            .send(Err(Arc::new(ExecServerError::Protocol(
                "primary failed".into(),
            ))))
            .expect("release failure");
        assert!(
            wait.await
                .unwrap_err()
                .to_string()
                .contains("primary failed")
        );
        assert_eq!(timing.complete_snapshot(), frozen);
    }

    #[tokio::test]
    async fn startup_reports_failed_selected_environment_without_fallback() {
        let (environments, release, _) = delayed_startup_fixture(false).await;
        let timing = crate::startup_timing::StartupTimingState::new("failed".into());
        let mut snapshot = Box::pin(environments.snapshot_for_startup(Arc::clone(&timing)));
        assert!(futures::poll!(snapshot.as_mut()).is_pending());
        release
            .send(Err(Arc::new(ExecServerError::Protocol(
                "primary failed".into(),
            ))))
            .expect("release failure");
        assert!(snapshot.await.primary().is_none());
        timing.finish_session_initialization();
        let profile = timing.complete_snapshot();
        assert!(profile.profile_valid);
        assert_eq!(profile.executor_status, "failed");
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

    #[test]
    fn default_thread_environment_selections_empty_when_default_disabled() {
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
    async fn environmentless_startup_does_not_demand_a_host_shell() {
        let resolver = crate::shell::LocalShell::default();
        let environments = ThreadEnvironments::new(
            Arc::new(EnvironmentManager::default_for_tests()),
            resolver.clone(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            false,
        );
        environments.update_selections(&[]);
        assert!(environments.snapshot().await.turn_environments.is_empty());
        assert!(resolver.cached().is_none(), "no host discovery was needed");
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
    async fn resolves_selected_local_environment_cwd_and_shell() {
        let cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let selected_cwd = cwd.join("selected");
        let selected_cwd_uri = PathUri::from_abs_path(&selected_cwd);
        let manager = Arc::new(EnvironmentManager::default_for_tests());

        let resolved = resolve_turn_environments(
            Arc::clone(&manager),
            &[TurnEnvironmentSelection {
                environment_id: "local".to_string(),
                cwd: selected_cwd_uri.clone(),
            }],
        )
        .await;

        let resolved = resolved.snapshot().await;
        assert_eq!(
            resolved.primary().expect("primary environment").cwd(),
            &selected_cwd_uri
        );
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
        let snapshot_task = environments.snapshot();
        tokio::pin!(snapshot_task);
        assert!(futures::poll!(&mut snapshot_task).is_pending());

        let server = tokio::spawn(serve_environment_info(listener));
        let snapshot = timeout(Duration::from_secs(5), snapshot_task)
            .await
            .expect("snapshot should finish after the environment starts");

        assert!(snapshot.starting.is_empty());
        assert_eq!(snapshot.to_selections(), vec![selection]);
        assert_eq!(
            snapshot.primary().unwrap().operating_system.as_deref(),
            Some("windows")
        );
        let (_session, turn_context) = crate::session::tests::make_session_and_context().await;
        let mut state = crate::context::world_state::WorldState::default();
        state.add_section(
            crate::context::world_state::EnvironmentsState::from_turn_context_with_environments(
                &turn_context,
                &snapshot,
            ),
        );
        let rendered = state
            .render_full()
            .into_iter()
            .map(|fragment| serde_json::to_string(&fragment.into_boxed_response_item()).unwrap())
            .collect::<String>();
        assert!(rendered.contains("<os>windows</os>"), "{rendered}");
        assert!(rendered.contains("<shell>powershell</shell>"), "{rendered}");
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
        assert_eq!(
            starting.to_selections(),
            vec![remote.clone(), local.clone()]
        );
        assert!(starting.primary().is_none());
        assert!(starting.single_local_environment().is_none());

        let git_cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
        let git_snapshot = git_cache.snapshot(&starting).await;
        assert_eq!(git_snapshot.primary_is_git(), None);
        assert!(git_snapshot.primary_local_metadata_source().is_none());
        assert_eq!(git_snapshot.display_roots().len(), 1);

        let cancellation = CancellationToken::new();
        let pending_dispatch =
            crate::tools::handlers::wait_for_tool_environment(&starting, None, &cancellation);
        tokio::pin!(pending_dispatch);
        assert!(
            timeout(Duration::from_millis(20), &mut pending_dispatch)
                .await
                .is_err()
        );
        let server = tokio::spawn(serve_environment_info(listener));
        let selected = timeout(Duration::from_secs(5), pending_dispatch)
            .await
            .expect("environment resolution should finish")
            .expect("environment resolution should succeed")
            .expect("selected primary");
        assert_eq!(selected.environment_id, remote.environment_id);
        let attached = turn_environments.snapshot().await;

        assert!(attached.starting.is_empty());
        assert_eq!(
            attached.primary().expect("resolved primary").environment_id,
            remote.environment_id
        );
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
        assert!(
            timeout(
                Duration::from_secs(5),
                snapshot.starting[0].wait_until_ready()
            )
            .await
            .expect("owner cancellation releases its stalled resolver")
            .is_err()
        );
        timeout(Duration::from_secs(5), turn_environments.finish_shutdown())
            .await
            .expect("owner shutdown does not wait for the stalled dependency");
        // A late capture or settings update must not start a new owner after shutdown.
        turn_environments.update_selections(std::slice::from_ref(&selection));
        assert!(
            turn_environments
                .snapshot_now()
                .await
                .to_selections()
                .is_empty()
        );
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
        let server = tokio::spawn(serve_environment_info(listener));
        let resolved = timeout(Duration::from_secs(5), replacement.wait_until_ready())
            .await
            .expect("replacement should finish starting")
            .expect("replacement should resolve successfully");
        assert_eq!(resolved.selection(), selection);
        assert_eq!(
            resolved.environment.approval_scope_id(),
            manager
                .get_environment(REMOTE_ENVIRONMENT_ID)
                .expect("replacement remains registered")
                .approval_scope_id(),
        );
        server.await.expect("replacement server task");
    }

    /// A selected remote environment whose only startup attempt was rejected
    /// with a retryable HTTP 500, as in the exec-server client recovery tests.
    /// The listener stays bound, so a later attempt can be served, or would hang.
    struct FailedRemoteStartup {
        manager: Arc<EnvironmentManager>,
        environments: ThreadEnvironments,
        selection: TurnEnvironmentSelection,
        listener: TcpListener,
        failed: TurnEnvironmentResolution,
        error: Arc<ExecServerError>,
    }

    async fn failed_remote_startup_fixture() -> FailedRemoteStartup {
        use tokio::io::AsyncWriteExt as _;

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
        let environments = ThreadEnvironments::new(
            Arc::clone(&manager),
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ false,
        );
        // Select before rejecting, so the resolution shares the only startup attempt.
        environments.update_selections(std::slice::from_ref(&selection));
        let (mut startup, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("startup connection should arrive")
            .expect("accept startup connection");
        startup
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("failed handshake response should write");
        let failed = environments.environments.load().environments[0]
            .resolution
            .clone();
        let error = timeout(Duration::from_secs(5), failed.clone())
            .await
            .expect("startup should fail promptly")
            .expect_err("startup should fail");
        FailedRemoteStartup {
            manager,
            environments,
            selection,
            listener,
            failed,
            error,
        }
    }

    /// Every resolution a thread starts registers exactly one owned lifecycle.
    fn started_resolutions(environments: &ThreadEnvironments) -> usize {
        environments
            .owned_lifecycles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[tokio::test]
    async fn repeated_snapshots_keep_failed_startup_and_its_original_error() {
        let FailedRemoteStartup {
            environments,
            selection,
            listener: _listener,
            failed,
            error,
            ..
        } = failed_remote_startup_fixture().await;
        let generation = environments.environments.load().generation;
        let started = started_resolutions(&environments);

        // Alternate the non-blocking and blocking snapshot paths.
        for blocking in [false, true, false, true] {
            let snapshot = if blocking {
                timeout(Duration::from_secs(5), environments.snapshot())
                    .await
                    .expect("a failed startup must not make a snapshot wait")
            } else {
                environments.snapshot_now().await
            };
            let [reported] = snapshot.starting.as_slice() else {
                panic!("the failed environment must stay selected");
            };
            assert!(
                reported.resolution.ptr_eq(&failed),
                "a snapshot must not restart a failed startup"
            );
            assert!(
                reported.has_failed(),
                "a snapshot must not turn a failed startup back into starting"
            );
            let reported_error = reported
                .wait_until_ready()
                .await
                .expect_err("the startup failure is reported");
            assert!(
                Arc::ptr_eq(&reported_error, &error),
                "every snapshot reports the original failure, got: {reported_error}"
            );
            assert_eq!(snapshot.generation, generation);
            assert!(snapshot.primary().is_none());
            assert_eq!(snapshot.to_selections(), vec![selection.clone()]);
        }
        assert_eq!(
            started_resolutions(&environments),
            started,
            "snapshots started no new resolution"
        );
    }

    #[tokio::test]
    async fn explicit_update_retries_failed_startup_with_the_same_selection() {
        let FailedRemoteStartup {
            environments,
            selection,
            listener,
            failed,
            ..
        } = failed_remote_startup_fixture().await;
        let unchanged = environments.snapshot_now().await;
        assert!(
            unchanged
                .starting
                .first()
                .is_some_and(|reported| reported.resolution.ptr_eq(&failed)),
            "only the explicit update below may retry"
        );
        let generation = environments.environments.load().generation;
        let started = started_resolutions(&environments);

        environments.update_selections(std::slice::from_ref(&selection));

        let retry = environments.environments.load().environments[0]
            .resolution
            .clone();
        assert!(
            !retry.ptr_eq(&failed),
            "an explicit update must start a new attempt"
        );
        assert_eq!(started_resolutions(&environments), started + 1);
        assert_eq!(environments.environments.load().generation, generation + 1);
        let server = tokio::spawn(serve_environment_info(listener));
        let resolved = timeout(Duration::from_secs(5), retry)
            .await
            .expect("the retry should finish starting")
            .expect("the retry should succeed");
        assert_eq!(resolved.selection(), selection);
        // Keep the peer open while the recovered environment is attached.
        let _peer = server.await.expect("retry server task");
        let recovered = environments.snapshot_now().await;
        assert!(recovered.starting.is_empty());
        assert_eq!(
            recovered
                .primary()
                .map(|environment| environment.environment_id.as_str()),
            Some(REMOTE_ENVIRONMENT_ID)
        );
    }

    #[tokio::test]
    async fn re_registration_re_resolves_failed_startup_at_snapshot() {
        let FailedRemoteStartup {
            manager,
            environments,
            selection,
            listener,
            failed,
            ..
        } = failed_remote_startup_fixture().await;
        let failed_scope = manager
            .get_environment(REMOTE_ENVIRONMENT_ID)
            .expect("failed environment remains registered")
            .approval_scope_id()
            .to_string();
        let unchanged = environments.snapshot_now().await;
        assert!(
            unchanged
                .starting
                .first()
                .is_some_and(|reported| reported.resolution.ptr_eq(&failed)),
            "an unchanged registration keeps the failure"
        );
        let started = started_resolutions(&environments);

        // Re-adding the same URL, as `environment/add` does, registers a new
        // environment with a new approval scope. No selection update follows.
        manager
            .upsert_environment(
                REMOTE_ENVIRONMENT_ID.to_string(),
                format!("ws://{}", listener.local_addr().expect("listener address")),
                /*connect_timeout*/ None,
            )
            .expect("re-register environment");
        let registration = manager
            .get_environment(REMOTE_ENVIRONMENT_ID)
            .expect("re-registered environment");
        assert_ne!(registration.approval_scope_id(), failed_scope);

        let snapshot = environments.snapshot_now().await;
        let [replacement] = snapshot.starting.as_slice() else {
            panic!("the re-registered environment should be starting");
        };
        assert!(
            !replacement.resolution.ptr_eq(&failed),
            "a registration change must re-resolve at snapshot time"
        );
        assert_eq!(started_resolutions(&environments), started + 1);
        assert_eq!(
            replacement.environment.approval_scope_id(),
            registration.approval_scope_id()
        );
        let server = tokio::spawn(serve_environment_info(listener));
        let resolved = timeout(Duration::from_secs(5), replacement.wait_until_ready())
            .await
            .expect("the re-registered environment should finish starting")
            .expect("the re-registered environment should resolve");
        assert_eq!(resolved.selection(), selection);
        assert_eq!(
            resolved.environment.approval_scope_id(),
            registration.approval_scope_id()
        );
        server.await.expect("re-registered server task");
    }

    /// Another thread on the same manager explicitly selects `selection`.
    /// Returns that thread and the resolution its explicit update started.
    fn select_from_another_thread(
        manager: &Arc<EnvironmentManager>,
        selection: &TurnEnvironmentSelection,
    ) -> (ThreadEnvironments, TurnEnvironmentResolution) {
        let other = ThreadEnvironments::new(
            Arc::clone(manager),
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ false,
        );
        other.update_selections(std::slice::from_ref(selection));
        let retry = other.environments.load().environments[0].resolution.clone();
        (other, retry)
    }

    /// Asserts that `snapshot` still reports the resolution `failed` with its
    /// original `error`.
    async fn assert_keeps_failure(
        snapshot: &TurnEnvironmentSnapshot,
        failed: &TurnEnvironmentResolution,
        error: &Arc<ExecServerError>,
        context: &str,
    ) {
        let [reported] = snapshot.starting.as_slice() else {
            panic!("{context}: the failed environment must stay selected");
        };
        assert!(
            reported.resolution.ptr_eq(failed),
            "{context}: a snapshot must not replace the failure"
        );
        assert!(
            reported.has_failed(),
            "{context}: a snapshot must not report the failure as starting"
        );
        let reported_error = reported
            .wait_until_ready()
            .await
            .expect_err("the failure is reported");
        assert!(
            Arc::ptr_eq(&reported_error, error),
            "{context}: the original failure must be reported, got: {reported_error}"
        );
    }

    #[tokio::test]
    async fn recovery_by_another_selector_supersedes_failed_startup_at_snapshot() {
        let FailedRemoteStartup {
            manager,
            environments,
            selection,
            listener,
            failed,
            error,
        } = failed_remote_startup_fixture().await;
        let generation = environments.environments.load().generation;
        let started = started_resolutions(&environments);
        let (_other, other_retry) = select_from_another_thread(&manager, &selection);

        // The other thread's retry is still connecting, so no instance is ready.
        let pending = environments.snapshot_now().await;
        assert_keeps_failure(&pending, &failed, &error, "retry elsewhere pending").await;
        assert_eq!(pending.generation, generation);

        let server = tokio::spawn(serve_environment_info(listener));
        let recovered = timeout(Duration::from_secs(5), other_retry)
            .await
            .expect("the other thread's retry should finish starting")
            .expect("the other thread's retry should succeed");
        // Keep the peer open. The listener is closed now, so any new
        // connection attempt would fail.
        let _peer = server.await.expect("retry server task");
        let instance = recovered
            .runtime
            .as_ref()
            .map(|runtime| runtime.incarnation.clone())
            .expect("the recovered remote environment has a runtime");

        let snapshot = environments.snapshot_now().await;
        let replacement = environments.environments.load().environments[0]
            .resolution
            .clone();
        assert!(
            !replacement.ptr_eq(&failed),
            "a ready instance recovered elsewhere must supersede the failure"
        );
        assert_eq!(started_resolutions(&environments), started + 1);
        assert_eq!(snapshot.generation, generation + 1);
        assert_eq!(snapshot.to_selections(), vec![selection]);
        let attached = timeout(Duration::from_secs(5), replacement)
            .await
            .expect("attaching to the recovered instance should not wait")
            .expect("the recovered instance should resolve");
        assert_eq!(
            attached
                .runtime
                .as_ref()
                .map(|runtime| runtime.incarnation.as_str()),
            Some(instance.as_str()),
            "the snapshot attaches to the recovered instance and opens no connection"
        );
        let ready = environments.snapshot_now().await;
        assert!(ready.starting.is_empty());
        assert_eq!(
            ready
                .primary()
                .map(|environment| environment.environment_id.as_str()),
            Some(REMOTE_ENVIRONMENT_ID)
        );
    }

    #[tokio::test]
    async fn failed_retry_elsewhere_keeps_the_failure_until_an_explicit_retry() {
        use tokio::io::AsyncWriteExt as _;

        let FailedRemoteStartup {
            manager,
            environments,
            selection,
            listener,
            failed,
            error,
        } = failed_remote_startup_fixture().await;
        let generation = environments.environments.load().generation;
        let started = started_resolutions(&environments);
        let (_other, other_retry) = select_from_another_thread(&manager, &selection);
        let (mut rejected, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("the other thread's retry should connect")
            .expect("accept retry connection");
        rejected
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("failed handshake response should write");
        assert!(
            timeout(Duration::from_secs(5), other_retry)
                .await
                .expect("the other thread's retry should fail promptly")
                .is_err(),
            "the other thread's retry should fail"
        );
        assert!(
            !manager
                .get_environment(REMOTE_ENVIRONMENT_ID)
                .expect("the environment remains registered")
                .is_ready()
        );

        // Plain snapshots, non-blocking and blocking, keep the original failure.
        for blocking in [false, true] {
            let snapshot = if blocking {
                timeout(Duration::from_secs(5), environments.snapshot())
                    .await
                    .expect("a kept failure must not make a snapshot wait")
            } else {
                environments.snapshot_now().await
            };
            assert_keeps_failure(&snapshot, &failed, &error, "failed retry elsewhere").await;
            assert_eq!(snapshot.generation, generation);
        }
        assert_eq!(started_resolutions(&environments), started);

        // An explicit update still retries.
        environments.update_selections(std::slice::from_ref(&selection));
        let retry = environments.environments.load().environments[0]
            .resolution
            .clone();
        assert!(
            !retry.ptr_eq(&failed),
            "an explicit update must start a new attempt"
        );
        assert_eq!(started_resolutions(&environments), started + 1);
        let server = tokio::spawn(serve_environment_info(listener));
        let resolved = timeout(Duration::from_secs(5), retry)
            .await
            .expect("the explicit retry should finish starting")
            .expect("the explicit retry should succeed");
        assert_eq!(resolved.selection(), selection);
        server.await.expect("retry server task");
    }

    #[tokio::test]
    async fn inherited_failure_observes_recovery_by_another_selector() {
        use tokio::io::AsyncWriteExt as _;

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
        let parent = ThreadEnvironments::new(
            Arc::clone(&manager),
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ true,
        );
        parent.update_selections(std::slice::from_ref(&selection));
        // The child inherits the parent's startup while it is still pending.
        let inherited = parent.snapshot_now().await;
        assert_eq!(inherited.starting.len(), 1);
        let child = ThreadEnvironments::new(
            Arc::clone(&manager),
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            inherited,
            /*non_blocking_snapshots*/ false,
        );
        child.update_selections(std::slice::from_ref(&selection));
        let (mut startup, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("startup connection should arrive")
            .expect("accept startup connection");
        startup
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("failed handshake response should write");
        let failed = child.environments.load().environments[0].resolution.clone();
        let error = timeout(Duration::from_secs(5), failed.clone())
            .await
            .expect("startup should fail promptly")
            .expect_err("startup should fail");
        let kept = child.snapshot_now().await;
        assert_keeps_failure(&kept, &failed, &error, "an inherited failure").await;
        let started = started_resolutions(&child);

        let (_other, other_retry) = select_from_another_thread(&manager, &selection);
        let server = tokio::spawn(serve_environment_info(listener));
        let recovered = timeout(Duration::from_secs(5), other_retry)
            .await
            .expect("the other thread's retry should finish starting")
            .expect("the other thread's retry should succeed");
        let _peer = server.await.expect("retry server task");
        let instance = recovered
            .runtime
            .as_ref()
            .map(|runtime| runtime.incarnation.clone())
            .expect("the recovered remote environment has a runtime");

        child.snapshot_now().await;
        let replacement = child.environments.load().environments[0].resolution.clone();
        assert!(
            !replacement.ptr_eq(&failed),
            "a ready instance recovered elsewhere must supersede an inherited failure"
        );
        assert_eq!(started_resolutions(&child), started + 1);
        let attached = timeout(Duration::from_secs(5), replacement)
            .await
            .expect("attaching to the recovered instance should not wait")
            .expect("the recovered instance should resolve");
        assert_eq!(
            attached
                .runtime
                .as_ref()
                .map(|runtime| runtime.incarnation.as_str()),
            Some(instance.as_str()),
            "the child attaches to the instance recovered elsewhere"
        );
    }

    #[tokio::test]
    async fn git_snapshot_does_not_promote_a_secondary_after_primary_failure() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let cwd = PathUri::from_host_native_path(temp.path()).unwrap();
        let manager = Arc::new(
            EnvironmentManager::create_for_tests_with_local(
                Some("http://example.com".to_string()),
                test_runtime_paths(),
            )
            .await,
        );
        let environments = ThreadEnvironments::new(
            manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            true,
        );
        environments.update_selections(&[
            TurnEnvironmentSelection {
                environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
                cwd: cwd.clone(),
            },
            TurnEnvironmentSelection {
                environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                cwd,
            },
        ]);
        let failed = environments.environments.load().environments[0]
            .resolution
            .clone();
        assert!(failed.await.is_err());
        let captured = environments.snapshot().await;
        assert!(captured.primary().is_none());
        assert!(captured.starting[0].has_failed());
        let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
        let git = cache.snapshot(&captured).await;
        assert_eq!(git.primary_is_git(), None);
        assert!(git.primary_local_metadata_source().is_none());
        assert_eq!(git.display_roots().len(), 1);
        assert_eq!(git.display_roots()[0].0, LOCAL_ENVIRONMENT_ID);
    }

    #[tokio::test]
    async fn same_label_replacement_refreshes_pending_resolution_without_reselection() {
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
        assert!(!initial.resolution.ptr_eq(&reused.resolution));
        assert!(!reused.resolution.ptr_eq(&changed.resolution));
    }

    #[tokio::test]
    async fn child_shutdown_preserves_stalled_parent_environment() {
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
        let parent = ThreadEnvironments::new(
            Arc::clone(&manager),
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            TurnEnvironmentSnapshot::default(),
            /*non_blocking_snapshots*/ true,
        );
        parent.update_selections(std::slice::from_ref(&selection));
        let inherited = parent.snapshot().await;
        assert_eq!(inherited.starting.len(), 1);
        let pending = inherited.starting[0].clone();
        let cancellation = pending
            ._lifecycle
            .as_ref()
            .expect("parent resolution lifecycle")
            .cancellation_token();
        let child = ThreadEnvironments::new(
            manager,
            crate::shell::default_user_shell(),
            ShellSnapshot::disabled(),
            inherited,
            /*non_blocking_snapshots*/ true,
        );
        child.update_selections(std::slice::from_ref(&selection));

        child.shutdown();
        assert!(
            !cancellation.is_cancelled(),
            "child shutdown must not cancel the parent's stalled resolver"
        );
        timeout(Duration::from_secs(5), child.finish_shutdown())
            .await
            .expect("child shutdown must not await the stalled parent dependency");
        assert!(pending.resolution.clone().now_or_never().is_none());

        let server = tokio::spawn(serve_environment_info(listener));
        let resolved = timeout(Duration::from_secs(5), pending.wait_until_ready())
            .await
            .expect("parent resolver finishes after dependency is released")
            .expect("parent resolver was not cancelled by child shutdown");
        assert_eq!(resolved.environment_id, selection.environment_id);
        server.await.expect("server task");
        parent.shutdown();
        timeout(Duration::from_secs(5), parent.finish_shutdown())
            .await
            .expect("parent shutdown finishes");
        assert!(
            cancellation.is_cancelled(),
            "the owner still cancels its lifecycle"
        );
    }

    #[tokio::test]
    async fn inherited_environment_reports_replacement_without_retargeting_assignment() {
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
        let snapshot = environments.snapshot_now().await;
        assert!(snapshot.primary().is_none());
        assert_eq!(snapshot.to_selections(), vec![selection]);
        let replacement = snapshot.starting.first().expect("replacement is selected");
        assert_eq!(
            replacement.environment.approval_scope_id(),
            inherited_environment.approval_scope_id()
        );
        assert!(replacement.wait_until_ready().await.is_err());
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
