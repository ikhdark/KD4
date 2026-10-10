use std::fs;
use std::fs::FileTimes;

use std::time::Duration;
use std::time::SystemTime;

use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;
use crate::RolloutConfig;
use crate::RolloutRecorder;
use crate::RolloutRecorderParams;
use crate::append_rollout_item_to_path;
use crate::search_rollout_matches;

#[cfg(windows)]
#[tokio::test]
async fn orphan_lock_cleanup_never_unlinks_an_open_lock() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let directory = home.path().join(crate::SESSIONS_SUBDIR);
    fs::create_dir_all(&directory)?;
    let missing = directory.join("rollout-missing.jsonl");
    let orphan = rollout_lock_path(&missing);
    let orphan_write = rollout_write_lock_path(&missing);
    fs::write(&orphan, [])?;
    fs::write(&orphan_write, [])?;
    let held_path = rollout_lock_path(&directory.join("rollout-held.jsonl"));
    let held = open_lock_file(&held_path)?;
    // An unlocked open handle is sufficient protection, including OS-lock waiters.
    let compressed = directory.join("rollout-compressed.jsonl");
    let compressed_lock = rollout_lock_path(&compressed);
    fs::write(path::compressed_rollout_path(&compressed), [])?;
    fs::write(&compressed_lock, [])?;
    worker::run(home.path().to_path_buf()).await?;
    assert!(!orphan.exists());
    assert!(!orphan_write.exists());
    assert!(held_path.exists());
    assert!(compressed_lock.exists());
    drop(held);
    assert!(cleanup_orphan_rollout_lock(&held_path)?);
    assert!(!held_path.exists());
    Ok(())
}

#[tokio::test]
async fn archive_waits_for_compression_and_moves_its_final_representation() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(909);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "message survives archive")?;
    set_old_mtime(&path)?;
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let compression_path = path.clone();
    let compression_task = tokio::task::spawn_blocking(move || {
        worker::compress_rollout_if_cold_blocking_paused(&compression_path, reached_tx, resume_rx)
    });
    reached_rx.await?;
    let move_path = path.clone();
    let archived_dir = home.path().join(crate::ARCHIVED_SESSIONS_SUBDIR);
    let mut archive_task =
        tokio::spawn(async move { move_rollout_to_directory(&move_path, &archived_dir).await });
    let waited = tokio::time::timeout(Duration::from_millis(100), &mut archive_task)
        .await
        .is_err();
    // Always release the blocking worker, including when the assertion would fail.
    resume_tx.send(())?;
    compression_task.await??;
    assert!(
        waited,
        "archive must wait for compression's exclusive representation lock"
    );
    let archived_path = archive_task.await??;
    assert!(!path.exists());
    assert!(!compressed_rollout_path(&path).exists());
    assert!(archived_path.to_string_lossy().ends_with(".jsonl.zst"));
    let (items, loaded_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&archived_path).await?;
    assert_eq!(loaded_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    assert!(
        matches!(&items[1], RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == "message survives archive")
    );
    Ok(())
}

#[tokio::test]
async fn archive_waits_until_live_append_handle_closes() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(910);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&path, thread_id, "live thread")?;
    let (_, append_guard) = lock_rollout_for_append_blocking(&path)?;
    let source = path.clone();
    let destination = home.path().join(crate::ARCHIVED_SESSIONS_SUBDIR);
    let mut archive = tokio::spawn(async move { move_rollout_to_directory(&source, &destination).await });
    let waited = tokio::time::timeout(Duration::from_millis(100), &mut archive).await.is_err();
    drop(append_guard);
    assert!(waited, "the source lock must remain owned until append completion");
    let archived_path = archive.await??;
    assert!(!path.exists());
    assert!(archived_path.exists());
    let (_, loaded_id, parse_errors) = RolloutRecorder::load_rollout_items(&archived_path).await?;
    assert_eq!(loaded_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    Ok(())
}

