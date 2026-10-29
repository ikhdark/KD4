use crate::config_manager::ConfigManager;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_protocol::protocol::McpServerRefreshConfig;
use futures::StreamExt;
use futures::TryStreamExt;
use std::io;
use std::sync::Arc;
use tracing::Instrument;
use tracing::warn;

fn refresh_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CODEX_PLUGIN_REFRESH_TRACE")
        .is_some_and(|value| value == "1"))
}

fn refresh_trace_span(refresh_id: u64, thread_id: Option<codex_protocol::ThreadId>) -> tracing::Span {
    if !refresh_trace_enabled() {
        return tracing::Span::none();
    }
    let thread_id = thread_id.map(|id| id.to_string()).unwrap_or_default();
    tracing::info_span!(target: "codex_plugin_refresh", "plugin_refresh", refresh_id, %thread_id)
}

// Diagnostic correlation only; no authority or ordering decision uses this ID.
fn refresh_trace_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    if refresh_trace_enabled() {
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    }
}

const REFRESH_PLAN_CONCURRENCY: usize = 4;

#[expect(
    clippy::await_holding_invalid_type,
    reason = "The runtime-refresh owner spans capture and publication so an older plan cannot overwrite a newer batch"
)]
pub(crate) async fn queue_strict_refresh(
    thread_manager: &Arc<ThreadManager>,
    config_manager: &ConfigManager,
) -> io::Result<()> {
    let refresh_id = refresh_trace_id();
    let started = std::time::Instant::now();
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "requested", refresh_id, config_generation = config_manager.policy_generation(), kind = "strict"); }
    let _refresh_guard = config_manager.lock_runtime_refresh().await;
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "planning", refresh_id, config_generation = config_manager.policy_generation(), wait_us = started.elapsed().as_micros() as u64); }
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
        let config = build_refresh_config(thread.as_ref(), config_manager, false)
            .instrument(refresh_trace_span(refresh_id, Some(thread_id)))
            .await?;
        Ok::<_, io::Error>((thread_id, thread, config))
    }))
    .buffered(REFRESH_PLAN_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?;
    thread_manager
        .queue_mcp_server_refreshes_atomically(refreshes)
        .instrument(refresh_trace_span(refresh_id, None))
        .await
        .map_err(|err| io::Error::other(format!("failed to queue MCP refresh batch: {err}")))?;
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "queued", refresh_id, config_generation = config_manager.policy_generation(), elapsed_us = started.elapsed().as_micros() as u64); }
    Ok(())
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "The runtime-refresh owner spans capture and publication so an older plan cannot overwrite a newer batch"
)]
pub(crate) async fn queue_best_effort_refresh(
    thread_manager: &Arc<ThreadManager>,
    config_manager: &ConfigManager,
) {
    let refresh_id = refresh_trace_id();
    let started = std::time::Instant::now();
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "requested", refresh_id, config_generation = config_manager.policy_generation(), kind = "lifecycle"); }
    // Capture and publish under the same owner as strict/config refreshes. An
    // older invocation that starts late rebuilds from current accepted state;
    // a slow earlier plan cannot overwrite a newer completed batch.
    let _refresh_guard = config_manager.lock_runtime_refresh().await;
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "planning", refresh_id, config_generation = config_manager.policy_generation(), wait_us = started.elapsed().as_micros() as u64); }
    let thread_ids = thread_manager.list_thread_ids().await;
    let plans = futures::stream::iter(thread_ids.into_iter().map(|thread_id| async move {
        let thread = thread_manager.get_thread(thread_id).await
            .map_err(|err| format!("failed to load thread {thread_id} for MCP refresh: {err}"))?;
        let config = build_refresh_config(thread.as_ref(), config_manager, true)
            .instrument(refresh_trace_span(refresh_id, Some(thread_id)))
            .await
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
        .instrument(refresh_trace_span(refresh_id, None))
        .await
    {
        warn!("failed to queue MCP refresh batch: {err}");
        return;
    }
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "queued", refresh_id, config_generation = config_manager.policy_generation(), elapsed_us = started.elapsed().as_micros() as u64); }
}

async fn build_refresh_config(
    thread: &CodexThread,
    config_manager: &ConfigManager,
    refresh_runtime_config: bool,
) -> io::Result<McpServerRefreshConfig> {
    let thread_config = thread.config().await;
    let config = config_manager
        .load_latest_config_for_thread(thread_config.as_ref())
        .await?;
    let captured_generation = config_manager.policy_generation();
    let started = std::time::Instant::now();
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "captured", config_generation = captured_generation); }
    if refresh_runtime_config {
        // Invalidation reloads bytes, not the loaded session's selected plugin
        // set or compiled hooks. Reuse the accepted user-layer/hooks owner,
        // preserving thread overrides and scoped hook run history.
        thread
            .refresh_runtime_config_features_after_discovery_invalidation(config.clone(), &[])
            .await;
    }
    let mcp_config = thread.runtime_mcp_config(&config).await;
    let mcp_servers = codex_mcp::configured_mcp_servers(&mcp_config);
    if refresh_trace_enabled() { tracing::info!(target: "codex_plugin_refresh", phase = "materialized", config_generation = captured_generation, current_generation = config_manager.policy_generation(), server_count = mcp_servers.len(), elapsed_us = started.elapsed().as_micros() as u64); }
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

