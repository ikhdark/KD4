use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
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

#[cfg(not(test))]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct SkillsWatcher {
    runtime: OnceLock<SkillsWatcherRuntime>,
    initialization: Mutex<Option<(Instant, String)>>,
    skills_service: Arc<SkillsService>,
    outgoing: Arc<OutgoingMessageSender>,
    runtime_extra_roots_registration: Mutex<WatchRegistration>,
    shutdown_requested: AtomicBool,
    #[cfg(test)]
    initialization_count: AtomicUsize,
    #[cfg(test)]
    thread_config_registration_count: AtomicUsize,
    #[cfg(test)]
    file_watcher_result: Mutex<Option<Result<Arc<FileWatcher>, String>>>,
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
            #[cfg(test)]
            initialization_count: AtomicUsize::new(0),
            #[cfg(test)]
            thread_config_registration_count: AtomicUsize::new(0),
            #[cfg(test)]
            file_watcher_result: Mutex::new(None),
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
            initialization: Mutex::new(None),
            skills_service,
            outgoing,
            runtime_extra_roots_registration: Mutex::new(WatchRegistration::default()),
            shutdown_requested: AtomicBool::new(false),
            initialization_count: AtomicUsize::new(0),
            thread_config_registration_count: AtomicUsize::new(0),
            file_watcher_result: Mutex::new(Some(file_watcher_result)),
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

    #[cfg(test)]
    pub(crate) fn thread_config_registration_count(&self) -> usize {
        self.thread_config_registration_count
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn is_initialized(&self) -> bool {
        self.runtime.get().is_some()
    }

    #[cfg(test)]
    pub(crate) fn initialization_count(&self) -> usize {
        self.initialization_count.load(Ordering::Acquire)
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
        #[cfg(test)]
        if let Some(result) = self.file_watcher_result.lock().unwrap().as_ref() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scoped_events_keep_unrelated_skill_snapshots_warm_and_overflow_cools_them() {
        let home = tempfile::tempdir().unwrap();
        let repos = tempfile::tempdir().unwrap();
        let service = SkillsService::new(AbsolutePathBuf::from_absolute_path(home.path()).unwrap(), false);
        let mut inputs = Vec::new();
        for name in ["a", "b"] {
            let cwd = repos.path().join(name);
            let skill = cwd.join(".agents/skills").join(name).join("SKILL.md");
            std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
            std::fs::write(&skill, format!("---\nname: {name}\ndescription: initial\n---\nBody\n")).unwrap();
            inputs.push(SkillsLoadInput::new(
                AbsolutePathBuf::from_absolute_path(cwd).unwrap(),
                Vec::new(),
                codex_config::ConfigLayerStack::default(),
                false,
            ));
        }
        let a = service.snapshot_for_config(&inputs[0], None).await;
        let b = service.snapshot_for_config(&inputs[1], None).await;
        let path = repos.path().join("a/.agents/skills/a/SKILL.md");
        std::fs::write(&path, "---\nname: a\ndescription: changed\n---\nBody\n").unwrap();
        invalidate_changed_skills(&service, &FileWatcherEvent { paths: vec![path], rescan_required: false });
        let fresh_a = service.snapshot_for_config(&inputs[0], None).await;
        assert!(fresh_a.outcome().skills.iter().any(|skill| skill.name == "a" && skill.description == "changed"));
        assert!(!std::ptr::eq(a.outcome(), fresh_a.outcome()));
        let warm_b = service.snapshot_for_config(&inputs[1], None).await;
        assert!(std::ptr::eq(b.outcome(), warm_b.outcome()));
        for event in [
            FileWatcherEvent { paths: Vec::new(), rescan_required: true },
            FileWatcherEvent { paths: vec!["relative/SKILL.md".into()], rescan_required: false },
        ] {
            let before = service.snapshot_for_config(&inputs[1], None).await;
            invalidate_changed_skills(&service, &event);
            let after = service.snapshot_for_config(&inputs[1], None).await;
            assert!(!std::ptr::eq(before.outcome(), after.outcome()), "ambiguous events must retain global freshness");
        }
    }

    #[tokio::test]
    async fn initialization_failure_retries_once_after_recovery_and_shutdown_stays_closed() {
        let home = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let root = AbsolutePathBuf::from_absolute_path(roots.path()).unwrap();
        let skills = Arc::new(SkillsService::new(
            AbsolutePathBuf::from_absolute_path(home.path()).unwrap(),
            false,
        ));
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let watcher = SkillsWatcher::new_with_file_watcher_result(
            skills,
            outgoing,
            Err("transient native failure".to_string()),
        );
        for _ in 0..2 {
            assert!(
                watcher
                    .register_runtime_extra_roots(std::slice::from_ref(&root))
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            watcher.initialization_count(),
            1,
            "failure cooldown bounds retries"
        );
        *watcher.file_watcher_result.lock().unwrap() = Some(Ok(Arc::new(FileWatcher::noop())));
        watcher.initialization.lock().unwrap().as_mut().unwrap().0 =
            Instant::now() - Duration::from_secs(1);
        let results = futures::future::join_all(
            (0..8).map(|_| watcher.register_runtime_extra_roots(std::slice::from_ref(&root))),
        )
        .await;
        assert!(results.into_iter().all(|result| result.is_ok()));
        assert_eq!(
            watcher.initialization_count(),
            2,
            "one successful shared runtime"
        );
        assert!(watcher.is_initialized());
        watcher.shutdown();
        assert!(watcher.runtime().unwrap().is_none());
        assert!(watcher.register_runtime_extra_roots(&[root]).await.is_err());
        assert_eq!(watcher.initialization_count(), 2);
    }

    #[tokio::test]
    async fn acquiring_coverage_reconciles_warm_snapshots_and_reacquisition_gaps() {
        let home = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let skill = roots.path().join("watched/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        let write = |description: &str| {
            std::fs::write(
                &skill,
                format!("---\nname: watched\ndescription: {description}\n---\nBody\n"),
            )
            .unwrap()
        };
        write("A");
        let root = AbsolutePathBuf::from_absolute_path(roots.path()).unwrap();
        let skills = Arc::new(SkillsService::new(
            AbsolutePathBuf::from_absolute_path(home.path()).unwrap(),
            false,
        ));
        skills.set_extra_roots(vec![root.clone()]);
        let input = SkillsLoadInput::new(
            root.clone(),
            Vec::new(),
            codex_config::ConfigLayerStack::default(),
            false,
        );
        let first = skills.snapshot_for_config(&input, None).await;
        assert_eq!(first.outcome().skills[0].description, "A");
        write("B");
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let watcher = SkillsWatcher::new(Arc::clone(&skills), outgoing);
        watcher
            .register_runtime_extra_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        let covered = skills.snapshot_for_config(&input, None).await;
        assert_eq!(covered.outcome().skills[0].description, "B");
        assert_eq!(
            first.outcome().skills[0].description,
            "A",
            "running snapshots stay immutable"
        );
        watcher
            .register_runtime_extra_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        let continuous = skills.snapshot_for_config(&input, None).await;
        assert!(
            std::ptr::eq(covered.outcome(), continuous.outcome()),
            "a continuously covered replacement must preserve the warm snapshot"
        );
        watcher.register_runtime_extra_roots(&[]).await.unwrap();
        write("C");
        watcher.register_runtime_extra_roots(&[root]).await.unwrap();
        assert_eq!(
            skills
                .snapshot_for_config(&input, None)
                .await
                .outcome()
                .skills[0]
                .description,
            "C"
        );
        watcher.shutdown();
    }

    #[tokio::test]
    async fn backpressured_notifications_allow_later_invalidation_and_shutdown() {
        let home = tempfile::tempdir().expect("temporary home");
        let roots = tempfile::tempdir().expect("temporary skills");
        let skill_dir = roots.path().join("watched");
        std::fs::create_dir(&skill_dir).expect("create skill directory");
        let skill_path = skill_dir.join("SKILL.md");
        let write_skill = |description: &str| {
            std::fs::write(
                &skill_path,
                format!("---\nname: watched\ndescription: {description}\n---\nInstructions\n"),
            )
            .expect("write skill");
        };
        write_skill("initial");
        let root = AbsolutePathBuf::from_absolute_path(roots.path()).expect("absolute root");
        let skills = Arc::new(SkillsService::new(
            AbsolutePathBuf::from_absolute_path(home.path()).expect("absolute home"),
            false,
        ));
        skills.set_extra_roots(vec![root.clone()]);
        let input = SkillsLoadInput::new(
            root.clone(),
            Vec::new(),
            codex_config::ConfigLayerStack::default(),
            false,
        );
        let initial = skills.snapshot_for_config(&input, None).await;
        assert_eq!(initial.outcome().skills[0].description, "initial");
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        assert!(
            outgoing.try_send_server_notification(ServerNotification::SkillsChanged(
                SkillsChangedNotification {},
            ))
        );
        let watcher = SkillsWatcher::new(Arc::clone(&skills), Arc::clone(&outgoing));
        watcher
            .register_runtime_extra_roots(&[root])
            .await
            .expect("watch skills");

        // Observe real cache invalidation while the outgoing queue remains full.
        for description in ["first change", "second change"] {
            write_skill(description);
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let snapshot = skills.snapshot_for_config(&input, None).await;
                    if snapshot.outcome().skills[0].description == description {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("notification backpressure must not indefinitely block invalidation");
            // Let the throttled notification reach the already full transport.
            tokio::time::sleep(WATCHER_THROTTLE_INTERVAL * 2).await;
        }
        // The final dirty marker must deliver after capacity returns, even
        // though no further filesystem event wakes the listener.
        rx.recv().await.expect("prefilled notification");
        let delivered = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("pending skills notification must survive quiescence")
            .expect("skills notification");
        assert!(matches!(
            delivered,
            crate::outgoing_message::OutgoingEnvelope::Broadcast {
                message: crate::outgoing_message::OutgoingMessage::AppServerNotification(
                    ServerNotification::SkillsChanged(_)
                ),
            }
        ));
        // Shutdown must still release the listener with a full outgoing queue.
        let _ = outgoing.try_send_server_notification(ServerNotification::SkillsChanged(
            SkillsChangedNotification {},
        ));
        assert_eq!(rx.len(), 1);
        watcher.shutdown();
        drop(watcher);
        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&outgoing) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown must release the event loop despite a full queue");
        assert_eq!(rx.len(), 1);
    }
}
