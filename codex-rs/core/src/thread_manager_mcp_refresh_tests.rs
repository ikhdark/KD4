use super::*;
use crate::config::test_config;
use codex_exec_server::EnvironmentManager;
use codex_protocol::protocol::McpServerRefreshConfig;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

fn refresh_config(server: &str) -> McpServerRefreshConfig {
    McpServerRefreshConfig {
        mcp_servers: serde_json::json!({server: {"command": server}}),
        mcp_oauth_credentials_store_mode: serde_json::json!("auto"),
        auth_keyring_backend_kind: serde_json::json!("direct"),
    }
}

#[tokio::test]
async fn atomic_mcp_refresh_commits_without_thread_submissions() {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");

    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(EnvironmentManager::default_for_tests()),
    );
    let first = manager
        .start_thread(config.clone())
        .await
        .expect("start first thread");
    let second = manager
        .start_thread(config)
        .await
        .expect("start second thread");
    let first_refresh = refresh_config("first");
    let second_refresh = refresh_config("second");

    second
        .thread
        .shutdown_and_wait()
        .await
        .expect("terminate second thread");
    manager
        .queue_mcp_server_refreshes_atomically(vec![
            (
                first.thread_id,
                Arc::clone(&first.thread),
                first_refresh.clone(),
            ),
            (
                second.thread_id,
                Arc::clone(&second.thread),
                second_refresh.clone(),
            ),
        ])
        .await
        .expect("refresh batch should not depend on thread submission channels");
    assert_eq!(
        *first
            .thread
            .codex
            .session
            .lock_pending_mcp_server_refresh_config()
            .await,
        Some(first_refresh.clone())
    );
    assert_eq!(
        *second
            .thread
            .codex
            .session
            .lock_pending_mcp_server_refresh_config()
            .await,
        Some(second_refresh.clone())
    );

    let next_first_refresh = refresh_config("next-first");
    let next_second_refresh = refresh_config("next-second");
    let mismatched_thread_id = ThreadId::new();
    let err = manager
        .queue_mcp_server_refreshes_atomically(vec![
            (
                first.thread_id,
                Arc::clone(&first.thread),
                next_first_refresh,
            ),
            (
                mismatched_thread_id,
                Arc::clone(&second.thread),
                next_second_refresh,
            ),
        ])
        .await
        .expect_err("invalid refresh batch should fail before commit");

    assert!(matches!(err, CodexErr::InvalidRequest(_)));
    assert_eq!(
        *first
            .thread
            .codex
            .session
            .lock_pending_mcp_server_refresh_config()
            .await,
        Some(first_refresh)
    );
    assert_eq!(
        *second
            .thread
            .codex
            .session
            .lock_pending_mcp_server_refresh_config()
            .await,
        Some(second_refresh)
    );
}
