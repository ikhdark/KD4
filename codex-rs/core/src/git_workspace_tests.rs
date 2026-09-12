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

#[tokio::test]
#[cfg(windows)]
async fn workspace_evidence_timeout_terminates_clean_filter_and_descendant() {
    assert_workspace_evidence_cleans_filter_tree(false).await;
}

#[tokio::test]
#[cfg(windows)]
async fn workspace_evidence_cancellation_terminates_clean_filter_and_descendant() {
    assert_workspace_evidence_cleans_filter_tree(true).await;
}

#[cfg(windows)]
async fn assert_workspace_evidence_cleans_filter_tree(cancel_after_spawn: bool) {
    let (_temp, repo) = create_clean_git_repo().await;
    let before = capture_workspace_evidence_identity(repo.as_path())
        .await
        .expect("clean repository identity");
    let head = run_git(repo.as_path(), &["rev-parse", "HEAD"]).await;
    assert_eq!(
        before.head_identity.as_deref(),
        Some(String::from_utf8(head.stdout).unwrap().trim())
    );
    let helper = repo.join("clean-filter.ps1");
    let pids = repo.join("filter-pids.txt");
    let pids_literal = pids.display().to_string().replace('\'', "''");
    std::fs::write(
        &helper,
        format!(
            "$child = Start-Process powershell.exe -ArgumentList '-NoProfile', '-Command', 'Start-Sleep -Seconds 60' -WindowStyle Hidden -PassThru\n[IO.File]::WriteAllText('{pids_literal}', \"$PID $($child.Id)\")\nStart-Sleep -Seconds 60\n"
        ),
    )
    .expect("write clean filter");
    std::fs::write(repo.join(".gitattributes"), "README.md filter=blocked\n")
        .expect("write filter attribute");
    run_git(
        repo.as_path(),
        &[
            "config",
            "filter.blocked.clean",
            &format!(
                "powershell.exe -NoProfile -ExecutionPolicy Bypass -File '{}'",
                helper.display().to_string().replace('\\', "/")
            ),
        ],
    )
    .await;
    run_git(
        repo.as_path(),
        &["config", "filter.blocked.required", "true"],
    )
    .await;
    // Git status can conclude different-sized files changed without cleaning;
    // keep the length of the original "initial\n" to force content inspection.
    std::fs::write(repo.join("README.md"), "changed\n").expect("edit tracked file");

    let result = if cancel_after_spawn {
        let operation = capture_workspace_evidence_identity(repo.as_path());
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("capture completed before cancellation: {result:?}"),
            _ = async {
                while !std::fs::read_to_string(&pids).is_ok_and(|contents| {
                    contents.split_whitespace().filter_map(|pid| pid.parse::<u32>().ok()).count() == 2
                }) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            } => {}
        }
        None
    } else {
        capture_workspace_evidence_identity(repo.as_path()).await
    };
    let ids = std::fs::read_to_string(&pids)
        .expect("Git must launch the configured filter")
        .split_whitespace()
        .map(|pid| pid.parse::<u32>().expect("recorded process id").to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    let ids = ids.join(",");
    let observed = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!("$live = @(Get-Process -Id {ids} -ErrorAction SilentlyContinue); $live | ForEach-Object {{ $_.Id }}; $live | Stop-Process -Force -ErrorAction SilentlyContinue"),
        ])
        .output()
        .await
        .expect("observe and clean up filter processes");
    assert!(
        result.is_none_or(|identity| identity.unavailable),
        "incomplete status must not produce an available workspace identity"
    );
    assert!(observed.status.success());
    assert!(
        observed.stdout.is_empty(),
        "workspace capture left filter processes alive: {}",
        String::from_utf8_lossy(&observed.stdout)
    );
}

#[tokio::test]
async fn ordinary_workspace_identity_accepts_256_paths_and_rejects_257() {
    let (_temp, repo) = create_clean_git_repo().await;
    for index in 0..255 {
        std::fs::write(repo.join(format!("untracked-{index}.txt")), b"").unwrap();
    }
    assert!(
        capture_workspace_evidence_identity(repo.as_path())
            .await
            .is_some()
    );
    std::fs::write(repo.join("path-256.txt"), b"").unwrap();
    assert!(
        capture_workspace_evidence_identity(repo.as_path())
            .await
            .is_some()
    );
    std::fs::write(repo.join("path-257.txt"), b"").unwrap();
    assert!(
        capture_workspace_evidence_identity(repo.as_path())
            .await
            .is_some_and(|identity| identity.unavailable)
    );
}

