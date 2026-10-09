use super::*;
use crate::config::ConfigBuilder;
use crate::environment_selection::ThreadEnvironments;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::default_user_shell;
use crate::shell_snapshot::ShellSnapshot;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::ExecutorFileSystemFuture;
use codex_exec_server::FileMetadata;
use codex_exec_server::FileSystemReadStream;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::LOCAL_FS;
use codex_exec_server::ReadDirectoryEntry;
use codex_exec_server::RemoveOptions;
use codex_utils_absolute_path::AbsolutePathBuf;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::io;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::bytes::Bytes;
use toml::Value as TomlValue;

enum NextProjectRead {
    Normal,
    Fail(io::ErrorKind),
    Block {
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
}

#[tokio::test]
async fn project_root_markers_recover_after_cache_poisoning_and_are_reused() {
    let temp = tempfile::tempdir().expect("tempdir");
    let codex_home = tempfile::tempdir().expect("codex home");
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(temp.path().to_path_buf()))
            .build()
            .await
            .expect("config"),
    );
    let manager = AgentsMdManager::new(None);
    let poisoning = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cache = manager
            .project_root_markers_cache
            .lock()
            .expect("unpoisoned project root marker cache");
        panic!("poison project root marker cache");
    }));
    assert!(poisoning.is_err());

    let first = manager.project_root_markers(config.as_ref());
    let second = manager.project_root_markers(config.as_ref());

    assert!(Arc::ptr_eq(&first, &second));

    let replacement = Arc::new((*config).clone());
    let replacement_markers = manager.project_root_markers(replacement.as_ref());
    assert_eq!(first.as_ref(), replacement_markers.as_ref());
    assert!(Arc::ptr_eq(&first, &replacement_markers));
}

struct ControlledFileSystem {
    target: AbsolutePathBuf,
    next_project_read: StdMutex<NextProjectRead>,
    target_metadata: StdMutex<Option<FileMetadata>>,
    target_stream_calls: AtomicUsize,
}

impl ControlledFileSystem {
    fn new(target: AbsolutePathBuf) -> Self {
        Self {
            target,
            next_project_read: StdMutex::new(NextProjectRead::Normal),
            target_metadata: StdMutex::new(None),
            target_stream_calls: AtomicUsize::new(0),
        }
    }

    fn set_next_project_read(&self, next_project_read: NextProjectRead) {
        *self
            .next_project_read
            .lock()
            .expect("project read control lock") = next_project_read;
    }

    fn target_stream_calls(&self) -> usize {
        self.target_stream_calls.load(Ordering::SeqCst)
    }

    fn set_target_metadata(&self, metadata: FileMetadata) {
        *self
            .target_metadata
            .lock()
            .expect("project metadata control lock") = Some(metadata);
    }
}

impl ExecutorFileSystem for ControlledFileSystem {
    fn canonicalize<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        LOCAL_FS.canonicalize(path, sandbox)
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        LOCAL_FS.read_file(path, sandbox)
    }

    fn read_file_stream<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async move {
            if path.to_abs_path()? != self.target {
                return LOCAL_FS.read_file_stream(path, sandbox).await;
            }
            self.target_stream_calls.fetch_add(1, Ordering::SeqCst);
            let next_project_read = {
                let mut next_project_read = self
                    .next_project_read
                    .lock()
                    .expect("project read control lock");
                std::mem::replace(&mut *next_project_read, NextProjectRead::Normal)
            };
            match next_project_read {
                NextProjectRead::Normal => LOCAL_FS.read_file_stream(path, sandbox).await,
                NextProjectRead::Fail(kind) => {
                    Err(io::Error::new(kind, "injected project read failure"))
                }
                NextProjectRead::Block { started, release } => {
                    let data = LOCAL_FS.read_file(path, sandbox).await?;
                    started.notify_one();
                    release.notified().await;
                    Ok(FileSystemReadStream::new(futures::stream::once(
                        async move { Ok::<Bytes, io::Error>(Bytes::from(data)) },
                    )))
                }
            }
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a PathUri,
        contents: Vec<u8>,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.write_file(path, contents, sandbox)
    }

    fn create_directory<'a>(
        &'a self,
        path: &'a PathUri,
        options: CreateDirectoryOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.create_directory(path, options, sandbox)
    }

    fn get_metadata<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async move {
            if path.to_abs_path()? == self.target
                && let Some(metadata) = self
                    .target_metadata
                    .lock()
                    .expect("project metadata control lock")
                    .clone()
            {
                return Ok(metadata);
            }
            LOCAL_FS.get_metadata(path, sandbox).await
        })
    }

    fn read_directory<'a>(
        &'a self,
        path: &'a PathUri,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        LOCAL_FS.read_directory(path, sandbox)
    }

    fn remove<'a>(
        &'a self,
        path: &'a PathUri,
        options: RemoveOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.remove(path, options, sandbox)
    }

    fn copy<'a>(
        &'a self,
        source_path: &'a PathUri,
        destination_path: &'a PathUri,
        options: CopyOptions,
        sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        LOCAL_FS.copy(source_path, destination_path, options, sandbox)
    }
}

