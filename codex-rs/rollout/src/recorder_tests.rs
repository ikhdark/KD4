#![allow(warnings, clippy::all)]

use super::*;
use crate::ToolManifestDictionary;
use crate::config::RolloutConfig;
use chrono::TimeZone;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::ToolManifestItem;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Barrier;
use uuid::Uuid;

/// Stamp items the way `record_canonical_items` does: one capture instant for
/// the whole enqueued batch.
fn captured(items: Vec<RolloutItem>) -> Vec<CapturedRolloutItem> {
    let captured_at = OffsetDateTime::now_utc();
    items
        .into_iter()
        .map(|item| CapturedRolloutItem::new(item, captured_at))
        .collect()
}

#[tokio::test]
async fn failed_rollout_backlog_is_bounded_and_only_barriers_retry() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let blocked = home.path().join("blocked");
    fs::write(&blocked, "not a directory")?;
    let path = blocked.join("rollout.jsonl");
    let item = RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent { message: "retained evidence".into(), phase: None }));
    let charge = rollout_admission_bytes(std::slice::from_ref(&item))? as usize;
    let mut task = RolloutWriterTask::new();
    task.pending_bytes = Arc::new(tokio::sync::Semaphore::new(charge * 2));
    let task = Arc::new(task);
    let (tx, rx) = mpsc::channel(4);
    let owner = Arc::clone(&task);
    let writer_path = path.clone();
    let cwd = home.path().to_path_buf();
    task.set_handle(tokio::spawn(async move {
        if let Err(error) = rollout_writer(None, None, rx, None, cwd, Some(None), writer_path, Default::default(), Arc::clone(&owner)).await {
            owner.mark_failed(&error);
        }
    }));
    let recorder = RolloutRecorder { tx, writer_task: Arc::clone(&task), rollout_path: path.clone() };
    recorder.record_canonical_items_ordered(std::slice::from_ref(&item)).await?;
    assert!(recorder.flush().await.is_err(), "durability failure must be observable");
    // Repair storage, but additions must not each restart the failed episode.
    fs::remove_file(&blocked)?;
    fs::create_dir(&blocked)?;
    recorder.record_canonical_items_ordered(std::slice::from_ref(&item)).await?;
    for _ in 0..200 {
        assert!(recorder.record_canonical_items_ordered(std::slice::from_ref(&item)).await.unwrap_err().to_string().contains("no items accepted"));
    }
    assert!(!path.exists(), "ordinary additions must not retry degraded I/O");
    assert_eq!(task.pending_bytes.available_permits(), 0);
    recorder.flush().await?;
    assert_eq!(task.pending_bytes.available_permits(), charge * 2);
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await?;
    assert_eq!(errors, 0);
    assert_eq!(serde_json::to_value(items)?, serde_json::to_value(vec![item.clone(), item])?);
    recorder.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resume_reuses_canonical_load_dictionary_for_subsequent_appends() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).unwrap();
    let path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid)?;
    let manifest = RolloutItem::ToolManifest(ToolManifestItem::full("resume-shared".into(), serde_json::json!({"tools": []})));
    append_rollout_item_to_path(&path, &manifest).await?;
    let (expected, _, _) = RolloutRecorder::load_rollout_items(&path).await?;
    let (recorder, history) = RolloutRecorder::resume_and_load(&test_config(home.path()), path.clone(), id).await?;
    assert_eq!(serde_json::to_value(&history)?, serde_json::to_value(expected)?);
    recorder.record_canonical_items_ordered(&[manifest]).await?;
    recorder.shutdown().await?;
    let (items, _, _) = RolloutRecorder::load_rollout_items(&path).await?;
    let RolloutItem::ToolManifest(last) = items.last().unwrap() else { panic!("manifest") };
    assert!(last.is_reference());
    assert_eq!(serde_json::to_value(&items[..history.len()])?, serde_json::to_value(&history)?);
    Ok(())
}

fn test_config(codex_home: &Path) -> RolloutConfig {
    RolloutConfig {
        codex_home: codex_home.to_path_buf(),
        sqlite_home: codex_home.to_path_buf(),
        cwd: codex_home.to_path_buf(),
        model_provider_id: "test-provider".to_string(),
    }
}

fn write_session_file(root: &Path, ts: &str, uuid: Uuid) -> std::io::Result<PathBuf> {
    let day_dir = root.join("sessions/2025/01/03");
    fs::create_dir_all(&day_dir)?;
    let path = day_dir.join(format!("rollout-{ts}-{uuid}.jsonl"));
    let mut file = File::create(&path)?;
    let meta = serde_json::json!({
        "timestamp": ts,
        "type": "session_meta",
        "payload": {
            "session_id": uuid,
            "id": uuid,
            "timestamp": ts,
            "cwd": ".",
            "originator": "test_originator",
            "cli_version": "test_version",
            "source": "cli",
            "model_provider": "test-provider",
        },
    });
    writeln!(file, "{meta}")?;
    let user_event = serde_json::json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": "Hello from user",
            "kind": "plain",
        },
    });
    writeln!(file, "{user_event}")?;
    Ok(path)
}

#[test]
fn db_hits_already_reconciled_from_filesystem_are_not_reconciled_again() {
    let overlapping_id = ThreadId::new();
    let db_only_id = ThreadId::new();
    let filesystem_ids = HashSet::from([overlapping_id]);

    assert!(!RolloutRecorder::db_hit_needs_reconciliation(
        &filesystem_ids,
        overlapping_id
    ));
    assert!(RolloutRecorder::db_hit_needs_reconciliation(
        &filesystem_ids,
        db_only_id
    ));
}

#[tokio::test]
async fn filesystem_search_matches_preview_or_title_in_both_sort_orders() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let first = Uuid::from_u128(30_001);
    let second = Uuid::from_u128(30_002);
    write_session_file(home.path(), "2025-01-03T12-00-00", first).unwrap();
    write_session_file(home.path(), "2025-01-03T12-01-00", second).unwrap();
    let first_id = ThreadId::from_string(&first.to_string()).unwrap();
    let second_id = ThreadId::from_string(&second.to_string()).unwrap();
    crate::append_thread_name(home.path(), first_id, "Chosen title")
        .await
        .unwrap();

    for direction in [SortDirection::Asc, SortDirection::Desc] {
        for (term, expected) in [
            (
                "Hello",
                if direction == SortDirection::Asc {
                    vec![first_id, second_id]
                } else {
                    vec![second_id, first_id]
                },
            ),
            ("Chosen", vec![first_id]),
            ("absent", vec![]),
        ] {
            let mut cursor = None;
            let mut found = Vec::new();
            loop {
                let page = RolloutRecorder::list_threads(
                    None,
                    &config,
                    1,
                    cursor.as_ref(),
                    ThreadSortKey::CreatedAt,
                    direction,
                    &[],
                    None,
                    None,
                    &config.model_provider_id,
                    Some(term),
                )
                .await
                .expect("filesystem search");
                found.extend(page.items.into_iter().map(|item| item.thread_id.unwrap()));
                assert!(
                    found.len() <= expected.len(),
                    "search must not repeat pages"
                );
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(found, expected, "search term: {term}");
        }
    }
}

#[tokio::test]
async fn ascending_filesystem_search_continues_after_a_nonmatching_page() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    for index in 0..257 {
        let timestamp = format!("2025-01-03T12-{:02}-{:02}", index / 60, index % 60);
        write_session_file(home.path(), &timestamp, Uuid::from_u128(40_000 + index)).unwrap();
    }
    let last_id = ThreadId::from_string(&Uuid::from_u128(40_256).to_string()).unwrap();
    crate::append_thread_name(home.path(), last_id, "Late match")
        .await
        .unwrap();
    let page = RolloutRecorder::list_threads(
        None,
        &config,
        1,
        None,
        ThreadSortKey::CreatedAt,
        SortDirection::Asc,
        &[],
        None,
        None,
        &config.model_provider_id,
        Some("Late match"),
    )
    .await
    .expect("search beyond the first scan page");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].thread_id, Some(last_id));
    assert!(page.next_cursor.is_none());
}

struct EventCountingSubscriber(Arc<AtomicUsize>);

impl tracing::Subscriber for EventCountingSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, _event: &tracing::Event<'_>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

#[test]
fn thread_list_db_fallback_emits_one_diagnostic() {
    let events = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(EventCountingSubscriber(Arc::clone(&events)), || {
        warn_thread_list_db_fallback();
    });

    assert_eq!(events.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ascending_thread_listing_scans_each_rollout_once_per_page() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    const ROLLOUT_COUNT: usize = 260;
    for index in 0..ROLLOUT_COUNT {
        let timestamp = format!("2025-01-{:02}T12-00-{:02}", index / 60 + 1, index % 60);
        write_session_file(
            home.path(),
            &timestamp,
            Uuid::from_u128(10_000 + index as u128),
        )?;
    }

    let first = RolloutRecorder::list_threads(
        /*state_db_ctx*/ None,
        &config,
        /*page_size*/ 1,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Asc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert!(first.num_scanned_files <= 2);
    let cursor = first.next_cursor.as_ref().expect("second page cursor");

    let second = RolloutRecorder::list_threads(
        /*state_db_ctx*/ None,
        &config,
        /*page_size*/ 1,
        Some(cursor),
        ThreadSortKey::CreatedAt,
        SortDirection::Asc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert!(second.num_scanned_files <= 2);
    assert_eq!(
        first.items[0].thread_id.map(|id| id.to_string()),
        Some(Uuid::from_u128(10_000).to_string())
    );
    assert_eq!(
        second.items[0].thread_id.map(|id| id.to_string()),
        Some(Uuid::from_u128(10_001).to_string())
    );
    Ok(())
}

#[test]
fn append_repair_terminates_nonempty_rollout_tail() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    fs::write(&rollout_path, b"{\"type\":\"event_msg\"}")?;
    drop(open_log_file(&rollout_path)?);
    drop(open_log_file(&rollout_path)?);

    assert_eq!(fs::read(&rollout_path)?, b"{\"type\":\"event_msg\"}\n");
    Ok(())
}

#[tokio::test]
async fn state_db_init_backfills_before_returning() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = home.path().join(format!(
        "sessions/2026/01/27/rollout-2026-01-27T12-34-56-{uuid}.jsonl"
    ));
    let parent = rollout_path
        .parent()
        .expect("rollout path should have parent");
    fs::create_dir_all(parent)?;

    let session_meta_line = SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            cwd: home.path().to_path_buf(),
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
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            item: RolloutItem::SessionMeta(session_meta_line),
        },
        RolloutLine {
            timestamp: "2026-01-27T12:34:57Z".to_string(),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: "hello from startup backfill".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            })),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(&rollout_path, format!("{jsonl}\n"))?;

    let runtime = crate::state_integration::init(&test_config(home.path()))
        .await
        .expect("state db should initialize");

    let metadata = runtime
        .get_thread(thread_id)
        .await?
        .expect("thread should be backfilled before init returns");
    assert_eq!(metadata.rollout_path, rollout_path);
    assert_eq!(
        runtime.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Complete
    );

    Ok(())
}

#[tokio::test]
async fn load_rollout_items_defaults_legacy_session_id() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "format_version": 0,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": {
                "type": "ghost_snapshot",
                "ghost_commit": {
                    "id": "deadbeef",
                    "preexisting_untracked_dirs": [],
                    "preexisting_untracked_files": [],
                },
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": "hello",
                    }
                ],
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::SessionMeta(session_meta) = &items[0] else {
        panic!("expected session metadata");
    };
    assert_eq!(session_meta.meta.session_id, SessionId::from(thread_id));
    assert!(session_meta.meta.harness_build.is_none(), "legacy rollouts remain readable");
    assert!(matches!(
        items[1],
        RolloutItem::ResponseItem(ResponseItem::Message { .. })
    ));

    Ok(())
}

