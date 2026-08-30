use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SkillsChangedNotification;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::skills::SkillsLoadInput;
use codex_core::skills::SkillsService;
use codex_file_watcher::FileWatcher;
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

#[cfg(not(test))]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct SkillsWatcher {
    runtime: OnceLock<Result<SkillsWatcherRuntime, String>>,
    skills_service: Arc<SkillsService>,
    outgoing: Arc<OutgoingMessageSender>,
    runtime_extra_roots_registration: Mutex<WatchRegistration>,
    shutdown_requested: AtomicBool,
    #[cfg(test)]
    initialization_count: AtomicUsize,
    #[cfg(test)]
    thread_config_registration_count: AtomicUsize,
    #[cfg(test)]
    file_watcher_result: Option<Result<Arc<FileWatcher>, String>>,
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
            skills_service,
            outgoing,
            runtime_extra_roots_registration: Mutex::new(WatchRegistration::default()),
            shutdown_requested: AtomicBool::new(false),
            #[cfg(test)]
            initialization_count: AtomicUsize::new(0),
            #[cfg(test)]
            thread_config_registration_count: AtomicUsize::new(0),
            #[cfg(test)]
            file_watcher_result: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_with_file_watcher_result(
        skills_service: Arc<SkillsService>,
        outgoing: Arc<OutgoingMessageSender>,
        file_watcher_result: Result<Arc<FileWatcher>, String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime: OnceLock::new(),
            skills_service,
            outgoing,
            runtime_extra_roots_registration: Mutex::new(WatchRegistration::default()),
            shutdown_requested: AtomicBool::new(false),
            initialization_count: AtomicUsize::new(0),
            thread_config_registration_count: AtomicUsize::new(0),
            file_watcher_result: Some(file_watcher_result),
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        if let Some(Ok(runtime)) = self.runtime.get() {
            runtime.shutdown_token.cancel();
        }
    }

    pub(crate) fn register_runtime_extra_roots(
        &self,
        extra_roots: &[AbsolutePathBuf],
    ) -> Result<(), String> {
        let roots = extra_roots
            .iter()
            .map(|root| WatchPath {
                path: root.clone().into_path_buf(),
                recursive: true,
            })
            .collect::<Vec<_>>();
        let registration = if roots.is_empty() {
            WatchRegistration::default()
        } else {
            let runtime = self
                .runtime()?
                .ok_or_else(|| "skills watcher is shut down".to_string())?;
            runtime
                .subscriber
                .register_paths(roots)
                .map_err(|err| format!("failed to register runtime skills roots: {err}"))?
        };
        let mut guard = self
            .runtime_extra_roots_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = registration;
        Ok(())
    }

    pub(crate) async fn register_thread_config(
        &self,
        config: &Config,
        thread_manager: &ThreadManager,
        environments: &[TurnEnvironmentSelection],
    ) -> Result<WatchRegistration, String> {
        #[cfg(test)]
        self.thread_config_registration_count
            .fetch_add(1, Ordering::AcqRel);
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
        let runtime = self
            .runtime()?
            .ok_or_else(|| "skills watcher is shut down".to_string())?;
        runtime
            .subscriber
            .register_paths(roots)
            .map_err(|err| format!("failed to register skills roots: {err}"))
    }

    #[cfg(test)]
    pub(crate) fn thread_config_registration_count(&self) -> usize {
        self.thread_config_registration_count
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn is_initialized(&self) -> bool {
        matches!(self.runtime.get(), Some(Ok(_)))
    }

    #[cfg(test)]
    pub(crate) fn initialization_count(&self) -> usize {
        self.initialization_count.load(Ordering::Acquire)
    }

    fn runtime(&self) -> Result<Option<&SkillsWatcherRuntime>, String> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let runtime = self.runtime.get_or_init(|| {
            #[cfg(test)]
            self.initialization_count.fetch_add(1, Ordering::AcqRel);
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
        });
        match runtime {
            Ok(runtime) if self.shutdown_requested.load(Ordering::Acquire) => {
                runtime.shutdown_token.cancel();
                Ok(None)
            }
            Ok(runtime) => Ok(Some(runtime)),
            Err(err) => Err(err.clone()),
        }
    }

    fn create_file_watcher(&self) -> Result<Arc<FileWatcher>, String> {
        #[cfg(test)]
        if let Some(result) = &self.file_watcher_result {
            return result.clone();
        }
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
            loop {
                let event = tokio::select! {
                    _ = shutdown_token.cancelled() => break,
                    event = rx.recv_with_observer(|_| skills_service.clear_cache()) => event,
                };
                if event.is_none() {
                    break;
                }
                outgoing
                    .send_server_notification(ServerNotification::SkillsChanged(
                        SkillsChangedNotification {},
                    ))
                    .await;
            }
        });
        Ok(())
    }
}