async fn config_for(root: &TempDir) -> Config {
    let codex_home = tempfile::tempdir().expect("codex home");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(root.path().to_path_buf()))
        .build()
        .await
        .expect("test config");
    config.cwd = AbsolutePathBuf::from_absolute_path(root.path()).expect("absolute root");
    config.project_doc_max_bytes = 4_096;
    config
}

fn environment_snapshot(cwd: &AbsolutePathBuf, generation: u64) -> TurnEnvironmentSnapshot {
    environment_snapshot_with_environment(
        cwd,
        generation,
        Arc::new(Environment::default_for_tests()),
    )
}

fn environment_snapshot_with_environment(
    cwd: &AbsolutePathBuf,
    generation: u64,
    environment: Arc<Environment>,
) -> TurnEnvironmentSnapshot {
    TurnEnvironmentSnapshot {
        generation,
        turn_environments: vec![TurnEnvironment::new(
            "local".to_string(),
            environment,
            PathUri::from_abs_path(cwd),
            /*shell*/ None,
        )],
        starting: Vec::new(),
    }
}

#[tokio::test]
async fn repository_stable_context_reuse_is_scoped_to_one_manager() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "stable instructions").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 3);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    let first_observation = manager.refresh_and_observe(&config, &environments).await;
    let first = first_observation.loaded.expect("first load");
    let first_stable = first_observation
        .stable_context
        .expect("first stable context");
    assert!(!first_stable.reused);

    let second_observation = manager.refresh_and_observe(&config, &environments).await;
    let second = second_observation.loaded.expect("cached load");
    let second_stable = second_observation
        .stable_context
        .expect("cached stable context");

    assert!(Arc::ptr_eq(&first, &second));
    assert!(second_stable.reused);
    assert!(!second_stable.semantic_replacement);
    assert!(Arc::ptr_eq(&first_stable.rendered, &second_stable.rendered));
    let expected_digest: [u8; 32] = Sha256::digest(first.text().as_bytes()).into();
    assert_eq!(first.semantic_digest(), expected_digest);

    let independent_manager = AgentsMdManager::new(/*user_instructions*/ None);
    let independent = independent_manager
        .refresh_and_observe(&config, &environments)
        .await
        .stable_context
        .expect("independent stable context");
    assert!(!independent.reused);
    assert!(!independent.semantic_replacement);
    assert!(!Arc::ptr_eq(&first_stable.rendered, &independent.rendered));
}

#[tokio::test]
async fn unchanged_source_metadata_reloads_project_file() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "stable instructions").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let environments = environment_snapshot_with_environment(&config.cwd, 3, environment);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    let first_observation = manager.refresh_and_observe(&config, &environments).await;
    let first = first_observation.loaded.expect("first load");
    assert_eq!(filesystem.target_stream_calls(), 1);

    let second_observation = manager.refresh_and_observe(&config, &environments).await;
    let second = second_observation.loaded.expect("metadata-cached load");

    assert_eq!(second_observation.freshness, AgentsMdFreshness::Refreshed);
    assert_eq!(filesystem.target_stream_calls(), 2);
    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn same_size_same_metadata_rewrite_refreshes_project_instructions() {
    let root = tempfile::tempdir().expect("workspace");
    let agents_path = root.path().join("AGENTS.md");
    fs::write(&agents_path, "version one").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let environments = environment_snapshot_with_environment(&config.cwd, 3, environment);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    let first = manager
        .refresh_and_get_loaded(&config, &environments)
        .await
        .expect("first load");
    let metadata = LOCAL_FS
        .get_metadata(&PathUri::from_abs_path(&config.cwd.join("AGENTS.md")), None)
        .await
        .expect("AGENTS.md metadata");
    filesystem.set_target_metadata(metadata);
    fs::write(&agents_path, "version two").expect("replace same-size AGENTS.md");

    let second = manager
        .refresh_and_get_loaded(&config, &environments)
        .await
        .expect("second load");

    assert_eq!(filesystem.target_stream_calls(), 2);
    assert_eq!(
        second.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nversion two",
            agents_path.display()
        )
    );
    assert!(!Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn repository_stable_context_refreshes_when_instruction_source_changes() {
    let root = tempfile::tempdir().expect("workspace");
    let agents_path = root.path().join("AGENTS.md");
    fs::write(&agents_path, "stable instructions").expect("write AGENTS.md");
    let mut config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 3);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    let first = manager
        .refresh_and_observe(&config, &environments)
        .await
        .stable_context
        .expect("first stable context");
    fs::remove_file(&agents_path).expect("remove AGENTS.md");
    fs::write(root.path().join("WORKFLOW.md"), "stable instructions")
        .expect("write fallback instructions");
    config.project_doc_fallback_filenames = vec!["WORKFLOW.md".to_string()];

    let replacement = manager
        .refresh_and_observe(&config, &environments)
        .await
        .stable_context
        .expect("replacement stable context");

    assert_ne!(first.identity, replacement.identity);
    assert_eq!(
        first.rendered.as_ref(),
        format!(
            "## AGENTS.md instructions from {}\n\nstable instructions",
            agents_path.display()
        )
    );
    assert_eq!(
        replacement.rendered.as_ref(),
        format!(
            "## AGENTS.md instructions from {}\n\nstable instructions",
            root.path().join("WORKFLOW.md").display()
        )
    );
    assert!(!replacement.reused);
    assert!(!replacement.semantic_replacement);
}

