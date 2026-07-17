use super::LocalThreadStore;
use crate::CreateThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::error::reject_paginated_history_mode;
use chrono::Utc;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;
use codex_state::ThreadMetadataBuilder;

pub(super) async fn create_thread(
    store: &LocalThreadStore,
    params: CreateThreadParams,
) -> ThreadStoreResult<RolloutRecorder> {
    reject_paginated_history_mode(params.history_mode)?;
    let cwd = params
        .metadata
        .cwd
        .clone()
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: "local thread store requires a cwd".to_string(),
        })?;
    let config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite_home: store.config.sqlite_home.clone(),
        cwd,
        model_provider_id: params.metadata.model_provider.clone(),
        generate_memories: matches!(params.metadata.memory_mode, ThreadMemoryMode::Enabled),
    };
    let created_at = Utc::now();
    let source = params.source.clone();
    let thread_source = params.thread_source.clone();
    let history_mode = params.history_mode;
    let model_provider = params.metadata.model_provider.clone();
    let memory_mode = match params.metadata.memory_mode {
        ThreadMemoryMode::Enabled => "enabled",
        ThreadMemoryMode::Disabled => "disabled",
    };
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            params.thread_id,
            params.forked_from_id,
            params.parent_thread_id,
            params.source,
            params.thread_source,
            params.originator,
            params.base_instructions,
            params.dynamic_tools,
        )
        .with_session_id(params.session_id)
        .with_selected_capability_roots(params.selected_capability_roots)
        .with_multi_agent_version(params.multi_agent_version)
        .with_history_mode(params.history_mode)
        .with_initial_window_id(params.initial_window_id),
    )
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to initialize local thread recorder: {err}"),
    })?;

    let mut builder = ThreadMetadataBuilder::new(
        params.thread_id,
        recorder.rollout_path().to_path_buf(),
        created_at,
        source.clone(),
    );
    builder.updated_at = Some(created_at);
    builder.history_mode = history_mode;
    builder.thread_source = thread_source;
    builder.agent_nickname = source.get_nickname();
    builder.agent_role = source.get_agent_role();
    builder.agent_path = source.get_agent_path().map(Into::into);
    builder.model_provider = Some(model_provider.clone());
    builder.cwd = config.cwd.clone();
    builder.cli_version = Some(env!("CARGO_PKG_VERSION").to_string());
    let state_db = store.state_db().await;
    codex_rollout::state_db::index_current_thread(
        state_db.as_deref(),
        &builder,
        model_provider.as_str(),
        memory_mode,
    )
    .await;
    Ok(recorder)
}
