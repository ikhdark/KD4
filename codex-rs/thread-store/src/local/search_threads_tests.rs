use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_rollout::ThreadItem;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use uuid::Uuid;

use super::ThreadSearchItem;
use super::cursor_from_thread_search_item;
use crate::SearchThreadsParams;
use crate::SortDirection;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::local::LocalThreadStore;
use crate::local::test_support::test_config;
use crate::local::test_support::write_session_file;

#[test]
fn recency_cursor_includes_thread_id_tie_breaker() {
    let thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000123")
        .expect("thread ID should parse");
    let item = ThreadSearchItem {
        item: ThreadItem {
            thread_id: Some(thread_id),
            recency_at: Some("2026-01-27T12:34:56Z".to_string()),
            ..Default::default()
        },
        snippet: String::new(),
    };

    let cursor = cursor_from_thread_search_item(&item, ThreadSortKey::RecencyAt)
        .expect("cursor should build");

    assert_eq!(
        serde_json::to_string(&cursor).expect("cursor should serialize"),
        format!("\"2026-01-27T12:34:56Z|{thread_id}\"")
    );
}

#[tokio::test]
async fn search_threads_falls_back_to_legacy_name_for_default_sqlite_title() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let uuid = Uuid::from_u128(401);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let rollout_path =
        write_session_file(home.path(), "2025-01-03T12-30-00", uuid).expect("session file");
    codex_rollout::append_thread_name(home.path(), thread_id, "Legacy chosen name")
        .await
        .expect("write legacy name");

    let runtime = codex_state::StateRuntime::init(
        home.path().to_path_buf(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    runtime
        .mark_backfill_complete(/*last_watermark*/ None)
        .await
        .expect("backfill should be complete");
    let mut builder = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        rollout_path,
        Utc::now(),
        SessionSource::Cli,
    );
    builder.model_provider = Some(config.default_model_provider_id.clone());
    builder.cwd = home.path().to_path_buf();
    builder.cli_version = Some("test_version".to_string());
    let mut metadata = builder.build(config.default_model_provider_id.as_str());
    metadata.title = "Hello from user".to_string();
    metadata.first_user_message = Some("Hello from user".to_string());
    metadata.preview = metadata.first_user_message.clone();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("state db upsert should succeed");
    let store = LocalThreadStore::new(config, Some(runtime));

    let page = store
        .search_threads(SearchThreadsParams {
            page_size: 10,
            cursor: None,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: SortDirection::Desc,
            allowed_sources: Vec::new(),
            archived: false,
            search_term: "Hello from user".to_string(),
        })
        .await
        .expect("thread search");

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].thread.thread_id, thread_id);
    assert_eq!(
        page.items[0].thread.name.as_deref(),
        Some("Legacy chosen name")
    );
}
