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
    pub(crate) fn has_failed(&self) -> bool {
        self.resolution.peek().is_some_and(Result::is_err)
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.environment.is_remote()
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

