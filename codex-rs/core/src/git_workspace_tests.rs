use super::*;

use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::LocalAgentTaskStore;
use codex_agent_task_store::WorkspaceActorKind;
use codex_agent_task_store::WorkspaceActorRegistration;
use codex_agent_task_store::WorkspaceMutationRequest;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_state::StateRuntime;
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
async fn stable_metadata_dependencies_refresh_head_but_dirty_is_always_fresh() {
    let (_temp_dir, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let source = GitWorkspaceMetadataSource {
        cwd: repo.clone(),
        repo_root: repo.clone(),
        cache,
    };

    let first = source.metadata().await;
    assert_eq!(first.has_changes, Some(false));
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

    let changed = source.metadata().await;
    assert_ne!(
        changed.latest_git_commit_hash.as_deref(),
        Some(first_head.as_str())
    );
    assert_eq!(changed.has_changes, Some(false));
}

#[tokio::test]
async fn stable_metadata_dependencies_refresh_remotes() {
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
    assert_eq!(
        first
            .associated_remote_urls
            .as_ref()
            .and_then(|remotes| remotes.get("origin"))
            .map(String::as_str),
        Some("https://example.com/old.git")
    );

    run_git(
        repo.as_path(),
        &["remote", "set-url", "origin", "https://example.com/new.git"],
    )
    .await;

    let changed = source.metadata().await;
    assert_eq!(
        changed
            .associated_remote_urls
            .as_ref()
            .and_then(|remotes| remotes.get("origin"))
            .map(String::as_str),
        Some("https://example.com/new.git")
    );
}

#[tokio::test]
async fn namespace_dependencies_refresh_head_and_root_history() {
    let (_temp_dir, repo) = create_clean_git_repo().await;
    let source = GitWorkspaceMetadataSource {
        cwd: repo.clone(),
        repo_root: repo.clone(),
        cache: GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop()))),
    };
    let namespace_before = source.project_namespace().await.expect("namespace");
    let dependencies_before = StableMetadataDependencies::capture_project_namespace(&source)
        .expect("namespace dependencies");

    run_git(
        repo.as_path(),
        &["commit", "--allow-empty", "-q", "-m", "next"],
    )
    .await;

    let dependencies_after = StableMetadataDependencies::capture_project_namespace(&source)
        .expect("namespace dependencies");
    assert_ne!(dependencies_before, dependencies_after);
    assert_eq!(
        source.project_namespace().await,
        Some(namespace_before.clone())
    );

    run_git(
        repo.as_path(),
        &["checkout", "-q", "--orphan", "unrelated-root"],
    )
    .await;
    run_git(
        repo.as_path(),
        &["commit", "--allow-empty", "-q", "-m", "unrelated root"],
    )
    .await;

    let unrelated_namespace = source.project_namespace().await.expect("namespace");
    assert_ne!(namespace_before, unrelated_namespace);
}

