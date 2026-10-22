#![allow(warnings, clippy::all)]

use super::*;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Timelike;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_state::BackfillStatus;
use codex_state::ThreadMetadataBuilder;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::fs::FileTimes;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tempfile::tempdir;
use uuid::Uuid;

#[tokio::test]
async fn legacy_metadata_streams_before_optional_late_session_meta() {
    for (late_meta, standard_filename) in [(false, true), (true, true), (true, false)] {
        let dir = tempdir().expect("tempdir");
        let filename_id = ThreadId::new();
        let canonical_id = ThreadId::new();
        let path = dir.path().join(if standard_filename {
            format!("rollout-2026-01-27T12-34-56-{filename_id}.jsonl")
        } else {
            "imported-history.jsonl".to_string()
        });
        let mut file = File::create(&path).expect("create rollout");
        let mut accumulator = RolloutMetadataAccumulator::default();
        // Keep the large streaming regression on the imported/late-metadata
        // path; the other cases exercise metadata semantics, not file size.
        let large_stream = late_meta && !standard_filename;
        for index in 0..if large_stream { 1024 } else { 3 } {
            let item = RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: if index == 0 {
                    "first retained message".to_string()
                } else {
                    "later payload ".repeat(if large_stream { 1024 } else { 1 })
                },
                ..Default::default()
            }));
            writeln!(
                file,
                "{}",
                serde_json::to_string(&RolloutLine {
                    timestamp: "2026-01-27T12:34:56Z".to_string(),
                    item: item.clone(),
                })
                .expect("serialize line")
            )
            .expect("write line");
            accumulator.push(item, &path, "fallback-provider");
        }
        // Projection must happen before SessionMeta, including for imported paths.
        // Retaining the entire prefix until finish would fail this assertion.
        assert_eq!(
            accumulator
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.first_user_message.as_deref()),
            Some("first retained message")
        );
        if late_meta {
            let item = RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    id: canonical_id,
                    session_id: canonical_id.into(),
                    timestamp: "2026-02-01T00:00:00Z".to_string(),
                    history_mode: ThreadHistoryMode::Paginated,
                    model_provider: Some("canonical-provider".to_string()),
                    ..SessionMeta::default()
                },
                git: None,
            });
            writeln!(
                file,
                "{}",
                serde_json::to_string(&RolloutLine {
                    timestamp: "2026-02-01T00:00:00Z".to_string(),
                    item: item.clone(),
                })
                .expect("serialize meta")
            )
            .expect("write meta");
            accumulator.push(item, &path, "fallback-provider");
        }
        drop(file);
        let streamed = accumulator
            .finish(&path, "fallback-provider", 0)
            .await
            .expect("streamed extraction");
        let extracted = extract_metadata_from_rollout(&path, "fallback-provider")
            .await
            .expect("normal extraction");
        assert_eq!(streamed.metadata, extracted.metadata);
        assert_eq!(
            extracted.metadata.id,
            if late_meta { canonical_id } else { filename_id }
        );
        assert_eq!(
            extracted.metadata.first_user_message.as_deref(),
            Some("first retained message")
        );
        assert_eq!(extracted.metadata.title, "first retained message");
        assert_eq!(
            extracted.metadata.model_provider,
            if late_meta {
                "canonical-provider"
            } else {
                "fallback-provider"
            }
        );
        if late_meta {
            assert_eq!(
                extracted.metadata.history_mode,
                ThreadHistoryMode::Paginated
            );
            assert_eq!(
                extracted.metadata.created_at,
                DateTime::parse_from_rfc3339("2026-02-01T00:00:00Z")
                    .expect("timestamp")
                    .with_timezone(&Utc)
            );
        }
        assert_eq!(extracted.parse_errors, 0);
    }
}

#[tokio::test]
async fn extract_metadata_from_rollout_uses_session_meta() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let parent_thread_id = ThreadId::new();
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));

    let session_meta = SessionMeta {
        session_id: id.into(),
        id,
        forked_from_id: None,
        parent_thread_id: Some(parent_thread_id),
        timestamp: "2026-01-27T12:34:56Z".to_string(),
        cwd: dir.path().to_path_buf(),
        originator: "cli".to_string(),
        cli_version: "0.0.0".to_string(),
        harness_build: None,
        source: SessionSource::default(),
        thread_source: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        model_provider: Some("openai".to_string()),
        base_instructions: None,
        dynamic_tools: None,
        selected_capability_roots: Vec::new(),

        history_mode: ThreadHistoryMode::Paginated,
        multi_agent_version: None,
        context_window: None,
    };
    let session_meta_line = SessionMetaLine {
        meta: session_meta,
        git: None,
    };
    let rollout_line = RolloutLine {
        timestamp: "2026-01-27T12:34:56Z".to_string(),
        item: RolloutItem::SessionMeta(session_meta_line.clone()),
    };
    let json = serde_json::to_string(&rollout_line).expect("rollout json");
    let mut file = File::create(&path).expect("create rollout");
    writeln!(file, "{json}").expect("write rollout");

    let outcome = extract_metadata_from_rollout(&path, "openai")
        .await
        .expect("extract");

    let builder = builder_from_session_meta(&session_meta_line, path.as_path()).expect("builder");
    let mut expected = builder.build("openai");
    codex_state::apply_rollout_items(
        &mut expected,
        std::slice::from_ref(&rollout_line.item),
        "openai",
    );
    expected.updated_at = file_modified_time_utc(&path).await.expect("mtime");
    expected.recency_at = expected.updated_at;

    assert_eq!(outcome.metadata, expected);
    // `expected` is built by the extraction's own helpers; pin what SessionMeta supplies.
    assert_eq!(outcome.metadata.id, id);
    assert_eq!(
        outcome.metadata.created_at,
        DateTime::parse_from_rfc3339("2026-01-27T12:34:56Z")
            .expect("timestamp")
            .with_timezone(&Utc)
    );
    assert_eq!(outcome.metadata.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(outcome.metadata.cli_version, "0.0.0");
    assert_eq!(outcome.parent_thread_id, Some(parent_thread_id));
    assert_eq!(outcome.parse_errors, 0);
}