#[tokio::test]
async fn overlapping_refreshes_publish_in_request_order() {
    let root = tempfile::tempdir().expect("workspace");
    let agents_path = root.path().join("AGENTS.md");
    fs::write(&agents_path, "version one").expect("write first AGENTS.md");
    let config = config_for(&root).await;
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let first_environments =
        environment_snapshot_with_environment(&config.cwd, 10, Arc::clone(&environment));
    let second_environments =
        environment_snapshot_with_environment(&config.cwd, 11, Arc::clone(&environment));
    let first_key = AgentsMdCacheKey::capture(&config, &first_environments);
    let second_key = AgentsMdCacheKey::capture(&config, &second_environments);
    assert_ne!(first_key, second_key);

    let first_started = Arc::new(Notify::new());
    let first_release = Arc::new(Notify::new());
    filesystem.set_next_project_read(NextProjectRead::Block {
        started: Arc::clone(&first_started),
        release: Arc::clone(&first_release),
    });
    let manager = Arc::new(AgentsMdManager::new(/*user_instructions*/ None));
    let first_manager = Arc::clone(&manager);
    let first_config = config.clone();
    let first_refresh = tokio::spawn(async move {
        first_manager
            .refresh_and_get_loaded(&first_config, &first_environments)
            .await
    });
    timeout(Duration::from_secs(5), first_started.notified())
        .await
        .expect("first refresh should reach the project read");

    fs::write(&agents_path, "version two").expect("write second AGENTS.md");
    let second_started = Arc::new(Notify::new());
    let second_release = Arc::new(Notify::new());
    filesystem.set_next_project_read(NextProjectRead::Block {
        started: Arc::clone(&second_started),
        release: Arc::clone(&second_release),
    });
    let second_calling_refresh = Arc::new(Notify::new());
    let second_manager = Arc::clone(&manager);
    let second_config = config.clone();
    let second_calling_refresh_in_task = Arc::clone(&second_calling_refresh);
    let second_refresh = tokio::spawn(async move {
        second_calling_refresh_in_task.notify_one();
        second_manager
            .refresh_and_get_loaded(&second_config, &second_environments)
            .await
    });
    timeout(Duration::from_secs(5), second_calling_refresh.notified())
        .await
        .expect("second refresh should start");
    assert_eq!(filesystem.target_stream_calls(), 1);

    first_release.notify_one();
    let first_loaded = timeout(Duration::from_secs(5), first_refresh)
        .await
        .expect("first refresh should finish")
        .expect("first refresh task should succeed")
        .expect("first refresh should return its instructions");
    timeout(Duration::from_secs(5), second_started.notified())
        .await
        .expect("second refresh should read after the first publishes");
    assert_eq!(filesystem.target_stream_calls(), 2);
    second_release.notify_one();
    let second_loaded = timeout(Duration::from_secs(5), second_refresh)
        .await
        .expect("second refresh should finish")
        .expect("second refresh task should succeed")
        .expect("second refresh should return its instructions");

    assert_eq!(
        first_loaded.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nversion one",
            agents_path.display()
        )
    );
    assert_eq!(
        second_loaded.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nversion two",
            agents_path.display()
        )
    );
    let loaded = manager.get_loaded().await.expect("latest instructions");
    assert_eq!(
        loaded.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nversion two",
            agents_path.display()
        )
    );
    let cache = manager.cache.lock().await;
    assert_eq!(cache.key.as_ref(), Some(&second_key));
}