#[tokio::test]
async fn rollout_moves_accept_identical_duplicates_and_preserve_conflicts() -> anyhow::Result<()> {
    for compressed in [false, true] {
        let home = TempDir::new()?;
        let source = rollout_path(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(912));
        fs::create_dir_all(source.parent().unwrap())?;
        fs::write(&source, b"identical\n")?;
        let destination_dir = home.path().join(crate::ARCHIVED_SESSIONS_SUBDIR);
        fs::create_dir_all(&destination_dir)?;
        let destination = destination_dir.join(source.file_name().unwrap());
        let destination = if compressed { compressed_rollout_path(&destination) } else { destination };
        let bytes = if compressed { zstd::stream::encode_all(b"different\n".as_slice(), 1)? } else { b"different\n".to_vec() };
        fs::write(&destination, &bytes)?;
        assert!(move_rollout_to_directory(&source, &destination_dir).await.is_err());
        assert_eq!(fs::read(&source)?, b"identical\n");
        assert_eq!(fs::read(&destination)?, bytes);
        let bytes = if compressed { zstd::stream::encode_all(b"identical\n".as_slice(), 1)? } else { b"identical\n".to_vec() };
        fs::write(&destination, &bytes)?;
        assert_eq!(fs::canonicalize(move_rollout_to_directory(&source, &destination_dir).await?)?, fs::canonicalize(&destination)?);
        assert!(!source.exists());
        assert_eq!(fs::read(&destination)?, bytes);
    }
    Ok(())
}

#[tokio::test]
async fn unarchive_resolves_representation_after_compression() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(913);
    let active = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    write_rollout(&active, thread_id, "unarchive after compression")?;
    let archived = move_rollout_to_directory(&active, &home.path().join(crate::ARCHIVED_SESSIONS_SUBDIR)).await?;
    set_old_mtime(&archived)?;
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let path = archived.clone();
    let compressor = tokio::task::spawn_blocking(move ||
        worker::compress_rollout_if_cold_blocking_paused(&path, reached_tx, resume_rx));
    reached_rx.await?;
    let destination = active.parent().unwrap().to_path_buf();
    let mut mover = tokio::spawn(async move { move_rollout_to_directory(&archived, &destination).await });
    let waiting = tokio::time::timeout(Duration::from_millis(50), &mut mover).await.is_err();
    resume_tx.send(())?;
    compressor.await??;
    assert!(waiting);
    let restored = mover.await??;
    assert!(restored.to_string_lossy().ends_with(".jsonl.zst"));
    let (_, id, errors) = RolloutRecorder::load_rollout_items(&restored).await?;
    assert_eq!(id, Some(thread_id));
    assert_eq!(errors, 0);
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_reads_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(1);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello compressed")?;
    compress_now(&rollout_path)?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    assert!(!rollout_path.exists());
    assert!(compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[tokio::test]
async fn record_torn_inside_a_character_is_skipped_in_both_representations() -> anyhow::Result<()>
{
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(911);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "before the crash")?;
    // A killed writer can stop inside a multi-byte character of its final record.
    let torn = serde_json::to_vec(&RolloutLine {
        timestamp: "2025-01-03T12:00:02Z".to_string(),
        item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "réponse".to_string(),
            ..Default::default()
        })),
    })?;
    let cut = torn.iter().position(|byte| *byte == 0xC3).expect("é") + 1;
    fs::OpenOptions::new()
        .append(true)
        .open(&rollout_path)?
        .write_all(&torn[..cut])?;
    // The next append terminates the fragment in place and continues the history.
    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "after the crash".to_string(),
            ..Default::default()
        })),
    )
    .await?;

    for compressed in [false, true] {
        if compressed {
            compress_now(&rollout_path)?;
        }
        let (items, loaded_thread_id, parse_errors) =
            RolloutRecorder::load_rollout_items(&rollout_path).await?;
        assert_eq!(loaded_thread_id, Some(thread_id));
        assert_eq!(parse_errors, 1);
        let messages = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::EventMsg(EventMsg::UserMessage(event)) => Some(event.message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, vec!["before the crash", "after the crash"]);
    }
    Ok(())
}