#[tokio::test]
async fn listing_pass_supplies_reconciliation_extraction_without_reopening_rollout() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));
    let lines = [
        RolloutLine {
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    session_id: id.into(),
                    id,
                    timestamp: "2026-01-27T12:34:56Z".to_string(),
                    cwd: dir.path().to_path_buf(),
                    originator: "cli".to_string(),
                    cli_version: "0.0.0".to_string(),
                    harness_build: None,
                    ..SessionMeta::default()
                },
                git: None,
            }),
        },
        RolloutLine {
            timestamp: "2026-01-27T12:35:00Z".to_string(),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "listing preview".to_string(),
                ..Default::default()
            })),
        },
    ];
    let mut file = File::create(&path).expect("create rollout");
    for line in lines {
        writeln!(
            file,
            "{}",
            serde_json::to_string(&line).expect("rollout json")
        )
        .expect("write rollout");
    }
    drop(file);

    let item = crate::list::read_thread_item_from_rollout(path.clone())
        .await
        .expect("listing item");
    assert_eq!(item.thread_id, Some(id));

    let (outcome, cache_hit) = extract_metadata_from_rollout_with_cache_status(&path, "")
        .await
        .expect("cached extraction");
    assert!(cache_hit);
    assert_eq!(outcome.metadata.id, id);

    let compressed_path = crate::compression::compressed_rollout_path(&path);
    std::fs::write(&compressed_path, b"stale compressed sibling").expect("write sibling");
    assert!(
        crate::list::read_thread_item_from_rollout(compressed_path.clone())
            .await
            .is_some()
    );
    assert!(
        extract_metadata_from_rollout_with_cache_status(&compressed_path, "")
            .await
            .unwrap()
            .1
    );
    let line = RolloutLine {
        timestamp: "2026-01-28T00:00:00Z".into(),
        item: RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "new-turn".into(),
            trace_id: None,
            started_at: Some(
                DateTime::parse_from_rfc3339("2026-01-28T00:00:00Z")
                    .unwrap()
                    .timestamp(),
            ),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", serde_json::to_string(&line).unwrap()).unwrap();
    drop(file);
    let (outcome, cache_hit) =
        extract_metadata_from_rollout_with_cache_status(&compressed_path, "")
            .await
            .unwrap();
    assert!(!cache_hit);
    assert_eq!(
        outcome.metadata.recency_at,
        DateTime::parse_from_rfc3339("2026-01-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    );
}

#[tokio::test]
async fn listing_metadata_cache_recovers_after_payload_repair() {
    for warm_healthy_cache in [false, true] {
        let dir = tempdir().expect("tempdir");
        let id = ThreadId::new();
        let parent_id = ThreadId::new();
        let shared_session_id: codex_protocol::SessionId = ThreadId::new().into();
        let sessions = dir.path().join(SESSIONS_SUBDIR);
        std::fs::create_dir_all(&sessions).expect("sessions directory");
        let path = sessions.join(format!("rollout-2026-01-27T12-34-56-{id}.jsonl"));
        let session_meta = RolloutLine {
            timestamp: "2026-01-27T12:34:56Z".into(),
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    session_id: shared_session_id,
                    id,
                    parent_thread_id: Some(parent_id),
                    timestamp: "2026-01-27T12:34:56Z".into(),
                    cwd: dir.path().to_path_buf(),
                    originator: "artifact-backed metadata fixture ".repeat(1024),
                    model_provider: Some("artifact-provider".into()),
                    history_mode: ThreadHistoryMode::Paginated,
                    ..SessionMeta::default()
                },
                git: None,
            }),
        };
        let serialized = serde_json::to_vec(&session_meta).expect("serialize session metadata");
        let reference = crate::payload_artifact::store_line(&path, &serialized)
            .expect("publish metadata payload");
        let reference_value: serde_json::Value = serde_json::from_slice(&reference).unwrap();
        assert_eq!(reference_value["type"], crate::payload_artifact::KIND);
        let payload_path = crate::payload_artifact::root(&path).join(format!(
            "{}.json",
            reference_value["payload"]["sha256"].as_str().unwrap()
        ));
        let mut rollout_bytes = reference.clone();
        let user_message = RolloutLine {
            timestamp: "2026-01-27T12:35:00Z".into(),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "retained inline preview".into(),
                ..Default::default()
            })),
        };
        rollout_bytes.extend(serde_json::to_vec(&user_message).unwrap());
        rollout_bytes.push(b'\n');
        std::fs::write(&path, &rollout_bytes).expect("write referencing rollout");
        crate::payload_artifact::sync_payload_artifacts(&path)
            .await
            .unwrap();
        let original_key = extraction_cache_key(&path, "")
            .await
            .expect("rollout cache key");

        if warm_healthy_cache {
            crate::list::read_thread_item_from_rollout(path.clone())
                .await
                .expect("healthy listing");
            let (healthy, cache_hit) = extract_metadata_from_rollout_with_cache_status(&path, "")
                .await
                .unwrap();
            assert!(
                cache_hit,
                "healthy listing still supplies reconciliation metadata"
            );
            assert_eq!(healthy.parse_errors, 0);
            assert_eq!(healthy.metadata.model_provider, "artifact-provider");
        }

        std::fs::write(&payload_path, b"corrupt").expect("corrupt only the sidecar");
        let damaged = crate::list::read_thread_item_from_rollout(path.clone())
            .await
            .expect("inline history remains listable");
        assert_eq!(damaged.thread_id, Some(id));
        assert_eq!(damaged.model_provider, None);
        let (degraded, _) = extract_metadata_from_rollout_with_cache_status(&path, "")
            .await
            .unwrap();
        assert_eq!(degraded.parse_errors, 1, "one damaged metadata payload");
        assert_eq!(degraded.unresolved_payloads, 1);
        assert_eq!(degraded.parent_thread_id, None);

        let runtime = codex_state::StateRuntime::init(dir.path().to_path_buf(), String::new())
            .await
            .expect("fresh index");
        backfill_sessions(runtime.as_ref(), dir.path(), "").await;
        let pending = runtime.get_backfill_state().await.unwrap();
        assert_eq!(pending.status, BackfillStatus::Pending);
        assert_eq!(
            pending.last_watermark, None,
            "do not checkpoint the repairable gap"
        );
        assert!(
            runtime
                .list_thread_spawn_children(parent_id)
                .await
                .unwrap()
                .is_empty()
        );
        // Simulate an older index with unknown identity and newer explicit settings.
        // Repairing lineage/identity must not roll those settings back.
        let mut indexed = runtime.get_thread(id).await.unwrap().unwrap();
        indexed.updated_at = Utc::now() + chrono::Duration::hours(1);
        indexed.model = Some("explicit-newer-model".to_owned());
        runtime
            .upsert_thread_preserving_timestamps(&indexed)
            .await
            .unwrap();

        // The normal publisher repairs the shared artifact without appending to
        // this rollout. Its length, mtime, and both cache paths remain unchanged.
        let repaired_reference = crate::payload_artifact::store_line(&path, &serialized)
            .expect("repair metadata payload");
        assert_eq!(repaired_reference, reference);
        crate::payload_artifact::sync_payload_artifacts(&path)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), rollout_bytes);
        assert_eq!(extraction_cache_key(&path, "").await, Some(original_key));

        let (recovered, cache_hit) = extract_metadata_from_rollout_with_cache_status(&path, "")
            .await
            .expect("extract repaired metadata");
        assert_eq!(recovered.parse_errors, 0, "repaired payload must be reread");
        assert!(
            !cache_hit,
            "degraded listing must not leave reusable cache state"
        );
        assert_eq!(recovered.metadata.id, id);
        assert_eq!(recovered.metadata.model_provider, "artifact-provider");
        assert_eq!(
            recovered.metadata.history_mode,
            ThreadHistoryMode::Paginated
        );
        assert_eq!(recovered.parent_thread_id, Some(parent_id));
        assert_eq!(
            recovered.metadata.first_user_message.as_deref(),
            Some("retained inline preview")
        );
        backfill_sessions(runtime.as_ref(), dir.path(), "").await;
        assert_eq!(
            runtime.get_backfill_state().await.unwrap().status,
            BackfillStatus::Complete
        );
        assert_eq!(
            runtime.list_thread_spawn_children(parent_id).await.unwrap(),
            vec![id]
        );
        let indexed = runtime.get_thread(id).await.unwrap().unwrap();
        assert_eq!(indexed.session_id, Some(shared_session_id));
        assert_eq!(indexed.model.as_deref(), Some("explicit-newer-model"));
        runtime.close().await;
    }
}