#[tokio::test]
async fn step_refresh_captures_environments_after_entering_refresh_gate() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "initial instructions").expect("write AGENTS.md");
    let config = Arc::new(config_for(&root).await);
    let initial_generation = 20;
    let thread_environments = ThreadEnvironments::new(
        Arc::new(EnvironmentManager::default_for_tests()),
        default_user_shell(),
        ShellSnapshot::disabled(),
        environment_snapshot(&config.cwd, initial_generation),
        /*non_blocking_snapshots*/ false,
    );
    let manager = AgentsMdManager::new(/*user_instructions*/ None);
    let refresh_guard = manager
        .refresh_gate
        .acquire()
        .await
        .expect("refresh gate should remain open");
    let mut refresh = Box::pin(manager.refresh_for_step(
        &config,
        &thread_environments,
        |snapshot| async move { snapshot.generation },
    ));

    assert!(futures::poll!(refresh.as_mut()).is_pending());
    thread_environments.update_selections(&[]);
    drop(refresh_guard);

    let (environments, observation, prepared_generation) = refresh.await;
    assert_eq!(environments.generation, initial_generation + 1);
    assert_eq!(prepared_generation, environments.generation);
    assert!(environments.turn_environments.is_empty());
    assert!(observation.loaded.is_none());
    assert_eq!(observation.freshness, AgentsMdFreshness::Refreshed);
    let cache = manager.cache.lock().await;
    assert_eq!(
        cache.key.as_ref().map(|key| key.environment_generation),
        Some(initial_generation + 1)
    );
}

#[tokio::test]
async fn step_refresh_overlaps_preparation_and_finishes_after_preparation_failure() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "fresh instructions").unwrap();
    let config = Arc::new(config_for(&root).await);
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let thread_environments = ThreadEnvironments::new(
        Arc::new(EnvironmentManager::default_for_tests()),
        default_user_shell(),
        ShellSnapshot::disabled(),
        environment_snapshot_with_environment(&config.cwd, 3, environment),
        /*non_blocking_snapshots*/ false,
    );
    let manager = AgentsMdManager::new(None);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    filesystem.set_next_project_read(NextProjectRead::Block {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });

    let (snapshot, observation, prepared) = timeout(
        REFRESH_TIMEOUT,
        manager.refresh_for_step(&config, &thread_environments, |snapshot| async move {
            started.notified().await;
            assert_eq!(snapshot.generation, 3);
            release.notify_one();
            Err::<(), _>("preparation failed")
        }),
    )
    .await
    .expect("preparation must unblock instruction reading before its timeout");

    assert_eq!(snapshot.generation, 3);
    assert_eq!(prepared, Err("preparation failed"));
    assert_eq!(observation.freshness, AgentsMdFreshness::Refreshed);
    assert!(
        observation
            .loaded
            .unwrap()
            .text()
            .contains("fresh instructions")
    );
    assert!(manager.refresh_gate.try_acquire().is_ok());
}

#[tokio::test]
async fn step_refresh_timeout_keeps_preparation_scope_and_releases_gate() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "cached instructions").unwrap();
    let config = Arc::new(config_for(&root).await);
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let environments = environment_snapshot_with_environment(&config.cwd, 3, environment);
    let manager = AgentsMdManager::new(None);
    let cached = manager
        .refresh_and_observe_shared(&config, &environments)
        .await;
    let thread_environments = ThreadEnvironments::new(
        Arc::new(EnvironmentManager::default_for_tests()),
        default_user_shell(),
        ShellSnapshot::disabled(),
        environments,
        /*non_blocking_snapshots*/ false,
    );
    let started = Arc::new(Notify::new());
    filesystem.set_next_project_read(NextProjectRead::Block {
        started: Arc::clone(&started),
        release: Arc::new(Notify::new()),
    });

    let (snapshot, observation, prepared_generation) = timeout(
        REFRESH_TIMEOUT + Duration::from_secs(1),
        manager.refresh_for_step(&config, &thread_environments, |snapshot| {
            let started = &started;
            let thread_environments = &thread_environments;
            let manager = &manager;
            async move {
                started.notified().await;
                thread_environments.update_selections(&[]);
                // Instruction timeout must release serialization even while preparation is pending.
                let _permit = manager.refresh_gate.acquire().await.unwrap();
                snapshot.generation
            }
        }),
    )
    .await
    .expect("instruction timeout must not retain the gate while waiting for preparation");

    assert_eq!(snapshot.generation, 3);
    assert_eq!(prepared_generation, snapshot.generation);
    assert_eq!(observation.freshness, AgentsMdFreshness::CachedFallback);
    assert_eq!(
        observation.loaded.unwrap().text(),
        cached.loaded.unwrap().text()
    );
    assert!(
        thread_environments
            .snapshot_now()
            .await
            .turn_environments
            .is_empty()
    );
}