#[tokio::test]
async fn watcher_generation_rejects_stable_identity_caches() {
    let (_temp_dir, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let source = GitWorkspaceMetadataSource {
        cwd: repo.clone(),
        repo_root: repo.clone(),
        cache: Arc::clone(&cache),
    };
    let expected_metadata = source.metadata().await;
    let expected_namespace = source.project_namespace().await.expect("namespace");

    {
        let mut state = cache.state.lock().await;
        state
            .metadata
            .get_mut(repo.as_path())
            .expect("metadata cache entry")
            .metadata = StableGitMetadata::default();
        state
            .project_namespaces
            .get_mut(repo.as_path())
            .expect("namespace cache entry")
            .namespace = "stale-namespace".to_string();
    }
    cache.watcher_generation.fetch_add(1, Ordering::AcqRel);

    assert_eq!(
        source.metadata().await,
        GitWorkspaceMetadata {
            associated_remote_urls: expected_metadata.associated_remote_urls,
            latest_git_commit_hash: expected_metadata.latest_git_commit_hash,
            has_changes: Some(false),
        }
    );
    assert_eq!(source.project_namespace().await, Some(expected_namespace));
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
    assert!(state.project_namespaces.is_empty());
    assert!(!cache.watcher_reliable.load(Ordering::Acquire));
}

#[tokio::test]
async fn repository_manifest_revalidates_overlay_and_rehashes_only_changed_paths() {
    let (_repo_dir, repo) = create_clean_git_repo().await;
    std::fs::write(repo.join("overlay.txt"), "overlay\n").expect("overlay file");
    let codex_home = TempDir::new().expect("codex home");
    let state = StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime");
    let store = LocalAgentTaskStore::initialize(&state)
        .await
        .expect("task store");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    let first = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("first manifest")
        .expect("prepared manifests supported");
    assert_eq!(first.work().overlay_traversals, 3);
    assert_eq!(first.work().git_subprocesses, 10);
    assert_eq!(first.work().manifests_constructed, 1);

    store
        .register_workspace_actor(
            repo.as_path(),
            WorkspaceActorRegistration {
                root_session_id: "unrelated-root".to_string(),
                actor_id: "unrelated-reader".to_string(),
                kind: WorkspaceActorKind::Root,
                assignment_id: None,
                attempt_id: None,
                strategy: Default::default(),
            },
        )
        .await
        .expect("unrelated coordination write");
    let hit = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("cached manifest")
        .expect("prepared manifests supported");
    assert_eq!(hit.work().overlay_traversals, 2);
    assert_eq!(hit.work().files_hashed, 0);
    assert_eq!(hit.work().bytes_hashed, 0);
    assert_eq!(hit.work().git_subprocesses, 8);
    assert_eq!(hit.work().manifests_constructed, 0);
    assert_eq!(hit.receipt().manifest_id(), first.receipt().manifest_id());
    assert_eq!(
        cache
            .manifest_diagnostics
            .cache_hits
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        cache
            .manifest_diagnostics
            .admission_overlay_traversals
            .load(Ordering::Relaxed),
        first.work().overlay_traversals + 2,
        "the hit performs authoritative before/after overlay enumeration"
    );
    assert_eq!(
        cache
            .manifest_diagnostics
            .admission_files_hashed
            .load(Ordering::Relaxed),
        first.work().files_hashed,
        "the hit adds no admission file hashes"
    );
    assert_eq!(
        cache
            .manifest_diagnostics
            .full_manifests_constructed
            .load(Ordering::Relaxed),
        1,
        "the hit adds no full manifest construction"
    );

    std::fs::write(repo.join("overlay.txt"), "changed overlay\n").expect("overlay changes");
    let incrementally_refreshed = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("identity-invalidated manifest")
        .expect("prepared manifests supported");
    assert_eq!(incrementally_refreshed.work().manifests_constructed, 1);
    assert_eq!(incrementally_refreshed.work().full_constructions, 0);
    assert_eq!(incrementally_refreshed.work().incremental_constructions, 1);
    assert_eq!(incrementally_refreshed.work().files_hashed, 1);
    assert_eq!(incrementally_refreshed.work().overlay_traversals, 2);

    cache.watcher_generation.fetch_add(1, Ordering::AcqRel);
    let invalidated = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("invalidated manifest")
        .expect("prepared manifests supported");
    assert_eq!(invalidated.work().manifests_constructed, 1);

    cache.invalidate_for_watcher_failure().await;
    let uncertain = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("uncertain watcher fallback")
        .expect("fresh store reconstruction remains available");
    assert_eq!(uncertain.work().manifests_constructed, 1);
    assert_eq!(uncertain.work().overlay_traversals, 1);
}

#[tokio::test]
async fn repository_manifest_falls_back_when_host_mutations_prevent_cache_admission() {
    let (_repo_dir, repo) = create_clean_git_repo().await;
    let codex_home = TempDir::new().expect("codex home");
    let state = StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime");
    let store = LocalAgentTaskStore::initialize(&state)
        .await
        .expect("task store");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    let mutating_cache = Arc::clone(&cache);
    let mutator = tokio::spawn(async move {
        loop {
            mutating_cache.note_host_workspace_mutation();
            tokio::task::yield_now().await;
        }
    });
    tokio::task::yield_now().await;

    let prepared = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("transient cache-admission races fall back to a fresh manifest")
        .expect("prepared manifests supported");
    mutator.abort();

    assert_eq!(prepared.work().manifests_constructed, 1);
    assert!(cache.state.lock().await.repository_manifests.is_empty());
}

#[tokio::test]
async fn source_freshness_tracks_watcher_and_host_mutations_by_path() {
    let root = TempDir::new().expect("source freshness root");
    let first = root.path().join("first.rs");
    let second = root.path().join("second.rs");
    std::fs::write(&first, "first\n").expect("first fixture");
    std::fs::write(&second, "second\n").expect("second fixture");
    let watcher = Arc::new(FileWatcher::noop());
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::clone(&watcher)));
    let first_registration = cache
        .register_source_freshness_paths([first])
        .expect("first registration");
    let second_registration = cache
        .register_source_freshness_paths([second.clone()])
        .expect("second registration");

    cache.watcher_generation.fetch_add(1, Ordering::AcqRel);
    cache.record_source_watcher_event(FileWatcherEvent {
        paths: vec![second.clone()],
        rescan_required: false,
    });

    assert!(cache.source_registration_is_current(&first_registration));
    assert!(!cache.source_registration_is_current(&second_registration));

    let refreshed_second = cache
        .register_source_freshness_paths([second])
        .expect("refreshed second registration");
    cache.note_host_workspace_mutation_paths(root.path(), &["first.rs".to_string()]);

    assert!(!cache.source_registration_is_current(&first_registration));
    assert!(cache.source_registration_is_current(&refreshed_second));
}