#[tokio::test]
async fn extract_metadata_preserves_persisted_turn_start_recency_over_file_mtime() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));
    let session_meta_line = SessionMetaLine {
        meta: SessionMeta {
            session_id: id.into(),
            id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            cwd: dir.path().to_path_buf(),
            originator: "cli".to_string(),
            cli_version: "0.0.0".to_string(),
            harness_build: None,
            source: SessionSource::default(),
            thread_source: None,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
            model_provider: Some("openai".to_string()),
            base_instructions: None,
            dynamic_tools: None,
            selected_capability_roots: Vec::new(),

            history_mode: ThreadHistoryMode::Paginated,
            multi_agent_version: None,
            context_window: None,
        },
        git: None,
    };
    let started_at = 1_769_518_923;
    let lines = [
        RolloutLine {
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            item: RolloutItem::SessionMeta(session_meta_line),
        },
        RolloutLine {
            timestamp: "2026-01-27T12:35:23Z".to_string(),
            item: RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                trace_id: None,
                started_at: Some(started_at),
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            })),
        },
    ];
    let mut file = File::create(&path).expect("create rollout");
    for line in lines {
        writeln!(
            file,
            "{}",
            serde_json::to_string(&line).expect("rollout json")
        )
        .expect("write rollout");
    }
    let updated_at =
        DateTime::<Utc>::from_timestamp(started_at + 3_600, 0).expect("valid updated timestamp");
    file.set_times(FileTimes::new().set_modified(updated_at.into()))
        .expect("set rollout mtime");
    drop(file);

    let outcome = extract_metadata_from_rollout(&path, "openai")
        .await
        .expect("extract");

    assert_eq!(
        outcome.metadata.recency_at,
        DateTime::<Utc>::from_timestamp(started_at, 0).expect("valid timestamp")
    );
    assert_eq!(outcome.metadata.updated_at, updated_at);
}