#[tokio::test]
async fn for_each_rollout_item_streams_items_and_reports_identity() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let rollout_path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid)?;
    let mut visited = Vec::new();

    let (loaded_thread_id, parse_errors) =
        RolloutRecorder::for_each_rollout_item(&rollout_path, |item| visited.push(item)).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    let expected = recorded_lines(&rollout_path)?
        .into_iter()
        .map(|line| line.item)
        .collect::<Vec<_>>();
    assert_eq!(serde_json::to_value(visited)?, serde_json::to_value(expected)?);
    Ok(())
}

#[tokio::test]
async fn resume_skips_invalid_utf8_without_fabricating_or_losing_history() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::new_v4();
    let path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid)?;
    let (mut expected, thread_id, _) = RolloutRecorder::load_rollout_items(&path).await?;
    let mut file = fs::OpenOptions::new().append(true).open(&path)?;
    // This complete JSON record becomes valid only if its corrupt byte is replaced lossily.
    file.write_all(b"{\"timestamp\":\"2025-01-03T12:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"bad\xff\",\"kind\":\"plain\"}}\n")?;
    // A crash inside a UTF-8 character must not prevent a later append and resume.
    file.write_all(b"{\"torn\":\"\xc3")?;
    drop(file);
    let retained = RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: "completed evidence survives: \u{fffd}".into(),
        ..Default::default()
    }));
    append_rollout_item_to_path(&path, &retained).await?;
    expected.push(retained);
    let bytes = fs::read(&path)?;
    let compressed_path = compression::compressed_rollout_path(&path);
    fs::write(
        &compressed_path,
        zstd::stream::encode_all(bytes.as_slice(), 0)?,
    )?;

    for compressed in [false, true] {
        if compressed {
            // Force the real compressed reader, which prefers the plain sibling when present.
            fs::rename(&path, home.path().join("original.jsonl"))?;
        }
        let (items, loaded_id, parse_errors) = RolloutRecorder::load_rollout_items(&path).await?;
        assert_eq!(parse_errors, 2);
        assert_eq!(loaded_id, thread_id);
        assert_eq!(
            serde_json::to_value(&items[..expected.len()])?,
            serde_json::to_value(&expected)?
        );
        assert_eq!(items.len(), expected.len() + 1);
        assert_reconstruction_gap(items.last().unwrap(), 2);
        let InitialHistory::Resumed(history) = RolloutRecorder::get_rollout_history(&path).await?
        else {
            panic!("expected resumed history");
        };
        assert_eq!(
            serde_json::to_value(history.history.as_ref())?,
            serde_json::to_value(&items)?
        );
    }
    assert_eq!(fs::read(home.path().join("original.jsonl"))?, bytes);
    Ok(())
}

fn assert_reconstruction_gap(item: &RolloutItem, count: usize) {
    let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = item else {
        panic!("expected a model-visible reconstruction gap");
    };
    assert_eq!(role, "developer");
    let [codex_protocol::models::ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected bounded gap text");
    };
    assert!(text.starts_with("<rollout_reconstruction_gap>"));
    assert!(text.contains(&format!("{count} malformed rollout records")));
    assert!(text.len() < 1024);
}

#[tokio::test]
async fn resume_integrity_distinguishes_interior_corruption_from_partial_tail() -> std::io::Result<()> {
    for compressed in [false, true] {
        for partial in [false, true] {
            let home = TempDir::new()?;
            let mut path = write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::new_v4())?;
            let mut file = fs::OpenOptions::new().append(true).open(&path)?;
            if partial {
                write!(file, "{{\"timestamp\":")?;
            } else {
                writeln!(file, "corrupt interior result")?;
            }
            drop(file);
            if !partial {
                append_rollout_item_to_path(&path, &RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                    codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
                ))).await?;
            }
            if compressed {
                let bytes = fs::read(&path)?;
                path = path.with_extension("jsonl.zst");
                fs::write(&path, zstd::stream::encode_all(bytes.as_slice(), 0)?)?;
                fs::remove_file(path.with_extension(""))?;
            }
            let InitialHistory::Resumed(history) = RolloutRecorder::get_rollout_history(&path).await? else {
                panic!("expected resume");
            };
            assert_reconstruction_gap(history.history.last().unwrap(), 1);
            let notice = serde_json::to_string(history.history.last().unwrap())?;
            assert!(notice.contains(if partial {
                "malformed_records=0, trailing_partial_records=1, complete=false"
            } else {
                "malformed_records=1, trailing_partial_records=0, complete=false"
            }));
            let (recorder, resumed) = RolloutRecorder::resume_and_load(
                &test_config(home.path()), path, history.conversation_id,
            ).await?;
            assert_reconstruction_gap(resumed.last().unwrap(), 1);
            recorder.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn malformed_middle_records_remain_visible_after_replacement_history() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let path = write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::new_v4())?;
    let mut file = fs::OpenOptions::new().append(true).open(&path)?;
    for _ in 0..100 {
        writeln!(file, "malformed middle record")?;
    }
    drop(file);
    let replacement = RolloutItem::Compacted(codex_protocol::protocol::CompactedItem {
        message: "surviving checkpoint".into(),
        replacement_history: Some(Vec::new()),
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    });
    append_rollout_item_to_path(&path, &replacement).await?;
    let original = fs::read(&path)?;
    for _ in 0..2 {
        let InitialHistory::Resumed(history) = RolloutRecorder::get_rollout_history(&path).await? else {
            panic!("expected resumed history");
        };
        assert_eq!(history.history.len(), 4);
        assert!(matches!(history.history[2], RolloutItem::Compacted(_)));
        assert_reconstruction_gap(history.history.last().unwrap(), 100);
    }
    assert_eq!(fs::read(&path)?, original);
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_ignores_unknown_fork_source_history_mode() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::new_v4();
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let rollout_path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid)?;
    let mut file = fs::OpenOptions::new().append(true).open(&rollout_path)?;
    let source_uuid = Uuid::new_v4();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": "2025-01-03T12:00:01Z",
            "type": "session_meta",
            "payload": {
                "session_id": source_uuid,
                "id": source_uuid,
                "timestamp": "2025-01-03T12:00:01Z",
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
                "history_mode": "future",
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 1);
    assert_eq!(items.len(), 3);
    assert_reconstruction_gap(items.last().unwrap(), 1);
    Ok(())
}

#[tokio::test]
async fn load_rollout_items_filters_legacy_ghost_snapshots_from_compaction_history()
-> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut file = File::create(&rollout_path)?;
    let thread_id = ThreadId::new();
    let ts = "2025-01-03T12:00:00Z";

    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "type": "session_meta",
            "payload": {
                "session_id": thread_id,
                "id": thread_id,
                "timestamp": ts,
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "source": "cli",
                "model_provider": "test-provider",
            },
        })
    )?;
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "timestamp": ts,
            "format_version": 0,
            "type": "compacted",
            "payload": {
                "message": "summary",
                "replacement_history": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "kept",
                            }
                        ],
                    },
                    {
                        "type": "ghost_snapshot",
                        "ghost_commit": {
                            "id": "deadbeef",
                            "preexisting_untracked_dirs": [],
                            "preexisting_untracked_files": [],
                        },
                    }
                ],
            },
        })
    )?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    let RolloutItem::Compacted(compacted) = &items[1] else {
        panic!("expected compacted rollout item");
    };
    let replacement_history = compacted
        .replacement_history
        .as_ref()
        .expect("replacement history");
    assert_eq!(replacement_history.len(), 1);
    assert!(matches!(
        &replacement_history[0],
        ResponseItem::Message { .. }
    ));

    Ok(())
}