#[tokio::test]
async fn cancelling_step_refresh_drops_preparation_and_releases_gate() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "fresh instructions").unwrap();
    let config = Arc::new(config_for(&root).await);
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let thread_environments = ThreadEnvironments::new(
        Arc::new(EnvironmentManager::default_for_tests()),
        default_user_shell(),
        ShellSnapshot::disabled(),
        environment_snapshot_with_environment(&config.cwd, 3, environment),
        /*non_blocking_snapshots*/ false,
    );
    let manager = AgentsMdManager::new(None);
    let started = Arc::new(Notify::new());
    filesystem.set_next_project_read(NextProjectRead::Block {
        started: Arc::clone(&started),
        release: Arc::new(Notify::new()),
    });
    let preparation_dropped = tokio_util::sync::CancellationToken::new();
    let prepare = |_| async {
        let _guard = preparation_dropped.clone().drop_guard();
        std::future::pending::<()>().await;
    };
    let mut refresh = Box::pin(manager.refresh_for_step(&config, &thread_environments, prepare));
    tokio::select! {
        _ = &mut refresh => panic!("blocked refresh must not finish"),
        _ = started.notified() => {}
    }
    assert!(!preparation_dropped.is_cancelled());
    drop(refresh);

    assert!(preparation_dropped.is_cancelled());
    assert!(manager.refresh_gate.try_acquire().is_ok());
    assert!(manager.get_loaded().await.is_none());
}

#[tokio::test]
async fn same_key_read_failure_retains_last_successful_instructions_and_recovers() {
    let root = tempfile::tempdir().expect("workspace");
    let agents_path = root.path().join("AGENTS.md");
    fs::write(&agents_path, "version one").expect("write first AGENTS.md");
    let config = config_for(&root).await;
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let environments = environment_snapshot_with_environment(&config.cwd, 12, environment);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    let first_observation = manager.refresh_and_observe(&config, &environments).await;
    assert_eq!(first_observation.freshness, AgentsMdFreshness::Refreshed);
    let first = first_observation.loaded.expect("first load");
    let cached_observation = manager.get_cached_observation().await;
    assert_eq!(
        cached_observation.freshness,
        AgentsMdFreshness::CachedFallback
    );
    assert!(Arc::ptr_eq(
        &first,
        cached_observation.loaded.as_ref().expect("cached load")
    ));
    fs::write(&agents_path, "unreadable version two").expect("write unreadable AGENTS.md");
    filesystem.set_next_project_read(NextProjectRead::Fail(io::ErrorKind::PermissionDenied));
    let retained_observation = manager.refresh_and_observe(&config, &environments).await;
    assert_eq!(
        retained_observation.freshness,
        AgentsMdFreshness::CachedFallback
    );
    let retained = retained_observation.loaded.expect("retained load");
    assert!(Arc::ptr_eq(&first, &retained));
    assert_eq!(
        retained.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nversion one",
            agents_path.display()
        )
    );

    fs::write(&agents_path, "version two recovered").expect("write recovered AGENTS.md");
    let recovered_observation = manager.refresh_and_observe(&config, &environments).await;
    assert_eq!(
        recovered_observation.freshness,
        AgentsMdFreshness::Refreshed
    );
    let recovered = recovered_observation.loaded.expect("recovered load");
    assert!(!Arc::ptr_eq(&retained, &recovered));
    assert_eq!(
        recovered.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nversion two recovered",
            agents_path.display()
        )
    );
}