#[tokio::test]
async fn extract_metadata_from_rollout_rejects_unknown_history_mode() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));
    let mut rollout_line = serde_json::to_value(RolloutLine {
        timestamp: "2026-01-27T12:34:56Z".to_string(),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: id.into(),
                id,
                timestamp: "2026-01-27T12:34:56Z".to_string(),
                cwd: dir.path().to_path_buf(),
                originator: "cli".to_string(),
                cli_version: "0.0.0".to_string(),
                harness_build: None,
                ..SessionMeta::default()
            },
            git: None,
        }),
    })
    .expect("serialize rollout line");
    rollout_line["payload"]["history_mode"] = serde_json::json!("future");
    let mut file = File::create(&path).expect("create rollout");
    writeln!(file, "{rollout_line}").expect("write rollout");

    let Err(err) = extract_metadata_from_rollout(&path, "openai").await else {
        panic!("unknown history mode must fail extraction");
    };
    // Skipping the line as malformed also fails, as an empty rollout; pin the rejection.
    assert!(
        format!("{err:#}").contains("invalid session metadata history_mode"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn builder_from_items_falls_back_to_filename() {
    let dir = tempdir().expect("tempdir");
    let uuid = Uuid::new_v4();
    let path = dir
        .path()
        .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));
    let items = vec![RolloutItem::Compacted(CompactedItem {
        message: "noop".to_string(),
        replacement_history: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    })];

    let builder = builder_from_items(items.as_slice(), path.as_path()).expect("builder");
    let naive = NaiveDateTime::parse_from_str("2026-01-27T12-34-56", "%Y-%m-%dT%H-%M-%S")
        .expect("timestamp");
    let created_at = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        .with_nanosecond(0)
        .expect("nanosecond");
    let expected = ThreadMetadataBuilder::new(
        ThreadId::from_string(&uuid.to_string()).expect("thread id"),
        path,
        created_at,
        SessionSource::default(),
    );

    assert_eq!(builder, expected);
}

#[tokio::test]
async fn backfill_sessions_resumes_from_watermark_and_marks_complete() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let first_uuid = Uuid::new_v4();
    let second_uuid = Uuid::new_v4();
    let first_path = write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        first_uuid,
        /*git*/ None,
    );
    let second_path = write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-35-56",
        "2026-01-27T12:35:56Z",
        second_uuid,
        /*git*/ None,
    );

    let runtime = codex_state::StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let first_watermark = backfill_watermark_for_path(codex_home.as_path(), first_path.as_path());
    runtime.mark_backfill_running().await.expect("mark running");
    runtime
        .checkpoint_backfill(first_watermark.as_str())
        .await
        .expect("checkpoint first watermark");
    // The production lease must still protect a worker after the former
    // one-second test-only lease would have expired.
    {
        use sqlx::Connection;
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(codex_state::state_db_path(&codex_home)),
        )
        .await
        .expect("open fixture state database");
        let changed = sqlx::query("UPDATE backfill_state SET updated_at = ? WHERE id = 1")
            .bind(Utc::now().timestamp() - 2)
            .execute(&mut connection)
            .await
            .expect("backdate active fixture lease");
        assert_eq!(changed.rows_affected(), 1);
        connection.close().await.expect("close fixture connection");
    }
    backfill_sessions(runtime.as_ref(), codex_home.as_path(), "test-provider").await;
    assert_eq!(
        runtime
            .get_backfill_state()
            .await
            .expect("active backfill")
            .status,
        BackfillStatus::Running
    );
    assert_eq!(
        runtime
            .get_thread(ThreadId::from_string(&second_uuid.to_string()).expect("second thread id"))
            .await
            .expect("unprocessed thread"),
        None
    );

    // Expire this fixture explicitly, without changing the default lease for
    // every unit test or waiting out a full lease to exercise recovery.
    backfill_sessions_with_lease(
        runtime.as_ref(),
        codex_home.as_path(),
        "test-provider",
        /*backfill_lease_seconds*/ 0,
    )
    .await;

    let first_id = ThreadId::from_string(&first_uuid.to_string()).expect("first thread id");
    let second_id = ThreadId::from_string(&second_uuid.to_string()).expect("second thread id");
    assert_eq!(
        runtime
            .get_thread(first_id)
            .await
            .expect("get first thread"),
        None
    );
    assert!(
        runtime
            .get_thread(second_id)
            .await
            .expect("get second thread")
            .is_some()
    );

    let state = runtime
        .get_backfill_state()
        .await
        .expect("get backfill state");
    assert_eq!(state.status, BackfillStatus::Complete);
    assert_eq!(
        state.last_watermark,
        Some(backfill_watermark_for_path(
            codex_home.as_path(),
            second_path.as_path()
        ))
    );
    assert!(state.last_success_at.is_some());
}