#[tokio::test]
async fn recorder_persists_every_sampling_request_token_count() -> std::io::Result<()> {
    for barrier in ["flush", "persist", "shutdown"] {
        let home = TempDir::new()?;
        let recorder = RolloutRecorder::new(
            &test_config(home.path()),
            RolloutRecorderParams::new(
                ThreadId::new(),
                None,
                None,
                SessionSource::Exec,
                None,
                "barrier-test".to_string(),
                BaseInstructions::default(),
                Vec::new(),
            )
            .with_history_mode(ThreadHistoryMode::Paginated),
        )
        .await?;
        recorder.persist().await?;
        let rollout_path = recorder.rollout_path().to_path_buf();
        let token_count = |count| {
            RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                info: Some(codex_protocol::protocol::TokenUsageInfo {
                    total_token_usage: codex_protocol::protocol::TokenUsage {
                        total_tokens: count,
                        ..Default::default()
                    },
                    last_token_usage: Default::default(),
                    model_context_window: None,
                }),
                rate_limits: None,
            }))
        };
        recorder
            .record_canonical_items(&[token_count(10), token_count(20)])
            .await?;
        // The explicit barrier below fences both prior writes and this marker.
        recorder
            .record_canonical_items_ordered(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
                AgentMessageEvent {
                    message: "accepted-marker".into(),
                    phase: None,
                },
            ))])
            .await?;
        match barrier {
            "flush" => recorder.flush().await?,
            "persist" => recorder.persist().await?,
            "shutdown" => recorder.shutdown().await?,
            _ => unreachable!(),
        }
        let contents = fs::read_to_string(&rollout_path)?;
        let records = recorded_lines(&rollout_path)?;
        let counts = records
            .iter()
            .filter_map(|line| match &line.item {
                RolloutItem::EventMsg(EventMsg::TokenCount(event)) => event
                    .info
                    .as_ref()
                    .map(|info| info.total_token_usage.total_tokens),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            counts,
            vec![10, 20],
            "every sampling request's usage is retained, not just the last (barrier: {barrier})"
        );
        assert!(
            contents.contains("accepted-marker"),
            "barrier persists all queued records"
        );
        if barrier != "shutdown" {
            recorder.flush().await?;
            assert_eq!(
                fs::read_to_string(&rollout_path)?,
                contents,
                "repeated barrier must not duplicate records"
            );
            recorder.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn recorder_last_handle_drop_drains_accepted_deferred_records() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let recorder = RolloutRecorder::new(
        &test_config(home.path()),
        RolloutRecorderParams::new(
            ThreadId::new(),
            None,
            None,
            SessionSource::Exec,
            None,
            "drop-test".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        )
        .with_history_mode(ThreadHistoryMode::Paginated),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();
    recorder
        .record_canonical_items_ordered(&[
            RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
                message: "accepted-before-last-drop".into(),
                phase: None,
            })),
            RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                info: None,
                rate_limits: None,
            })),
        ])
        .await?;
    assert!(
        !rollout_path.exists(),
        "records remain deferred before the last handle drops"
    );
    let writer = recorder
        .writer_task
        .handle
        .lock()
        .expect("writer handle")
        .take()
        .expect("running writer");
    drop(recorder);
    tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .expect("writer drains after channel closure")
        .expect("writer task");
    let records = recorded_lines(&rollout_path)?;
    assert_eq!(records.iter().filter(|line| matches!(&line.item,
        RolloutItem::EventMsg(EventMsg::AgentMessage(event)) if event.message == "accepted-before-last-drop"
    )).count(), 1);
    assert_eq!(
        records
            .iter()
            .filter(|line| matches!(&line.item, RolloutItem::EventMsg(EventMsg::TokenCount(_))))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn recorder_materializes_on_flush_with_pending_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let session_id = SessionId::default();
    let thread_id = ThreadId::new();
    let initial_window_id = Uuid::now_v7().to_string();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            thread_id,
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        )
        .with_session_id(session_id)
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_initial_window_id(initial_window_id.clone()),
    )
    .await?;

    let rollout_path = recorder.rollout_path().to_path_buf();
    assert!(
        !rollout_path.exists(),
        "rollout file should not exist before the first recordable item"
    );

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
            AgentMessageEvent {
                message: "buffered-event".to_string(),
                phase: None,
            },
        ))])
        .await?;
    recorder.flush().await?;
    assert!(
        rollout_path.exists(),
        "flush with pending items should materialize the rollout"
    );

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(
            UserMessageEvent {
                client_id: None,
                message: "first-user-message".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        ))])
        .await?;
    // The terminal-checkpoint barrier flushes like `flush()` and then syncs the file.
    recorder.flush_durable().await?;

    recorder.persist().await?;
    // Second call verifies `persist()` is idempotent after materialization.
    recorder.persist().await?;
    assert!(rollout_path.exists(), "rollout file should be materialized");

    let text = std::fs::read_to_string(&rollout_path)?;
    let first_line = text.lines().next().expect("session metadata line");
    let first_value: serde_json::Value = serde_json::from_str(first_line)?;
    assert_eq!(
        first_value["format_version"],
        serde_json::json!(codex_protocol::protocol::CURRENT_ROLLOUT_FORMAT_VERSION)
    );
    let session_meta: RolloutLine = serde_json::from_str(
        &crate::payload_artifact::hydrate_line(&rollout_path, first_line.to_owned())?,
    )?;
    let RolloutItem::SessionMeta(session_meta) = session_meta.item else {
        panic!("expected session metadata in rollout");
    };
    assert_eq!(session_meta.meta.session_id, session_id);
    let build = session_meta.meta.harness_build.as_ref().expect("harness provenance is persisted");
    let embedded = codex_utils_build_info::BuildInfo::current();
    assert_eq!(build.version, embedded.version);
    assert_eq!(build.commit, embedded.commit);
    assert_eq!(build.dirty, embedded.dirty);
    assert_eq!(build.profile, embedded.profile);
    assert_eq!(build.built, embedded.built);
    assert_eq!(build.token_accounting["local_input_estimate"].estimator, "utf8_bytes_div4_ceil_v1");
    assert_eq!(build.token_accounting["projection_text_budget"].estimator, "o200k_base_count_ordinary_v1");
    assert_ne!(
        build.token_accounting["canonical_tool_tokens"].scope,
        build.token_accounting["model_facing_tool_tokens"].scope,
        "different serialized representations must not be labeled the same accounting scope",
    );
    assert_eq!(build.token_accounting["provider_input_tokens"].estimator, "provider_reported_tokenizer_unknown");
    assert_eq!(
        build.executable_sha256.as_deref(),
        codex_utils_build_info::executable_sha256(),
        "the binary hash distinguishes dirty builds of one commit"
    );
    assert_eq!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(
        session_meta
            .meta
            .context_window
            .map(|window| window.window_id),
        Some(initial_window_id)
    );
    let buffered_idx = text
        .find("buffered-event")
        .expect("buffered event in rollout");
    let user_idx = text
        .find("first-user-message")
        .expect("first user message in rollout");
    assert!(
        buffered_idx < user_idx,
        "buffered items should preserve ordering"
    );
    let text_after_second_persist = std::fs::read_to_string(&rollout_path)?;
    assert_eq!(text_after_second_persist, text);

    recorder.shutdown().await?;
    assert!(recorder.flush().await.is_err());
    assert!(recorder.flush_durable().await.is_err());
    assert!(recorder.record_canonical_items(&[]).await.is_err());
    assert!(recorder.shutdown().await.is_err());
    Ok(())
}

#[tokio::test]
async fn concurrent_shutdown_never_acknowledges_items_behind_shutdown() -> std::io::Result<()> {
    const WRITERS: usize = 32;
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            None,
            None,
            SessionSource::Exec,
            None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();
    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
            AgentMessageEvent {
                message: "before-concurrent-shutdown".to_string(),
                phase: None,
            },
        ))])
        .await?;
    recorder.flush().await?;
    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let mut writers = Vec::new();
    for index in 0..WRITERS {
        let writer = recorder.clone();
        let barrier = Arc::clone(&barrier);
        writers.push(tokio::spawn(async move {
            let message = format!("concurrent-item-{index:03}");
            barrier.wait().await;
            let result = writer
                .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
                    AgentMessageEvent {
                        message: message.clone(),
                        phase: None,
                    },
                ))])
                .await;
            (message, result)
        }));
    }
    let shutdown_recorder = recorder.clone();
    let shutdown_barrier = Arc::clone(&barrier);
    let shutdown = tokio::spawn(async move {
        shutdown_barrier.wait().await;
        shutdown_recorder.shutdown().await
    });

    let mut accepted = Vec::new();
    for writer in writers {
        let (message, result) = writer.await.expect("writer task");
        if result.is_ok() {
            accepted.push(message);
        }
    }
    shutdown.await.expect("shutdown task")?;
    accepted.push("before-concurrent-shutdown".to_string());
    accepted.sort();
    let mut persisted = recorded_lines(&rollout_path)?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::EventMsg(EventMsg::AgentMessage(event)) => Some(event.message),
            _ => None,
        })
        .collect::<Vec<_>>();
    persisted.sort();
    assert_eq!(persisted, accepted, "every accepted item exactly once, no rejected items");
    Ok(())
}

#[tokio::test]
async fn persist_reports_filesystem_error_and_retries_buffered_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let thread_id = ThreadId::new();
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            thread_id,
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();

    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::AgentMessage(
            AgentMessageEvent {
                message: "buffered-before-persist".to_string(),
                phase: None,
            },
        ))])
        .await?;
    let sessions_blocker_path = home.path().join("sessions");
    File::create(&sessions_blocker_path)?;

    let err = recorder
        .persist()
        .await
        .expect_err("blocked sessions directory should fail persist");
    assert_ne!(err.kind(), std::io::ErrorKind::Interrupted);
    assert!(
        !rollout_path.exists(),
        "failed persist should keep the rollout deferred"
    );

    fs::remove_file(sessions_blocker_path)?;
    recorder.flush().await?;
    let text = std::fs::read_to_string(&rollout_path)?;
    assert!(
        text.contains("buffered-before-persist"),
        "retry should preserve items buffered before the failed persist"
    );

    recorder.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn empty_writer_flush_does_not_wait_for_another_append() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("rollout.jsonl");
    let mut state = RolloutWriterState::new(
        Some(open_log_file(&path)?.into_jsonl_writer()),
        None,
        None,
        home.path().to_path_buf(),
        Some(None),
        path.clone(),
        Default::default(),
    );
    state.add_items(captured(vec![agent_message("already-flushed")]));
    state.flush().await?;
    let persisted = fs::read(&path)?;

    // Report repeated empty-barrier cost without a timing-dependent assertion.
    let mut samples = Vec::new();
    for _ in 0..5 {
        let started = std::time::Instant::now();
        for _ in 0..20 {
            state.flush().await?;
        }
        samples.push(started.elapsed().as_nanos() / 20);
    }
    eprintln!("empty_flush_ns_per_call={samples:?}");

    let write_lock = compression::lock_rollout_for_write_blocking(&path)?;
    let outcome = {
        let mut flush = Box::pin(state.flush());
        futures::poll!(flush.as_mut())
    };
    // Release even on the old implementation, whose blocking task is still waiting.
    drop(write_lock);
    assert!(
        matches!(outcome, std::task::Poll::Ready(Ok(()))),
        "an empty flush must not schedule or wait for an append transaction: {outcome:?}"
    );
    assert_eq!(fs::read(&path)?, persisted);

    state.retry_blocked_error = Some("unconfirmed append".to_string());
    let error = state.flush().await.expect_err("empty barriers cannot hide errors");
    assert!(error.to_string().contains("unconfirmed append"));
    Ok(())
}



#[test]
fn writer_state_defines_manifests_once_then_references_them() {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let mut tool_manifests = ToolManifestDictionary::default();
    tool_manifests
        .apply(&ToolManifestItem::full(
            "already-persisted".to_string(),
            serde_json::json!({"hash": "already-persisted"}),
        ))
        .expect("seed persisted manifest");
    let mut state = RolloutWriterState::new(
        /*writer*/ None,
        /*deferred_log_file_info*/ None,
        /*meta*/ None,
        home.path().to_path_buf(),
        /*known_repository_context*/ None,
        rollout_path,
        tool_manifests,
    );
    let manifest = |hash: &str| {
        RolloutItem::ToolManifest(ToolManifestItem::full(
            hash.to_string(),
            serde_json::json!({"hash": hash}),
        ))
    };
    let token_count = || {
        RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
            info: None,
            rate_limits: None,
        }))
    };
    let boundary = RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "turn-2".to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }));

    state.add_items(captured(vec![
        manifest("already-persisted"),
        manifest("new"),
        manifest("new"),
        token_count(),
        token_count(),
        boundary,
    ]));

    assert_eq!(state.pending_items.len(), 6);
    assert!(matches!(
        &state.pending_items[0].item,
        RolloutItem::ToolManifest(item) if item.hash == "already-persisted" && item.is_reference()
    ));
    assert!(matches!(
        &state.pending_items[1].item,
        RolloutItem::ToolManifest(item) if item.hash == "new" && item.manifest.is_some()
    ));
    assert!(matches!(
        &state.pending_items[2].item,
        RolloutItem::ToolManifest(item) if item.hash == "new" && item.is_reference()
    ));
    // Both counts survive in order: manifest de-duplication must not be
    // mistaken for a licence to drop per-request usage.
    assert!(matches!(
        &state.pending_items[3].item,
        RolloutItem::EventMsg(EventMsg::TokenCount(_))
    ));
    assert!(matches!(
        &state.pending_items[4].item,
        RolloutItem::EventMsg(EventMsg::TokenCount(_))
    ));
    assert!(matches!(
        &state.pending_items[5].item,
        RolloutItem::EventMsg(EventMsg::TurnStarted(_))
    ));
}

#[test]
fn identical_settings_are_omitted_but_transitions_are_preserved() {
    let home = TempDir::new().unwrap();
    let mut state = RolloutWriterState::new(
        None, None, None, home.path().to_path_buf(), None,
        home.path().join("rollout.jsonl"), ToolManifestDictionary::default(),
    );
    let settings = |tier: &str| {
        RolloutItem::EventMsg(serde_json::from_value(serde_json::json!({
            "type": "thread_settings_applied",
            "thread_settings": {
                "model": "test-model", "model_provider_id": "test-provider",
                "service_tier": tier, "developer_instructions": "shared instructions",
                "approval_policy": "never", "permission_profile": {"type": "disabled"},
                "cwd": home.path(),
                "collaboration_mode": {"mode": "default", "settings": {
                    "model": "test-model", "reasoning_effort": null,
                    "developer_instructions": null
                }}
            }
        })).unwrap())
    };
    state.add_items(captured(vec![settings("default"), settings("default")]));
    state.add_items(captured(vec![
        settings("default"), settings("priority"), settings("priority"), settings("default"),
    ]));
    assert_eq!(state.pending_items.len(), 3);
    let tiers: Vec<_> = state.pending_items.iter().map(|item| {
        serde_json::to_value(&item.item).unwrap()["payload"]["thread_settings"]["service_tier"].clone()
    }).collect();
    assert_eq!(tiers, vec!["default", "priority", "default"]);
    for item in &state.pending_items[1..] {
        let value = serde_json::to_value(&item.item).unwrap();
        assert!(value["payload"]["thread_settings"].get("developer_instructions").is_none());
    }
    let persisted: Vec<_> = state.pending_items.iter().map(|item| item.item.clone()).collect();
    let restored = codex_protocol::persisted_thread_settings::reduce_persisted_thread_settings(
        &persisted, Default::default(),
    );
    assert_eq!(restored.developer_instructions, Some(Some("shared instructions".into())));
    let mut cleared = settings("default");
    if let RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) = &mut cleared {
        event.thread_settings.developer_instructions = Some(None);
    }
    state.add_items(captured(vec![cleared]));
    let persisted: Vec<_> = state.pending_items.iter().map(|item| item.item.clone()).collect();
    let restored = codex_protocol::persisted_thread_settings::reduce_persisted_thread_settings(
        &persisted, Default::default(),
    );
    assert_eq!(restored.developer_instructions, Some(None));
}