#[tokio::test]
async fn successful_primary_refresh_is_not_hidden_by_secondary_failure() {
    let primary = tempfile::tempdir().unwrap();
    let secondary = tempfile::tempdir().unwrap();
    fs::write(primary.path().join("AGENTS.md"), "primary one").unwrap();
    fs::write(secondary.path().join("AGENTS.md"), "secondary one").unwrap();
    let config = config_for(&primary).await;
    let secondary_cwd = AbsolutePathBuf::try_from(secondary.path().to_path_buf()).unwrap();
    let secondary_fs = Arc::new(ControlledFileSystem::new(secondary_cwd.join("AGENTS.md")));
    let mut environments = environment_snapshot(&config.cwd, 1);
    environments.turn_environments.push(TurnEnvironment::new("secondary".into(),
        Arc::new(Environment::default_for_tests_with_filesystem(secondary_fs.clone())),
        PathUri::from_abs_path(&secondary_cwd), None));
    let manager = AgentsMdManager::new(None);
    manager.refresh_and_observe(&config, &environments).await;
    fs::write(primary.path().join("AGENTS.md"), "primary two: do not modify X").unwrap();
    secondary_fs.set_next_project_read(NextProjectRead::Fail(io::ErrorKind::PermissionDenied));
    let result = manager.refresh_and_observe(&config, &environments).await;
    let text = result.loaded.unwrap().text();
    assert!(text.contains("primary two: do not modify X"));
    assert!(!text.contains("primary one"));
    assert!(text.contains("secondary one"));
    assert_eq!(result.freshness, AgentsMdFreshness::CachedFallback);
    fs::remove_file(secondary.path().join("AGENTS.md")).unwrap();
    let result = manager.refresh_and_observe(&config, &environments).await;
    assert!(!result.loaded.unwrap().text().contains("secondary one"));
}

#[tokio::test]
async fn content_and_missing_higher_precedence_file_invalidate_cache() {
    let root = tempfile::tempdir().expect("workspace");
    let agents = root.path().join("AGENTS.md");
    let override_path = root.path().join("AGENTS.override.md");
    fs::write(&agents, "version one").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 0);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&config, &environments).await;
    let first = manager.get_loaded().await.expect("first load");
    fs::write(&agents, "version two").expect("replace same-size contents");
    manager.refresh(&config, &environments).await;
    let changed = manager.get_loaded().await.expect("changed load");
    assert!(!Arc::ptr_eq(&first, &changed));
    assert_eq!(
        changed.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nversion two",
            agents.display()
        )
    );
    assert_ne!(first.semantic_digest(), changed.semantic_digest());

    fs::write(&override_path, "local override").expect("create override");
    manager.refresh(&config, &environments).await;
    let overridden = manager.get_loaded().await.expect("override load");
    assert!(!Arc::ptr_eq(&changed, &overridden));
    assert_eq!(
        overridden.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nlocal override",
            override_path.display()
        )
    );
}

#[tokio::test]
async fn a_failed_instruction_file_does_not_hide_a_sibling_change_or_deletion() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    let child = root.path().join("child");
    fs::create_dir(&child).unwrap();
    let parent_doc = root.path().join("AGENTS.md");
    let child_doc = child.join("AGENTS.md");
    fs::write(&parent_doc, "parent old").unwrap();
    fs::write(&child_doc, "child retained").unwrap();
    let mut config = config_for(&root).await;
    config.cwd = AbsolutePathBuf::from_absolute_path(&child).unwrap();
    let filesystem = Arc::new(ControlledFileSystem::new(
        AbsolutePathBuf::from_absolute_path(&child_doc).unwrap()));
    let environments = environment_snapshot_with_environment(&config.cwd, 1,
        Arc::new(Environment::default_for_tests_with_filesystem(filesystem.clone())));
    let manager = AgentsMdManager::new(None);
    manager.refresh_and_observe(&config, &environments).await;
    fs::write(&parent_doc, "parent new restriction").unwrap();
    filesystem.set_next_project_read(NextProjectRead::Fail(io::ErrorKind::PermissionDenied));
    let changed = manager.refresh_and_observe(&config, &environments).await;
    assert_eq!(changed.freshness, AgentsMdFreshness::CachedFallback);
    let text = changed.loaded.unwrap().text();
    assert!(text.contains("parent new restriction"));
    assert!(!text.contains("parent old"));
    assert!(text.contains("child retained"));
    fs::remove_file(parent_doc).unwrap();
    filesystem.set_next_project_read(NextProjectRead::Fail(io::ErrorKind::PermissionDenied));
    let deleted = manager.refresh_and_observe(&config, &environments).await;
    let text = deleted.loaded.unwrap().text();
    assert!(!text.contains("parent new restriction"));
    assert!(text.contains("child retained"));
}

