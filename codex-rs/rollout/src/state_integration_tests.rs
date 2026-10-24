#![allow(warnings, clippy::all)]

//! Contract tests for rollout-to-SQLite integration.

use super::*;
use crate::list::parse_cursor;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Timelike;
use chrono::Utc;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;

#[test]
fn cursor_to_anchor_normalizes_timestamp_format() {
    let ts_str = "2026-01-27T12-34-56";
    let cursor = parse_cursor(ts_str).expect("cursor should parse");
    let anchor = cursor_to_anchor(Some(&cursor)).expect("anchor should parse");

    let naive =
        NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H-%M-%S").expect("ts should parse");
    let expected_ts = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        .with_nanosecond(0)
        .expect("nanosecond");

    assert_eq!(anchor.ts, expected_ts);
    assert_eq!(anchor.id, None);
}

#[test]
fn cursor_to_anchor_preserves_recency_tie_breaker() {
    let id = ThreadId::from_string("00000000-0000-0000-0000-000000000123")
        .expect("thread id should parse");
    let token = format!("2026-01-27T12:34:56Z|{id}");
    let cursor = parse_cursor(&token).expect("cursor should parse");
    let anchor = cursor_to_anchor(Some(&cursor)).expect("anchor should parse");

    assert_eq!(anchor.id, Some(id));
    assert_eq!(
        serde_json::to_string(&cursor).expect("cursor should serialize"),
        format!("\"{token}\"")
    );
}

#[tokio::test]
async fn try_init_waits_for_concurrent_startup_backfill() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;
    let claimed = runtime.claim_backfill(/*lease_seconds*/ 60).await?.expect("claim");

    // Completion is observed by the next database poll; a short interval keeps
    // the production wait bound without spending the one-second cadence.
    let mut init = Box::pin(try_init_with_roots_and_backfill_lease(
        home.path().to_path_buf(),
        home.path().to_path_buf(),
        "test-provider".to_string(),
        /*backfill_lease_seconds*/ 60,
        STARTUP_BACKFILL_WAIT_TIMEOUT,
        /*poll_interval*/ std::time::Duration::from_millis(10),
    ));
    // The other worker still holds its claim, so init must not have returned yet.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), init.as_mut())
            .await
            .is_err(),
        "init must keep waiting while another worker's backfill is running"
    );
    runtime
        .complete_backfill_claim(&claimed, /*last_watermark*/ None)
        .await?;
    let initialized = init.await?;
    assert_eq!(
        initialized.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Complete
    );

    Ok(())
}

#[tokio::test]
async fn try_init_times_out_waiting_for_stuck_startup_backfill() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;
    let claimed = runtime.try_claim_backfill(/*lease_seconds*/ 60).await?;
    assert!(claimed);

    // The unexpired lease keeps this backfill stuck, so any bound must expire.
    // The production 30-second bound itself is covered with paused time below.
    let result = try_init_with_roots_and_backfill_lease(
        home.path().to_path_buf(),
        home.path().to_path_buf(),
        "test-provider".to_string(),
        /*backfill_lease_seconds*/ 60,
        /*wait_timeout*/ std::time::Duration::from_millis(200),
        /*poll_interval*/ std::time::Duration::from_millis(10),
    )
    .await;
    let err = match result {
        Ok(_) => panic!("state db init should not wait forever for incomplete backfill"),
        Err(err) => err,
    };
    assert!(
        err.to_string()
            .contains("timed out waiting for state db backfill"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[tokio::test]
async fn refreshed_index_stays_available_while_another_worker_reconciles() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    // A periodic refresh reopens reconciliation and another process claims it.
    runtime
        .request_backfill_refresh(/*minimum_age_seconds*/ 0)
        .await?;
    assert!(runtime.try_claim_backfill(/*lease_seconds*/ 60).await?);

    // The same claim makes a never-certified index wait out its bound (above).
    let initialized = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        try_init_with_roots_and_backfill_lease(
            home.path().to_path_buf(),
            home.path().to_path_buf(),
            "test-provider".to_string(),
            /*backfill_lease_seconds*/ 60,
            /*wait_timeout*/ std::time::Duration::from_millis(200),
            /*poll_interval*/ std::time::Duration::from_millis(10),
        ),
    )
    .await
    .expect("a certified index must not wait for its refresh")?;
    assert_eq!(
        initialized.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Running
    );
    assert!(
        require_backfill_complete(initialized, home.path())
            .await
            .is_some(),
        "optional readers keep a certified index during refresh"
    );

    Ok(())
}