#[test]
fn canonical_messages_omit_only_their_immediate_legacy_mirrors() {
    let home = TempDir::new().unwrap();
    let mut state = RolloutWriterState::new(
        None, None, None, home.path().to_path_buf(), None,
        home.path().join("rollout.jsonl"), ToolManifestDictionary::default(),
    );
    let mirror = || RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: "same text".into(), phase: None,
    }));
    for id in ["first", "second"] {
        state.add_items(captured(vec![RolloutItem::EventMsg(EventMsg::ItemCompleted(
            codex_protocol::protocol::ItemCompletedEvent {
                thread_id: ThreadId::new(),
                turn_id: "turn".into(),
                completed_at_ms: 0,
                item: codex_protocol::items::TurnItem::AgentMessage(
                    codex_protocol::items::AgentMessageItem {
                        id: id.into(),
                        content: vec![codex_protocol::items::AgentMessageContent::Text {
                            text: "same text".into(),
                        }],
                        phase: None,
                    },
                ),
            },
        ))]));
        // Core can enqueue the live legacy alias in a separate recorder call.
        state.add_items(captured(vec![mirror()]));
    }
    state.add_items(captured(vec![mirror()]));
    assert_eq!(state.pending_items.len(), 3);
    for (item, id) in state.pending_items.iter().zip(["first", "second"]) {
        assert!(matches!(&item.item, RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
            if event.item.id() == id));
    }
    assert!(matches!(&state.pending_items[2].item, RolloutItem::EventMsg(EventMsg::AgentMessage(_))));
}

#[tokio::test]
async fn replay_reconstructs_full_tool_surface_from_compact_references() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let writer = open_log_file(&rollout_path)?.into_jsonl_writer();
    let mut state = RolloutWriterState::new(
        Some(writer),
        /*deferred_log_file_info*/ None,
        /*meta*/ None,
        home.path().to_path_buf(),
        /*known_repository_context*/ None,
        rollout_path.clone(),
        ToolManifestDictionary::default(),
    );
    let surface = serde_json::json!({
        "model_visible": [
            {"type": "function", "name": "shell", "schema": {"type": "object"}},
            {"type": "function", "name": "read", "schema": {"type": "object"}}
        ],
        "registered": [
            {"name": "shell", "exposure": "direct", "activated": true},
            {"name": "read", "exposure": "direct", "activated": true}
        ]
    });
    let hash = "stable-surface";

    state.add_items(captured(vec![
        RolloutItem::ToolManifest(ToolManifestItem::full(hash.to_string(), surface.clone())),
        RolloutItem::ToolManifest(ToolManifestItem::reference(hash.to_string())),
        RolloutItem::ToolManifest(ToolManifestItem::reference(hash.to_string())),
    ]));
    state.flush().await?;

    let persisted = fs::read_to_string(&rollout_path)?
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?;
    let persisted_manifests = persisted
        .iter()
        .filter_map(|line| match &line.item {
            RolloutItem::ToolManifest(manifest) => Some(manifest),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(persisted_manifests.len(), 3);
    assert_eq!(persisted_manifests[0].manifest.as_ref(), Some(&surface));
    assert!(persisted_manifests[1].is_reference());
    assert!(persisted_manifests[2].is_reference());

    let replayed = RolloutRecorder::existing_tool_manifests(&rollout_path).await?;
    assert_eq!(replayed.manifest(hash), Some(&surface));
    assert_eq!(replayed.current_hash(), Some(hash));
    Ok(())
}

#[tokio::test]
async fn deferred_writer_reuses_existing_session_metadata_and_manifests() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::now_v7();
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let rollout_path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid)?;
    let persisted_manifest = RolloutLine {
        timestamp: "2025-01-03T12:00:01Z".to_string(),
        item: RolloutItem::ToolManifest(ToolManifestItem::full(
            "persisted".to_string(),
            serde_json::json!({"hash": "persisted"}),
        )),
    };
    let mut file = fs::OpenOptions::new().append(true).open(&rollout_path)?;
    writeln!(file, "{}", serde_json::to_string(&persisted_manifest)?)?;
    drop(file);

    let mut state = RolloutWriterState::new(
        /*writer*/ None,
        Some(LogFileInfo {
            path: rollout_path.clone(),
            conversation_id: thread_id,
            timestamp: OffsetDateTime::now_utc(),
        }),
        Some(SessionMeta {
            session_id: SessionId::from(thread_id),
            id: thread_id,
            ..SessionMeta::default()
        }),
        home.path().to_path_buf(),
        /*known_repository_context*/ None,
        rollout_path.clone(),
        ToolManifestDictionary::default(),
    );
    let manifest = |hash: &str| {
        RolloutItem::ToolManifest(ToolManifestItem::full(
            hash.to_string(),
            serde_json::json!({"hash": hash}),
        ))
    };
    state.add_items(captured(vec![manifest("persisted"), manifest("changed")]));

    state.flush().await?;

    let lines = fs::read_to_string(&rollout_path)?
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        lines
            .iter()
            .filter(|line| matches!(line.item, RolloutItem::SessionMeta(_)))
            .count(),
        1
    );
    let manifest_hashes = lines
        .iter()
        .filter_map(|line| match &line.item {
            RolloutItem::ToolManifest(manifest) => Some(manifest.hash.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(manifest_hashes, vec!["persisted", "persisted", "changed"]);
    Ok(())
}

fn agent_message(message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: message.to_string(),
        phase: None,
    }))
}

fn recorded_lines(path: &Path) -> std::io::Result<Vec<RolloutLine>> {
    fs::read_to_string(path)?
        .lines()
        .map(|line| {
            let line = crate::payload_artifact::hydrate_line(path, line.to_owned())?;
            serde_json::from_str::<RolloutLine>(&line).map_err(std::io::Error::from)
        })
        .collect()
}

fn parse_record_timestamp(timestamp: &str) -> OffsetDateTime {
    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    time::PrimitiveDateTime::parse(timestamp, &format)
        .unwrap_or_else(|err| {
            panic!("record timestamp {timestamp:?} is not in record format: {err}")
        })
        .assume_utc()
}

/// Records carry milliseconds, so an exact capture instant has to be compared
/// at the resolution the record format keeps.
fn at_record_resolution(instant: OffsetDateTime) -> OffsetDateTime {
    instant
        .replace_nanosecond(u32::from(instant.millisecond()) * 1_000_000)
        .expect("millisecond boundary is a valid nanosecond")
}

#[tokio::test]
async fn records_coalesced_into_one_flush_keep_their_own_capture_times() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();

    // Ordered records buffer until an explicit barrier, so both of these land
    // in the same flush even though they were produced far apart.
    let capture_gap = Duration::from_millis(120);
    recorder
        .record_canonical_items_ordered(&[agent_message("captured-first")])
        .await?;
    tokio::time::sleep(capture_gap).await;
    recorder
        .record_canonical_items_ordered(&[agent_message("captured-second")])
        .await?;
    recorder.flush().await?;

    let lines = recorded_lines(&rollout_path)?;
    assert!(matches!(lines[0].item, RolloutItem::SessionMeta(_)));
    assert!(parse_record_timestamp(&lines[0].timestamp) <= parse_record_timestamp(&lines[1].timestamp));
    let messages = lines
        .iter()
        .filter_map(|line| match &line.item {
            RolloutItem::EventMsg(EventMsg::AgentMessage(event)) => Some((
                event.message.clone(),
                parse_record_timestamp(&line.timestamp),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        messages
            .iter()
            .map(|(message, _)| message.as_str())
            .collect::<Vec<_>>(),
        vec!["captured-first", "captured-second"],
        "coalescing must not reorder records"
    );
    let observed_gap = messages[1].1 - messages[0].1;
    assert!(
        observed_gap >= capture_gap / 2,
        "records flushed together must keep their own capture times, but they are \
         {observed_gap:?} apart after a {capture_gap:?} gap between captures"
    );

    recorder.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rebased_records_keep_their_capture_time() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let uuid = Uuid::now_v7();
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let rollout_path = write_session_file(home.path(), "2025-01-03T12-00-00", uuid)?;
    let persisted_manifest = RolloutLine {
        timestamp: "2025-01-03T12:00:01Z".to_string(),
        item: RolloutItem::ToolManifest(ToolManifestItem::full(
            "persisted".to_string(),
            serde_json::json!({"hash": "persisted"}),
        )),
    };
    let mut file = fs::OpenOptions::new().append(true).open(&rollout_path)?;
    writeln!(file, "{}", serde_json::to_string(&persisted_manifest)?)?;
    drop(file);

    let mut state = RolloutWriterState::new(
        /*writer*/ None,
        Some(LogFileInfo {
            path: rollout_path.clone(),
            conversation_id: thread_id,
            timestamp: OffsetDateTime::now_utc(),
        }),
        Some(SessionMeta {
            session_id: SessionId::from(thread_id),
            id: thread_id,
            ..SessionMeta::default()
        }),
        home.path().to_path_buf(),
        /*known_repository_context*/ None,
        rollout_path.clone(),
        ToolManifestDictionary::default(),
    );
    // Rebasing rewrites this manifest against the persisted dictionary.
    let captured_at = OffsetDateTime::now_utc() - Duration::from_secs(3600);
    state.add_items(vec![CapturedRolloutItem::new(
        RolloutItem::ToolManifest(ToolManifestItem::full(
            "persisted".to_string(),
            serde_json::json!({"hash": "persisted"}),
        )),
        captured_at,
    )]);

    state.flush().await?;

    let rebased = recorded_lines(&rollout_path)?
        .into_iter()
        .filter(|line| matches!(&line.item, RolloutItem::ToolManifest(_)))
        .last()
        .expect("rebased manifest record");
    let RolloutItem::ToolManifest(manifest) = &rebased.item else {
        panic!("expected a tool manifest record");
    };
    assert!(
        manifest.is_reference(),
        "the record must actually have been rebased onto the persisted dictionary"
    );
    assert_eq!(
        parse_record_timestamp(&rebased.timestamp),
        at_record_resolution(captured_at),
        "a rebased record keeps the time it was captured, not the time it was written"
    );
    Ok(())
}

#[tokio::test]
async fn retried_records_keep_their_capture_time() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let read_only_file = std::fs::OpenOptions::new().read(true).open(&rollout_path)?;
    let (_, append_lock) = compression::lock_rollout_for_append_blocking(&rollout_path)?;
    let mut state = RolloutWriterState::new(
        Some(JsonlWriter {
            path: rollout_path.clone(),
            file: tokio::fs::File::from_std(read_only_file),
            _append_lock: append_lock,
            write_fault: None,
            append_transaction_count: 0,
        }),
        /*deferred_log_file_info*/ None,
        /*meta*/ None,
        home.path().to_path_buf(),
        /*known_repository_context*/ None,
        rollout_path.clone(),
        Default::default(),
    );
    let captured_at = OffsetDateTime::now_utc() - Duration::from_secs(3600);
    let messages = ["survives-the-retry", "queued-after-writer-error"];
    state.add_items(messages.into_iter().map(|message| {
        CapturedRolloutItem::new(agent_message(message), captured_at)
    }).collect());

    // The first append fails against the read-only handle; the retry reopens
    // and writes the same buffered record.
    state.flush().await?;

    let lines = recorded_lines(&rollout_path)?;
    assert_eq!(lines.len(), messages.len());
    for (line, message) in lines.iter().zip(messages) {
        assert_eq!(serde_json::to_value(&line.item)?, serde_json::to_value(agent_message(message))?);
        assert_eq!(
            parse_record_timestamp(&line.timestamp),
            at_record_resolution(captured_at),
            "a retried record keeps its capture time, not the retry time"
        );
    }
    assert!(state.pending_items.is_empty());
    Ok(())
}

#[tokio::test]
async fn existing_tool_manifests_skips_malformed_lines() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let manifest = RolloutLine {
        timestamp: "2026-08-06T00:00:00Z".to_string(),
        item: RolloutItem::ToolManifest(ToolManifestItem::full(
            "persisted".to_string(),
            serde_json::json!({"tools": []}),
        )),
    };
    fs::write(
        &rollout_path,
        format!("not-json\n{}\n", serde_json::to_string(&manifest)?),
    )?;

    let manifests = RolloutRecorder::existing_tool_manifests(&rollout_path).await?;

    assert_eq!(
        manifests.manifest("persisted"),
        Some(&serde_json::json!({"tools": []}))
    );
    Ok(())
}

