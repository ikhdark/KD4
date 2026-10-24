use super::test_support::test_config;
use super::test_support::write_archived_session_file;
use super::test_support::write_session_file;
use super::*;
use crate::SortDirection;
use crate::ThreadListStorageMode;
use crate::ThreadSortKey;
use codex_protocol::protocol::SessionSource;
use tempfile::TempDir;
use uuid::Uuid;

fn list_params() -> ListThreadsParams {
    ListThreadsParams {
        project_id: None,
        page_size: 10,
        cursor: None,
        sort_key: ThreadSortKey::CreatedAt,
        sort_direction: SortDirection::Asc,
        allowed_sources: Vec::new(),
        model_providers: None,
        cwd_filters: None,
        archived: false,
        search_term: None,
        relation_filter: None,
        storage_mode: ThreadListStorageMode::PreferStateDb,
    }
}

fn search_params() -> SearchThreadsParams {
    SearchThreadsParams {
        page_size: 10,
        cursor: None,
        sort_key: ThreadSortKey::CreatedAt,
        sort_direction: SortDirection::Asc,
        allowed_sources: Vec::new(),
        archived: false,
        search_term: "Hello from user".into(),
    }
}

#[tokio::test]
async fn duplicate_lifecycle_never_reports_archive_success_and_delete_covers_all_copies() {
    let home = TempDir::new().unwrap();
    let uuid = Uuid::from_u128(34004);
    let id = ThreadId::from_string(&uuid.to_string()).unwrap();
    let first = write_session_file(home.path(), "2025-01-03T12-00-00", uuid).unwrap();
    let second = write_session_file(home.path(), "2025-01-03T13-00-00", uuid).unwrap();
    let archived = write_archived_session_file(home.path(), "2025-01-03T14-00-00", uuid).unwrap();
    let paths = [first, second, archived];
    let bytes: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let store = LocalThreadStore::new(test_config(home.path()), None);
    assert!(matches!(
        store
            .archive_thread(crate::ArchiveThreadParams { thread_id: id })
            .await,
        Err(ThreadStoreError::Conflict { .. })
    ));
    for (path, expected) in paths.iter().zip(bytes) {
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }
    store
        .delete_thread(crate::DeleteThreadParams { thread_id: id })
        .await
        .unwrap();
    assert!(paths.iter().all(|path| !path.exists()));
    assert!(
        codex_rollout::find_thread_rollout_paths(home.path(), id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn search_reconciles_a_withheld_row_and_preserves_summary_identity_and_metadata() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        home.path().into(),
        config.default_model_provider_id.clone(),
    )
    .await
    .unwrap();
    runtime.mark_backfill_complete(None).await.unwrap();
    let uuid = Uuid::from_u128(34010);
    let id = ThreadId::from_string(&uuid.to_string()).unwrap();
    let root = codex_protocol::SessionId::new();
    let path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid).unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    let (header, rest) = original.split_once('\n').unwrap();
    let mut header: serde_json::Value = serde_json::from_str(header).unwrap();
    header["payload"]["session_id"] = serde_json::json!(root);
    header["payload"]["forked_from_id"] = serde_json::json!(Uuid::from_u128(34009));
    header["payload"]["thread_source"] = serde_json::json!("user");
    std::fs::write(&path, format!("{header}\n{rest}")).unwrap();
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    assert!(runtime.get_thread(id).await.unwrap().is_none());
    let found = store.search_threads(search_params()).await.unwrap();
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].thread.thread_id, id);
    assert_eq!(found.items[0].thread.session_id, Some(root));

    let project = runtime
        .create_project(
            "P".into(),
            Vec::new(),
            Default::default(),
            &[],
            "audit34-summary",
        )
        .await
        .unwrap()
        .project;
    // Exercise the public metadata owner rather than injecting its SQLite row.
    // Legacy rollout records need not contain a model observation.
    store
        .update_thread_metadata(crate::UpdateThreadMetadataParams {
            thread_id: id,
            include_archived: false,
            patch: crate::ThreadMetadataPatch {
                project_id: Some(Some(project.id.clone())),
                name: Some(Some("Explicit name".into())),
                model: Some("explicit-model".into()),
                thread_source: Some(Some(codex_protocol::protocol::ThreadSource::User)),
                source: Some(SessionSource::Cli),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let read = store
        .read_thread(ReadThreadParams {
            thread_id: id,
            include_archived: true,
            include_history: false,
        })
        .await
        .unwrap();
    let by_path = store
        .read_thread_by_rollout_path(path.clone(), true, false)
        .await
        .unwrap();
    let listed = store
        .list_threads(list_params())
        .await
        .unwrap()
        .items
        .remove(0);
    let searched = store
        .search_threads(search_params())
        .await
        .unwrap()
        .items
        .remove(0)
        .thread;
    store
        .archive_thread(crate::ArchiveThreadParams { thread_id: id })
        .await
        .unwrap();
    let restored = store
        .unarchive_thread(crate::ArchiveThreadParams { thread_id: id })
        .await
        .unwrap();
    let restored_path = restored.rollout_path.clone().unwrap();
    for (operation, summary) in [
        ("read", read),
        ("by_path", by_path),
        ("listed", listed),
        ("searched", searched),
        ("restored", restored),
    ] {
        assert_eq!(summary.session_id, Some(root));
        assert_eq!(summary.project_id.as_deref(), Some(project.id.as_str()));
        assert_eq!(
            summary.model.as_deref(),
            Some("explicit-model"),
            "{operation}"
        );
        assert_eq!(summary.name.as_deref(), Some("Explicit name"));
        assert_eq!(
            summary.thread_source,
            Some(codex_protocol::protocol::ThreadSource::User)
        );
        assert_eq!(
            summary.forked_from_id.map(|id| id.to_string()),
            Some(Uuid::from_u128(34009).to_string())
        );
    }
    // Preservation of an unknown model must not make the cached observation
    // sticky once the canonical transcript supplies a newer known model.
    let settings: codex_protocol::protocol::RolloutLine =
        serde_json::from_value(serde_json::json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": {
                    "model": "new-canonical-model",
                    "model_provider_id": config.default_model_provider_id,
                    "approval_policy": "never",
                    "permission_profile": {"type": "disabled"},
                    "cwd": home.path(),
                    "collaboration_mode": {"mode": "default", "settings": {
                        "model": "new-canonical-model", "reasoning_effort": null,
                        "developer_instructions": null
                    }}
                }
            }
        }))
        .unwrap();
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(restored_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&settings).unwrap()).unwrap();
    }
    let updated = store
        .search_threads(search_params())
        .await
        .unwrap()
        .items
        .remove(0)
        .thread;
    assert_eq!(updated.model.as_deref(), Some("new-canonical-model"));
}