#[tokio::test(start_paused = true)]
async fn startup_backfill_timeout_covers_in_flight_work() {
    let home = TempDir::new().expect("temp dir");
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(31),
        wait_for_backfill_gate_with_timeout(
            home.path(),
            STARTUP_BACKFILL_WAIT_TIMEOUT,
            std::future::pending::<anyhow::Result<()>>(),
        ),
    )
    .await
    .expect("backfill gate should enforce its own timeout");
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(30));
    let err = result.expect_err("pending backfill work should time out");
    assert!(
        err.to_string()
            .contains("timed out waiting for state db backfill"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn startup_gate_releases_its_claim_when_its_bound_stops_backfill() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::new();
    write_rollout_with_user_message(home.path(), thread_id, "resume me")?;
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;

    // An exhausted bound stops this startup's own backfill before its first rollout.
    let stopped = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait_for_backfill_gate(
            runtime.as_ref(),
            home.path(),
            "test-provider",
            /*backfill_lease_seconds*/ Some(60),
            /*wait_timeout*/ std::time::Duration::ZERO,
            /*poll_interval*/ std::time::Duration::from_millis(10),
            &mut metadata::BackfillMemo::default(),
        ),
    )
    .await
    .expect("a stopped backfill must not leave the gate polling its own claim");
    let err = stopped.expect_err("an exhausted bound leaves the backfill incomplete");
    assert!(
        err.to_string().contains("remains incomplete"),
        "unexpected error: {err}"
    );
    assert_eq!(
        runtime.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Pending
    );

    // The next startup claims at once instead of waiting out the 60-second lease.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait_for_backfill_gate(
            runtime.as_ref(),
            home.path(),
            "test-provider",
            /*backfill_lease_seconds*/ Some(60),
            STARTUP_BACKFILL_WAIT_TIMEOUT,
            /*poll_interval*/ std::time::Duration::from_millis(10),
            &mut metadata::BackfillMemo::default(),
        ),
    )
    .await
    .expect("a released claim must not block the next startup")?;
    assert!(runtime.get_thread(thread_id).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn periodic_refresh_reads_only_rollouts_changed_since_startup() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    write_rollout_with_user_message(home.path(), ThreadId::new(), "unchanged")?;
    let changed = ThreadId::new();
    let changed_path = write_rollout_with_user_message(home.path(), changed, "dropped")?;
    // Keep only its session metadata, so a message appended later is observable.
    let session_meta = std::fs::read_to_string(&changed_path)?
        .lines()
        .next()
        .map(str::to_owned)
        .expect("session metadata line");
    std::fs::write(&changed_path, format!("{session_meta}\n"))?;
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;
    // Startup and the periodic refresh share one runtime's memo, as `try_init` wires them.
    let mut memo = metadata::BackfillMemo::default();
    wait_for_backfill_gate(
        runtime.as_ref(),
        home.path(),
        "test-provider",
        /*backfill_lease_seconds*/ Some(60),
        STARTUP_BACKFILL_WAIT_TIMEOUT,
        /*poll_interval*/ std::time::Duration::from_millis(10),
        &mut memo,
    )
    .await?;
    assert_eq!(
        runtime.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Complete
    );
    assert_eq!(
        runtime
            .get_thread(changed)
            .await?
            .expect("indexed at startup")
            .first_user_message,
        None
    );

    // Coverage completed an interval ago is reopened, but no unchanged rollout
    // is read again. Before, every refresh re-read every rollout.
    age_complete_backfill(home.path()).await?;
    let report = refresh_backfill_once(runtime.as_ref(), home.path(), "test-provider", &mut memo)
        .await
        .expect("refresh pass");
    assert_eq!((report.extracted, report.unchanged), (0, 2));
    assert_eq!(
        runtime.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Complete
    );

    // A rollout appended after startup is still reconciled by the next refresh.
    {
        use std::io::Write;
        let line = RolloutLine {
            timestamp: "2026-06-01T14:30:00Z".to_string(),
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: "after startup".to_string(),
                ..Default::default()
            })),
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&changed_path)?;
        writeln!(file, "{}", serde_json::to_string(&line)?)?;
    }
    age_complete_backfill(home.path()).await?;
    let report = refresh_backfill_once(runtime.as_ref(), home.path(), "test-provider", &mut memo)
        .await
        .expect("refresh pass");
    assert_eq!((report.extracted, report.unchanged), (1, 1));
    assert_eq!(
        runtime
            .get_thread(changed)
            .await?
            .expect("still indexed")
            .first_user_message
            .as_deref(),
        Some("after startup")
    );
    assert_eq!(
        runtime.get_backfill_state().await?.status,
        codex_state::BackfillStatus::Complete
    );
    Ok(())
}