async fn assert_failed_append_is_written_once(
    fault: JsonlWriteFault,
    message: &str,
) -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let mut writer = open_log_file(&rollout_path)?.into_jsonl_writer();
    writer.write_fault = Some(fault);
    let mut state = RolloutWriterState::new(
        Some(writer),
        /*deferred_log_file_info*/ None,
        /*meta*/ None,
        home.path().to_path_buf(),
        /*known_repository_context*/ None,
        rollout_path.clone(),
        Default::default(),
    );
    state.add_items(captured(vec![RolloutItem::EventMsg(
        EventMsg::AgentMessage(AgentMessageEvent {
            message: message.to_string(),
            phase: None,
        }),
    )]));

    state.flush().await?;

    let text = std::fs::read_to_string(&rollout_path)?;
    assert_eq!(
        text.lines().count(),
        1,
        "failed append recovery must leave exactly one complete record: {text:?}"
    );
    let line: RolloutLine = serde_json::from_str(text.trim_end())?;
    let RolloutItem::EventMsg(EventMsg::AgentMessage(event)) = line.item else {
        panic!("expected agent message rollout item");
    };
    assert_eq!(event.message, message);
    Ok(())
}

#[tokio::test]
async fn cancelled_direct_append_finishes_before_releasing_write_ownership() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("rollout.jsonl");
    File::create(&path)?;
    let lock = compression::lock_rollout_for_write_blocking(&path)?;
    let first = RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: "first".into(), ..Default::default()
    }));
    let mut append = Box::pin(append_rollout_item_to_path(&path, &first));
    assert!(std::future::poll_fn(|cx| std::task::Poll::Ready(
        std::future::Future::poll(append.as_mut(), cx)
    )).await.is_pending());
    drop(append);
    drop(lock);
    let second = RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: "second".into(), ..Default::default()
    }));
    append_rollout_item_to_path(&path, &second).await?;
    let text = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let text = tokio::fs::read_to_string(&path).await.unwrap();
            if text.lines().count() == 2 { break text; }
            tokio::task::yield_now().await;
        }
    }).await.expect("the detached append must finish");
    let mut messages = text.lines().map(|line| {
        let line: RolloutLine = serde_json::from_str(line).unwrap();
        match line.item {
            RolloutItem::EventMsg(EventMsg::UserMessage(event)) => event.message,
            _ => panic!("unexpected record"),
        }
    }).collect::<Vec<_>>();
    messages.sort();
    assert_eq!(messages, vec!["first", "second"]);
    Ok(())
}

#[tokio::test]
async fn writer_state_does_not_retry_an_ambiguously_flushed_record() -> std::io::Result<()> {
    assert_failed_append_is_written_once(JsonlWriteFault::Complete, "ambiguous-flush-record").await
}

#[tokio::test]
async fn writer_state_rolls_back_a_partial_record_before_retry() -> std::io::Result<()> {
    assert_failed_append_is_written_once(JsonlWriteFault::Partial(32), "partial-write-record").await
}

#[tokio::test]
async fn unrecoverable_append_stops_writer_with_live_senders() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let mut writer = open_log_file(&rollout_path)?.into_jsonl_writer();
    writer.write_fault = Some(JsonlWriteFault::Unexpected);
    let writer_task = Arc::new(RolloutWriterTask::new());
    let (tx, rx) = mpsc::channel(1);
    let worker = tokio::spawn(rollout_writer(
        Some(writer),
        None,
        rx,
        None,
        home.path().to_path_buf(),
        None,
        rollout_path.clone(),
        Default::default(),
        Arc::clone(&writer_task),
    ));
    tx.send(RolloutCmd::AddItems {
        items: captured(vec![RolloutItem::EventMsg(EventMsg::AgentMessage(
            AgentMessageEvent {
                message: "cannot safely retry".into(),
                phase: None,
            },
        ))]),
        flush_if_materialized: true,
        accepted: None,
    })
    .await
    .expect("writer accepts initial command");

    let err = tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("writer must exit even while a sender remains alive")
        .expect("writer task must not panic")
        .expect_err("unrecoverable append must fail");
    assert!(err.to_string().contains("retry is unsafe"));
    assert!(
        err.to_string()
            .contains("1 buffered rollout records could not be confirmed persisted")
    );
    assert!(tx.is_closed());
    assert!(writer_task.ensure_active("append").is_err());
    assert!(
        writer_task
            .terminal_failure()
            .expect("terminal failure")
            .to_string()
            .contains("1 buffered rollout records")
    );
    assert_eq!(fs::read(&rollout_path)?, b"unexpected bytes");
    Ok(())
}

#[tokio::test]
async fn ordered_append_accepts_into_bounded_queue_without_materializing() -> std::io::Result<()>
{
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            None,
            None,
            SessionSource::Exec,
            None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();

    let (entered_tx, entered_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    recorder
        .tx
        .send(RolloutCmd::Pause {
            entered: entered_tx,
            resume: resume_rx,
        })
        .await
        .expect("writer command channel should be open");
    entered_rx.await.expect("writer should enter pause");

    let item = RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: "accepted-in-memory".to_string(),
        phase: None,
    }));
    let items = [item];
    tokio::time::timeout(Duration::from_secs(1), recorder.record_canonical_items_ordered(&items))
        .await
        .expect("queue custody must not wait for a stalled consumer")?;
    assert!(recorder.writer_task.pending_bytes.available_permits() < MAX_PENDING_ROLLOUT_BYTES);
    assert!(!rollout_path.exists(), "queue admission must not materialize the rollout");

    resume_tx
        .send(())
        .expect("writer pause receiver should remain open");
    assert!(
        !rollout_path.exists(),
        "in-memory acceptance must not materialize or flush the rollout"
    );

    recorder.shutdown().await?;
    let (recovered, _, errors) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(errors, 0);
    assert_eq!(serde_json::to_value(recovered.last())?, serde_json::to_value(items.last())?);
    assert_eq!(recorder.writer_task.pending_bytes.available_permits(), MAX_PENDING_ROLLOUT_BYTES);
    Ok(())
}

#[tokio::test]
async fn ordered_append_reaches_disk_before_a_terminal_barrier() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            ThreadId::new(),
            None,
            None,
            SessionSource::Exec,
            None,
            "test_originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();
    // Materialize the rollout the way the first user turn does.
    recorder.persist().await?;

    let items = [RolloutItem::EventMsg(EventMsg::AgentMessage(
        AgentMessageEvent {
            message: "persisted-without-barrier".to_string(),
            phase: None,
        },
    ))];
    recorder.record_canonical_items_ordered(&items).await?;

    // No flush, persist, or shutdown: a turn that is killed before its terminal
    // barrier must not lose every item it recorded.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if fs::read_to_string(&rollout_path)?.contains("persisted-without-barrier") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ordered append must reach the materialized rollout without an explicit barrier"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    recorder.shutdown().await
}

// Real materialized writer, with a small queue for deterministic backpressure.
fn storage_overlap_recorder(home: &Path, capacity: usize) -> std::io::Result<RolloutRecorder> {
    let path = home.join("rollout.jsonl");
    File::create(&path)?;
    let writer = open_log_file(&path)?.into_jsonl_writer();
    let task = Arc::new(RolloutWriterTask::new());
    let (tx, rx) = mpsc::channel(capacity);
    let owner = Arc::clone(&task);
    let cwd = home.to_path_buf();
    let writer_path = path.clone();
    task.set_handle(tokio::spawn(async move {
        if let Err(error) = rollout_writer(Some(writer), None, rx, None, cwd,
            Some(None), writer_path, Default::default(), Arc::clone(&owner)).await
        {
            owner.mark_failed(&error);
        }
    }));
    Ok(RolloutRecorder { tx, writer_task: task, rollout_path: path })
}

#[tokio::test]
async fn ordered_append_overlaps_blocked_storage_but_queue_remains_bounded() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let recorder = storage_overlap_recorder(home.path(), 1)?;
    let lock = compression::lock_rollout_for_write_blocking(recorder.rollout_path())?;
    // The old acceptance path is a deterministic fence: the writer owns this
    // first item and cannot consume another until the OS write lock is released.
    recorder.record_canonical_items_with_flush(&[agent_message("first")], true, true).await?;
    tokio::time::timeout(Duration::from_secs(1),
        recorder.record_canonical_items_ordered(&[agent_message("second")]))
        .await.expect("independent execution must not wait for the previous write")?;
    let permits = recorder.writer_task.pending_bytes.available_permits();
    assert!(tokio::time::timeout(Duration::from_millis(20),
        recorder.record_canonical_items_ordered(&[agent_message("cancelled")])).await.is_err());
    assert_eq!(recorder.writer_task.pending_bytes.available_permits(), permits,
        "cancelled queue admission must release its byte charge");
    // A cancelled barrier cannot remove either accepted item or unlock storage.
    assert!(tokio::time::timeout(Duration::from_millis(20), recorder.flush()).await.is_err());
    let other_home = TempDir::new()?;
    let other = storage_overlap_recorder(other_home.path(), 1)?;
    tokio::time::timeout(Duration::from_secs(2), async {
        other.record_canonical_items_ordered(&[agent_message("independent")]).await?;
        other.shutdown().await
    }).await.expect("another writer must remain independent")?;
    drop(lock);
    recorder.flush_durable().await?;
    recorder.shutdown().await?;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(recorder.rollout_path()).await?;
    assert_eq!(errors, 0);
    assert_eq!(serde_json::to_value(items)?,
        serde_json::to_value(vec![agent_message("first"), agent_message("second")])?);
    assert_eq!(recorder.writer_task.pending_bytes.available_permits(), MAX_PENDING_ROLLOUT_BYTES);
    Ok(())
}