#[tokio::test]
async fn backfill_sessions_completes_past_unreadable_rollout() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let first_uuid = Uuid::from_u128(1);
    let failed_uuid = Uuid::from_u128(2);
    let last_uuid = Uuid::from_u128(3);
    write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        first_uuid,
        /*git*/ None,
    );
    // A crash between creating a rollout and its first write leaves it empty for good.
    let failed_path = codex_home
        .join("sessions")
        .join(format!("rollout-2026-01-27T12-35-56-{failed_uuid}.jsonl"));
    File::create(&failed_path).expect("create empty failed rollout");
    let last_path = write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-36-56",
        "2026-01-27T12:36:56Z",
        last_uuid,
        /*git*/ None,
    );

    let runtime = codex_state::StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");

    backfill_sessions(runtime.as_ref(), codex_home.as_path(), "test-provider").await;

    let state = runtime
        .get_backfill_state()
        .await
        .expect("get backfill state");
    assert_eq!(state.status, BackfillStatus::Complete);
    assert_eq!(
        state.last_watermark,
        Some(backfill_watermark_for_path(
            codex_home.as_path(),
            last_path.as_path(),
        ))
    );
    let first_id = ThreadId::from_string(&first_uuid.to_string()).expect("first thread id");
    let failed_id = ThreadId::from_string(&failed_uuid.to_string()).expect("failed thread id");
    let last_id = ThreadId::from_string(&last_uuid.to_string()).expect("last thread id");
    assert!(
        runtime
            .get_thread(first_id)
            .await
            .expect("get first")
            .is_some()
    );
    assert_eq!(
        runtime.get_thread(failed_id).await.expect("get failed"),
        None
    );
    assert!(
        runtime
            .get_thread(last_id)
            .await
            .expect("get last")
            .is_some()
    );
}

#[tokio::test]
async fn backfill_sessions_retries_rollout_io_failure_without_losing_checkpoint() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path();
    let ids = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
    let mut paths = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        paths.push(write_rollout_in_sessions(
            home,
            &format!("2026-01-27T12-3{index}-00"),
            &format!("2026-01-27T12:3{index}:00Z"),
            *id,
            None,
        ));
    }
    let runtime = codex_state::StateRuntime::init(home.to_path_buf(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let hidden = home.join("temporarily-unavailable.jsonl");
    let mut moved = false;
    backfill_sessions_until(
        runtime.as_ref(),
        home,
        "test-provider",
        60,
        &mut BackfillMemo::default(),
        || {
            // Discovery has completed. A real file disappearance must not become a permanent
            // skip merely because the bounded open retries are exhausted.
            if !moved {
                std::fs::rename(&paths[1], &hidden).expect("hide middle rollout");
                moved = true;
            }
            false
        },
    )
    .await;
    let state = runtime.get_backfill_state().await.expect("pending state");
    assert_eq!(state.status, BackfillStatus::Pending);
    assert_eq!(
        state.last_watermark,
        Some(format!(
            "sessions/rollout-2026-01-27T12-30-00-{}.jsonl",
            ids[0]
        ))
    );
    for (index, id) in ids.iter().enumerate() {
        let id = ThreadId::from_string(&id.to_string()).expect("thread id");
        assert_eq!(
            runtime.get_thread(id).await.expect("thread").is_some(),
            index != 1
        );
    }
    std::fs::rename(&hidden, &paths[1]).expect("restore middle rollout");
    backfill_sessions(runtime.as_ref(), home, "test-provider").await;
    assert_eq!(
        runtime
            .get_backfill_state()
            .await
            .expect("complete state")
            .status,
        BackfillStatus::Complete
    );
    for id in ids {
        let id = ThreadId::from_string(&id.to_string()).expect("thread id");
        assert!(
            runtime
                .get_thread(id)
                .await
                .expect("recovered thread")
                .is_some()
        );
    }
}

#[tokio::test]
async fn stopped_backfill_checkpoints_progress_and_releases_its_claim() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let uuids = [
        Uuid::from_u128(11),
        Uuid::from_u128(12),
        Uuid::from_u128(13),
    ];
    let paths = uuids
        .iter()
        .enumerate()
        .map(|(index, uuid)| {
            write_rollout_in_sessions(
                codex_home.as_path(),
                &format!("2026-01-27T12-3{index}-56"),
                &format!("2026-01-27T12:3{index}:56Z"),
                *uuid,
                /*git*/ None,
            )
        })
        .collect::<Vec<_>>();
    let thread_ids = uuids.map(|uuid| ThreadId::from_string(&uuid.to_string()).expect("thread id"));
    let runtime = codex_state::StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");

    // Stop after the first rollout, as a startup deadline would between rollouts.
    let mut checks = 0;
    backfill_sessions_until(
        runtime.as_ref(),
        codex_home.as_path(),
        "test-provider",
        BACKFILL_LEASE_SECONDS,
        &mut BackfillMemo::default(),
        || {
            checks += 1;
            checks > 1
        },
    )
    .await;

    let state = runtime
        .get_backfill_state()
        .await
        .expect("stopped backfill state");
    assert_eq!(state.status, BackfillStatus::Pending);
    assert_eq!(
        state.last_watermark,
        Some(backfill_watermark_for_path(
            codex_home.as_path(),
            paths[0].as_path()
        ))
    );
    assert!(
        runtime
            .get_thread(thread_ids[0])
            .await
            .expect("first thread")
            .is_some()
    );
    assert_eq!(
        runtime
            .get_thread(thread_ids[1])
            .await
            .expect("second thread"),
        None
    );

    // The released claim lets the next worker start at once instead of waiting out the lease.
    backfill_sessions(runtime.as_ref(), codex_home.as_path(), "test-provider").await;
    let state = runtime
        .get_backfill_state()
        .await
        .expect("resumed backfill state");
    assert_eq!(state.status, BackfillStatus::Complete);
    for thread_id in thread_ids {
        assert!(
            runtime
                .get_thread(thread_id)
                .await
                .expect("resumed thread")
                .is_some()
        );
    }
}

