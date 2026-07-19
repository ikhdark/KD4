use super::*;

use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::turn_context::TurnEnvironment;
use crate::shell_snapshot::ShellSnapshot;

fn test_runtime_paths() -> ExecServerRuntimePaths {
    ExecServerRuntimePaths::new(
        std::env::current_exe().expect("current exe"),
        /*codex_linux_sandbox_exe*/ None,
    )
    .expect("runtime paths")
}

async fn local_environment_manager() -> Arc<EnvironmentManager> {
    Arc::new(
        EnvironmentManager::create_for_tests(
            /*remote_endpoint*/ None,
            Some(test_runtime_paths()),
        )
        .await,
    )
}

async fn local_snapshot(cwd: AbsolutePathBuf, generation: u64) -> TurnEnvironmentSnapshot {
    let manager = local_environment_manager().await;
    let environment = manager
        .get_environment(LOCAL_ENVIRONMENT_ID)
        .expect("local environment");
    TurnEnvironmentSnapshot {
        generation,
        turn_environments: vec![TurnEnvironment::new(
            LOCAL_ENVIRONMENT_ID.to_string(),
            environment,
            PathUri::from_abs_path(&cwd),
            None,
        )],
        starting: Vec::new(),
    }
}

async fn run_git(repo: &Path, args: &[&str]) -> std::process::Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .await
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

async fn create_clean_git_repo() -> (TempDir, AbsolutePathBuf) {
    let temp_dir = TempDir::new().expect("temp dir");
    let repo = AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute repo");
    run_git(repo.as_path(), &["init", "-q"]).await;
    run_git(repo.as_path(), &["config", "user.name", "Codex Tests"]).await;
    run_git(
        repo.as_path(),
        &["config", "user.email", "codex-tests@example.com"],
    )
    .await;
    std::fs::write(repo.join("README.md"), "initial\n").expect("write tracked file");
    run_git(repo.as_path(), &["add", "README.md"]).await;
    run_git(repo.as_path(), &["commit", "-q", "-m", "initial"]).await;
    (temp_dir, repo)
}

#[tokio::test]
async fn environment_generation_advances_only_when_selection_changes() {
    let manager = local_environment_manager().await;
    let root = TempDir::new().expect("temp root");
    let first_cwd = AbsolutePathBuf::from_absolute_path(root.path()).expect("absolute cwd");
    let second_cwd = first_cwd.join("next");
    std::fs::create_dir_all(&second_cwd).expect("create second cwd");
    let environments = ThreadEnvironments::new(
        manager,
        crate::shell::default_user_shell(),
        ShellSnapshot::disabled(),
        TurnEnvironmentSnapshot::default(),
        /*non_blocking_snapshots*/ false,
    );
    let first_selection = TurnEnvironmentSelection {
        environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
        cwd: PathUri::from_abs_path(&first_cwd),
    };

    environments.update_selections(std::slice::from_ref(&first_selection));
    assert_eq!(environments.snapshot().await.generation, 1);
    environments.update_selections(std::slice::from_ref(&first_selection));
    assert_eq!(environments.snapshot().await.generation, 1);
    environments.update_selections(&[TurnEnvironmentSelection {
        environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
        cwd: PathUri::from_abs_path(&second_cwd),
    }]);
    assert_eq!(environments.snapshot().await.generation, 2);
}

#[tokio::test]
async fn root_snapshot_invalidates_git_marker_creation_and_removal() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute cwd");
    let environments = local_snapshot(cwd.clone(), 7).await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    let before = cache.snapshot(&environments).await;
    assert_eq!(before.primary_is_git(), Some(false));

    std::fs::create_dir(cwd.join(".git")).expect("create git marker");
    let created = cache.snapshot(&environments).await;
    assert_eq!(created.primary_is_git(), Some(true));

    std::fs::remove_dir_all(cwd.join(".git")).expect("remove git marker");
    let removed = cache.snapshot(&environments).await;
    assert_eq!(removed.primary_is_git(), Some(false));
}

#[tokio::test]
async fn stable_metadata_dependencies_refresh_head_and_remotes_but_dirty_is_always_fresh() {
    let (_temp_dir, repo) = create_clean_git_repo().await;
    run_git(
        repo.as_path(),
        &["remote", "add", "origin", "https://example.com/old.git"],
    )
    .await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let source = GitWorkspaceMetadataSource {
        cwd: repo.clone(),
        repo_root: repo.clone(),
        cache,
    };

    let first = source.metadata().await;
    assert_eq!(first.has_changes, Some(false));
    assert_eq!(
        first
            .associated_remote_urls
            .as_ref()
            .and_then(|remotes| remotes.get("origin"))
            .map(String::as_str),
        Some("https://example.com/old.git")
    );
    let first_head = first.latest_git_commit_hash.expect("initial head");

    std::fs::write(repo.join("dirty.txt"), "dirty\n").expect("write dirty file");
    assert_eq!(source.metadata().await.has_changes, Some(true));
    std::fs::remove_file(repo.join("dirty.txt")).expect("remove dirty file");
    assert_eq!(source.metadata().await.has_changes, Some(false));

    run_git(repo.as_path(), &["checkout", "-q", "-b", "next"]).await;
    run_git(
        repo.as_path(),
        &["commit", "--allow-empty", "-q", "-m", "next"],
    )
    .await;
    run_git(
        repo.as_path(),
        &["remote", "set-url", "origin", "https://example.com/new.git"],
    )
    .await;

    let changed = source.metadata().await;
    assert_ne!(
        changed.latest_git_commit_hash.as_deref(),
        Some(first_head.as_str())
    );
    assert_eq!(
        changed
            .associated_remote_urls
            .as_ref()
            .and_then(|remotes| remotes.get("origin"))
            .map(String::as_str),
        Some("https://example.com/new.git")
    );
    assert_eq!(changed.has_changes, Some(false));
}

#[test]
fn executable_dependency_changes_when_binary_is_replaced() {
    let temp_dir = TempDir::new().expect("temp dir");
    let executable = temp_dir.path().join("git-test");
    std::fs::write(&executable, b"first").expect("write executable");
    let before = dependency_fingerprint(executable.clone(), false).expect("dependency");
    std::fs::write(&executable, b"replacement-binary").expect("replace executable");
    let after = dependency_fingerprint(executable, false).expect("dependency");
    assert_ne!(before, after);
}

#[tokio::test]
async fn watcher_failure_clears_and_disables_cached_identity() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute cwd");
    let environments = local_snapshot(cwd, 11).await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    cache.snapshot(&environments).await;
    assert!(cache.state.lock().await.root.is_some());
    cache.invalidate_for_watcher_failure().await;

    let state = cache.state.lock().await;
    assert!(state.root.is_none());
    assert!(state.metadata.is_empty());
    assert!(!cache.watcher_reliable.load(Ordering::Acquire));
}