#[test]
fn rollout_file_from_path_normalizes_compressed_file_names() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(7);
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    let compressed_path = compressed_rollout_path(&rollout_path);

    assert_eq!(
        RolloutFile::from_path(compressed_path.clone()),
        Some(RolloutFile {
            path: compressed_path,
            plain_file_name: format!("rollout-2025-01-03T12-00-00-{uuid}.jsonl"),
        })
    );
    Ok(())
}

#[test]
fn rollout_file_from_path_hides_compressed_sibling_when_plain_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(8);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "plain wins")?;

    assert_eq!(
        RolloutFile::from_path(compressed_rollout_path(&rollout_path)),
        None
    );
    Ok(())
}

#[tokio::test]
async fn append_rollout_item_materializes_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(2);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before append")?;
    compress_now(&rollout_path)?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "hello after append".to_string(),
            ..Default::default()
        })),
    )
    .await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[tokio::test]
async fn search_rollout_matches_uses_logical_path_for_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(15);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "targeted search term")?;
    compress_now(&rollout_path)?;

    let matches = search_rollout_matches(
        std::path::Path::new("missing-rg-for-test"),
        home.path(),
        /*archived*/ false,
        "search term",
    )
    .await?;

    assert_eq!(
        matches.get(rollout_path.as_path()),
        Some(&Some("targeted search term".to_string()))
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn append_materialized_rollout_survives_compressed_cleanup_sharing_violation()
-> anyhow::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(101);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "before materialization")?;
    let (mut expected, _, _) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    // Allow decompression to read the source, but prevent deleting it.
    let held_source = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&compressed_path)?;
    assert_eq!(
        fs::remove_file(&compressed_path)
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    let appended = RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: "after materialization".to_string(),
        ..Default::default()
    }));
    append_rollout_item_to_path(&compressed_path, &appended).await?;
    expected.push(appended);
    assert!(rollout_path.exists());
    assert!(compressed_path.exists());
    // Readers of either representation must see the newly appended canonical history.
    for path in [&rollout_path, &compressed_path] {
        let (items, loaded_thread_id, parse_errors) =
            RolloutRecorder::load_rollout_items(path).await?;
        assert_eq!(loaded_thread_id, Some(thread_id));
        assert_eq!(parse_errors, 0);
        assert_eq!(
            serde_json::to_value(items)?,
            serde_json::to_value(&expected)?
        );
    }
    drop(held_source);
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn rollout_reader_retries_windows_sharing_violation() -> anyhow::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(102);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "survives replacement")?;
    let expected = fs::read_to_string(&rollout_path)?;
    let held_source = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&rollout_path)?;
    assert_eq!(
        fs::File::open(&rollout_path).unwrap_err().raw_os_error(),
        Some(32)
    );
    let mut opening = Box::pin(open_rollout_line_reader(&rollout_path));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut opening)
            .await
            .is_err()
    );
    drop(held_source);
    let mut reader = tokio::time::timeout(Duration::from_secs(2), opening).await??;
    let mut actual = Vec::new();
    while let Some(line) = reader.next_line().await? {
        actual.push(line);
    }
    assert_eq!(
        actual,
        expected.lines().map(str::to_string).collect::<Vec<_>>()
    );
    Ok(())
}

#[tokio::test]
async fn search_rollout_matches_fallback_returns_plain_and_compressed_snippets()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let plain_uuid = Uuid::from_u128(16);
    let plain_id = ThreadId::from_string(&plain_uuid.to_string())?;
    let plain_path = rollout_path(home.path(), "2025-01-03T12-00-00", plain_uuid);
    write_rollout(&plain_path, plain_id, "plain targeted search term")?;

    let compressed_uuid = Uuid::from_u128(17);
    let compressed_id = ThreadId::from_string(&compressed_uuid.to_string())?;
    let compressed_path = rollout_path(home.path(), "2025-01-04T12-00-00", compressed_uuid);
    write_rollout(
        &compressed_path,
        compressed_id,
        "compressed targeted search term",
    )?;
    compress_now(&compressed_path)?;

    let matches = search_rollout_matches(
        std::path::Path::new("missing-rg-for-test"),
        home.path(),
        /*archived*/ false,
        "targeted search term",
    )
    .await?;

    assert_eq!(
        matches.get(plain_path.as_path()),
        Some(&Some("plain targeted search term".to_string()))
    );
    assert_eq!(
        matches.get(compressed_path.as_path()),
        Some(&Some("compressed targeted search term".to_string()))
    );
    Ok(())
}