#[tokio::test]
async fn ordered_append_cancelled_shutdown_still_drains_owned_queue() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let recorder = storage_overlap_recorder(home.path(), 4)?;
    let lock = compression::lock_rollout_for_write_blocking(recorder.rollout_path())?;
    recorder.record_canonical_items_with_flush(&[agent_message("first")], true, true).await?;
    recorder.record_canonical_items_ordered(&[agent_message("second")]).await?;
    assert!(tokio::time::timeout(Duration::from_millis(20), recorder.shutdown()).await.is_err());
    assert_eq!(recorder.writer_task.lifecycle.load(Ordering::Acquire), WRITER_SHUTTING_DOWN);
    assert!(recorder.record_canonical_items_ordered(&[agent_message("too-late")]).await.is_err());
    drop(lock);
    let worker = recorder.writer_task.handle.lock().unwrap().take().unwrap();
    tokio::time::timeout(Duration::from_secs(5), worker).await.expect("owned shutdown drain")
        .expect("writer task");
    assert_eq!(recorder.writer_task.lifecycle.load(Ordering::Acquire), WRITER_SHUT_DOWN);
    let (items, _, errors) = RolloutRecorder::load_rollout_items(recorder.rollout_path()).await?;
    assert_eq!(errors, 0);
    assert_eq!(serde_json::to_value(items)?,
        serde_json::to_value(vec![agent_message("first"), agent_message("second")])?);
    Ok(())
}

/// Matched real persistence pipeline: capture -> artifact publication -> next
/// work -> durable flush -> shutdown -> authenticated reload. No provider/model
/// calls are included. The unchanged private acceptance path is the baseline.
#[tokio::test]
#[ignore = "wall-clock benchmark; run explicitly without competing benchmarks"]
async fn benchmark_ordered_append_storage_overlap() -> std::io::Result<()> {
    for slow_ms in [0, 2_000] {
        for sample in 0..4 {
            for baseline in if sample % 2 == 0 { [true, false] } else { [false, true] } {
                let home = TempDir::new()?;
                let recorder = storage_overlap_recorder(home.path(), 4)?;
                let first = RolloutItem::ToolManifest(ToolManifestItem::full(
                    "storage-overlap".into(), serde_json::json!({
                        "tools":[{"name":"fixture", "description":"EXACT_EVIDENCE\n".repeat(4096)}]
                    }),
                ));
                let second = agent_message("next-work");
                let lock = if slow_ms > 0 {
                    Some(compression::lock_rollout_for_write_blocking(recorder.rollout_path())?)
                } else { None };
                let started = std::time::Instant::now();
                let release = tokio::spawn(async move {
                    if lock.is_some() { tokio::time::sleep(Duration::from_millis(slow_ms)).await; }
                    drop(lock);
                });
                recorder.record_canonical_items_with_flush(std::slice::from_ref(&first), true, true).await?;
                let append_started = std::time::Instant::now();
                if baseline {
                    recorder.record_canonical_items_with_flush(std::slice::from_ref(&second), true, true).await?;
                } else {
                    recorder.record_canonical_items_ordered(std::slice::from_ref(&second)).await?;
                }
                let continuation_us = append_started.elapsed().as_micros();
                // Fixed independent work on both sides, not a removed wait.
                tokio::time::sleep(Duration::from_millis(if slow_ms == 0 { 5 } else { 2_000 })).await;
                let drain_started = std::time::Instant::now();
                recorder.flush_durable().await?;
                recorder.shutdown().await?;
                release.await.expect("storage release");
                let drain_us = drain_started.elapsed().as_micros();
                let (items, _, errors) = RolloutRecorder::load_rollout_items(recorder.rollout_path()).await?;
                assert_eq!(errors, 0);
                assert_eq!(serde_json::to_value(items)?, serde_json::to_value(vec![first, second])?);
                assert!(fs::read_to_string(recorder.rollout_path())?.contains(crate::payload_artifact::KIND));
                assert_eq!(recorder.writer_task.pending_bytes.available_permits(), MAX_PENDING_ROLLOUT_BYTES);
                if sample > 0 {
                    println!("{}", serde_json::json!({"benchmark":"ordered_append_storage_overlap",
                        "baseline":baseline, "slow_storage_ms":slow_ms, "sample":sample,
                        "continuation_us":continuation_us, "drain_us":drain_us,
                        "end_to_end_us":started.elapsed().as_micros(), "records":2,
                        "exact_recovery":true, "model_calls":0}));
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn payload_sync_failure_keeps_durable_barrier_retryable() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let recorder = storage_overlap_recorder(home.path(), 4)?;
    let items = (0..16).map(|i| RolloutItem::ToolManifest(ToolManifestItem::full(
        format!("payload-{i}"), serde_json::json!({"index":i,"body":"exact evidence\n".repeat(2048)}),
    ))).collect::<Vec<_>>();
    recorder.record_canonical_items_ordered(&items).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let failed_calls = Arc::clone(&calls);
    crate::payload_artifact::sync_test_hooks().lock().unwrap().insert(recorder.rollout_path().to_path_buf(),
        crate::payload_artifact::SyncTestHook { parallelism:8, before_sync: Arc::new(move |_| {
            if failed_calls.fetch_add(1, Ordering::SeqCst) == 0 { Err(std::io::Error::other("injected sync failure")) }
            else { Ok(()) }
        }) });
    assert!(recorder.flush_durable().await.is_err(), "must not claim rollout durability after a failed payload sync");
    assert!(calls.load(Ordering::SeqCst) > 1, "successful workers also ran");
    let retry_calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&retry_calls);
    crate::payload_artifact::sync_test_hooks().lock().unwrap().insert(recorder.rollout_path().to_path_buf(),
        crate::payload_artifact::SyncTestHook { parallelism:8, before_sync: Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst); Ok(())
        }) });
    recorder.flush_durable().await?;
    assert_eq!(retry_calls.load(Ordering::SeqCst), 16, "failure acknowledges no payloads");
    recorder.flush_durable().await?;
    assert_eq!(retry_calls.load(Ordering::SeqCst), 16, "successful barrier clears its prefix");
    crate::payload_artifact::sync_test_hooks().lock().unwrap().remove(recorder.rollout_path());
    recorder.shutdown().await?;
    let (reloaded, _, errors) = RolloutRecorder::load_rollout_items(recorder.rollout_path()).await?;
    assert_eq!(errors, 0);
    assert_eq!(serde_json::to_value(reloaded)?, serde_json::to_value(items)?);
    Ok(())
}

/// Real recorder admission, artifact writes, durability, subsequent execution,
/// shutdown and authenticated reload. One worker reproduces the previous serial
/// barrier; eight uses the production bound. No model/provider time is measured.
#[tokio::test]
#[ignore = "wall-clock benchmark; run explicitly without competing benchmarks"]
async fn benchmark_payload_sync_end_to_end() -> std::io::Result<()> {
    for slow_ms in [0, 250] {
        for sample in 0..4 {
            for parallelism in if sample % 2 == 0 { [1, 8] } else { [8, 1] } {
                let home = TempDir::new()?;
                let recorder = storage_overlap_recorder(home.path(), 4)?;
                let items = (0..16).map(|i| RolloutItem::ToolManifest(ToolManifestItem::full(
                    format!("payload-{i}"), serde_json::json!({"index":i,"body":"exact evidence\n".repeat(2048)}),
                ))).collect::<Vec<_>>();
                let syncs = Arc::new(AtomicUsize::new(0));
                let counted = Arc::clone(&syncs);
                crate::payload_artifact::sync_test_hooks().lock().unwrap().insert(recorder.rollout_path().to_path_buf(),
                    crate::payload_artifact::SyncTestHook { parallelism, before_sync: Arc::new(move |_| {
                        counted.fetch_add(1, Ordering::SeqCst);
                        if slow_ms > 0 { std::thread::sleep(Duration::from_millis(slow_ms)); }
                        Ok(())
                    }) });
                let started = std::time::Instant::now();
                recorder.record_canonical_items_ordered(&items).await?;
                let admission_us = started.elapsed().as_micros();
                recorder.flush_durable().await?;
                let continuation_us = started.elapsed().as_micros();
                // Same useful subsequent work on both sides, after durability.
                recorder.record_canonical_items_ordered(&[agent_message("next execution")]).await?;
                recorder.shutdown().await?;
                let (reloaded, _, errors) = RolloutRecorder::load_rollout_items(recorder.rollout_path()).await?;
                let mut expected = items;
                expected.push(agent_message("next execution"));
                assert_eq!(errors, 0);
                assert_eq!(serde_json::to_value(reloaded)?, serde_json::to_value(expected)?);
                assert_eq!(syncs.load(Ordering::SeqCst), 16);
                assert_eq!(recorder.writer_task.pending_bytes.available_permits(), MAX_PENDING_ROLLOUT_BYTES);
                crate::payload_artifact::sync_test_hooks().lock().unwrap().remove(recorder.rollout_path());
                if sample > 0 {
                    println!("{}", serde_json::json!({"benchmark":"payload_sync_end_to_end",
                        "parallelism":parallelism,"slow_storage_ms_per_payload":slow_ms,"sample":sample,
                        "admission_us":admission_us,"continuation_us":continuation_us,
                        "end_to_end_us":started.elapsed().as_micros(),"payloads":16,
                        "exact_recovery":true,"model_calls":0}));
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn writer_state_flushes_multi_item_batch_in_one_transaction() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    File::create(&rollout_path)?;
    let writer = open_log_file(&rollout_path)?.into_jsonl_writer();
    let mut state = RolloutWriterState::new(
        Some(writer),
        /*deferred_log_file_info*/ None,
        /*meta*/ None,
        home.path().to_path_buf(),
        /*known_repository_context*/ None,
        rollout_path.clone(),
        Default::default(),
    );
    state.add_items(captured(
        ["first", "second", "third"]
            .into_iter()
            .map(|message| {
                RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
                    message: message.to_string(),
                    phase: None,
                }))
            })
            .collect(),
    ));

    state.flush().await?;

    assert_eq!(
        state
            .writer
            .as_ref()
            .expect("writer remains open")
            .append_transaction_count,
        1,
        "one durability barrier should use one append lock, write, and flush"
    );
    let bytes = fs::read_to_string(&rollout_path)?;
    assert!(bytes.ends_with('\n'));
    let messages = bytes
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(value["format_version"], CURRENT_ROLLOUT_FORMAT_VERSION);
            let line: RolloutLine = serde_json::from_str(line).expect("valid rollout JSONL");
            let RolloutItem::EventMsg(EventMsg::AgentMessage(message)) = line.item else {
                panic!("expected persisted agent message");
            };
            message.message
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, ["first", "second", "third"]);
    Ok(())
}

