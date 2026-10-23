use super::*;

#[tokio::test]
async fn exact_empty_settlement_resolves_only_its_own_admission() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source");
    std::fs::write(&path, "unchanged").unwrap();
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let observation = cache.begin_source_path_change_observation(root.path(), &path, false).await.unwrap();
    let first = cache.begin_host_workspace_mutation();
    let second = cache.begin_host_workspace_mutation();
    assert_eq!(cache.source_path_freshness(&observation), SourceFreshness::Unknown);
    first.settle();
    assert_eq!(cache.source_path_freshness(&observation), SourceFreshness::Unknown);
    second.settle();
    assert_eq!(cache.source_path_freshness(&observation), SourceFreshness::NoObservedChange);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "unchanged");
}

#[tokio::test]
async fn settling_empty_patch_keeps_independent_watcher_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source");
    std::fs::write(&path, "before").unwrap();
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let observation = cache.begin_source_path_change_observation(root.path(), &path, false).await.unwrap();
    let pending = cache.begin_host_workspace_mutation();
    std::fs::write(&path, "external").unwrap();
    cache.record_watched_source_change_event(Some(vec![path.clone()])).await;
    pending.settle();
    assert_eq!(cache.source_path_freshness(&observation), SourceFreshness::Changed);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "external");
}

#[tokio::test]
async fn abandoned_admission_stays_unknown_and_committed_paths_are_changed() {
    for committed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, "before").unwrap();
        let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
        let observation = cache.begin_source_path_change_observation(root.path(), &path, false).await.unwrap();
        let pending = cache.begin_host_workspace_mutation();
        if committed {
            std::fs::write(&path, "after").unwrap();
            cache.note_host_workspace_mutation_paths(root.path(), &["source".into()]).await;
            pending.settle();
        } else {
            drop(pending);
        }
        assert_eq!(cache.source_path_freshness(&observation),
            if committed { SourceFreshness::Changed } else { SourceFreshness::Unknown });
    }
}