#[tokio::test]
async fn backfill_sessions_preserves_existing_git_branch_and_fills_missing_git_fields() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let thread_uuid = Uuid::new_v4();
    let rollout_path = write_rollout_in_sessions(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        thread_uuid,
        Some(GitInfo {
            commit_hash: Some(codex_git_utils::GitSha::new("rollout-sha")),
            branch: Some("rollout-branch".to_string()),
            repository_url: Some("git@example.com:openai/codex.git".to_string()),
        }),
    );

    let runtime = codex_state::StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let thread_id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    let mut existing = extract_metadata_from_rollout(&rollout_path, "test-provider")
        .await
        .expect("extract")
        .metadata;
    existing.git_sha = None;
    existing.git_branch = Some("sqlite-branch".to_string());
    existing.git_origin_url = None;
    runtime
        .upsert_thread(&existing)
        .await
        .expect("existing metadata upsert");

    backfill_sessions(runtime.as_ref(), codex_home.as_path(), "test-provider").await;

    let persisted = runtime
        .get_thread(thread_id)
        .await
        .expect("get thread")
        .expect("thread exists");
    assert_eq!(persisted.git_sha.as_deref(), Some("rollout-sha"));
    assert_eq!(persisted.git_branch.as_deref(), Some("sqlite-branch"));
    assert_eq!(
        persisted.git_origin_url.as_deref(),
        Some("git@example.com:openai/codex.git")
    );
}

#[tokio::test]
async fn backfill_sessions_normalizes_cwd_before_upsert() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().to_path_buf();
    let thread_uuid = Uuid::new_v4();
    let session_cwd = codex_home.join(".");
    let rollout_path = write_rollout_in_sessions_with_cwd(
        codex_home.as_path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        thread_uuid,
        session_cwd.clone(),
        /*git*/ None,
    );

    let runtime = codex_state::StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");

    backfill_sessions(runtime.as_ref(), codex_home.as_path(), "test-provider").await;

    let thread_id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    let stored = runtime
        .get_thread(thread_id)
        .await
        .expect("get thread")
        .expect("thread should be backfilled");

    assert_eq!(stored.rollout_path, rollout_path);
    assert_eq!(stored.cwd, normalize_cwd_for_state_db(&session_cwd));
}

fn write_rollout_in_sessions(
    codex_home: &Path,
    filename_ts: &str,
    event_ts: &str,
    thread_uuid: Uuid,
    git: Option<GitInfo>,
) -> PathBuf {
    write_rollout_in_sessions_with_cwd(
        codex_home,
        filename_ts,
        event_ts,
        thread_uuid,
        codex_home.to_path_buf(),
        git,
    )
}

fn write_rollout_in_sessions_with_cwd(
    codex_home: &Path,
    filename_ts: &str,
    event_ts: &str,
    thread_uuid: Uuid,
    cwd: PathBuf,
    git: Option<GitInfo>,
) -> PathBuf {
    let id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
    let sessions_dir = codex_home.join("sessions");
    std::fs::create_dir_all(sessions_dir.as_path()).expect("create sessions dir");
    let path = sessions_dir.join(format!("rollout-{filename_ts}-{thread_uuid}.jsonl"));
    let session_meta = SessionMeta {
        session_id: id.into(),
        id,
        forked_from_id: None,
        parent_thread_id: None,
        timestamp: event_ts.to_string(),
        cwd,
        originator: "cli".to_string(),
        cli_version: "0.0.0".to_string(),
        harness_build: None,
        source: SessionSource::default(),
        thread_source: None,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
        model_provider: Some("test-provider".to_string()),
        base_instructions: None,
        dynamic_tools: None,
        selected_capability_roots: Vec::new(),

        history_mode: Default::default(),
        multi_agent_version: None,
        context_window: None,
    };
    let session_meta_line = SessionMetaLine {
        meta: session_meta,
        git,
    };
    let rollout_line = RolloutLine {
        timestamp: event_ts.to_string(),
        item: RolloutItem::SessionMeta(session_meta_line),
    };
    let json = serde_json::to_string(&rollout_line).expect("serialize rollout");
    let mut file = File::create(&path).expect("create rollout");
    writeln!(file, "{json}").expect("write rollout");
    path
}