#[tokio::test]
async fn artifact_selection_preserves_mixed_batch_bytes_and_inline_fallback() -> std::io::Result<()> {
    let text = "héllo 🦀 task_complete rollout_payload_artifact\n".repeat(300);
    let cases = vec![
        (RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta { originator: text.clone(), ..Default::default() },
            git: None,
        }), true),
        (RolloutItem::ToolManifest(ToolManifestItem::full(
            "large".into(), serde_json::json!({"description": text}),
        )), true),
        (RolloutItem::ToolManifest(ToolManifestItem::reference("small".into())), true),
        (serde_json::from_value(serde_json::json!({
            "type": "sampling_boundary", "payload": {
                "sampling_request_id": text, "physical_attempt_id": "attempt",
                "timing_checkpoint": {
                    "observed_at_unix_ms": 1, "tail_unknown": true,
                    "timing": codex_protocol::protocol::TurnTiming::default()
                }
            }
        }))?, true),
        (serde_json::from_value(serde_json::json!({
            "type": "sampling_boundary", "payload": {
                "sampling_request_id": text, "physical_attempt_id": "attempt"
            }
        }))?, false),
        (serde_json::from_value(serde_json::json!({
            "type": "event_msg", "payload": {
                "type": "task_complete", "turn_id": "turn", "last_agent_message": text
            }
        }))?, true),
        (serde_json::from_value(serde_json::json!({
            "type": "event_msg", "payload": {
                "type": "turn_aborted", "turn_id": text, "reason": "interrupted"
            }
        }))?, true),
        (serde_json::from_value(serde_json::json!({
            "type": "event_msg", "payload": {
                "type": "patch_apply_end", "call_id": "patch", "stdout": text,
                "stderr": "", "success": true, "status": "completed", "changes": {}
            }
        }))?, true),
        (RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
            message: text.clone(), phase: None,
        })), false),
        (serde_json::from_value(serde_json::json!({
            "type": "response_item", "payload": {
                "type": "function_call", "call_id": "call", "name": "tool", "arguments": text
            }
        }))?, false),
    ];
    for (item, candidate) in &cases {
        assert_eq!(crate::payload_artifact::is_artifact_candidate(item), *candidate);
    }
    let items = captured(cases.iter().map(|(item, _)| item.clone()).collect());
    let expected = items.iter().map(|item| {
        let mut line = Vec::new();
        JsonlWriter::serialize_rollout_item(&mut line, &item.item, item.captured_at)?;
        Ok(line)
    }).collect::<std::io::Result<Vec<_>>>()?;

    for fail_artifact_storage in [false, true] {
        let home = TempDir::new()?;
        let path = home.path().join("rollout.jsonl");
        if fail_artifact_storage {
            // Storage failure must retain the complete inline record, not drop the batch.
            fs::write(crate::payload_artifact::root(&path), b"not a directory")?;
        }
        let mut writer = open_log_file(&path)?.into_jsonl_writer();
        writer.write_rollout_items(&items.iter().collect::<Vec<_>>()).await?;
        assert_eq!(writer.append_transaction_count, 1);
        let bytes = fs::read(&path)?;
        let lines = bytes.split_inclusive(|byte| *byte == b'\n').collect::<Vec<_>>();
        assert_eq!(lines.len(), cases.len());
        for ((line, expected), (_, candidate)) in lines.iter().zip(&expected).zip(&cases) {
            let stored: serde_json::Value = serde_json::from_slice(line)?;
            let external = !fail_artifact_storage && *candidate
                && expected.len() >= crate::payload_artifact::INLINE_BYTES;
            assert_eq!(stored["type"] == "rollout_payload_artifact", external);
            if external {
                let hydrated = crate::payload_artifact::hydrate_line(
                    &path, String::from_utf8(line.to_vec()).expect("UTF-8"),
                )?;
                assert_eq!(serde_json::from_str::<serde_json::Value>(&hydrated)?,
                    serde_json::from_slice::<serde_json::Value>(expected)?);
            } else {
                assert_eq!(*line, expected.as_slice());
            }
        }
    }
    Ok(())
}

#[test]
fn rollout_write_lock_serializes_append_recovery() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let rollout_path = home.path().join("rollout.jsonl");
    let first_lock = compression::lock_rollout_for_write_blocking(&rollout_path)?;

    assert!(
        compression::try_lock_rollout_for_write_blocking(&rollout_path)?.is_none(),
        "a concurrent append transaction must not enter while recovery can truncate the tail"
    );

    drop(first_lock);
    assert!(
        compression::try_lock_rollout_for_write_blocking(&rollout_path)?.is_some(),
        "the append transaction lock should be released with its guard"
    );
    Ok(())
}

#[tokio::test]
async fn damaged_payloads_preserve_later_records_and_filename_thread_identity() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let id = ThreadId::new();
    let path = home.path().join(format!("rollout-2026-10-07T00-00-00-{id}.jsonl"));
    let meta = RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta { id, originator: "large metadata ".repeat(1000), ..Default::default() }, git: None,
    });
    let later = RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent { message: "retained later record".into(), phase: None }));
    let mut writer = open_log_file(&path)?.into_jsonl_writer();
    writer.write_rollout_item(&meta).await?;
    writer.write_rollout_item(&later).await?;
    let text = fs::read_to_string(&path)?;
    let reference: serde_json::Value = serde_json::from_str(text.lines().next().unwrap())?;
    let blob = crate::payload_artifact::root(&path).join(format!("{}.json", reference["payload"]["sha256"].as_str().unwrap()));
    let bytes = fs::read(&blob)?;
    for missing in [false, true] {
        if missing { fs::remove_file(&blob)?; } else { fs::write(&blob, b"corrupt")?; }
        let (items, thread, errors) = RolloutRecorder::load_rollout_items(&path).await?;
        assert_eq!(serde_json::to_value(&items[..1])?, serde_json::to_value(vec![later.clone()])?);
        assert_eq!(items.len(), 2);
        assert_reconstruction_gap(items.last().unwrap(), 1);
        assert_eq!(thread, Some(id));
        assert_eq!(errors, 1);
        assert!(matches!(RolloutRecorder::get_rollout_history(&path).await?, InitialHistory::Resumed(_)));
        assert!(crate::list::read_head_for_summary(&path).await.is_ok());
    }
    fs::write(&blob, bytes)?;
    crate::payload_artifact::sync_payload_artifacts(&path).await?;
    Ok(())
}

#[tokio::test]
async fn durable_flush_retries_unsynced_payloads_before_acknowledging_rollout() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("rollout.jsonl");
    let writer = open_log_file(&path)?.into_jsonl_writer();
    let mut state = RolloutWriterState::new(Some(writer), None, None, home.path().into(), None, path.clone(), Default::default());
    state.add_items(captured(vec![RolloutItem::ToolManifest(ToolManifestItem::full(
        "payload-sync".into(), serde_json::json!({"large": "x".repeat(10000)}),
    ))]));
    state.flush().await?;
    let text = fs::read_to_string(&path)?;
    let reference: serde_json::Value = serde_json::from_str(text.lines().next().unwrap())?;
    let blob = crate::payload_artifact::root(&path).join(format!("{}.json", reference["payload"]["sha256"].as_str().unwrap()));
    let saved = fs::read(&blob)?;
    fs::remove_file(&blob)?;
    assert!(state.flush_durable().await.is_err());
    fs::write(&blob, saved)?;
    state.flush_durable().await?;
    assert_eq!(fs::read_to_string(&path)?, text);
    Ok(())
}

#[tokio::test]
async fn list_threads_db_disabled_does_not_skip_paginated_items() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let newest = write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(9001))?;
    let middle = write_session_file(home.path(), "2025-01-02T12-00-00", Uuid::from_u128(9002))?;
    let _oldest = write_session_file(home.path(), "2025-01-01T12-00-00", Uuid::from_u128(9003))?;

    let default_provider = config.model_provider_id.clone();
    let page1 = RolloutRecorder::list_threads(
        /*state_db_ctx*/ None,
        &config,
        /*page_size*/ 1,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page1.items.len(), 1);
    assert_eq!(page1.items[0].path, newest);
    let cursor = page1.next_cursor.clone().expect("cursor should be present");

    let page2 = RolloutRecorder::list_threads(
        /*state_db_ctx*/ None,
        &config,
        /*page_size*/ 1,
        Some(&cursor),
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.items[0].path, middle);
    Ok(())
}

#[tokio::test]
async fn list_threads_db_enabled_preserves_metadata_for_missing_rollout_paths()
-> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9010);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let stale_path = home.path().join(format!(
        "sessions/2099/01/01/rollout-2099-01-01T00-00-00-{uuid}.jsonl"
    ));

    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 13, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        stale_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let default_provider = config.model_provider_id.clone();
    let page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page.items.len(), 0);
    let stored_path = runtime
        .find_rollout_path_by_id(thread_id, Some(false))
        .await
        .expect("state db lookup should succeed");
    assert_eq!(stored_path, Some(metadata.rollout_path));
    Ok(())
}

#[tokio::test]
async fn list_threads_db_enabled_repairs_stale_rollout_paths() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9011);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let real_path = write_session_file(home.path(), "2025-01-03T13-00-00", uuid)?;
    let stale_path = home.path().join(format!(
        "sessions/2099/01/01/rollout-2099-01-01T00-00-00-{uuid}.jsonl"
    ));

    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 13, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        stale_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.title = "SQLite title".to_string();
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let default_provider = config.model_provider_id.clone();
    let page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 1,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        default_provider.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title.as_deref(), Some("SQLite title"));
    assert_eq!(page.items[0].path, real_path);

    let repaired_path = runtime
        .find_rollout_path_by_id(thread_id, Some(false))
        .await
        .expect("state db lookup should succeed");
    assert_eq!(repaired_path, Some(real_path));
    Ok(())
}

#[tokio::test]
async fn list_threads_db_repairs_archived_path_without_deleting_metadata() -> std::io::Result<()> {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let uuid = Uuid::from_u128(9012);
    let thread_id = ThreadId::from_string(&uuid.to_string()).unwrap();
    let original = write_session_file(home.path(), "2025-01-03T13-00-00", uuid)?;
    let archive_dir = home.path().join(crate::ARCHIVED_SESSIONS_SUBDIR);
    std::fs::create_dir_all(&archive_dir)?;
    let archived = archive_dir.join(original.file_name().unwrap());
    std::fs::rename(&original, &archived)?;
    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(), config.model_provider_id.clone(),
    ).await.unwrap();
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id, original, chrono::Utc::now(), SessionSource::Cli,
    );
    builder.cwd = home.path().to_path_buf();
    let mut metadata = builder.build(&config.model_provider_id);
    metadata.title = "Preserve my title".into();
    metadata.first_user_message = Some("Hello from user".into());
    metadata.preview = metadata.first_user_message.clone();
    runtime.upsert_thread(&metadata).await.unwrap();

    let page = crate::state_integration::list_threads_db(
        Some(&runtime), home.path(), 10, None, ThreadSortKey::CreatedAt,
        SortDirection::Desc, &[], None, None, None, false, None, None,
    ).await.unwrap();
    assert!(page.items.is_empty(), "archived threads must not leak into active results");
    let repaired = runtime.get_thread(thread_id).await.unwrap().unwrap();
    assert_eq!(repaired.rollout_path, archived);
    assert!(repaired.archived_at.is_some());
    assert_eq!(repaired.title, metadata.title);
    let page = crate::state_integration::list_threads_db(
        Some(&runtime), home.path(), 10, None, ThreadSortKey::CreatedAt,
        SortDirection::Desc, &[], None, None, None, true, None, None,
    ).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].rollout_path, archived);
    Ok(())
}

#[tokio::test]
async fn list_threads_state_db_only_skips_jsonl_repair_scan() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");

    let uuid = Uuid::from_u128(9012);
    let ts = "2025-01-03T14-00-00";
    let day_dir = home.path().join("sessions/2025/01/03");
    fs::create_dir_all(&day_dir)?;
    let path = day_dir.join(format!("rollout-{ts}-{uuid}.jsonl"));
    let mut file = File::create(&path)?;
    let meta = serde_json::json!({
        "timestamp": ts,
        "type": "session_meta",
        "payload": {
            "session_id": uuid,
            "id": uuid,
            "timestamp": ts,
            "cwd": home.path().display().to_string(),
            "originator": "test_originator",
            "cli_version": "test_version",
            "source": "cli",
            "model_provider": "test-provider",
        },
    });
    writeln!(file, "{meta}")?;
    let user_event = serde_json::json!({
        "timestamp": ts,
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": "Hello from user",
            "kind": "plain",
        },
    });
    writeln!(file, "{user_event}")?;

    let cwd_filters = [home.path().to_path_buf()];
    let state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(state_db_only_page.items.len(), 0);

    let repaired_page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(repaired_page.items.len(), 1);

    let repaired_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(repaired_state_db_only_page.items.len(), 1);
    Ok(())
}