#[tokio::test]
async fn final_repository_manifest_seeds_the_post_commit_receipt_without_an_extra_scan() {
    let (_repo_dir, repo) = create_clean_git_repo().await;
    let codex_home = TempDir::new().expect("codex home");
    let state = StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime");
    let store = LocalAgentTaskStore::initialize(&state)
        .await
        .expect("task store");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    let prepared = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("initial manifest")
        .expect("prepared manifests supported");
    let lease = store
        .begin_workspace_mutation_prepared(
            repo.as_path(),
            WorkspaceMutationRequest {
                root_session_id: "final-seed-root".to_string(),
                actor_id: "root:final-seed-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![codex_agent_task_store::REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
            prepared,
        )
        .await
        .expect("repository-wide lease starts");
    let guard = cache
        .begin_repository_manifest_finalization(repo.as_path())
        .await
        .expect("finalization dependencies register");
    std::fs::write(repo.join("post-commit.txt"), "final state\n").expect("mutated fixture");
    let outcome = store
        .finish_workspace_mutation_with_receipt(repo.as_path(), lease)
        .await
        .expect("mutation finalizes");
    let final_manifest = outcome
        .final_manifest()
        .expect("authoritative final receipt")
        .clone();
    assert_eq!(outcome.work().manifests_constructed, 1);
    cache.record_final_manifest_work(outcome.work());
    cache.note_host_workspace_mutation();
    cache
        .publish_final_repository_manifest(
            &store,
            repo.as_path(),
            final_manifest.clone(),
            guard,
            true,
        )
        .await
        .expect("fresh final receipt publishes");

    let hit = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("post-commit cache lookup")
        .expect("prepared manifests supported");
    assert_eq!(
        hit.receipt().manifest_id(),
        final_manifest.receipt().manifest_id()
    );
    assert_eq!(hit.receipt().epoch(), final_manifest.receipt().epoch());
    assert_eq!(hit.work().overlay_traversals, 2);
    assert_eq!(hit.work().files_hashed, 0);
    assert_eq!(hit.work().git_subprocesses, 8);
    assert_eq!(hit.work().manifests_constructed, 0);
}

#[tokio::test]
async fn watcher_change_during_finalization_prevents_receipt_seeding() {
    let (_repo_dir, repo) = create_clean_git_repo().await;
    let codex_home = TempDir::new().expect("codex home");
    let state = StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime");
    let store = LocalAgentTaskStore::initialize(&state)
        .await
        .expect("task store");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    let prepared = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("initial manifest")
        .expect("prepared manifests supported");
    let lease = store
        .begin_workspace_mutation_prepared(
            repo.as_path(),
            WorkspaceMutationRequest {
                root_session_id: "stale-final-root".to_string(),
                actor_id: "root:stale-final-root".to_string(),
                kind: WorkspaceActorKind::Root,
                attempt_id: None,
                paths: vec![codex_agent_task_store::REPOSITORY_WIDE_PATH.to_string()],
                contracts: Vec::new(),
                expected_manifest: Vec::new(),
            },
            prepared,
        )
        .await
        .expect("repository-wide lease starts");
    let guard = cache
        .begin_repository_manifest_finalization(repo.as_path())
        .await
        .expect("finalization dependencies register");
    cache.watcher_generation.fetch_add(1, Ordering::AcqRel);
    std::fs::write(repo.join("stale-final.txt"), "final state\n").expect("mutated fixture");
    let outcome = store
        .finish_workspace_mutation_with_receipt(repo.as_path(), lease)
        .await
        .expect("mutation finalizes");
    let final_manifest = outcome
        .final_manifest()
        .expect("authoritative final receipt")
        .clone();
    cache.note_host_workspace_mutation();
    cache
        .publish_final_repository_manifest(&store, repo.as_path(), final_manifest, guard, true)
        .await
        .expect("stale receipt is ignored without failing finalization");

    let reconstructed = cache
        .prepare_repository_manifest(&store, repo.as_path())
        .await
        .expect("cache lookup after freshness change")
        .expect("prepared manifests supported");
    assert_eq!(reconstructed.work().manifests_constructed, 1);
    assert_eq!(reconstructed.work().overlay_traversals, 3);
}