#[tokio::test]
async fn backfill_discovery_failure_remains_retryable() {
    let dir = tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    std::fs::write(&sessions, b"not a directory").unwrap();
    let runtime = codex_state::StateRuntime::init(dir.path().to_path_buf(), "test-provider".into())
        .await
        .unwrap();
    backfill_sessions(runtime.as_ref(), dir.path(), "test-provider").await;
    assert_eq!(
        runtime.get_backfill_state().await.unwrap().status,
        BackfillStatus::Pending
    );
    std::fs::remove_file(&sessions).unwrap();
    let id = Uuid::new_v4();
    write_rollout_in_sessions(
        dir.path(),
        "2026-01-27T12-34-56",
        "2026-01-27T12:34:56Z",
        id,
        None,
    );
    backfill_sessions(runtime.as_ref(), dir.path(), "test-provider").await;
    assert_eq!(
        runtime.get_backfill_state().await.unwrap().status,
        BackfillStatus::Complete
    );
    assert!(
        runtime
            .get_thread(ThreadId::from_string(&id.to_string()).unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

/// Four rollouts in watermark order. The second names its session metadata
/// through a payload artifact that is missing, as after copying a home
/// without `rollout-payloads` or losing a payload before it was synced.
struct PayloadGapFixture {
    ids: [ThreadId; 4],
    paths: Vec<PathBuf>,
    parent_id: ThreadId,
    payload: PathBuf,
    payload_bytes: Vec<u8>,
}

fn write_payload_gap_fixture(home: &Path) -> PayloadGapFixture {
    let uuids = [
        Uuid::from_u128(21),
        Uuid::from_u128(22),
        Uuid::from_u128(23),
        Uuid::from_u128(24),
    ];
    let ids = uuids.map(|uuid| ThreadId::from_string(&uuid.to_string()).expect("thread id"));
    let paths = uuids
        .iter()
        .enumerate()
        .map(|(index, uuid)| {
            write_rollout_in_sessions(
                home,
                &format!("2026-01-27T12-3{index}-00"),
                &format!("2026-01-27T12:3{index}:00Z"),
                *uuid,
                /*git*/ None,
            )
        })
        .collect::<Vec<_>>();
    let parent_id = ThreadId::new();
    let session_meta = RolloutLine {
        timestamp: "2026-01-27T12:31:00Z".to_string(),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: ids[1].into(),
                id: ids[1],
                parent_thread_id: Some(parent_id),
                timestamp: "2026-01-27T12:31:00Z".to_string(),
                cwd: home.to_path_buf(),
                originator: "artifact-backed metadata fixture ".repeat(1024),
                model_provider: Some("artifact-provider".to_string()),
                ..SessionMeta::default()
            },
            git: None,
        }),
    };
    let serialized = serde_json::to_vec(&session_meta).expect("serialize session metadata");
    let mut rollout = crate::payload_artifact::store_line(&paths[1], &serialized)
        .expect("publish metadata payload");
    let reference: serde_json::Value = serde_json::from_slice(&rollout).expect("payload reference");
    assert_eq!(reference["type"], crate::payload_artifact::KIND);
    let payload = crate::payload_artifact::root(&paths[1]).join(format!(
        "{}.json",
        reference["payload"]["sha256"]
            .as_str()
            .expect("payload digest")
    ));
    let user_message = RolloutLine {
        timestamp: "2026-01-27T12:31:01Z".to_string(),
        item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "inline preview".to_string(),
            ..Default::default()
        })),
    };
    rollout.extend(serde_json::to_vec(&user_message).expect("serialize user message"));
    rollout.push(b'\n');
    std::fs::write(&paths[1], rollout).expect("write referencing rollout");
    let payload_bytes = std::fs::read(&payload).expect("published payload");
    std::fs::remove_file(&payload).expect("lose the payload");
    PayloadGapFixture {
        ids,
        paths,
        parent_id,
        payload,
        payload_bytes,
    }
}

fn append_user_message(path: &Path, message: &str) {
    let line = RolloutLine {
        timestamp: "2026-01-27T12:40:00Z".to_string(),
        item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: message.to_string(),
            ..Default::default()
        })),
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open rollout for append");
    writeln!(
        file,
        "{}",
        serde_json::to_string(&line).expect("serialize line")
    )
    .expect("append line");
}

/// One unbounded pass sharing `memo`, as consecutive refreshes of one runtime do.
async fn memo_pass(
    runtime: &codex_state::StateRuntime,
    home: &Path,
    memo: &mut BackfillMemo,
) -> BackfillPassReport {
    backfill_sessions_until(
        runtime,
        home,
        "test-provider",
        BACKFILL_LEASE_SECONDS,
        memo,
        || false,
    )
    .await
}

