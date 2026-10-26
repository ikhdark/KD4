use crate::config_manager::ConfigManager;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_protocol::protocol::McpServerRefreshConfig;
use futures::StreamExt;
use futures::TryStreamExt;
use std::io;
use std::sync::Arc;
use tracing::warn;

const REFRESH_PLAN_CONCURRENCY: usize = 4;

pub(crate) async fn queue_strict_refresh(
    thread_manager: &Arc<ThreadManager>,
    config_manager: &ConfigManager,
) -> io::Result<()> {
    config_manager
        .load_latest_config(/*fallback_cwd*/ None)
        .await?;
    let thread_ids = thread_manager.list_thread_ids().await;
    // Poll independent plans concurrently, but retain enumeration/error order.
    // Dropping the stream on failure cancels unfinished planning; no refresh is
    // published until every required plan has succeeded.
    let refreshes = futures::stream::iter(thread_ids.into_iter().map(|thread_id| async move {
        let thread = thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|err| io::Error::other(format!("failed to load thread {thread_id}: {err}")))?;
        let config = build_refresh_config(thread.as_ref(), config_manager).await?;
        Ok::<_, io::Error>((thread_id, thread, config))
    }))
    .buffered(REFRESH_PLAN_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?;
    thread_manager
        .queue_mcp_server_refreshes_atomically(refreshes)
        .await
        .map_err(|err| io::Error::other(format!("failed to queue MCP refresh batch: {err}")))?;
    Ok(())
}

pub(crate) async fn queue_best_effort_refresh(
    thread_manager: &Arc<ThreadManager>,
    config_manager: &ConfigManager,
) {
    let thread_ids = thread_manager.list_thread_ids().await;
    let plans = futures::stream::iter(thread_ids.into_iter().map(|thread_id| async move {
        let thread = thread_manager.get_thread(thread_id).await
            .map_err(|err| format!("failed to load thread {thread_id} for MCP refresh: {err}"))?;
        let config = build_refresh_config(thread.as_ref(), config_manager).await
            .map_err(|err| format!("failed to build MCP refresh config for thread {thread_id}: {err}"))?;
        Ok::<_, String>((thread_id, thread, config))
    }))
    .buffered(REFRESH_PLAN_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    let refreshes = plans.into_iter().filter_map(|plan| match plan {
        Ok(plan) => Some(plan),
        Err(err) => { warn!("{err}"); None }
    }).collect();
    if let Err(err) = thread_manager
        .queue_mcp_server_refreshes_atomically(refreshes)
        .await
    {
        warn!("failed to queue MCP refresh batch: {err}");
    }
}

async fn build_refresh_config(
    thread: &CodexThread,
    config_manager: &ConfigManager,
) -> io::Result<McpServerRefreshConfig> {
    let thread_config = thread.config().await;
    let config = config_manager
        .load_latest_config_for_thread(thread_config.as_ref())
        .await?;
    let mcp_config = thread.runtime_mcp_config(&config).await;
    let mcp_servers = codex_mcp::configured_mcp_servers(&mcp_config);
    Ok(McpServerRefreshConfig {
        mcp_servers: serde_json::to_value(mcp_servers).map_err(io::Error::other)?,
        mcp_oauth_credentials_store_mode: serde_json::to_value(
            config.mcp_oauth_credentials_store_mode,
        )
        .map_err(io::Error::other)?,
        auth_keyring_backend_kind: serde_json::to_value(config.auth_keyring_backend_kind())
            .map_err(io::Error::other)?,
    })
}

