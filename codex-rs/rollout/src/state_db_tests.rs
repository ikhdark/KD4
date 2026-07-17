#![allow(warnings, clippy::all)]

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
async fn try_init_returns_write_ready_while_concurrent_backfill_is_stuck() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let runtime =
        codex_state::StateRuntime::init(home.path().to_path_buf(), "test-provider".to_string())
            .await?;
    let claimed = runtime.try_claim_backfill(/*lease_seconds*/ 60).await?;
    assert!(claimed);

    let initialized = try_init_with_roots_and_backfill_lease(
        home.path().to_path_buf(),
        home.path().to_path_buf(),
        "test-provider".to_string(),
        /*backfill_lease_seconds*/ 60,
    )
    .await?;
    assert_eq!(
        readiness(initialized.as_ref()).await?,
        StateDbReadiness::WriteReady
    );

    let thread_id = ThreadId::new();
    let builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        home.path().join("sessions/current.jsonl"),
        Utc::now(),
        SessionSource::Cli,
    );
    initialized
        .upsert_thread(&builder.build("test-provider"))
        .await?;
    assert!(initialized.get_thread(thread_id).await?.is_some());

    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await?;
    assert_eq!(
        readiness(initialized.as_ref()).await?,
        StateDbReadiness::HistoricalIndexComplete
    );

    Ok(())
}

#[tokio::test]
async fn try_init_backfills_existing_rollout_in_background() -> anyhow::Result<()> {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::new();
    let rollout_path = write_rollout_with_user_message(home.path(), thread_id, "background")?;

    let initialized = try_init_with_roots(
        home.path().to_path_buf(),
        home.path().to_path_buf(),
        "test-provider".to_string(),
    )
    .await?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if readiness(initialized.as_ref()).await?
            == StateDbReadiness::HistoricalIndexComplete
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "historical backfill did not complete"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let metadata = initialized
        .get_thread(thread_id)
        .await?
        .expect("background backfill should index rollout");
    assert_eq!(metadata.rollout_path, rollout_path);
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

    reconcile_rollout(
        Some(runtime.as_ref()),
        rollout_path.as_path(),
        "test-provider",
        /*builder*/ None,
        &[],
        /*archived_only*/ Some(false),
        /*new_thread_memory_mode*/ None,
    )
    .await;

    let persisted = runtime
        .get_thread(thread_id)
        .await?
        .expect("thread should exist");
    assert_eq!(persisted.title, "math");
    assert_eq!(persisted.first_user_message.as_deref(), Some("Hey"));
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
                    source: SessionSource::Cli,
                    thread_source: None,
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    model_provider: Some("test-provider".to_string()),
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