#[tokio::test]
async fn search_rollout_matches_fallback_has_paginated_plain_compressed_parity()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let cases = [
        (
            Uuid::from_u128(18),
            false,
            TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                text: "user paginated needle".to_string(),
                text_elements: Vec::new(),
            }])),
            "user paginated needle",
        ),
        (
            Uuid::from_u128(19),
            true,
            TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                text: "user paginated needle".to_string(),
                text_elements: Vec::new(),
            }])),
            "user paginated needle",
        ),
        (
            Uuid::from_u128(20),
            false,
            TurnItem::AgentMessage(AgentMessageItem::new(&[AgentMessageContent::Text {
                text: "agent paginated needle".to_string(),
            }])),
            "agent paginated needle",
        ),
        (
            Uuid::from_u128(21),
            true,
            TurnItem::AgentMessage(AgentMessageItem::new(&[AgentMessageContent::Text {
                text: "agent paginated needle".to_string(),
            }])),
            "agent paginated needle",
        ),
    ];
    let mut expected = Vec::new();

    for (uuid, compressed, item, snippet) in cases {
        let thread_id = ThreadId::from_string(&uuid.to_string())?;
        let path = rollout_path(home.path(), "2025-01-05T12-00-00", uuid);
        write_rollout(&path, thread_id, "unrelated conversation")?;
        append_rollout_item_to_path(
            &path,
            &RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: "turn".to_string(),
                item,
                completed_at_ms: 0,
            })),
        )
        .await?;
        if compressed {
            compress_now(&path)?;
        }
        expected.push((path, snippet));
    }

    let matches = search_rollout_matches(
        std::path::Path::new("missing-rg-for-test"),
        home.path(),
        /*archived*/ false,
        "paginated needle",
    )
    .await?;

    for (path, snippet) in expected {
        assert_eq!(matches.get(&path), Some(&Some(snippet.to_string())));
    }
    Ok(())
}

#[tokio::test]
async fn search_rollout_matches_fallback_retains_metadata_only_compressed_match()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let plain_uuid = Uuid::from_u128(22);
    let plain_id = ThreadId::from_string(&plain_uuid.to_string())?;
    let plain_path = rollout_path(home.path(), "2025-01-06T12-00-00", plain_uuid);
    write_rollout(&plain_path, plain_id, "unrelated conversation")?;

    let compressed_uuid = Uuid::from_u128(23);
    let compressed_id = ThreadId::from_string(&compressed_uuid.to_string())?;
    let compressed_path = rollout_path(home.path(), "2025-01-07T12-00-00", compressed_uuid);
    write_rollout(&compressed_path, compressed_id, "unrelated conversation")?;
    compress_now(&compressed_path)?;

    let matches = search_rollout_matches(
        std::path::Path::new("missing-rg-for-test"),
        home.path(),
        /*archived*/ false,
        "2025-01-03T12:00:00Z",
    )
    .await?;

    assert_eq!(matches.get(&plain_path), Some(&None));
    assert_eq!(matches.get(&compressed_path), Some(&None));
    Ok(())
}

