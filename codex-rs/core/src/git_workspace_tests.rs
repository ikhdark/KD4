use super::*;

// The remaining owner discovers ordinary Git roots; it does not attest source
// freshness or start watches for files mentioned by a tool.
#[test]
fn ordinary_root_discovery_observes_git_marker_changes() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("src");
    std::fs::create_dir(&child).unwrap();
    let cache = WorkspaceRootCache::default();
    assert_eq!(cache.resolve(&child).unwrap(), None);
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let discovered = cache.resolve(&child).unwrap().unwrap();
    assert_eq!(discovered.canonicalize().unwrap(), root.path().canonicalize().unwrap());
    std::fs::remove_dir(root.path().join(".git")).unwrap();
    assert_eq!(cache.resolve(&child).unwrap(), None);
}

#[tokio::test]
async fn legacy_source_observations_remain_readable_without_becoming_freshness_proof() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.rs");
    let observation: SourcePathChangeObservation = serde_json::from_value(serde_json::json!({
        "watcher_epoch":1, "watcher_generation":2, "registration_generation":3,
        "repo_root":root.path(), "path":source, "recursive":false
    })).unwrap();
    let cache = GitWorkspaceCache::new();
    assert_eq!(cache.source_path_freshness(&observation), SourceFreshness::Unknown);
    assert!(!cache.source_path_change_observation_is_current(&observation));
    assert!(cache.begin_source_path_change_observations(root.path(), &[(source, false)]).await.is_none());
    assert!(cache.workspace_evidence_identity(root.path()).await.is_none());
    assert_eq!(cache.workspace_evidence_capture_sequence(), 0);
}
