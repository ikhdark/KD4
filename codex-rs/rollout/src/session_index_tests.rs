#![allow(warnings, clippy::all)]

use super::*;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
fn write_index(path: &Path, lines: &[SessionIndexEntry]) -> std::io::Result<()> {
    let mut out = String::new();
    for entry in lines {
        out.push_str(&serde_json::to_string(entry).unwrap());
        out.push('\n');
    }
    std::fs::write(path, out)
}

fn write_rollout_with_metadata(path: &Path, thread_id: ThreadId) -> std::io::Result<()> {
    let timestamp = "2024-01-01T00-00-00Z".to_string();
    let line = RolloutLine {
        timestamp: timestamp.clone(),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                forked_from_id: None,
                parent_thread_id: None,
                timestamp,
                cwd: ".".into(),
                originator: "test_originator".into(),
                cli_version: "test_version".into(),
                source: SessionSource::Cli,
                thread_source: None,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
                model_provider: Some("test-provider".into()),
                base_instructions: None,
                dynamic_tools: None,
                selected_capability_roots: Vec::new(),
                memory_mode: None,
                history_mode: Default::default(),
                multi_agent_version: None,
                context_window: None,
            },
            git: None,
        }),
    };
    let body = serde_json::to_string(&line).map_err(std::io::Error::other)?;
    std::fs::write(path, format!("{body}\n"))
}

#[tokio::test]
async fn session_index_locks_are_shared_per_canonical_path_only() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let home_a = temp.path().join("a");
    let home_b = temp.path().join("b");
    std::fs::create_dir_all(&home_a)?;
    std::fs::create_dir_all(&home_b)?;

    let path_a = canonical_session_index_path(&home_a).await?;
    let aliased_path_a = canonical_session_index_path(&home_a.join(".")).await?;
    let path_b = canonical_session_index_path(&home_b).await?;
    let lock_a = session_index_lock(&path_a).await;
    let aliased_lock_a = session_index_lock(&aliased_path_a).await;
    let lock_b = session_index_lock(&path_b).await;

    assert!(Arc::ptr_eq(&lock_a, &aliased_lock_a));
    assert!(!Arc::ptr_eq(&lock_a, &lock_b));

    let guard_a = lock_a.write_owned().await;
    let entry_a = SessionIndexEntry {
        id: ThreadId::new(),
        thread_name: "same path".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    };
    let mut append_a = Box::pin(append_session_index_entry(&home_a, &entry_a));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), append_a.as_mut())
            .await
            .is_err()
    );

    let entry_b = SessionIndexEntry {
        id: ThreadId::new(),
        thread_name: "different path".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    };
    tokio::time::timeout(
        Duration::from_secs(1),
        append_session_index_entry(&home_b, &entry_b),
    )
    .await
    .map_err(std::io::Error::other)??;

    drop(guard_a);
    tokio::time::timeout(Duration::from_secs(1), append_a)
        .await
        .map_err(std::io::Error::other)??;
    Ok(())
}

#[tokio::test]
async fn append_remove_and_read_keep_the_index_consistent() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let removed_id = ThreadId::new();
    let retained_id = ThreadId::new();
    let first = SessionIndexEntry {
        id: removed_id,
        thread_name: "first".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    };
    let retained = SessionIndexEntry {
        id: retained_id,
        thread_name: "retained".to_string(),
        updated_at: "2024-01-02T00:00:00Z".to_string(),
    };
    let latest = SessionIndexEntry {
        id: removed_id,
        thread_name: "latest".to_string(),
        updated_at: "2024-01-03T00:00:00Z".to_string(),
    };

    append_session_index_entry(temp.path(), &first).await?;
    append_session_index_entry(temp.path(), &retained).await?;
    append_session_index_entry(temp.path(), &latest).await?;
    assert_eq!(
        find_thread_name_by_id(temp.path(), &removed_id).await?,
        Some("latest".to_string())
    );

    remove_thread_name_entries(temp.path(), removed_id).await?;

    assert_eq!(find_thread_name_by_id(temp.path(), &removed_id).await?, None);
    let mut ids = HashSet::new();
    ids.insert(removed_id);
    ids.insert(retained_id);
    let mut expected = HashMap::new();
    expected.insert(retained_id, "retained".to_string());
    assert_eq!(find_thread_names_by_ids(temp.path(), &ids).await?, expected);

    let contents = std::fs::read_to_string(session_index_path(temp.path()))?;
    let entries = contents
        .lines()
        .map(serde_json::from_str::<SessionIndexEntry>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)?;
    assert_eq!(entries, vec![retained]);
    assert!(!session_index_path(temp.path())
        .with_extension("jsonl.tmp")
        .exists());
    Ok(())
}