#[tokio::test]
async fn unavailable_workspace_capture_cannot_reuse_successful_tool_output() {
    use crate::tool_history::ToolHistoryState;
    use crate::tool_history::WorkspaceEvidenceObservation;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;

    let (_temp, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    for index in 0..257 {
        std::fs::write(
            repo.join(format!("untracked-{index}.txt")),
            "first contents",
        )
        .unwrap();
    }
    let before = cache.workspace_evidence_identity(repo.as_path()).await;
    assert!(before.as_ref().is_some_and(|identity| identity.unavailable));
    assert!(
        cache
            .latest_workspace_evidence_identity(repo.as_path())
            .await
            .is_none()
    );
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "unavailable-repo".to_string(),
        output: FunctionCallOutputPayload::from_text("first contents".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };
    let canonical: Arc<[ResponseItem]> = Arc::from([
        ResponseItem::FunctionCall {
            id: None,
            name: "functions.exec".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "unavailable-repo".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        output.clone(),
    ]);
    std::fs::write(repo.join("untracked-0.txt"), "externally changed contents").unwrap();
    let after = cache.workspace_evidence_identity(repo.as_path()).await;
    assert!(after.as_ref().is_some_and(|identity| identity.unavailable));
    // Cover both new explicit failures and old persisted None observations.
    for captured in [before, None] {
        let mut state = ToolHistoryState::default();
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(captured, &output, BTreeSet::new())
                .unwrap(),
        );
        let restored: ToolHistoryState =
            serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        let projected =
            restored.project_with_workspace_identity(Arc::clone(&canonical), after.as_ref());
        let ResponseItem::FunctionCallOutput { output, .. } = &projected.items[1] else {
            panic!("projected tool output");
        };
        let text = output.text_content().expect("text output");
        assert!(text.contains("\"stale_workspace_evidence\":true"));
        assert!(!text.contains("first contents"));
    }
}

#[tokio::test]
async fn workspace_discovery_distinguishes_non_git_from_failure() {
    let temp = TempDir::new().unwrap();
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    assert_eq!(capture_workspace_evidence_identity(temp.path()).await, None);
    assert_eq!(cache.workspace_evidence_identity(temp.path()).await, None);
    let missing = temp.path().join("missing-cwd");
    assert!(
        capture_workspace_evidence_identity(&missing)
            .await
            .is_some_and(|identity| identity.unavailable)
    );
    assert!(
        cache
            .workspace_evidence_identity(&missing)
            .await
            .is_some_and(|identity| identity.unavailable)
    );
    std::fs::write(temp.path().join(".git"), "gitdir: nonexistent-git-dir").unwrap();
    assert!(
        capture_workspace_evidence_identity(temp.path())
            .await
            .is_some_and(|identity| identity.unavailable)
    );
    assert!(
        cache
            .workspace_evidence_identity(temp.path())
            .await
            .is_some_and(|identity| identity.unavailable)
    );
}

#[test]
fn workspace_content_capture_stops_between_chunks_and_preserves_sha256() {
    let control = WorkspaceCaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancellation: CancellationToken::new(),
    };
    let mut buffer = [0; 1024];
    let content = vec![42; 8193];
    assert_eq!(
        hash_workspace_content(&mut content.as_slice(), 8193, &mut buffer, &control),
        Some((format!("{:x}", Sha256::digest(&content)), 8193))
    );
    assert_eq!(
        hash_workspace_content(&mut &b""[..], 0, &mut buffer, &control),
        Some((format!("{:x}", Sha256::digest(b"")), 0))
    );
    assert!(hash_workspace_content(&mut content.as_slice(), 8192, &mut buffer, &control).is_none());
    struct CancelAfterRead<'a> {
        reads: usize,
        token: &'a CancellationToken,
    }
    impl Read for CancelAfterRead<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            buffer.fill(1);
            self.token.cancel();
            Ok(buffer.len())
        }
    }
    let mut reader = CancelAfterRead {
        reads: 0,
        token: &control.cancellation,
    };
    assert!(hash_workspace_content(&mut reader, 8193, &mut buffer, &control).is_none());
    assert_eq!(reader.reads, 1, "cancellation must prevent the next read");
}