/// Ages complete coverage past the refresh interval instead of waiting it out.
async fn age_complete_backfill(home: &Path) -> anyhow::Result<()> {
    use sqlx::Connection;
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(codex_state::state_db_path(home)),
    )
    .await?;
    let aged = sqlx::query("UPDATE backfill_state SET updated_at = updated_at - 61 WHERE id = 1")
        .execute(&mut connection)
        .await?;
    anyhow::ensure!(aged.rows_affected() == 1, "backfill state row is missing");
    connection.close().await?;
    Ok(())
}

#[tokio::test]
async fn startup_serves_an_uncertified_index_when_only_confirmed_payload_gaps_remain()
-> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let gap = ThreadId::new();
    let parent = ThreadId::new();
    write_payload_gap_rollout(home.path(), gap, parent)?;
    let after_gap = ThreadId::new();
    write_rollout_with_user_message(home.path(), after_gap, "after the gap")?;

    // Coverage was never certified, so before, this startup and every later one
    // indexed the rollouts and then fell back without the database.
    let runtime = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        try_init_with_roots_and_backfill_lease(
            home.path().to_path_buf(),
            home.path().to_path_buf(),
            "test-provider".to_string(),
            /*backfill_lease_seconds*/ 60,
            STARTUP_BACKFILL_WAIT_TIMEOUT,
            /*poll_interval*/ std::time::Duration::from_millis(10),
        ),
    )
    .await
    .expect("a permanent payload gap must not hold startup to its bound")?;

    // Serving certifies nothing: the gap keeps the checkpoint before it and its
    // lineage unsettled, while the rows indexed so far are served.
    let state = runtime.get_backfill_state().await?;
    assert_eq!(state.status, codex_state::BackfillStatus::Pending);
    assert_eq!(state.last_success_at, None);
    assert_eq!(state.last_watermark, None);
    let provisional = runtime
        .get_thread(gap)
        .await?
        .expect("the gap keeps its provisional row");
    assert_eq!(
        provisional.first_user_message.as_deref(),
        Some("inline preview")
    );
    assert!(runtime.list_thread_spawn_children(parent).await?.is_empty());
    assert!(runtime.get_thread(after_gap).await?.is_some());
    assert!(
        require_backfill_complete(runtime, home.path())
            .await
            .is_none(),
        "optional readers still require certified coverage"
    );
    Ok(())
}

