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

struct ControlledFileSystem {
    target: AbsolutePathBuf,
    next_project_read: StdMutex<NextProjectRead>,
    target_stream_calls: AtomicUsize,
}

impl ControlledFileSystem {
    fn new(target: AbsolutePathBuf) -> Self {
        Self {
            target,
            next_project_read: StdMutex::new(NextProjectRead::Normal),
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
        LOCAL_FS.get_metadata(path, sandbox)
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
async fn stable_refresh_reuses_loaded_arc_and_semantic_digest() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "stable instructions").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let environments = environment_snapshot(&config.cwd, 3);
    let manager = AgentsMdManager::new(/*user_instructions*/ None);

    manager.refresh(&config, &environments).await;
    let first = manager.get_loaded().await.expect("first load");
    manager.refresh(&config, &environments).await;
    let second = manager.get_loaded().await.expect("cached load");

    assert!(Arc::ptr_eq(&first, &second));
    let expected_digest: [u8; 32] = Sha256::digest(first.text().as_bytes()).into();
    assert_eq!(first.semantic_digest(), expected_digest);
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

    assert_eq!(first_loaded.text(), "version one");
    assert_eq!(second_loaded.text(), "version two");
    let loaded = manager.get_loaded().await.expect("latest instructions");
    assert_eq!(loaded.text(), "version two");
    let cache = manager.cache.lock().await;
    assert_eq!(cache.key.as_ref(), Some(&second_key));
}

#[tokio::test]
async fn step_refresh_captures_environments_after_entering_refresh_gate() {
    let root = tempfile::tempdir().expect("workspace");
    fs::write(root.path().join("AGENTS.md"), "initial instructions").expect("write AGENTS.md");
    let config = config_for(&root).await;
    let initial_generation = 20;
    let thread_environments = ThreadEnvironments::new(
        Arc::new(EnvironmentManager::default_for_tests()),
        default_user_shell(),
        ShellSnapshot::disabled(),
        environment_snapshot(&config.cwd, initial_generation),
        /*non_blocking_snapshots*/ false,
    );
    let manager = AgentsMdManager::new(/*user_instructions*/ None);
    let refresh_guard = manager.refresh_gate.lock().await;
    let mut refresh = Box::pin(manager.refresh_for_step(&config, &thread_environments));

    assert!(futures::poll!(refresh.as_mut()).is_pending());
    thread_environments.update_selections(&[]);
    drop(refresh_guard);

    let (environments, loaded) = refresh.await;
    assert_eq!(environments.generation, initial_generation + 1);
    assert!(environments.turn_environments.is_empty());
    assert!(loaded.is_none());
    let cache = manager.cache.lock().await;
    assert_eq!(
        cache.key.as_ref().map(|key| key.environment_generation),
        Some(initial_generation + 1)
    );
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

    manager.refresh(&config, &environments).await;
    let first = manager.get_loaded().await.expect("first load");
    filesystem.set_next_project_read(NextProjectRead::Fail(io::ErrorKind::PermissionDenied));
    manager.refresh(&config, &environments).await;
    let retained = manager.get_loaded().await.expect("retained load");
    assert!(Arc::ptr_eq(&first, &retained));
    assert_eq!(retained.text(), "version one");

    fs::write(&agents_path, "version two").expect("write recovered AGENTS.md");
    manager.refresh(&config, &environments).await;
    let recovered = manager.get_loaded().await.expect("recovered load");
    assert!(!Arc::ptr_eq(&retained, &recovered));
    assert_eq!(recovered.text(), "version two");
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
    assert_eq!(changed.text(), "version two");
    assert_ne!(first.semantic_digest(), changed.semantic_digest());

    fs::write(&override_path, "local override").expect("create override");
    manager.refresh(&config, &environments).await;
    let overridden = manager.get_loaded().await.expect("override load");
    assert!(!Arc::ptr_eq(&changed, &overridden));
    assert_eq!(overridden.text(), "local override");
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
    assert_eq!(fallback.text(), "fallback instructions");

    config.project_doc_max_bytes = 8;
    manager.refresh(&config, &environments).await;
    let truncated = manager.get_loaded().await.expect("truncated load");
    assert!(!Arc::ptr_eq(&fallback, &truncated));
    let truncated_text = truncated.text();
    assert!(truncated_text.starts_with("fallback"));
    assert!(truncated_text.contains(&root.path().join("WORKFLOW.md").display().to_string()));
    assert!(truncated_text.contains("original byte count: 21"));
    assert!(truncated_text.contains("retained byte count: 8"));
    assert!(truncated_text.contains("omitted byte count: 13"));
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
    assert_eq!(nested_only.text(), "nested instructions");

    manager.refresh(&marker_config, &environments).await;
    let with_root = manager.get_loaded().await.expect("root-aware load");
    assert!(!Arc::ptr_eq(&nested_only, &with_root));
    assert_eq!(with_root.text(), "root instructions\n\nnested instructions");
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