#[tokio::test]
async fn workspace_identity_distinguishes_deletion_recreation_and_rename() {
    let (_temp, repo) = create_clean_git_repo().await;
    let initial = capture_workspace_evidence_identity(repo.as_path())
        .await
        .unwrap();
    std::fs::remove_file(repo.join("README.md")).unwrap();
    let deleted = capture_workspace_evidence_identity(repo.as_path())
        .await
        .unwrap();
    assert_ne!(initial, deleted);
    std::fs::write(repo.join("README.md"), "recreated\n").unwrap();
    let recreated = capture_workspace_evidence_identity(repo.as_path())
        .await
        .unwrap();
    assert_ne!(deleted, recreated);
    run_git(repo.as_path(), &["mv", "README.md", "renamed.md"]).await;
    let renamed = capture_workspace_evidence_identity(repo.as_path())
        .await
        .unwrap();
    assert_ne!(recreated, renamed);
    assert!(repo.join("renamed.md").exists());
    let stale_deletion = capture_workspace_metadata(
        repo.to_path_buf(),
        vec![WorkspaceGenerationPath {
            path: "renamed.md".to_string(),
            deleted: true,
        }],
        WorkspaceCaptureControl {
            deadline: Instant::now() + Duration::from_secs(5),
            cancellation: CancellationToken::new(),
        },
    )
    .await;
    assert!(
        stale_deletion.is_none(),
        "a recreated path cannot inherit a deleted identity"
    );
}