#[tokio::test]
async fn names_and_limits_are_cache_dependencies() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("WORKFLOW.md"), "fallback instructions").expect("write fallback");
    let mut config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 1);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&config, &environments).await;
    assert!(manager.get_loaded().await.is_none());

    config.project_doc_fallback_filenames = vec!["WORKFLOW.md".to_string()];
    manager.refresh(&config, &environments).await;
    let fallback = manager.get_loaded().await.expect("fallback load");
    let fallback_path = root.path().join("WORKFLOW.md");
    assert_eq!(
        fallback.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nfallback instructions",
            fallback_path.display()
        )
    );

    config.project_doc_max_bytes = 8;
    manager.refresh(&config, &environments).await;
    let truncated = manager.get_loaded().await.expect("truncated fallback load");
    assert_eq!(
        truncated.text(),
        format!(
            "## AGENTS.md instructions from {path}\n\nfallback\n\n[Project documentation truncation notice: source path: {path}; original byte count: 21; retained byte count: 8; omitted byte count: 13.]",
            path = fallback_path.display()
        )
    );
}

#[tokio::test]
async fn effective_marker_configuration_invalidates_cached_discovery() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join(".codex-root"), "").expect("write marker");
    fs::write(root.path().join("AGENTS.md"), "root instructions").expect("write root doc");
    let nested = root.path().join("nested");
    fs::create_dir(&nested).expect("create nested directory");
    fs::write(nested.join("AGENTS.md"), "nested instructions").expect("write nested doc");

    let mut default_config = config_for(&root).await;
    default_config.cwd = AbsolutePathBuf::from_absolute_path(&nested).expect("absolute nested");
    let codex_home = tempfile::tempdir().expect("marker codex home");
    let mut marker_config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(nested.clone()))
        .cli_overrides(vec![(
            "project_root_markers".to_string(),
            TomlValue::Array(vec![TomlValue::String(".codex-root".to_string())]),
        )])
        .build()
        .await
        .expect("marker config");
    marker_config.cwd = default_config.cwd.clone();
    marker_config.project_doc_max_bytes = default_config.project_doc_max_bytes;
    let environments = environment_snapshot(&default_config.cwd, 4);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&default_config, &environments).await;
    let nested_only = manager.get_loaded().await.expect("nested load");
    assert_eq!(
        nested_only.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nnested instructions",
            nested.join("AGENTS.md").display()
        )
    );

    manager.refresh(&marker_config, &environments).await;
    let with_root = manager.get_loaded().await.expect("root-aware load");
    assert!(!Arc::ptr_eq(&nested_only, &with_root));
    assert_eq!(
        with_root.text(),
        format!(
            "## AGENTS.md instructions from {}\n\nroot instructions\n\n## AGENTS.md instructions from {}\n\nnested instructions",
            root.path().join("AGENTS.md").display(),
            nested.join("AGENTS.md").display()
        )
    );
}

#[tokio::test]
async fn identical_paths_on_distinct_filesystems_do_not_share_cache_entries() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "instructions").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let first_environments = environment_snapshot(&config.cwd, 9);
    let second_environments = environment_snapshot(&config.cwd, 9);
    let first_key = AgentsMdCacheKey::capture(&config, &first_environments);
    let second_key = AgentsMdCacheKey::capture(&config, &second_environments);
    assert_ne!(first_key, second_key);

    let manager = AgentsMdManager::new(/*user_instructions*/ None);
    manager.refresh(&config, &first_environments).await;
    let first = manager.get_loaded().await.expect("first filesystem load");
    manager.refresh(&config, &second_environments).await;
    let second = manager.get_loaded().await.expect("second filesystem load");
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(first.text(), second.text());

    let mut next_generation = second_environments.clone();
    next_generation.generation += 1;
    assert_ne!(
        AgentsMdCacheKey::capture(&config, &second_environments),
        AgentsMdCacheKey::capture(&config, &next_generation)
    );
}

