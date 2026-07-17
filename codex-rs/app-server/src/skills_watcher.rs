use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SkillsChangedNotification;
use codex_core::ThreadManager;
use codex_core::ThreadConfigSnapshot;
use codex_core::config::Config;
use codex_core::skills::SkillsLoadInput;
use codex_core::skills::SkillsService;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::Receiver;
use codex_file_watcher::ThrottledWatchReceiver;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::DropGuard;
use tracing::warn;

#[cfg(not(test))]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct SkillsWatcher {
    subscriber: FileWatcherSubscriber,
    runtime_extra_roots_registration: Mutex<WatchRegistration>,
    shutdown_token: CancellationToken,
    _shutdown_drop_guard: DropGuard,
}

impl SkillsWatcher {
    pub(crate) fn new(
        skills_service: Arc<SkillsService>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Arc<Self> {
        let file_watcher = match FileWatcher::new() {
            Ok(file_watcher) => Arc::new(file_watcher),
            Err(err) => {
                warn!("failed to initialize skills file watcher: {err}");
                Arc::new(FileWatcher::noop())
            }
        };
        let (subscriber, rx) = file_watcher.add_subscriber();
        let shutdown_token = CancellationToken::new();
        let shutdown_drop_guard = shutdown_token.clone().drop_guard();
        Self::spawn_event_loop(rx, skills_service, outgoing, shutdown_token.child_token());
        Arc::new(Self {
            subscriber,
            runtime_extra_roots_registration: Mutex::new(WatchRegistration::default()),
            shutdown_token,
            _shutdown_drop_guard: shutdown_drop_guard,
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    pub(crate) fn register_runtime_extra_roots(&self, extra_roots: &[AbsolutePathBuf]) {
        let roots = extra_roots
            .iter()
            .map(|root| WatchPath {
                path: root.clone().into_path_buf(),
                recursive: true,
            })
            .collect();
        let registration = self.subscriber.register_paths(roots);
        let mut guard = self
            .runtime_extra_roots_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = registration;
    }

    pub(crate) async fn register_thread_config(
        &self,
        config: &Config,
        thread_manager: &ThreadManager,
        config_snapshot: &ThreadConfigSnapshot,
    ) -> WatchRegistration {
        let Some(environment_selection) = config_snapshot.environment_selections().first() else {
            return WatchRegistration::default();
        };
        let Some(environment) = thread_manager
            .environment_manager()
            .get_environment(&environment_selection.environment_id)
        else {
            warn!(
                "failed to register skills watcher for unknown environment `{}`",
                environment_selection.environment_id
            );
            return WatchRegistration::default();
        };
        if environment.is_remote() {
            return WatchRegistration::default();
        }
        let discovery_cwd = match environment_selection.cwd.to_abs_path() {
            Ok(cwd) => cwd,
            Err(err) => {
                warn!(
                    "failed to register skills watcher for non-local cwd `{}`: {err}",
                    environment_selection.cwd
                );
                return WatchRegistration::default();
            }
        };

        let plugins_input = config.plugins_config_input();
        let plugins_manager = thread_manager.plugins_manager();
        let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
        let skills_input = SkillsLoadInput::new(
            discovery_cwd.clone(),
            plugin_outcome.effective_plugin_skill_roots(),
            config.config_layer_stack.clone(),
            config.bundled_skills_enabled(),
        );
        let mut roots = thread_manager
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
        let project_root_markers =
            codex_core::skills::loader::project_root_markers_from_stack(
                &config.config_layer_stack,
            );
        for ancestor in discovery_cwd.ancestors() {
            // Register missing discovery paths too. FileWatcher falls back to an existing ancestor
            // and migrates the watch as path components are created.
            roots.push(WatchPath {
                path: ancestor.join(".agents").join("skills").into_path_buf(),
                recursive: true,
            });
            roots.extend(project_root_markers.iter().map(|marker| WatchPath {
                path: ancestor.join(marker).into_path_buf(),
                recursive: false,
            }));
        }
        roots.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.recursive.cmp(&right.recursive))
        });
        roots.dedup();
        self.subscriber.register_paths(roots)
    }

    fn spawn_event_loop(
        rx: Receiver,
        skills_service: Arc<SkillsService>,
        outgoing: Arc<OutgoingMessageSender>,
        shutdown_token: CancellationToken,
    ) {
        let mut rx = ThrottledWatchReceiver::new(rx, WATCHER_THROTTLE_INTERVAL);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("skills watcher listener skipped: no Tokio runtime available");
            return;
        };
        handle.spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = shutdown_token.cancelled() => break,
                    event = rx.recv() => event,
                };
                let Some(event) = event else {
                    break;
                };
                skills_service.invalidate_paths(&event.paths);
                outgoing
                    .send_server_notification(ServerNotification::SkillsChanged(
                        SkillsChangedNotification {},
                    ))
                    .await;
            }
        });
    }
}