#[tokio::test]
async fn refresh_pass_rereads_nothing_while_a_payload_gap_is_unchanged() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path();
    let fixture = write_payload_gap_fixture(home);
    let runtime = codex_state::StateRuntime::init(home.to_path_buf(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let before_gap = backfill_watermark_for_path(home, &fixture.paths[0]);
    let mut memo = BackfillMemo::default();

    assert_eq!(
        memo_pass(runtime.as_ref(), home, &mut memo).await,
        BackfillPassReport {
            extracted: 4,
            unchanged: 0
        }
    );
    // A newly seen gap is read once more, after the payloads it names are stamped.
    assert_eq!(
        memo_pass(runtime.as_ref(), home, &mut memo).await,
        BackfillPassReport {
            extracted: 1,
            unchanged: 2
        }
    );
    // Nothing changed since, so a pass reads no rollout. Without the memo every
    // pass re-read the gap and every rollout after it.
    for _ in 0..2 {
        assert_eq!(
            memo_pass(runtime.as_ref(), home, &mut memo).await,
            BackfillPassReport {
                extracted: 0,
                unchanged: 3
            }
        );
        // The gap still may not certify lineage: it keeps the checkpoint before it.
        let state = runtime.get_backfill_state().await.expect("backfill state");
        assert_eq!(state.status, BackfillStatus::Pending);
        assert_eq!(state.last_watermark.as_deref(), Some(before_gap.as_str()));
    }
    let provisional = runtime
        .get_thread(fixture.ids[1])
        .await
        .expect("get gap thread")
        .expect("the provisional row stays indexed");
    assert_eq!(
        provisional.first_user_message.as_deref(),
        Some("inline preview")
    );
    assert!(
        runtime
            .list_thread_spawn_children(fixture.parent_id)
            .await
            .expect("list children")
            .is_empty()
    );
    runtime.close().await;
}

#[tokio::test]
async fn refresh_pass_still_reads_changed_rollouts_and_repaired_payloads() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path();
    let fixture = write_payload_gap_fixture(home);
    let runtime = codex_state::StateRuntime::init(home.to_path_buf(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let mut memo = BackfillMemo::default();
    // Reach the steady state: the first pass, then the gap's confirming read.
    memo_pass(runtime.as_ref(), home, &mut memo).await;
    memo_pass(runtime.as_ref(), home, &mut memo).await;

    // An append after reconciliation is read again, and nothing else is.
    append_user_message(&fixture.paths[3], "appended after reconciliation");
    assert_eq!(
        memo_pass(runtime.as_ref(), home, &mut memo).await,
        BackfillPassReport {
            extracted: 1,
            unchanged: 2
        }
    );
    let appended = runtime
        .get_thread(fixture.ids[3])
        .await
        .expect("get appended thread")
        .expect("appended thread is indexed");
    assert_eq!(
        appended.first_user_message.as_deref(),
        Some("appended after reconciliation")
    );

    // Restoring the payload changes no rollout byte, yet the gap is read again.
    std::fs::write(&fixture.payload, &fixture.payload_bytes).expect("restore payload");
    assert_eq!(
        memo_pass(runtime.as_ref(), home, &mut memo).await,
        BackfillPassReport {
            extracted: 1,
            unchanged: 2
        }
    );
    let state = runtime.get_backfill_state().await.expect("backfill state");
    assert_eq!(state.status, BackfillStatus::Complete);
    assert_eq!(
        state.last_watermark,
        Some(backfill_watermark_for_path(home, &fixture.paths[3]))
    );
    assert_eq!(
        runtime
            .list_thread_spawn_children(fixture.parent_id)
            .await
            .expect("list children"),
        vec![fixture.ids[1]]
    );
    let repaired = runtime
        .get_thread(fixture.ids[1])
        .await
        .expect("get repaired thread")
        .expect("repaired thread is indexed");
    assert_eq!(repaired.model_provider, "artifact-provider");

    // The periodic refresh reopens complete coverage; unchanged rollouts are not read.
    runtime
        .request_backfill_refresh(/*minimum_age_seconds*/ 0)
        .await
        .expect("reopen coverage");
    assert_eq!(
        memo_pass(runtime.as_ref(), home, &mut memo).await,
        BackfillPassReport {
            extracted: 0,
            unchanged: 4
        }
    );
    assert_eq!(
        runtime
            .get_backfill_state()
            .await
            .expect("backfill state")
            .status,
        BackfillStatus::Complete
    );
    runtime.close().await;
}

#[tokio::test]
async fn pass_remainder_confirms_a_payload_gap_only_after_an_unchanged_reread() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path();
    let fixture = write_payload_gap_fixture(home);
    let runtime = codex_state::StateRuntime::init(home.to_path_buf(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let mut memo = BackfillMemo::default();

    // A gap read once may be a transient payload read failure.
    memo_pass(runtime.as_ref(), home, &mut memo).await;
    assert_eq!(
        memo.last_pass_remainder(),
        Some(PassRemainder::UnconfirmedPayloadGaps)
    );
    // Read again at the same version with its payloads stamped first, it is
    // confirmed, and stays confirmed while nothing it read changes.
    for _ in 0..2 {
        memo_pass(runtime.as_ref(), home, &mut memo).await;
        assert_eq!(
            memo.last_pass_remainder(),
            Some(PassRemainder::ConfirmedPayloadGaps(1))
        );
    }

    // A pass stopped after the gap leaves the rollouts after it unverified.
    let mut checks = 0;
    backfill_sessions_until(
        runtime.as_ref(),
        home,
        "test-provider",
        BACKFILL_LEASE_SECONDS,
        &mut memo,
        || {
            checks += 1;
            checks > 1
        },
    )
    .await;
    assert_eq!(memo.last_pass_remainder(), Some(PassRemainder::Unresolved));

    // Rollouts that discovery could not list may need any repair.
    let archived = home.join(crate::ARCHIVED_SESSIONS_SUBDIR);
    std::fs::write(&archived, b"not a directory").expect("block archived discovery");
    memo_pass(runtime.as_ref(), home, &mut memo).await;
    assert_eq!(memo.last_pass_remainder(), Some(PassRemainder::Unresolved));
    std::fs::remove_file(&archived).expect("restore archived discovery");

    // A gap rollout that changed must be confirmed again.
    append_user_message(&fixture.paths[1], "appended to the gap");
    memo_pass(runtime.as_ref(), home, &mut memo).await;
    assert_eq!(
        memo.last_pass_remainder(),
        Some(PassRemainder::UnconfirmedPayloadGaps)
    );

    // A call that cannot claim the lease reports no pass at all.
    assert!(
        runtime
            .try_claim_backfill(BACKFILL_LEASE_SECONDS)
            .await
            .expect("claim backfill")
    );
    memo_pass(runtime.as_ref(), home, &mut memo).await;
    assert_eq!(memo.last_pass_remainder(), None);
    runtime.close().await;
}