#[tokio::test]
async fn startup_gate_stays_closed_while_a_retryable_failure_remains_beside_a_payload_gap()
-> anyhow::Result<()> {
    // A retryable read error: a record in a format this build cannot read yet.
    // Only `InvalidData` settles a rollout as unreadable.
    let unreadable = TempDir::new().expect("temp dir");
    write_payload_gap_rollout(unreadable.path(), ThreadId::new(), ThreadId::new())?;
    let newer = write_rollout_with_user_message(unreadable.path(), ThreadId::new(), "newer")?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&newer)?;
        writeln!(
            file,
            "{}",
            serde_json::json!({"timestamp": "2026-06-01T14:26:27Z", "format_version": 999})
        )?;
    }

    // A rejected reconciliation, as when a live writer updates the row during
    // extraction. Here another id already indexes the rollout's path, so every
    // snapshot is stale.
    let rejected = TempDir::new().expect("temp dir");
    write_payload_gap_rollout(rejected.path(), ThreadId::new(), ThreadId::new())?;
    let contested = write_rollout_with_user_message(rejected.path(), ThreadId::new(), "contested")?;
    let runtime =
        codex_state::StateRuntime::init(rejected.path().to_path_buf(), "test-provider".to_string())
            .await?;
    let mut claimant =
        metadata::extract_metadata_from_rollout(contested.as_path(), "test-provider")
            .await?
            .metadata;
    claimant.id = ThreadId::new();
    // Spell the path as discovery does, so the backfill finds this claim by path.
    claimant.rollout_path = contested.components().collect();
    runtime.upsert_thread(&claimant).await?;

    for (home, case) in [
        (unreadable.path(), "unsupported record"),
        (rejected.path(), "stale snapshot"),
    ] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            try_init_with_roots_and_backfill_lease(
                home.to_path_buf(),
                home.to_path_buf(),
                "test-provider".to_string(),
                /*backfill_lease_seconds*/ 60,
                STARTUP_BACKFILL_WAIT_TIMEOUT,
                /*poll_interval*/ std::time::Duration::from_millis(10),
            ),
        )
        .await
        .expect("startup must finish within its bound");
        let err = match result {
            Ok(_) => panic!("{case}: a retryable failure must keep the startup gate closed"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("remains incomplete"),
            "{case}: unexpected error: {err}"
        );
        let reopened =
            codex_state::StateRuntime::init(home.to_path_buf(), "test-provider".to_string())
                .await?;
        let state = reopened.get_backfill_state().await?;
        assert_eq!(
            (state.status, state.last_success_at),
            (codex_state::BackfillStatus::Pending, None),
            "{case}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn reconcile_rollout_preserves_existing_explicit_title() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::new();
    let rollout_path = write_rollout_with_user_message(home.path(), thread_id, "Hey")?;
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;

    let mut metadata =
        metadata::extract_metadata_from_rollout(rollout_path.as_path(), "test-provider")
            .await?
            .metadata;
    assert_eq!(metadata.title, "Hey");
    assert_eq!(metadata.first_user_message.as_deref(), Some("Hey"));
    metadata.title = "math".to_string();
    runtime.upsert_thread(&metadata).await?;

    assert!(
        reconcile_rollout(
            Some(runtime.as_ref()),
            rollout_path.as_path(),
            "test-provider",
            /*builder*/ None,
            &[],
            /*archived_only*/ Some(false),
        )
        .await
    );

    let persisted = runtime
        .get_thread(thread_id)
        .await?
        .expect("thread should exist");
    assert_eq!(persisted.title, "math");
    assert_eq!(persisted.first_user_message.as_deref(), Some("Hey"));
    Ok(())
}

#[tokio::test]
async fn slow_read_repair_uses_the_full_rollout_as_provider_authority() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::new();
    let rollout_path = write_rollout_with_user_message(home.path(), thread_id, "repair me")?;
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "fallback-provider".to_string())
            .await?;

    read_repair_rollout_path(
        Some(runtime.as_ref()),
        Some(thread_id),
        Some(false),
        rollout_path.as_path(),
    )
    .await;

    let repaired = runtime
        .get_thread(thread_id)
        .await?
        .expect("slow repair should create the missing row");
    assert_eq!(repaired.model_provider, "test-provider");
    Ok(())
}

#[tokio::test]
async fn list_threads_db_preserves_order_while_filtering_rollout_paths() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;
    let base_time = "2026-06-01T14:26:25Z".parse::<DateTime<Utc>>()?;
    let mut expected_ids = Vec::new();
    let mut expected_compressed_path = None;

    for index in 0..12 {
        let thread_id = ThreadId::new();
        let rollout_path =
            write_rollout_with_user_message(home.path(), thread_id, &format!("message {index}"))?;
        let mut thread =
            metadata::extract_metadata_from_rollout(rollout_path.as_path(), "test-provider")
                .await?
                .metadata;
        let timestamp = base_time + chrono::Duration::seconds(index);
        thread.created_at = timestamp;
        thread.updated_at = timestamp;
        thread.recency_at = timestamp;
        runtime.upsert_thread(&thread).await?;

        if index == 3 {
            std::fs::remove_file(rollout_path)?;
        } else {
            expected_ids.push(thread_id);
            if index == 7 {
                let compressed_path =
                    crate::compression::compressed_rollout_path(rollout_path.as_path());
                std::fs::rename(rollout_path, compressed_path.as_path())?;
                expected_compressed_path = Some((thread_id, compressed_path));
            }
        }
    }

    let page = list_threads_db(
        Some(runtime.as_ref()),
        home.path(),
        20,
        /*cursor*/ None,
        ThreadSortKey::CreatedAt,
        SortDirection::Asc,
        &[SessionSource::Cli],
        /*model_providers*/ None,
        /*cwd_filters*/ None,
        /*relation_filter*/ None,
        /*archived*/ false,
        /*project_id*/ None,
        /*search_term*/ None,
    )
    .await
    .expect("thread listing should succeed");

    assert_eq!(
        page.items.iter().map(|item| item.id).collect::<Vec<_>>(),
        expected_ids
    );
    let (compressed_id, compressed_path) =
        expected_compressed_path.expect("compressed fixture should be recorded");
    assert_eq!(
        page.items
            .iter()
            .find(|item| item.id == compressed_id)
            .map(|item| item.rollout_path.clone()),
        Some(compressed_path)
    );
    Ok(())
}