#[tokio::test]
async fn worker_compresses_old_active_and_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let active_uuid = Uuid::from_u128(3);
    let active_id = ThreadId::from_string(&active_uuid.to_string())?;
    let active_path = rollout_path(home.path(), "2025-01-03T12-00-00", active_uuid);
    write_rollout(&active_path, active_id, "old active")?;
    set_old_mtime(&active_path)?;

    let archived_uuid = Uuid::from_u128(4);
    let archived_id = ThreadId::from_string(&archived_uuid.to_string())?;
    let archived_path = archived_rollout_path(home.path(), "2025-01-04T12-00-00", archived_uuid);
    write_rollout(&archived_path, archived_id, "old archived")?;
    set_old_mtime(&archived_path)?;

    let fresh_uuid = Uuid::from_u128(5);
    let fresh_id = ThreadId::from_string(&fresh_uuid.to_string())?;
    let fresh_path = rollout_path(home.path(), "2025-01-05T12-00-00", fresh_uuid);
    write_rollout(&fresh_path, fresh_id, "fresh active")?;

    let stale_temp = active_path.with_file_name("rollout-stale.jsonl.zst.tmp");
    fs::write(&stale_temp, "stale temp")?;
    set_old_mtime(&stale_temp)?;

    let fresh_temp = active_path.with_file_name("rollout-fresh.jsonl.zst.tmp");
    fs::write(&fresh_temp, "fresh temp")?;

    let current_owned_temps = [
        active_path.with_file_name("rollout-compress-Ab12xy.tmp"),
        archived_path.with_file_name("rollout-old.jsonl.decompress.123.0.tmp"),
    ];
    for path in &current_owned_temps {
        fs::write(path, "stale owned temporary data")?;
        set_old_mtime(path)?;
    }

    let unrelated_temps = [
        active_path.with_file_name("notes.tmp"),
        archived_path.with_file_name("notes.tmp"),
        active_path.with_file_name("rollout-notes.tmp"),
        archived_path.with_file_name("rollout-old.jsonl.decompress.notes.tmp"),
    ];
    for path in &unrelated_temps {
        fs::write(path, "user-owned temporary data")?;
        set_old_mtime(path)?;
    }

    worker::run(home.path().to_path_buf()).await?;

    assert!(!active_path.exists());
    assert!(compressed_rollout_path(&active_path).exists());
    assert!(!archived_path.exists());
    assert!(compressed_rollout_path(&archived_path).exists());
    assert!(fresh_path.exists());
    assert!(!compressed_rollout_path(&fresh_path).exists());
    assert!(!stale_temp.exists());
    assert!(fresh_temp.exists());
    for path in &current_owned_temps {
        assert!(!path.exists(), "owned stale temporary file must be cleaned: {}", path.display());
    }
    for path in &unrelated_temps {
        assert_eq!(fs::read_to_string(path)?, "user-owned temporary data");
    }
    // Claiming creates the marker; only a completed run leaves the nonempty
    // contents that record its cooldown.
    assert!(
        fs::metadata(home.path().join(".tmp").join("rollout-compression.lock"))?.len() > 0
    );
    Ok(())
}

#[tokio::test]
async fn worker_skips_rollout_while_append_handle_is_open() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let config = RolloutConfig {
        codex_home: home.path().to_path_buf(),
        sqlite_home: home.path().to_path_buf(),
        cwd: home.path().to_path_buf(),
        model_provider_id: "test-provider".to_string(),
    };
    let uuid = Uuid::from_u128(18);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "before active append")?;
    set_old_mtime(&rollout_path)?;

    let recorder =
        RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path.clone())).await?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(
            UserMessageEvent {
                message: "after skipped compression".to_string(),
                ..Default::default()
            },
        ))])
        .await?;
    recorder.flush().await?;
    recorder.shutdown().await?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[tokio::test]
