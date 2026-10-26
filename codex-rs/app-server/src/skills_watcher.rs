use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SkillsChangedNotification;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::skills::SkillsLoadInput;
use codex_core::skills::SkillsService;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherEvent;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::Receiver;
use codex_file_watcher::ThrottledWatchReceiver;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::DropGuard;
use tracing::warn;

const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) struct SkillsWatcher {
    runtime: OnceLock<SkillsWatcherRuntime>,
    initialization: Mutex<Option<(Instant, String)>>,
    skills_service: Arc<SkillsService>,
    outgoing: Arc<OutgoingMessageSender>,
    runtime_extra_roots_registration: Mutex<WatchRegistration>,
    shutdown_requested: AtomicBool,
}

struct SkillsWatcherRuntime {
    subscriber: FileWatcherSubscriber,
    shutdown_token: CancellationToken,
    _shutdown_drop_guard: DropGuard,
}

impl SkillsWatcher {
    pub(crate) fn new(
        skills_service: Arc<SkillsService>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime: OnceLock::new(),
            initialization: Mutex::new(None),
            skills_service,
            outgoing,
            runtime_extra_roots_registration: Mutex::new(WatchRegistration::default()),
            shutdown_requested: AtomicBool::new(false),
        })
    }


    pub(crate) fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        if let Some(runtime) = self.runtime.get() {
            runtime.shutdown_token.cancel();
        }
    }

    pub(crate) async fn register_runtime_extra_roots(
        self: &Arc<Self>,
        extra_roots: &[AbsolutePathBuf],
    ) -> Result<(), String> {
        let roots = extra_roots
            .iter()
            .map(|root| WatchPath {
                path: root.clone().into_path_buf(),
                recursive: true,
            })
            .collect::<Vec<_>>();
        let watcher = Arc::clone(self);
        tokio::task::spawn_blocking(move || watcher.replace_runtime_extra_roots(roots))
            .await
            .map_err(|err| format!("skills roots registration worker failed: {err}"))?
    }

    fn replace_runtime_extra_roots(&self, roots: Vec<WatchPath>) -> Result<(), String> {
        let registration = if roots.is_empty() {
            WatchRegistration::default()
        } else {
            let runtime = self
                .runtime()?
                .ok_or_else(|| "skills watcher is shut down".to_string())?;
            runtime.subscriber.register_paths(roots).map_err(|err| {
                self.skills_service.disable_cache_reuse();
                format!("failed to register runtime skills roots: {err}")
            })?
        };
        self.skills_service
            .invalidate_roots(registration.newly_covered_paths());
        let mut guard = self
            .runtime_extra_roots_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::mem::replace(&mut *guard, registration);
        drop(guard);
        drop(previous);
        Ok(())
    }

    pub(crate) async fn register_thread_config(
        self: &Arc<Self>,
        config: &Config,
        thread_manager: &ThreadManager,
        environments: &[TurnEnvironmentSelection],
    ) -> Result<WatchRegistration, String> {
        let Some(environment_selection) = environments.first() else {
            return Ok(WatchRegistration::default());
        };
        let Some(environment) = thread_manager
            .environment_manager()
            .get_environment(&environment_selection.environment_id)
        else {
            warn!(
                "failed to register skills watcher for unknown environment `{}`",
                environment_selection.environment_id
            );
            return Ok(WatchRegistration::default());
        };
        if environment.is_remote() {
            return Ok(WatchRegistration::default());
        }

        let plugins_input = config.plugins_config_input();
        let plugins_manager = thread_manager.plugins_manager();
        let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
        let skills_input = SkillsLoadInput::new(
            config.cwd.clone(),
            plugin_outcome.effective_plugin_skill_roots(),
            config.config_layer_stack.clone(),
            config.bundled_skills_enabled(),
        );
        let roots = thread_manager
            .skills_service()
            .skill_roots_for_config(&skills_input, Some(environment.get_filesystem()))
            .await
            .into_iter()
            // Plugin roots are invalidated by plugin lifecycle operations.
            .filter(|root| root.plugin_id.is_none())
            .map(|root| WatchPath {
                path: root.path.into_path_buf(),
                recursive: true,
            })
            .collect::<Vec<_>>();
        if roots.is_empty() {
            return Ok(WatchRegistration::default());
        }
        let watcher = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let runtime = watcher
                .runtime()?
                .ok_or_else(|| "skills watcher is shut down".to_string())?;
            let registration = runtime.subscriber.register_paths(roots).map_err(|err| {
                watcher.skills_service.disable_cache_reuse();
                format!("failed to register skills roots: {err}")
            })?;
            watcher
                .skills_service
                .invalidate_roots(registration.newly_covered_paths());
            Ok(registration)
        })
        .await
        .map_err(|err| format!("skills registration worker failed: {err}"))?
    }




    fn runtime(&self) -> Result<Option<&SkillsWatcherRuntime>, String> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        if self.runtime.get().is_none() {
            // Serialize construction, but retain only success. Bound repeated native
            // initialization failures without making them process-lifetime facts.
            let mut failure = self
                .initialization
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.shutdown_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            if self.runtime.get().is_none() {
                if let Some((when, error)) = failure.as_ref()
                    && when.elapsed() < Duration::from_secs(1)
                {
                    return Err(error.clone());
                }
                let created: Result<SkillsWatcherRuntime, String> = (|| {
                    let file_watcher = self.create_file_watcher()?;
                    let (subscriber, rx) = file_watcher.add_subscriber();
                    let shutdown_token = CancellationToken::new();
                    Self::spawn_event_loop(
                        rx,
                        Arc::clone(&self.skills_service),
                        Arc::clone(&self.outgoing),
                        shutdown_token.child_token(),
                    )?;
                    Ok(SkillsWatcherRuntime {
                        subscriber,
                        _shutdown_drop_guard: shutdown_token.clone().drop_guard(),
                        shutdown_token,
                    })
                })();
                match created {
                    Ok(runtime) => {
                        if self.shutdown_requested.load(Ordering::Acquire) {
                            runtime.shutdown_token.cancel();
                            return Ok(None);
                        }
                        let _ = self.runtime.set(runtime);
                        *failure = None;
                    }
                    Err(error) => {
                        *failure = Some((Instant::now(), error.clone()));
                        self.skills_service.disable_cache_reuse();
                        return Err(error);
                    }
                }
            }
        }
        // Each path above either publishes the runtime under the initialization
        // lock or returns, so an empty slot here is reported, not assumed away.
        let Some(runtime) = self.runtime.get() else {
            return Err("skills watcher initialization did not publish a runtime".to_string());
        };
        match runtime {
            runtime if self.shutdown_requested.load(Ordering::Acquire) => {
                runtime.shutdown_token.cancel();
                Ok(None)
            }
            runtime if runtime.shutdown_token.is_cancelled() => {
                Err("skills observer has terminated".to_string())
            }
            runtime => Ok(Some(runtime)),
        }
    }

    fn create_file_watcher(&self) -> Result<Arc<FileWatcher>, String> {
        FileWatcher::new()
            .map(Arc::new)
            .map_err(|err| format!("failed to initialize skills file watcher: {err}"))
    }

    fn spawn_event_loop(
        rx: Receiver,
        skills_service: Arc<SkillsService>,
        outgoing: Arc<OutgoingMessageSender>,
        shutdown_token: CancellationToken,
    ) -> Result<(), String> {
        let mut rx = ThrottledWatchReceiver::new(rx, WATCHER_THROTTLE_INTERVAL);
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|err| format!("skills watcher listener requires a Tokio runtime: {err}"))?;
        handle.spawn(async move {
            'watch: loop {
                let event = tokio::select! {
                    _ = shutdown_token.cancelled() => break,
                    event = rx.recv_with_observer(|event| invalidate_changed_skills(&skills_service, event)) => event,
                };
                if event.is_none() {
                    skills_service.disable_cache_reuse();
                    shutdown_token.cancel();
                    break;
                }
                let notification = outgoing.send_server_notification(
                    ServerNotification::SkillsChanged(SkillsChangedNotification {}),
                );
                tokio::pin!(notification);
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown_token.cancelled() => break 'watch,
                        _ = &mut notification => break,
                        event = rx.recv_with_observer(|event| invalidate_changed_skills(&skills_service, event)) => {
                            if event.is_none() {
                                skills_service.disable_cache_reuse();
                                shutdown_token.cancel();
                                break 'watch;
                            }
                            // This pending notification covers the new invalidation.
                            // Poll the same send so events cannot reset its deadline.
                        }
                    }
                }
            }
        });
        Ok(())
    }
}

fn invalidate_changed_skills(skills_service: &SkillsService, event: &FileWatcherEvent) {
    // Registered skill roots are absolute. FileWatcher maps native reports back
    // into that requested namespace, including roots removed or renamed. Keep
    // the fail-safe invalidation for overflow or unattributable input.
    if event.rescan_required || event.paths.is_empty() || event.paths.iter().any(|path| !path.is_absolute()) {
        skills_service.clear_cache();
    } else {
        skills_service.invalidate_roots(&event.paths);
    }
}

