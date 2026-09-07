use super::*;

use codex_config::ProjectDiscoveryContext;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecServerRuntimePaths;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::LOCAL_FS;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_path_uri::PathUri;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::MetricData;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use tempfile::TempDir;

use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::turn_context::TurnEnvironment;
use crate::shell_snapshot::ShellSnapshot;

fn test_runtime_paths() -> ExecServerRuntimePaths {
    ExecServerRuntimePaths::new(std::env::current_exe().expect("current exe"))
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

#[tokio::test]
async fn root_discovery_starts_independent_resolutions_concurrently_and_preserves_order() {
    async fn wait_and_return(
        barrier: Arc<tokio::sync::Barrier>,
        value: &'static str,
    ) -> &'static str {
        barrier.wait().await;
        value
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let first_barrier = Arc::clone(&barrier);
    let second_barrier = Arc::clone(&barrier);

    let roots = tokio::time::timeout(
        Duration::from_millis(200),
        resolve_roots_in_order([
            wait_and_return(first_barrier, "first"),
            wait_and_return(second_barrier, "second"),
        ]),
    )
    .await
    .expect("independent root probes should start concurrently");

    assert_eq!(roots, vec!["first", "second"]);
}

#[tokio::test]
async fn snapshot_preserves_native_environment_pairing_when_foreign_cwds_are_skipped() {
    let manager = local_environment_manager().await;
    let environment = manager
        .get_environment(LOCAL_ENVIRONMENT_ID)
        .expect("local environment");

    let foreign_cwd = PathUri::parse("file:///usr/local/workspace").expect("foreign cwd");
    let temp_dir = TempDir::new().expect("native cwd");
    let native_cwd =
        AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute native cwd");
    let environments = TurnEnvironmentSnapshot {
        generation: 1,
        turn_environments: vec![
            TurnEnvironment::new(
                "foreign".to_string(),
                Arc::clone(&environment),
                foreign_cwd,
                None,
            ),
            TurnEnvironment::new(
                LOCAL_ENVIRONMENT_ID.to_string(),
                environment,
                PathUri::from_abs_path(&native_cwd),
                None,
            ),
        ],
        starting: Vec::new(),
    };

    let snapshot = GitWorkspaceCache::with_noop_watcher_for_tests()
        .snapshot(&environments)
        .await;

    assert_eq!(
        snapshot.display_roots(),
        vec![(LOCAL_ENVIRONMENT_ID.to_string(), native_cwd.to_path_buf())]
    );
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

#[test]
fn ordinary_workspace_identity_does_not_request_patch_or_content_materialization() {
    let args = workspace_generation_status_args();

    assert!(args.contains(&"status"));
    assert!(args.contains(&"--porcelain=v2"));
    for expensive_arg in ["diff", "--patch", "--binary", "ls-files", "hash-object"] {
        assert!(!args.contains(&expensive_arg));
    }
}

#[tokio::test]
async fn workspace_generation_capture_has_one_absolute_deadline() {
    let result = within_workspace_generation_deadline(
        Duration::from_millis(10),
        std::future::pending::<()>(),
    )
    .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn workspace_generation_metadata_fails_closed_at_resource_bounds() {
    let temp = TempDir::new().expect("metadata fixture");
    let paths = (0..=WORKSPACE_GENERATION_MAX_PATHS)
        .map(|index| format!("path-{index:04}.txt"))
        .collect::<Vec<_>>();
    for path in &paths {
        std::fs::write(temp.path().join(path), []).expect("write metadata fixture");
    }
    assert!(
        workspace_generation_metadata(temp.path().to_path_buf(), paths)
            .await
            .is_none()
    );

    let large_path = temp.path().join("large.bin");
    let large = std::fs::File::create(&large_path).expect("create sparse large fixture");
    large
        .set_len(WORKSPACE_GENERATION_MAX_DECLARED_BYTES.saturating_add(1))
        .expect("size sparse large fixture");
    assert!(
        workspace_generation_metadata(temp.path().to_path_buf(), vec!["large.bin".to_string()])
            .await
            .is_none()
    );
}

#[tokio::test]
async fn failed_workspace_refresh_invalidates_the_latest_identity() {
    let (_temp, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();

    let identity = cache
        .workspace_evidence_identity(repo.as_path())
        .await
        .expect("initial identity");
    assert_eq!(
        cache.latest_workspace_evidence_identity(repo.as_path()),
        Some(identity)
    );

    std::fs::rename(repo.join(".git"), repo.join(".git-disabled"))
        .expect("disable repository metadata");
    assert_eq!(
        cache.workspace_evidence_identity(repo.as_path()).await,
        None
    );
    assert_eq!(
        cache.latest_workspace_evidence_identity(repo.as_path()),
        None
    );
}

#[tokio::test]
async fn ordinary_workspace_identity_tracks_dirty_content_when_status_is_unchanged() {
    let (_temp, repo) = create_clean_git_repo().await;
    let readme = repo.join("README.md");
    std::fs::write(&readme, "first dirty value\n").expect("first dirty write");
    let original_modified = std::fs::metadata(&readme)
        .expect("first dirty metadata")
        .modified()
        .expect("first dirty modified time");
    let first_status =
        workspace_generation_git_output(repo.as_path(), workspace_generation_status_args())
            .await
            .expect("first status");
    let first = capture_workspace_evidence_identity(repo.as_path())
        .await
        .expect("first identity");

    std::fs::write(&readme, "other dirty value\n").expect("second dirty write");
    std::fs::File::options()
        .write(true)
        .open(&readme)
        .expect("open second dirty value")
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .expect("restore dirty modified time");
    let second_status =
        workspace_generation_git_output(repo.as_path(), workspace_generation_status_args())
            .await
            .expect("second status");
    let second = capture_workspace_evidence_identity(repo.as_path())
        .await
        .expect("second identity");

    assert_eq!(first_status, second_status);
    assert_eq!(first.index_identity, second.index_identity);
    assert_ne!(first.worktree_identity, second.worktree_identity);
}

#[tokio::test]
async fn workspace_evidence_identity_tracks_unborn_repository_changes() {
    let temp_dir = TempDir::new().expect("temp git repository");
    let repo = AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute repo");
    run_git(repo.as_path(), &["init", "-q"]).await;

    let before = capture_workspace_evidence_identity(repo.as_path())
        .await
        .expect("unborn repository identity");
    assert_eq!(before.head_identity, None);
    std::fs::write(repo.join("untracked.txt"), "new evidence\n").expect("write untracked file");
    let after = capture_workspace_evidence_identity(repo.as_path())
        .await
        .expect("changed unborn repository identity");

    assert_ne!(before, after);
    assert_eq!(before.repository_root, after.repository_root);
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
async fn git_workspace_snapshot_reuses_matching_local_discovery() {
    let (_temp_dir, repo) = create_clean_git_repo().await;
    let cwd = repo.join("src").join("nested");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    let environments = local_snapshot(cwd.clone(), 8).await;
    let discovery = ProjectDiscoveryContext::new(
        cwd,
        repo.clone(),
        vec![".git".to_string()],
        Some(repo.clone()),
        Some(repo),
        LOCAL_FS.as_ref(),
    );
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();

    let snapshot = cache
        .snapshot_with_project_discovery(&environments, Some(&discovery))
        .await;

    assert_eq!(snapshot.primary_is_git(), Some(true));
    assert_eq!(cache.root_resolution_count(), 0);
}

#[tokio::test]
async fn activation_metric_distinguishes_git_project_discovery_hit_and_miss() {
    let (_temp_dir, repo) = create_clean_git_repo().await;
    let cwd = repo.join("src").join("nested");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    let environments = local_snapshot(cwd.clone(), 9).await;
    let discovery = ProjectDiscoveryContext::new(
        cwd,
        repo.clone(),
        vec![".git".to_string()],
        Some(repo.clone()),
        Some(repo),
        LOCAL_FS.as_ref(),
    );
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory(
            "test",
            "codex-core",
            env!("CARGO_PKG_VERSION"),
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )
    .expect("in-memory metrics client");

    let hit_cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let hit = hit_cache
        .snapshot_with_project_discovery_and_metrics(
            &environments,
            Some(&discovery),
            Some(&metrics),
        )
        .await;
    assert_eq!(hit.primary_is_git(), Some(true));
    assert_eq!(hit_cache.root_resolution_count(), 0);

    let miss_cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let miss = miss_cache
        .snapshot_with_project_discovery_and_metrics(&environments, None, Some(&metrics))
        .await;
    assert_eq!(miss.primary_is_git(), Some(true));
    assert_eq!(miss_cache.root_resolution_count(), 1);

    let snapshot = metrics.snapshot().expect("metrics snapshot");
    let metric = snapshot
        .scope_metrics()
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .find(|metric| metric.name() == PROJECT_DISCOVERY_REUSE_METRIC)
        .expect("project discovery reuse metric");
    let points = match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => sum
                .data_points()
                .map(|point| {
                    let tags = point
                        .attributes()
                        .map(|attribute| {
                            (
                                attribute.key.as_str().to_string(),
                                attribute.value.as_str().to_string(),
                            )
                        })
                        .collect::<std::collections::BTreeMap<_, _>>();
                    (
                        tags.get("consumer").cloned().unwrap_or_default(),
                        tags.get("result").cloned().unwrap_or_default(),
                        tags.get("reason").cloned().unwrap_or_default(),
                        point.value(),
                    )
                })
                .collect::<BTreeSet<_>>(),
            _ => panic!("unexpected project discovery metric aggregation"),
        },
        _ => panic!("unexpected project discovery metric type"),
    };
    assert_eq!(
        points,
        BTreeSet::from([
            (
                "git".to_string(),
                "hit".to_string(),
                "matched".to_string(),
                1,
            ),
            (
                "git".to_string(),
                "miss".to_string(),
                "context_unavailable".to_string(),
                1,
            ),
        ])
    );
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
        .await
        .expect("namespace dependencies");

    run_git(
        repo.as_path(),
        &["commit", "--allow-empty", "-q", "-m", "next"],
    )
    .await;

    let dependencies_after = StableMetadataDependencies::capture_project_namespace(&source)
        .await
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

#[tokio::test(flavor = "current_thread")]
async fn confirmed_performance_git_dependency_fingerprints_use_blocking_pool() {
    let runtime_thread = std::thread::current().id();
    let worker_thread = run_blocking_git_metadata(|| Some(std::thread::current().id()))
        .await
        .expect("blocking metadata result");

    assert_ne!(worker_thread, runtime_thread);
}

#[tokio::test]
async fn missing_project_namespace_is_cached_with_its_dependencies() {
    let temp_dir = TempDir::new().expect("temp git repository");
    let repo =
        AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute repository path");
    run_git(repo.as_path(), &["init", "-q"]).await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let source = GitWorkspaceMetadataSource {
        cwd: repo.clone(),
        repo_root: repo.clone(),
        cache: Arc::clone(&cache),
    };

    assert_eq!(source.project_namespace().await, None);
    {
        let mut state = cache.state.lock().await;
        let entry = state
            .project_namespaces
            .get_mut(repo.as_path())
            .expect("negative namespace cache entry");
        assert_eq!(entry.namespace, None);
        entry.namespace = Some("cached-negative-entry".to_string());
    }
    assert_eq!(
        source.project_namespace().await.as_deref(),
        Some("cached-negative-entry")
    );
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
            .namespace = Some("stale-namespace".to_string());
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

#[tokio::test]
async fn source_watcher_generation_preserves_git_identity_caches() {
    let (_temp_dir, repo) = create_clean_git_repo().await;
    let environments = local_snapshot(repo.clone(), 12).await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let source = GitWorkspaceMetadataSource {
        cwd: repo.clone(),
        repo_root: repo.clone(),
        cache: Arc::clone(&cache),
    };

    cache.snapshot(&environments).await;
    source.metadata().await;
    source.project_namespace().await.expect("namespace");
    assert_eq!(cache.root_resolution_count(), 1);
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
            .namespace = Some("source-event-cache-sentinel".to_string());
    }

    cache.record_source_change_event(Some(vec![repo.as_path().join("src").join("lib.rs")]));

    cache.snapshot(&environments).await;
    assert_eq!(cache.root_resolution_count(), 1);
    assert_eq!(
        source.metadata().await,
        GitWorkspaceMetadata {
            associated_remote_urls: None,
            latest_git_commit_hash: None,
            has_changes: Some(false),
        }
    );
    assert_eq!(
        source.project_namespace().await.as_deref(),
        Some("source-event-cache-sentinel")
    );
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

#[test]
fn duplicate_repository_reads_file_state_hashes_the_already_opened_file() {
    let temp_dir = TempDir::new().expect("temp dir");
    let path = temp_dir.path().join("dependency");
    let moved = temp_dir.path().join("opened-dependency");
    std::fs::write(&path, b"first").expect("write first file");
    let file = File::open(&path).expect("open first file");
    std::fs::rename(&path, &moved).expect("move opened file");
    std::fs::write(&path, b"replacement").expect("write replacement file");

    let state = file_dependency_state(file, true).expect("file state");
    let expected_digest: [u8; 32] = Sha256::digest(b"first").into();

    assert!(matches!(
        state,
        DependencyState::File {
            len: 5,
            digest: Some(digest),
            ..
        } if digest == expected_digest
    ));
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
async fn workspace_evidence_identity_recaptures_without_waiting_for_watcher_delivery() {
    let (_temp, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    let first = cache
        .workspace_evidence_identity(repo.as_path())
        .await
        .expect("first identity");
    assert_eq!(
        cache.workspace_evidence_capture_count(),
        1,
        "the first identity requires one Git capture"
    );
    let second = cache
        .workspace_evidence_identity(repo.as_path())
        .await
        .expect("second identity");
    assert_eq!(second, first);
    assert_eq!(cache.workspace_evidence_capture_count(), 2);

    std::fs::write(repo.join("README.md"), "external edit\n").expect("write external edit");
    let after_external_edit = cache
        .workspace_evidence_identity(repo.as_path())
        .await
        .expect("identity after external edit");
    assert_ne!(after_external_edit, first);
    assert_eq!(cache.workspace_evidence_capture_count(), 3);
}

#[tokio::test]
async fn workspace_evidence_root_resolution_accepts_nested_working_directories() {
    let (_temp, repo) = create_clean_git_repo().await;
    let nested = repo.join("nested").join("deeper");
    std::fs::create_dir_all(&nested).expect("create nested cwd");

    let resolved = resolve_workspace_evidence_root(&nested)
        .await
        .expect("resolve repository root");

    assert_eq!(resolved, canonical_workspace_evidence_root(repo.as_path()));
}

#[tokio::test]
async fn concurrent_workspace_evidence_capture_coalesces_without_crossing_mutation_epoch() {
    let (_temp, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let pause = cache.pause_next_workspace_evidence_capture();

    let first_cache = Arc::clone(&cache);
    let first_repo = repo.clone();
    let first = tokio::spawn(async move {
        first_cache
            .workspace_evidence_identity(first_repo.as_path())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.started.notified())
        .await
        .expect("first workspace evidence capture should reach its test boundary");

    let joined = cache.workspace_evidence_waiter_joined.notified();
    let second_cache = Arc::clone(&cache);
    let second_repo = repo.clone();
    let second = tokio::spawn(async move {
        second_cache
            .workspace_evidence_identity(second_repo.as_path())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), joined)
        .await
        .expect("same-epoch workspace evidence capture should join the in-flight capture");
    assert_eq!(cache.workspace_evidence_capture_count(), 1);

    cache.note_host_workspace_mutation_paths(repo.as_path(), &["README.md".to_string()]);
    let third_cache = Arc::clone(&cache);
    let third_repo = repo.clone();
    let third = tokio::spawn(async move {
        third_cache
            .workspace_evidence_identity(third_repo.as_path())
            .await
    });
    let third_identity = third.await.expect("new-epoch capture task joins");
    assert_eq!(cache.workspace_evidence_capture_count(), 2);

    pause.release.notify_one();
    let first_identity = first.await.expect("first capture task joins");
    let second_identity = second.await.expect("coalesced capture task joins");
    assert_eq!(second_identity, first_identity);
    assert_eq!(third_identity, first_identity);
}

#[tokio::test]
async fn workspace_evidence_identity_excludes_codex_eval_artifacts() {
    let (_temp, repo) = create_clean_git_repo().await;
    let eval_dir = repo.join(".codex").join("evals");
    std::fs::create_dir_all(&eval_dir).expect("create eval directory");
    let eval_artifact = eval_dir.join("generated.jsonl");
    std::fs::write(&eval_artifact, "first\n").expect("write first eval artifact");

    let first = capture_workspace_evidence_identity(repo.as_path())
        .await
        .expect("first identity");
    std::fs::write(&eval_artifact, "second\n").expect("write second eval artifact");
    let second = capture_workspace_evidence_identity(repo.as_path())
        .await
        .expect("second identity");

    assert_eq!(second, first);
}

#[tokio::test]
async fn source_path_observation_ignores_unrelated_changes_and_fails_open() {
    let root = TempDir::new().expect("source observation root");
    let source = root.path().join("src").join("lib.rs");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("create src");
    std::fs::write(&source, "fn owner() {}\n").expect("write source");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));

    let observation = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .expect("path observation");
    cache.note_host_workspace_mutation_paths(root.path(), &["README.md".to_string()]);
    assert!(cache.source_path_change_observation_is_current(&observation));

    cache.note_host_workspace_mutation_paths(root.path(), &["src/lib.rs".to_string()]);
    assert!(!cache.source_path_change_observation_is_current(&observation));

    let uncertain = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .expect("refreshed path observation");
    cache.note_host_workspace_mutation();
    assert!(!cache.source_path_change_observation_is_current(&uncertain));

    let overflowed = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .expect("overflow path observation");
    for index in 0..=SOURCE_CHANGE_JOURNAL_CAPACITY {
        cache.note_host_workspace_mutation_paths(root.path(), &[format!("unrelated/{index}.txt")]);
    }
    assert!(!cache.source_path_change_observation_is_current(&overflowed));
}

#[test]
fn source_path_freshness_uses_the_generation_index() {
    let root = TempDir::new().expect("source observation root");
    let source = root.path().join("src").join("lib.rs");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("create src");
    std::fs::write(&source, "fn owner() {}\n").expect("write source");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let observation = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .expect("path observation");
    let unrelated_paths = (0..1_024)
        .map(|index| root.path().join("unrelated").join(format!("{index}.txt")))
        .collect();

    cache.record_source_change_event(Some(unrelated_paths));

    assert!(cache.source_path_change_observation_is_current(&observation));
    assert!(cache.take_source_change_freshness_lookup_count_for_test() < 32);
}

#[test]
fn repository_retention_eviction_invalidates_source_observation_and_cached_evidence() {
    let root = TempDir::new().expect("repository retention root");
    let first_repo = root.path().join("repo-0");
    std::fs::create_dir(&first_repo).expect("create first repository");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let observation = cache
        .begin_source_path_change_observation(&first_repo, &first_repo, true)
        .expect("first repository observation");
    {
        let mut retention = cache
            .repository_retention
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retention.latest_workspace_evidence.insert(
            dunce::canonicalize(&first_repo).expect("canonical first repository"),
            CachedWorkspaceEvidenceIdentity {
                capture_sequence: 1,
                identity: None,
            },
        );
    }
    assert!(cache.source_path_change_observation_is_current(&observation));

    for index in 1..=RETAINED_REPOSITORY_CAPACITY {
        let repo = root.path().join(format!("repo-{index}"));
        std::fs::create_dir(&repo).expect("create retained repository");
        cache
            .begin_source_path_change_observation(&repo, &repo, true)
            .expect("retained repository observation");
    }

    assert!(!cache.source_path_change_observation_is_current(&observation));
    let retention = cache
        .repository_retention
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let first_repo = dunce::canonicalize(first_repo).expect("canonical first repository");
    assert!(retention.source_watch_registrations.len() <= RETAINED_REPOSITORY_CAPACITY);
    assert!(
        !retention
            .latest_workspace_evidence
            .contains_key(&first_repo)
    );
}

#[test]
fn recursive_source_path_observation_detects_descendant_changes() {
    let root = TempDir::new().expect("source observation root");
    let source_root = root.path().join("src");
    std::fs::create_dir_all(&source_root).expect("create src");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let observation = cache
        .begin_source_path_change_observation(root.path(), &source_root, true)
        .expect("recursive path observation");

    cache.note_host_workspace_mutation_paths(root.path(), &["src/nested/lib.rs".to_string()]);

    assert!(!cache.source_path_change_observation_is_current(&observation));
}

#[test]
fn path_relationships_preserve_case_on_case_sensitive_filesystems() {
    assert!(!path_is_same_or_descendant_with_case_sensitivity(
        Path::new("repo/src/Owner.rs"),
        Path::new("repo/src/owner.rs"),
        true,
    ));
    assert!(path_is_same_or_descendant_with_case_sensitivity(
        Path::new("repo/src/Owner.rs"),
        Path::new("repo/src/owner.rs"),
        false,
    ));
}