async fn append_after_compression_final_check_survives_in_one_representation() -> anyhow::Result<()>
{
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(19);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "before compression race")?;
    set_old_mtime(&rollout_path)?;

    let (reached_final_check_tx, reached_final_check_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let compression_path = rollout_path.clone();
    let compression_task = tokio::task::spawn_blocking(move || {
        worker::compress_rollout_if_cold_blocking_paused(
            compression_path.as_path(),
            reached_final_check_tx,
            resume_rx,
        )
    });
    reached_final_check_rx.await?;

    let append_path = rollout_path.clone();
    let mut append_task = tokio::spawn(async move {
        append_rollout_item_to_path(
            append_path.as_path(),
            &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "append during compression".to_string(),
                ..Default::default()
            })),
        )
        .await
    });
    let waited = tokio::time::timeout(Duration::from_millis(100), &mut append_task)
        .await
        .is_err();

    resume_tx.send(())?;
    compression_task.await??;
    append_task.await??;
    assert!(waited, "append should wait for compression's exclusive rollout lock");

    let compressed_path = compressed_rollout_path(&rollout_path);
    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(
                item,
                RolloutItem::EventMsg(EventMsg::UserMessage(event))
                    if event.message == "append during compression"
            ))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn resume_materializes_compressed_rollout_path() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let config = RolloutConfig {
        codex_home: home.path().to_path_buf(),
        sqlite_home: home.path().to_path_buf(),
        cwd: home.path().to_path_buf(),
        model_provider_id: "test-provider".to_string(),
    };
    let uuid = Uuid::from_u128(3);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before resume")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    set_old_mtime(&compressed_path)?;
    let compressed_modified = fs::metadata(&compressed_path)?.modified()?;

    let InitialHistory::Resumed(history) =
        RolloutRecorder::get_rollout_history(compressed_path.as_path()).await?
    else {
        panic!("expected compressed rollout to load as resumed history");
    };
    assert_eq!(history.rollout_path, Some(rollout_path.clone()));

    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::resume(compressed_path.clone()),
    )
    .await?;

    assert_eq!(recorder.rollout_path(), rollout_path.as_path());
    assert_eq!(fs::metadata(&rollout_path)?.modified()?, compressed_modified);
    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(
            UserMessageEvent {
                message: "hello after resume".to_string(),
                ..Default::default()
            },
        ))])
        .await?;
    recorder.flush().await?;
    recorder.shutdown().await?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    let messages = items.iter().filter_map(|item| match item {
        RolloutItem::EventMsg(EventMsg::UserMessage(event)) => Some(event.message.as_str()),
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(messages, ["hello before resume", "hello after resume"]);
    Ok(())
}



#[test]
fn persist_temp_file_noclobber_installs_completed_temp() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let temp_path = home.path().join("rollout.jsonl.tmp");
    let destination = home.path().join("rollout.jsonl");
    fs::write(&temp_path, "completed rollout")?;

    persist_temp_file_noclobber(&temp_path, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "completed rollout");
    Ok(())
}

#[test]
fn persist_temp_file_noclobber_does_not_replace_existing_destination() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let temp_path = home.path().join("rollout.jsonl.tmp");
    let destination = home.path().join("rollout.jsonl");
    fs::write(&temp_path, "candidate rollout")?;
    fs::write(&destination, "existing rollout")?;

    persist_temp_file_noclobber(&temp_path, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "existing rollout");
    Ok(())
}

#[tokio::test]
async fn worker_skips_existing_compressed_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(10);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "already compressed")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    set_old_mtime(&compressed_path)?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(!rollout_path.exists());
    assert!(compressed_path.exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    Ok(())
}

#[tokio::test]
async fn worker_skips_when_fresh_run_marker_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(11);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "throttled worker")?;
    set_old_mtime(&rollout_path)?;
    let marker_dir = home.path().join(".tmp");
    fs::create_dir_all(marker_dir.as_path())?;
    fs::write(marker_dir.join("rollout-compression.lock"), "recent run")?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[tokio::test]