#[tokio::test]
async fn audit_stalled_instruction_read_falls_back_and_releases_gate() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("AGENTS.md"), "retained instructions").unwrap();
    let config = config_for(&root).await;
    let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
    let environment = Arc::new(Environment::default_for_tests_with_filesystem(
        filesystem.clone(),
    ));
    let environments = environment_snapshot_with_environment(&config.cwd, 1, environment);
    let manager = AgentsMdManager::new(None);
    let first = manager.refresh_and_observe(&config, &environments).await;
    filesystem.set_next_project_read(NextProjectRead::Block {
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let fallback = timeout(
        REFRESH_TIMEOUT + Duration::from_secs(1),
        manager.refresh_and_observe(&config, &environments),
    )
    .await
    .expect("bounded refresh");
    assert_eq!(fallback.freshness, AgentsMdFreshness::CachedFallback);
    assert_eq!(
        fallback.loaded.unwrap().text(),
        first.loaded.unwrap().text()
    );
    fs::write(root.path().join("AGENTS.md"), "recovered instructions").unwrap();
    let recovered = manager.refresh_and_observe(&config, &environments).await;
    assert_eq!(recovered.freshness, AgentsMdFreshness::Refreshed);
    assert!(
        recovered
            .loaded
            .unwrap()
            .text()
            .contains("recovered instructions")
    );
    let mut changed_scope = environments.clone();
    changed_scope.generation += 1;
    assert!(
        manager
            .scoped_fallback(&config, &changed_scope)
            .loaded
            .is_none()
    );
}
#[tokio::test]
async fn survivability_failed_nearest_source_keeps_priority_under_parent_growth() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    let child = root.path().join("child");
    fs::create_dir(&child).unwrap();
    let parent_doc = root.path().join("AGENTS.md");
    let child_doc = child.join("AGENTS.md");
    fs::write(&parent_doc, "parent old").unwrap();
    let restriction = "Never edit protected.txt. ".repeat(20);
    fs::write(&child_doc, &restriction).unwrap();
    let mut config = config_for(&root).await;
    config.project_doc_max_bytes = 1024;
    config.cwd = AbsolutePathBuf::from_absolute_path(&child).unwrap();
    let filesystem = Arc::new(ControlledFileSystem::new(AbsolutePathBuf::from_absolute_path(&child_doc).unwrap()));
    let environments = environment_snapshot_with_environment(&config.cwd, 1,
        Arc::new(Environment::default_for_tests_with_filesystem(filesystem.clone())));
    let manager = AgentsMdManager::new(None);
    manager.refresh_and_observe(&config, &environments).await;
    fs::write(&parent_doc, "parent current ".repeat(1000)).unwrap();
    filesystem.set_next_project_read(NextProjectRead::Fail(io::ErrorKind::PermissionDenied));
    let observed = manager.refresh_and_observe(&config, &environments).await;
    assert_eq!(observed.freshness, AgentsMdFreshness::CachedFallback);
    let text = observed.loaded.unwrap().text();
    assert!(text.contains(&restriction));
    assert!(text.contains("parent current"));
    assert!(!text.contains("parent old"));
    assert_eq!(filesystem.target_stream_calls(), 2, "one read per refresh; no retry");
}
#[tokio::test]
async fn failed_truncated_instruction_reads_preserve_source_counts_and_encoding_notice() {
    for contents in [b"abcdefgh".to_vec(), b"ab\xffdefgh".to_vec()] {
        let root = tempfile::tempdir().unwrap();
        let agents_path = root.path().join("AGENTS.md");
        fs::write(&agents_path, &contents).unwrap();
        let mut config = config_for(&root).await;
        config.project_doc_max_bytes = 4;
        let filesystem = Arc::new(ControlledFileSystem::new(config.cwd.join("AGENTS.md")));
        let environments = environment_snapshot_with_environment(
            &config.cwd, 1,
            Arc::new(Environment::default_for_tests_with_filesystem(filesystem.clone())),
        );
        let manager = AgentsMdManager::new(None);
        let first = manager.refresh_and_observe(&config, &environments).await;
        let first = first.loaded.unwrap();
        // The fixture has eight source bytes and a four-byte source allowance.
        // Generated notices are not source bytes and must not become input on fallback.
        let expected_body = if contents[2] == 0xff {
            "ab\u{fffd}d\n\n[Project documentation encoding notice: invalid UTF-8 bytes were replaced with U+FFFD; instructions may be incomplete.]"
        } else {
            "abcd"
        };
        let expected = format!(
            "## AGENTS.md instructions from {path}\n\n{expected_body}\n\n[Project documentation truncation notice: source path: {path}; original byte count: 8; retained byte count: 4; omitted byte count: 4.]",
            path = agents_path.display(),
        );
        assert_eq!(first.text(), expected);
        for _ in 0..2 {
            filesystem.set_next_project_read(NextProjectRead::Fail(io::ErrorKind::PermissionDenied));
            let fallback = manager.refresh_and_observe(&config, &environments).await;
            assert_eq!(fallback.freshness, AgentsMdFreshness::CachedFallback);
            let retained = fallback.loaded.unwrap();
            assert_eq!(retained.text(), expected);
            assert!(Arc::ptr_eq(&first, &retained));
        }
        fs::write(&agents_path, "new!").unwrap();
        let recovered = manager.refresh_and_observe(&config, &environments).await;
        assert_eq!(recovered.freshness, AgentsMdFreshness::Refreshed);
        assert_eq!(recovered.loaded.unwrap().text(), format!(
            "## AGENTS.md instructions from {}\n\nnew!", agents_path.display(),
        ));
    }
}