#[tokio::test]
async fn physical_created_keys_page_same_second_headers_with_different_milliseconds() {
    let home = TempDir::new().unwrap();
    let mut expected = Vec::new();
    for (value, millis) in [(34091, "100"), (34092, "500")] {
        let uuid = Uuid::from_u128(value);
        let path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let (header, rest) = original.split_once('\n').unwrap();
        let mut header: serde_json::Value = serde_json::from_str(header).unwrap();
        header["payload"]["timestamp"] =
            serde_json::json!(format!("2025-01-03T18:00:00.{millis}Z"));
        std::fs::write(&path, format!("{header}\n{rest}")).unwrap();
        expected.push(ThreadId::from_string(&uuid.to_string()).unwrap());
    }
    let store = LocalThreadStore::new(test_config(home.path()), None);
    for direction in [SortDirection::Asc, SortDirection::Desc] {
        let mut params = search_params();
        params.page_size = 1;
        params.sort_direction = direction;
        let first = store.search_threads(params.clone()).await.unwrap();
        params.cursor = first.next_cursor;
        assert!(params.cursor.is_some());
        let second = store.search_threads(params).await.unwrap();
        let mut wanted = expected.clone();
        if direction == SortDirection::Desc {
            wanted.reverse();
        }
        assert_eq!(
            vec![
                first.items[0].thread.thread_id,
                second.items[0].thread.thread_id
            ],
            wanted
        );
        assert!(second.next_cursor.is_none());
    }
}