fn write_rollout_with_user_message(
    home: &Path,
    thread_id: ThreadId,
    message: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = home.join("sessions/2026/06/01");
    std::fs::create_dir_all(dir.as_path())?;
    let path = dir.join(format!("rollout-2026-06-01T14-26-25-{thread_id}.jsonl"));
    let lines = [
        RolloutLine {
            timestamp: "2026-06-01T14:26:25Z".to_string(),
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    session_id: thread_id.into(),
                    id: thread_id,
                    forked_from_id: None,
                    parent_thread_id: None,
                    timestamp: "2026-06-01T14:26:25Z".to_string(),
                    cwd: home.to_path_buf(),
                    originator: "test".to_string(),
                    cli_version: "test".to_string(),
                    harness_build: None,
                    source: SessionSource::Cli,
                    thread_source: None,
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    model_provider: Some("test-provider".to_string()),
                    base_instructions: None,
                    dynamic_tools: None,
                    selected_capability_roots: Vec::new(),

                    history_mode: Default::default(),
                    multi_agent_version: None,
                    context_window: None,
                },
                git: None,
            }),
        },
        RolloutLine {
            timestamp: "2026-06-01T14:26:26Z".to_string(),
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
    std::fs::write(path.as_path(), format!("{jsonl}\n"))?;
    Ok(path)
}

/// A rollout whose session metadata lives in a payload artifact that is missing,
/// as after copying a home without `rollout-payloads`. Its inline user message
/// keeps a provisional row indexable. It sorts before the rollouts
/// `write_rollout_with_user_message` writes.
fn write_payload_gap_rollout(
    home: &Path,
    thread_id: ThreadId,
    parent_thread_id: ThreadId,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = home.join("sessions/2026/05/31");
    std::fs::create_dir_all(dir.as_path())?;
    let path = dir.join(format!("rollout-2026-05-31T09-00-00-{thread_id}.jsonl"));
    let session_meta = RolloutLine {
        timestamp: "2026-05-31T09:00:00Z".to_string(),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                parent_thread_id: Some(parent_thread_id),
                timestamp: "2026-05-31T09:00:00Z".to_string(),
                cwd: home.to_path_buf(),
                // Large enough to be stored as a payload artifact.
                originator: "payload-backed session metadata ".repeat(1024),
                model_provider: Some("test-provider".to_string()),
                ..SessionMeta::default()
            },
            git: None,
        }),
    };
    let mut rollout =
        crate::payload_artifact::store_line(&path, &serde_json::to_vec(&session_meta)?)?;
    let reference: serde_json::Value = serde_json::from_slice(&rollout)?;
    let digest = reference["payload"]["sha256"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("session metadata was not stored as a payload"))?;
    let payload = crate::payload_artifact::root(&path).join(format!("{digest}.json"));
    let user_message = RolloutLine {
        timestamp: "2026-05-31T09:00:01Z".to_string(),
        item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "inline preview".to_string(),
            ..Default::default()
        })),
    };
    rollout.extend(serde_json::to_vec(&user_message)?);
    rollout.push(b'\n');
    std::fs::write(path.as_path(), rollout)?;
    std::fs::remove_file(payload)?;
    Ok(path)
}