#[tokio::test]
async fn list_threads_default_filter_returns_filesystem_scan_results() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9013);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let real_path = write_session_file(home.path(), "2025-01-03T13-00-00", uuid)?;
    let stale_cwd = home.path().join("stale-cwd");

    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 13, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        real_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = stale_cwd.clone();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let cwd_filters = [stale_cwd];
    let state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(state_db_only_page.items.len(), 1);

    let scanned_page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(scanned_page.items.len(), 0);

    let repaired_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ Some(cwd_filters.as_slice()),
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;
    assert_eq!(repaired_state_db_only_page.items.len(), 0);
    Ok(())
}

#[tokio::test]
async fn list_threads_metadata_filter_overlays_state_db_list_metadata() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9015);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let rollout_path = write_session_file(home.path(), "2025-01-03T16-00-00", uuid)?;

    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 16, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        rollout_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    builder.git_branch = Some("sqlite-branch".to_string());
    builder.git_sha = Some("sqlite-sha".to_string());
    builder.git_origin_url = Some("https://example.com/repo.git".to_string());
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[SessionSource::Cli],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        /*search_term*/ None,
    )
    .await?;

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].git_branch.as_deref(), Some("sqlite-branch"));
    assert_eq!(page.items[0].git_sha.as_deref(), Some("sqlite-sha"));
    assert_eq!(
        page.items[0].git_origin_url.as_deref(),
        Some("https://example.com/repo.git")
    );
    Ok(())
}

#[test]
fn overlay_thread_item_metadata_preserves_thread_id_and_uses_authoritative_state_fields() {
    let filesystem_thread_id = ThreadId::new();
    let state_thread_id = ThreadId::new();
    let filesystem_path = PathBuf::from("/tmp/filesystem-rollout.jsonl");
    let state_path = PathBuf::from("/tmp/state-rollout.jsonl");
    let mut item = ThreadItem {
        path: filesystem_path,
        thread_id: Some(filesystem_thread_id),
        first_user_message: Some("filesystem message".to_string()),
        title: None,
        preview: Some("filesystem preview".to_string()),
        cwd: None,
        git_branch: Some("filesystem-branch".to_string()),
        git_sha: Some("filesystem-sha".to_string()),
        git_origin_url: Some("https://example.com/filesystem.git".to_string()),
        source: None,
        history_mode: Default::default(),
        parent_thread_id: None,
        agent_nickname: None,
        agent_role: None,
        model_provider: None,
        cli_version: None,
        created_at: None,
        recency_at: Some("2025-01-03T15:59:00.000Z".to_string()),
        updated_at: None,
    };
    let state_item = ThreadItem {
        path: state_path,
        thread_id: Some(state_thread_id),
        first_user_message: Some("state message".to_string()),
        title: Some("state title".to_string()),
        preview: Some("state preview".to_string()),
        cwd: Some(PathBuf::from("/tmp/state-cwd")),
        git_branch: Some("state-branch".to_string()),
        git_sha: Some("state-sha".to_string()),
        git_origin_url: Some("https://example.com/state.git".to_string()),
        source: Some(SessionSource::Exec),
        history_mode: Default::default(),
        parent_thread_id: None,
        agent_nickname: Some("state-agent".to_string()),
        agent_role: Some("state-role".to_string()),
        model_provider: Some("state-provider".to_string()),
        cli_version: Some("state-version".to_string()),
        created_at: Some("2025-01-03T16:00:00Z".to_string()),
        recency_at: Some("2025-01-03T16:00:30.001Z".to_string()),
        updated_at: Some("2025-01-03T16:01:02.003Z".to_string()),
    };

    overlay_thread_item_metadata(&mut item, state_item);

    assert_eq!(item.path, PathBuf::from("/tmp/state-rollout.jsonl"));
    assert_eq!(item.thread_id, Some(filesystem_thread_id));
    assert_eq!(item.first_user_message.as_deref(), Some("state message"));
    assert_eq!(item.title.as_deref(), Some("state title"));
    assert_eq!(item.preview.as_deref(), Some("state preview"));
    assert_eq!(item.cwd.as_deref(), Some(Path::new("/tmp/state-cwd")));
    assert_eq!(item.git_branch.as_deref(), Some("state-branch"));
    assert_eq!(item.git_sha.as_deref(), Some("state-sha"));
    assert_eq!(
        item.git_origin_url.as_deref(),
        Some("https://example.com/state.git")
    );
    assert_eq!(item.source, Some(SessionSource::Exec));
    assert_eq!(item.agent_nickname.as_deref(), Some("state-agent"));
    assert_eq!(item.agent_role.as_deref(), Some("state-role"));
    assert_eq!(item.model_provider.as_deref(), Some("state-provider"));
    assert_eq!(item.cli_version.as_deref(), Some("state-version"));
    assert_eq!(item.created_at.as_deref(), Some("2025-01-03T16:00:00Z"));
    assert_eq!(item.recency_at.as_deref(), Some("2025-01-03T16:00:30.001Z"));
    assert_eq!(item.updated_at.as_deref(), Some("2025-01-03T16:01:02.003Z"));
}

#[tokio::test]
async fn list_threads_search_repairs_stale_state_db_hits_before_returning() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());

    let uuid = Uuid::from_u128(9014);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let real_path = write_session_file(home.path(), "2025-01-03T15-00-00", uuid)?;

    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let created_at = chrono::Utc
        .with_ymd_and_hms(2025, 1, 3, 15, 0, 0)
        .single()
        .expect("valid datetime");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        real_path,
        created_at,
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    let mut metadata = builder.build(config.model_provider_id.as_str());
    metadata.title = "needle stale first user".to_string();
    metadata.first_user_message = Some(metadata.title.clone());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");

    let stale_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        Some("needle"),
    )
    .await?;
    assert_eq!(stale_state_db_only_page.items.len(), 1);

    let scanned_page = RolloutRecorder::list_threads(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        Some("needle"),
    )
    .await?;
    assert_eq!(scanned_page.items.len(), 0);

    let repaired_state_db_only_page = RolloutRecorder::list_threads_from_state_db(
        Some(runtime.clone()),
        &config,
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        config.model_provider_id.as_str(),
        Some("needle"),
    )
    .await?;
    assert_eq!(repaired_state_db_only_page.items.len(), 0);
    Ok(())
}

#[tokio::test]
async fn list_threads_filters_and_projects_latest_persisted_cwd() -> std::io::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let latest_cwd = home.path().join("latest-list-cwd");
    fs::create_dir_all(&latest_cwd)?;
    let path = write_session_file(home.path(), "2025-01-03T13-30-00", Uuid::from_u128(9016))?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
    let turn_context = RolloutLine {
        timestamp: "2025-01-03T13:30:01Z".to_string(),
        item: RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some("turn-1".to_string()),
            cwd: serde_json::from_value(serde_json::json!(&latest_cwd))
                .expect("absolute latest cwd"),
            workspace_roots: None,
            current_date: None,
            timezone: None,
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            permission_profile: None,
            network: None,
            file_system_sandbox_policy: None,
            model: "test-model".to_string(),
            comp_hash: None,
            personality: None,
            collaboration_mode: None,
            multi_agent_version: None,
            multi_agent_mode: None,
            effort: None,
            context_provenance: None,
        }),
    };
    writeln!(file, "{}", serde_json::to_string(&turn_context)?)?;

    let cwd_filters = [latest_cwd.clone()];
    let page = RolloutRecorder::list_threads(
        /*state_db_ctx*/ None,
        &test_config(home.path()),
        /*page_size*/ 10,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Desc,
        &[SessionSource::Cli],
        /*model_providers*/ None,
        Some(cwd_filters.as_slice()),
        "test-provider",
        /*search_term*/ None,
    )
    .await?;

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].path, path);
    assert_eq!(page.items[0].cwd.as_deref(), Some(latest_cwd.as_path()));
    Ok(())
}

#[tokio::test]
async fn deferred_writers_install_one_canonical_header_after_both_open() -> std::io::Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    let make_state = || {
        RolloutWriterState::new(
            None,
            Some(LogFileInfo {
                path: path.clone(),
                conversation_id: id,
                timestamp: OffsetDateTime::now_utc(),
            }),
            Some(SessionMeta {
                session_id: id.into(),
                id,
                timestamp: chrono::Utc::now().to_rfc3339(),
                ..SessionMeta::default()
            }),
            home.path().to_path_buf(),
            Some(None),
            path.clone(),
            ToolManifestDictionary::default(),
        )
    };
    let mut first = make_state();
    let mut second = make_state();
    first.ensure_writer_open().await?;
    second.ensure_writer_open().await?;
    let manifest = RolloutItem::ToolManifest(ToolManifestItem::full(
        "shared".into(),
        serde_json::json!({"tools": []}),
    ));
    first.add_items(captured(vec![manifest.clone()]));
    second.add_items(captured(vec![manifest]));
    let (first_result, second_result) = tokio::join!(first.persist(), second.persist());
    first_result?;
    second_result?;
    let (items, loaded_id, errors) = RolloutRecorder::load_rollout_items(&path).await?;
    assert_eq!(loaded_id, Some(id));
    assert_eq!(errors, 0);
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item, RolloutItem::SessionMeta(_)))
            .count(),
        1
    );
    let manifests = items
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ToolManifest(item) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(manifests.len(), 2);
    assert!(manifests[0].manifest.is_some());
    assert!(manifests[1].is_reference());
    Ok(())
}

#[tokio::test]
async fn failed_reconciliation_does_not_overlay_stale_readable_metadata() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let path = write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::new_v4())?;
    let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
    let mut metadata = crate::metadata::extract_metadata_from_rollout(&path, "test-provider")
        .await?
        .metadata;
    metadata.model_provider = "stale-provider".into();
    metadata.cwd = home.path().join("stale-cwd");
    metadata.title = "My chosen title".into();
    runtime.upsert_thread(&metadata).await?;
    let item = crate::list::read_thread_item_from_rollout(path.clone())
        .await
        .expect("filesystem snapshot");
    fs::remove_file(&path)?;
    let reconciled = state_integration::reconcile_rollout(
        Some(runtime.as_ref()),
        &path,
        "test-provider",
        None,
        &[],
        None,
    )
    .await;
    assert!(!reconciled);
    assert_eq!(
        runtime
            .get_thread(metadata.id)
            .await?
            .unwrap()
            .model_provider,
        "stale-provider"
    );
    let expected_provider = item.model_provider.clone();
    let expected_cwd = item.cwd.clone();
    let expected_updated_at = item.updated_at.clone();
    let page = overlay_thread_item_metadata_from_state_db(
        Some(runtime.as_ref()),
        ThreadsPage {
            items: vec![item],
            ..Default::default()
        },
        &HashSet::new(),
    )
    .await;
    let actual = &page.items[0];
    assert_eq!(actual.model_provider, expected_provider);
    assert_eq!(actual.cwd, expected_cwd);
    assert_eq!(actual.updated_at, expected_updated_at);
    assert_eq!(actual.title.as_deref(), Some("My chosen title"));
    Ok(())
}