#[test]
fn nul_status_reader_handles_split_renames_conflicts_and_limits() {
    let status = b"2 R. N... 100644 100644 100644 a b R100 renamed\0? false-untracked\0u UU N... 100644 100644 100644 100644 a b c conflict\0? real-untracked\0";
    for chunk_size in [1, 7, status.len()] {
        let mut reader = WorkspaceStatusReader::new();
        for chunk in status.chunks(chunk_size) {
            reader.push(chunk).unwrap();
        }
        let (bytes, paths) = reader.finish().unwrap();
        assert_eq!(bytes, status);
        assert_eq!(
            paths
                .iter()
                .map(|path| path.path.as_str())
                .collect::<Vec<_>>(),
            ["conflict", "real-untracked", "renamed"]
        );
    }
    let mut reader = WorkspaceStatusReader::new();
    for index in 0..256 {
        reader.push(format!("? {index}\0").as_bytes()).unwrap();
    }
    reader.push(b"? 0\0").unwrap();
    assert!(reader.push(b"? path-257\0").is_none());
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
        cache
            .latest_workspace_evidence_identity(repo.as_path())
            .await,
        Some(identity)
    );

    std::fs::rename(repo.join(".git"), repo.join(".git-disabled"))
        .expect("disable repository metadata");
    assert_eq!(
        cache.workspace_evidence_identity(repo.as_path()).await,
        None
    );
    assert_eq!(
        cache
            .latest_workspace_evidence_identity(repo.as_path())
            .await,
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
    let original = b"first".repeat(8193);
    std::fs::write(&path, &original).expect("write first file");
    let file = File::open(&path).expect("open first file");
    std::fs::rename(&path, &moved).expect("move opened file");
    std::fs::write(&path, b"replacement").expect("write replacement file");

    let state = file_dependency_state(file, true).expect("file state");
    let expected_digest: [u8; 32] = Sha256::digest(&original).into();

    assert!(matches!(
        state,
        DependencyState::File {
            len,
            digest: Some(digest),
            ..
        } if digest == expected_digest && len == original.len() as u64
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

    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let expected_root = dunce::canonicalize(repo.as_path()).expect("canonical fixture root");
    let nested_identity = cache
        .workspace_evidence_identity(&nested)
        .await
        .expect("nested working directory belongs to the repository");
    assert!(!nested_identity.unavailable);
    assert_eq!(
        nested_identity.repository_root.as_deref(),
        Some(expected_root.to_string_lossy().as_ref()),
    );
    assert_eq!(
        cache.workspace_evidence_identity(repo.as_path()).await,
        Some(nested_identity.clone()),
    );
    std::fs::write(repo.join("README.md"), "nested-capture external edit\n")
        .expect("edit tracked content");
    let changed = cache.workspace_evidence_identity(&nested).await.expect("changed identity");
    assert!(!changed.unavailable);
    assert_eq!(changed.repository_root, nested_identity.repository_root);
    assert_ne!(changed.worktree_identity, nested_identity.worktree_identity);
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

    cache.note_host_workspace_mutation_paths(repo.as_path(), &["README.md".to_string()]).await;
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
async fn cancelling_a_waiter_preserves_live_capture_and_last_waiter_releases_it() {
    let (_temp, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let pause = cache.pause_next_workspace_evidence_capture();
    let first_cache = Arc::clone(&cache);
    let first_repo = repo.clone();
    let first = tokio::spawn(async move {
        first_cache
            .workspace_evidence_identity(first_repo.as_path())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), pause.wait_until_started())
        .await
        .unwrap();
    let joined = cache.workspace_evidence_waiter_joined.notified();
    let second_cache = Arc::clone(&cache);
    let second_repo = repo.clone();
    let second = tokio::spawn(async move {
        second_cache
            .workspace_evidence_identity(second_repo.as_path())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), joined)
        .await
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    {
        let captures = cache.in_flight_workspace_evidence.lock().unwrap();
        assert_eq!(captures.len(), 1);
        assert!(
            !captures
                .values()
                .next()
                .unwrap()
                .interest
                .cancellation
                .is_cancelled()
        );
    }
    pause.release();
    assert!(second.await.unwrap().is_some());
    assert_eq!(cache.workspace_evidence_capture_count(), 1);

    let pause = cache.pause_next_workspace_evidence_capture();
    let abandoned_cache = Arc::clone(&cache);
    let abandoned_repo = repo.clone();
    let abandoned = tokio::spawn(async move {
        abandoned_cache
            .workspace_evidence_identity(abandoned_repo.as_path())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), pause.wait_until_started())
        .await
        .unwrap();
    let token = cache
        .in_flight_workspace_evidence
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .interest
        .cancellation
        .clone();
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());
    assert!(token.is_cancelled());
    assert!(
        cache
            .in_flight_workspace_evidence
            .lock()
            .unwrap()
            .is_empty()
    );
    assert!(
        cache
            .workspace_evidence_identity(repo.as_path())
            .await
            .is_some()
    );
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
        .await
        .expect("path observation");
    cache.note_host_workspace_mutation_paths(root.path(), &["README.md".to_string()]).await;
    assert!(cache.source_path_change_observation_is_current(&observation));

    cache.note_host_workspace_mutation_paths(root.path(), &["src/lib.rs".to_string()]).await;
    assert!(!cache.source_path_change_observation_is_current(&observation));

    let uncertain = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .await
        .expect("refreshed path observation");
    cache.note_host_workspace_mutation();
    assert!(!cache.source_path_change_observation_is_current(&uncertain));

    let overflowed = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .await
        .expect("overflow path observation");
    for index in 0..=SOURCE_CHANGE_JOURNAL_CAPACITY {
        cache.note_host_workspace_mutation_paths(root.path(), &[format!("unrelated/{index}.txt")]).await;
    }
    assert!(!cache.source_path_change_observation_is_current(&overflowed));
}

#[tokio::test]
async fn source_path_freshness_uses_the_generation_index() {
    let root = TempDir::new().expect("source observation root");
    let source = root.path().join("src").join("lib.rs");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("create src");
    std::fs::write(&source, "fn owner() {}\n").expect("write source");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let observation = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .await
        .expect("path observation");
    let unrelated_paths = (0..1_024)
        .map(|index| root.path().join("unrelated").join(format!("{index}.txt")))
        .collect();

    cache.record_source_change_event(Some(unrelated_paths));

    assert!(cache.source_path_change_observation_is_current(&observation));
    assert!(cache.take_source_change_freshness_lookup_count_for_test() < 32);
}

#[tokio::test]
async fn repository_retention_eviction_invalidates_source_observation_and_cached_evidence() {
    let root = TempDir::new().expect("repository retention root");
    let first_repo = root.path().join("repo-0");
    std::fs::create_dir(&first_repo).expect("create first repository");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let observation = cache
        .begin_source_path_change_observation(&first_repo, &first_repo, true)
        .await
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
            .await
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

#[tokio::test]
async fn recursive_source_path_observation_detects_descendant_changes() {
    let root = TempDir::new().expect("source observation root");
    let source_root = root.path().join("src");
    std::fs::create_dir_all(&source_root).expect("create src");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let observation = cache
        .begin_source_path_change_observation(root.path(), &source_root, true)
        .await
        .expect("recursive path observation");

    cache.note_host_workspace_mutation_paths(root.path(), &["src/nested/lib.rs".to_string()]).await;

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

#[tokio::test(flavor = "current_thread")]
async fn host_path_mutation_worker_publishes_invalidation_after_caller_cancellation() {
    let root = TempDir::new().expect("mutation root");
    let source = root.path().join("source.txt");
    let unrelated = root.path().join("unrelated.txt");
    std::fs::write(&source, "before").expect("initial source");
    std::fs::write(&unrelated, "unchanged").expect("unrelated source");
    // The external watcher is quiet so only the normal host mutation API can
    // invalidate these real filesystem paths.
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(FileWatcher::noop())));
    let source_observation = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .await
        .expect("source observation");
    let unrelated_observation = cache
        .begin_source_path_change_observation(root.path(), &unrelated, false)
        .await
        .expect("unrelated observation");
    std::fs::write(&source, "after").expect("committed source mutation");

    let runtime_thread = std::thread::current().id();
    let started = Arc::new(tokio::sync::Notify::new());
    let worker_started = Arc::clone(&started);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    *cache.host_mutation_worker_hook.lock().unwrap() = Some(Box::new(move || {
        assert_ne!(std::thread::current().id(), runtime_thread);
        worker_started.notify_one();
        released
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("release filesystem worker");
    }));
    let call_cache = Arc::clone(&cache);
    let call_root = root.path().to_path_buf();
    let call = tokio::spawn(async move {
        call_cache
            .note_host_workspace_mutation_paths(&call_root, &["source.txt".to_string()])
            .await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
        .await
        .expect("the current-thread runtime progresses while the filesystem worker is paused");
    call.abort();
    assert!(call.await.expect_err("caller was cancelled").is_cancelled());
    release.send(()).expect("worker still owns mutation inputs");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while cache.source_path_change_observation_is_current(&source_observation) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker publishes invalidation even after caller cancellation");
    assert!(cache.source_path_change_observation_is_current(&unrelated_observation));
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "after");
}

#[tokio::test(flavor = "current_thread")]
async fn git_watch_worker_cancellation_and_cache_drop_retire_native_owners() {
    let root = TempDir::new().expect("watch root");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(
        FileWatcher::new().expect("native watcher"),
    )));
    let probe = Arc::clone(&cache.watcher_worker.as_ref().expect("watch worker").probe);
    let (started, release) = GitWatchWorkerProbe::arm(&probe.before_reply);
    let pending_cache = Arc::clone(&cache);
    let path = root.path().to_path_buf();
    let pending = tokio::spawn(async move {
        pending_cache
            .begin_source_path_change_observation(&path, &path, true)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), started)
        .await
        .expect("registration must reach worker reply")
        .expect("worker alive");
    assert_eq!(probe.registered.load(Ordering::Acquire), 1);
    assert_eq!(probe.retired.load(Ordering::Acquire), 0);
    assert_ne!(
        *probe.thread.lock().unwrap(),
        Some(std::thread::current().id())
    );
    pending.abort();
    assert!(pending.await.expect_err("aborted request").is_cancelled());
    assert!(
        cache
            .repository_retention
            .try_lock()
            .expect("registration must not hold retention lock")
            .source_watch_registrations
            .is_empty()
    );
    release.send(()).expect("release worker reply");
    probe.wait_for(1, false).await;
    assert!(
        cache
            .repository_retention
            .lock()
            .unwrap()
            .source_watch_registrations
            .is_empty()
    );

    let observation = cache
        .begin_source_path_change_observation(root.path(), root.path(), true)
        .await
        .expect("normal retry must register");
    assert!(cache.source_path_change_observation_is_current(&observation));
    tokio::fs::write(
        root.path().join("changed-after-retry.txt"),
        "new source content",
    )
    .await
    .expect("write watched dependency");
    tokio::time::timeout(Duration::from_secs(10), async {
        while cache.source_path_change_observation_is_current(&observation) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native watcher must invalidate the retained dependency after a real write");
    assert_eq!(probe.registered.load(Ordering::Acquire), 2);
    drop(cache);
    probe.wait_for(2, true).await;
    assert_eq!(probe.retired.load(Ordering::Acquire), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn git_watch_worker_eviction_invalidates_before_native_retirement() {
    let (_repo_temp, repo) = create_clean_git_repo().await;
    let other_roots = TempDir::new().expect("other roots");
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(
        FileWatcher::new().expect("native watcher"),
    )));
    let probe = Arc::clone(&cache.watcher_worker.as_ref().expect("watch worker").probe);
    let original = cache
        .begin_source_path_change_observation(repo.as_path(), repo.as_path(), true)
        .await
        .expect("original watch");
    let identity = cache
        .workspace_evidence_identity(repo.as_path())
        .await
        .expect("normal identity");
    assert!(!identity.unavailable);
    assert_eq!(
        cache
            .latest_workspace_evidence_identity(repo.as_path())
            .await,
        Some(identity)
    );
    let (started, release) = GitWatchWorkerProbe::arm(&probe.before_retire);
    let mut newest = None;
    for index in 0..RETAINED_REPOSITORY_CAPACITY {
        let path = other_roots.path().join(format!("repo-{index}"));
        tokio::fs::create_dir(&path).await.expect("other root");
        newest = cache
            .begin_source_path_change_observation(&path, &path, true)
            .await;
        assert!(newest.is_some());
    }
    tokio::time::timeout(Duration::from_secs(10), started)
        .await
        .expect("eviction must reach worker retirement")
        .expect("worker alive");
    assert_eq!(probe.retired.load(Ordering::Acquire), 0);
    assert_ne!(
        *probe.thread.lock().unwrap(),
        Some(std::thread::current().id())
    );
    assert!(!cache.source_path_change_observation_is_current(&original));
    assert!(cache.source_path_change_observation_is_current(newest.as_ref().unwrap()));
    assert_eq!(
        cache
            .latest_workspace_evidence_identity(repo.as_path())
            .await,
        None
    );
    assert_eq!(
        cache
            .repository_retention
            .try_lock()
            .expect("native retirement must not hold cache lock")
            .source_watch_registrations
            .len(),
        RETAINED_REPOSITORY_CAPACITY
    );
    release.send(()).expect("release native retirement");
    probe.wait_for(1, false).await;
    let renewed = cache
        .begin_source_path_change_observation(repo.as_path(), repo.as_path(), true)
        .await
        .expect("re-register evicted root");
    assert_ne!(
        original.registration_generation,
        renewed.registration_generation
    );
    assert!(!cache.source_path_change_observation_is_current(&original));
    assert!(cache.source_path_change_observation_is_current(&renewed));
    let registered = probe.registered.load(Ordering::Acquire);
    assert_eq!(registered, RETAINED_REPOSITORY_CAPACITY + 2);
    drop(cache);
    probe.wait_for(registered, true).await;
    assert_eq!(probe.retired.load(Ordering::Acquire), registered);
}

#[tokio::test(flavor = "current_thread")]
async fn git_watch_worker_root_replacement_retires_only_previous_registration() {
    let (_repo_temp, repo) = create_clean_git_repo().await;
    let cache = GitWorkspaceCache::with_watcher(Some(Arc::new(
        FileWatcher::new().expect("native watcher"),
    )));
    let probe = Arc::clone(&cache.watcher_worker.as_ref().expect("watch worker").probe);
    let first = cache.snapshot(&local_snapshot(repo.clone(), 1).await).await;
    assert_eq!(first.entries.len(), 1);
    assert_eq!(probe.registered.load(Ordering::Acquire), 1);
    let (started, release) = GitWatchWorkerProbe::arm(&probe.before_retire);
    let second = cache.snapshot(&local_snapshot(repo.clone(), 2).await).await;
    tokio::time::timeout(Duration::from_secs(10), started)
        .await
        .expect("replacement must reach worker retirement")
        .expect("worker alive");
    assert_eq!(second.environment_generation, 2);
    assert_eq!(second.entries[0].repo_root, first.entries[0].repo_root);
    assert_eq!(probe.registered.load(Ordering::Acquire), 2);
    assert_eq!(probe.retired.load(Ordering::Acquire), 0);
    assert_eq!(
        cache
            .state
            .try_lock()
            .expect("retirement must not hold cache state")
            .root
            .as_ref()
            .expect("current root entry")
            .key
            .environment_generation,
        2
    );
    release
        .send(())
        .expect("release previous native registration");
    probe.wait_for(1, false).await;
    assert_eq!(probe.retired.load(Ordering::Acquire), 1);
    drop(first);
    drop(second);
    drop(cache);
    probe.wait_for(2, true).await;
}