#[test]
fn find_thread_id_by_name_prefers_latest_entry() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id1 = ThreadId::new();
    let id2 = ThreadId::new();
    let lines = vec![
        SessionIndexEntry {
            id: id1,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: id2,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = scan_index_from_end(&path, |entry| entry.thread_name == "same")?;
    assert_eq!(found.map(|entry| entry.id), Some(id2));
    Ok(())
}

#[tokio::test]
async fn find_thread_meta_by_name_str_skips_newest_entry_without_rollout() -> std::io::Result<()> {
    // A newer unsaved name entry should not shadow an older persisted rollout with the same name.
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let saved_id = ThreadId::new();
    let unsaved_id = ThreadId::new();
    let saved_rollout_path = temp
        .path()
        .join("sessions/2024/01/01")
        .join(format!("rollout-2024-01-01T00-00-00-{saved_id}.jsonl"));
    std::fs::create_dir_all(saved_rollout_path.parent().expect("rollout parent"))?;
    write_rollout_with_metadata(&saved_rollout_path, saved_id)?;
    let lines = vec![
        SessionIndexEntry {
            id: saved_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: unsaved_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = find_thread_meta_by_name_str(temp.path(), "same", /*state_db_ctx*/ None).await?;

    assert_eq!(
        found.map(|(path, session_meta)| (path, session_meta.meta.id)),
        Some((saved_rollout_path, saved_id))
    );
    Ok(())
}

#[tokio::test]
async fn find_thread_meta_by_name_str_skips_partial_rollout() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let saved_id = ThreadId::new();
    let partial_id = ThreadId::new();
    let rollout_dir = temp.path().join("sessions/2024/01/01");
    let saved_rollout_path =
        rollout_dir.join(format!("rollout-2024-01-01T00-00-00-{saved_id}.jsonl"));
    let partial_rollout_path =
        rollout_dir.join(format!("rollout-2024-01-01T00-00-01-{partial_id}.jsonl"));
    std::fs::create_dir_all(&rollout_dir)?;
    write_rollout_with_metadata(&saved_rollout_path, saved_id)?;
    std::fs::write(&partial_rollout_path, "")?;
    let lines = vec![
        SessionIndexEntry {
            id: saved_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: partial_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = find_thread_meta_by_name_str(temp.path(), "same", /*state_db_ctx*/ None).await?;

    assert_eq!(found.map(|(path, _)| path), Some(saved_rollout_path));
    Ok(())
}

#[tokio::test]
async fn find_thread_meta_by_name_str_ignores_historical_name_after_rename() -> std::io::Result<()>
{
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let renamed_id = ThreadId::new();
    let current_id = ThreadId::new();
    let current_rollout_path = temp
        .path()
        .join("sessions/2024/01/01")
        .join(format!("rollout-2024-01-01T00-00-00-{current_id}.jsonl"));
    std::fs::create_dir_all(current_rollout_path.parent().expect("rollout parent"))?;
    write_rollout_with_metadata(&current_rollout_path, current_id)?;
    let lines = vec![
        SessionIndexEntry {
            id: renamed_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: current_id,
            thread_name: "same".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: renamed_id,
            thread_name: "different".to_string(),
            updated_at: "2024-01-03T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = find_thread_meta_by_name_str(temp.path(), "same", /*state_db_ctx*/ None).await?;

    assert_eq!(found.map(|(path, _)| path), Some(current_rollout_path));
    Ok(())
}

#[test]
fn find_thread_name_by_id_prefers_latest_entry() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id = ThreadId::new();
    let lines = vec![
        SessionIndexEntry {
            id,
            thread_name: "first".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id,
            thread_name: "second".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found = scan_index_from_end_by_id(&path, &id)?;
    assert_eq!(
        found.map(|entry| entry.thread_name),
        Some("second".to_string())
    );
    Ok(())
}

#[test]
fn scan_index_returns_none_when_entry_missing() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id = ThreadId::new();
    let lines = vec![SessionIndexEntry {
        id,
        thread_name: "present".to_string(),
        updated_at: "2024-01-01T00:00:00Z".to_string(),
    }];
    write_index(&path, &lines)?;

    let missing_name = scan_index_from_end(&path, |entry| entry.thread_name == "missing")?;
    assert_eq!(missing_name, None);

    let missing_id = scan_index_from_end_by_id(&path, &ThreadId::new())?;
    assert_eq!(missing_id, None);
    Ok(())
}

#[tokio::test]
async fn find_thread_names_by_ids_prefers_latest_entry() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id1 = ThreadId::new();
    let id2 = ThreadId::new();
    let lines = vec![
        SessionIndexEntry {
            id: id1,
            thread_name: "first".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: id2,
            thread_name: "other".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        SessionIndexEntry {
            id: id1,
            thread_name: "latest".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let mut ids = HashSet::new();
    ids.insert(id1);
    ids.insert(id2);

    let mut expected = HashMap::new();
    expected.insert(id1, "latest".to_string());
    expected.insert(id2, "other".to_string());

    let found = find_thread_names_by_ids(temp.path(), &ids).await?;
    assert_eq!(found, expected);
    Ok(())
}

#[test]
fn scan_index_finds_latest_match_among_mixed_entries() -> std::io::Result<()> {
    let temp = TempDir::new()?;
    let path = session_index_path(temp.path());
    let id_target = ThreadId::new();
    let id_other = ThreadId::new();
    let expected = SessionIndexEntry {
        id: id_target,
        thread_name: "target".to_string(),
        updated_at: "2024-01-03T00:00:00Z".to_string(),
    };
    let expected_other = SessionIndexEntry {
        id: id_other,
        thread_name: "target".to_string(),
        updated_at: "2024-01-02T00:00:00Z".to_string(),
    };
    // Resolution is based on append order (scan from end), not updated_at.
    let lines = vec![
        SessionIndexEntry {
            id: id_target,
            thread_name: "target".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        },
        expected_other.clone(),
        expected.clone(),
        SessionIndexEntry {
            id: ThreadId::new(),
            thread_name: "another".to_string(),
            updated_at: "2024-01-04T00:00:00Z".to_string(),
        },
    ];
    write_index(&path, &lines)?;

    let found_by_name = scan_index_from_end(&path, |entry| entry.thread_name == "target")?;
    assert_eq!(found_by_name, Some(expected.clone()));

    let found_by_id = scan_index_from_end_by_id(&path, &id_target)?;
    assert_eq!(found_by_id, Some(expected));

    let found_other_by_id = scan_index_from_end_by_id(&path, &id_other)?;
    assert_eq!(found_other_by_id, Some(expected_other));
    Ok(())
}