async fn long_lived_worker_compresses_after_a_fresh_marker_expires() -> anyhow::Result<()> {
    // The process starts inside another process's cooldown and keeps running.
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(12);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "long-lived worker")?;
    set_old_mtime(&rollout_path)?;
    let marker_path = home.path().join(".tmp").join("rollout-compression.lock");
    fs::create_dir_all(home.path().join(".tmp"))?;
    fs::write(&marker_path, "recent run")?;

    let worker = tokio::spawn(worker::run_periodically(
        home.path().to_path_buf(),
        Duration::from_millis(50),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !compressed_rollout_path(&rollout_path).exists(),
        "the cooldown is respected"
    );

    set_old_mtime(&marker_path)?;
    let compressed = tokio::time::timeout(Duration::from_secs(10), async {
        while !compressed_rollout_path(&rollout_path).exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    worker.abort();
    assert!(
        compressed.is_ok(),
        "the same process retries once the cooldown expires"
    );
    Ok(())
}

#[test]
fn run_marker_is_reusable_unless_persisted() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let marker_path = home.path().join(".tmp").join("rollout-compression.lock");

    {
        let marker = worker::CompressionRunMarker::try_claim(home.path())?;
        assert!(marker.is_some());
        set_old_mtime(&marker_path)?;
        // Even a stale timestamp cannot override an active owner.
        assert!(worker::CompressionRunMarker::try_claim(home.path())?.is_none());
    }
    assert_eq!(fs::metadata(&marker_path)?.len(), 0);

    let marker = worker::CompressionRunMarker::try_claim(home.path())?;
    let Some(marker) = marker else {
        panic!("expected run marker claim");
    };
    marker.persist();
    assert!(marker_path.exists());
    assert!(worker::CompressionRunMarker::try_claim(home.path())?.is_none());
    Ok(())
}

#[tokio::test]
async fn find_thread_path_by_id_handles_compressed_rollout_filenames() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(8);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "compressed filename lookup")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);

    assert_eq!(
        crate::find_thread_path_by_id_str(
            home.path(),
            &uuid.to_string(),
            /*state_db_ctx*/ None
        )
        .await?,
        Some(compressed_path)
    );
    assert_eq!(
        crate::find_thread_path_by_id_str(home.path(), "not-a-uuid", /*state_db_ctx*/ None).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn find_thread_path_by_id_ignores_compression_temp_matches() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(9);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let temp_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid).with_file_name(format!(
        "rollout-2025-01-03T12-00-00-{uuid}.jsonl.zst.compress.1.0.tmp"
    ));
    write_rollout(&temp_path, thread_id, "temporary file should not resolve")?;

    assert_eq!(
        crate::find_thread_path_by_id_str(
            home.path(),
            &uuid.to_string(),
            /*state_db_ctx*/ None
        )
        .await?,
        None
    );
    Ok(())
}

fn rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("sessions/2025/01/03")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn archived_rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("archived_sessions")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn write_rollout(path: &std::path::Path, thread_id: ThreadId, message: &str) -> anyhow::Result<()> {
    let parent = path.parent().expect("rollout path should have parent");
    fs::create_dir_all(parent)?;
    let session_meta_line = SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            cwd: parent.to_path_buf(),
            originator: "test".to_string(),
            cli_version: "test".to_string(),
            harness_build: None,
            source: SessionSource::Cli,
            thread_source: None,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
            model_provider: None,
            base_instructions: None,
            dynamic_tools: None,
            selected_capability_roots: Vec::new(),

            history_mode: Default::default(),
            multi_agent_version: None,
            context_window: None,
        },
        git: None,
    };
    let lines = [
        RolloutLine {
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            item: RolloutItem::SessionMeta(session_meta_line),
        },
        RolloutLine {
            timestamp: "2025-01-03T12:00:01Z".to_string(),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: message.to_string(),
                ..Default::default()
            })),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(path, format!("{jsonl}\n"))?;
    Ok(())
}

fn compress_now(path: &std::path::Path) -> anyhow::Result<()> {
    let compressed_path = compressed_rollout_path(path);
    let input = fs::File::open(path)?;
    let output = fs::File::create(compressed_path)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, 3)?;
    let mut input = std::io::BufReader::new(input);
    std::io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    fs::remove_file(path)?;
    Ok(())
}

fn set_old_mtime(path: &std::path::Path) -> anyhow::Result<()> {
    let old = SystemTime::now()
        .checked_sub(Duration::from_secs(8 * 24 * 60 * 60))
        .expect("old timestamp should be representable");
    let times = FileTimes::new().set_modified(old);
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_times(times)?;
    Ok(())
}
