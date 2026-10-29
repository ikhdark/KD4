//! Endpoint-local compatibility adapter. No retired migration engine is loaded.
use super::ConfigRequestProcessor;
use crate::error_code::internal_error;
use crate::outgoing_message::{ConnectionRequestId, OutgoingMessageSender, RESOURCE_DELIVERY_TIMEOUT};
use codex_app_server_protocol::*;
use codex_rollout::StateDbHandle;
use codex_state::{ExternalAgentConfigImportFailureRecord, ExternalAgentConfigImportSuccessRecord};
use std::path::PathBuf;
use std::result::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

mod files;
mod formats;
mod plugins;
mod sessions;

#[derive(Clone)]
pub(crate) struct ExternalAgentConfigRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    config_processor: ConfigRequestProcessor,
    state_db: Option<StateDbHandle>,
    codex_home: PathBuf,
    background_tasks: TaskTracker,
    shutdown: CancellationToken,
    config_manager: crate::config_manager::ConfigManager,
    thread_manager: Arc<codex_core::ThreadManager>,
    thread_store: Arc<dyn codex_thread_store::ThreadStore>,
    session_imports: Arc<tokio::sync::Mutex<()>>,
}

impl ExternalAgentConfigRequestProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        config_processor: ConfigRequestProcessor,
        state_db: Option<StateDbHandle>,
        codex_home: PathBuf,
        background_tasks: TaskTracker,
        shutdown: CancellationToken,
        config_manager: crate::config_manager::ConfigManager,
        thread_manager: Arc<codex_core::ThreadManager>,
        thread_store: Arc<dyn codex_thread_store::ThreadStore>,
    ) -> Self {
        Self { outgoing, config_processor, state_db, codex_home, background_tasks, shutdown, config_manager, thread_manager, thread_store, session_imports: Arc::new(tokio::sync::Mutex::new(())) }
    }

    pub(crate) async fn detect(
        &self,
        params: ExternalAgentConfigDetectParams,
    ) -> Result<ExternalAgentConfigDetectResponse, JSONRPCErrorError> {
        let home = self.codex_home.clone();
        let mut items = tokio::task::spawn_blocking(move || files::detect(&home, params))
            .await
            .map_err(|err| internal_error(format!("external-agent detection task failed: {err}")))?
            .map_err(|err| internal_error(err.to_string()))?;
        self.filter_detected_plugins(&mut items).await.map_err(|err| internal_error(err.to_string()))?;
        Ok(ExternalAgentConfigDetectResponse { items })
    }

    pub(crate) async fn import(
        &self,
        request_id: ConnectionRequestId,
        params: ExternalAgentConfigImportParams,
    ) -> Result<(), JSONRPCErrorError> {
        // Disconnect must not detach file mutations from response, cache
        // invalidation, history persistence, and completion notification.
        let processor = self.clone();
        self.background_tasks.spawn(async move {
            processor.import_owned(request_id, params).await
        }).await.map_err(|err| internal_error(format!("import task failed: {err}")))?
    }

    async fn import_owned(
        &self,
        request_id: ConnectionRequestId,
        params: ExternalAgentConfigImportParams,
    ) -> Result<(), JSONRPCErrorError> {
        let import_id = uuid::Uuid::new_v4().to_string();
        // Keep the source field as attribution only; it never selects a
        // migration engine or authorizes a path.
        tracing::info!(
            %import_id,
            source = params.source.as_deref().unwrap_or_default(),
            "external-agent compatibility import started"
        );
        let has_items = !params.migration_items.is_empty();
        let needs_runtime_refresh = params.migration_items.iter().any(|item| {
            matches!(
                item.item_type,
                ExternalAgentConfigMigrationItemType::Config
                    | ExternalAgentConfigMigrationItemType::Skills
                    | ExternalAgentConfigMigrationItemType::McpServerConfig
                    | ExternalAgentConfigMigrationItemType::Hooks
                    | ExternalAgentConfigMigrationItemType::Commands
                    | ExternalAgentConfigMigrationItemType::Plugins
                    | ExternalAgentConfigMigrationItemType::Subagents
            )
        });
        let home = self.codex_home.clone();
        let cancellation = self.shutdown.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let worker_cancellation = cancellation.clone();
        let (async_items, items): (Vec<_>, Vec<_>) = params.migration_items.into_iter().partition(|item| matches!(item.item_type, ExternalAgentConfigMigrationItemType::Plugins | ExternalAgentConfigMigrationItemType::Sessions));
        let item_type_results = tokio::task::spawn_blocking(move || {
            files::import(&home, items, &worker_cancellation)
        }).await.map_err(|err| internal_error(format!("import worker failed: {err}")))?;
        if needs_runtime_refresh {
            self.config_processor.handle_config_mutation();
        }
        self.outgoing.send_response(request_id, ExternalAgentConfigImportResponse {
            import_id: import_id.clone(),
        }).await;
        // Historical empty requests return an ID, but do not create history or
        // issue completion notifications.
        if !has_items {
            return Ok(());
        }
        if !async_items.is_empty() {
            let processor = self.clone();
            self.background_tasks.spawn(async move {
                processor.complete_import(import_id, item_type_results, async_items).await;
            });
            return Ok(());
        }
        self.complete_import(import_id, item_type_results, Vec::new()).await;
        Ok(())
    }

    async fn complete_import(
        &self,
        import_id: String,
        mut item_type_results: Vec<ExternalAgentConfigImportTypeResult>,
        async_items: Vec<ExternalAgentConfigMigrationItem>,
    ) {
        for result in &item_type_results {
            let _ = tokio::time::timeout(RESOURCE_DELIVERY_TIMEOUT,
                self.outgoing.send_server_notification(
                    ServerNotification::ExternalAgentConfigImportProgress(
                        ExternalAgentConfigImportProgressNotification {
                            import_id: import_id.clone(),
                            item_type_results: vec![result.clone()],
                        },
                    ),
                ),
            ).await;
        }
        for item in async_items {
            let result = match item.item_type {
                ExternalAgentConfigMigrationItemType::Plugins => self.import_plugin_item(item).await,
                ExternalAgentConfigMigrationItemType::Sessions => self.import_session_item(item).await,
                _ => unreachable!("foreground item reached background import"),
            };
            let _ = tokio::time::timeout(RESOURCE_DELIVERY_TIMEOUT,
                self.outgoing.send_server_notification(ServerNotification::ExternalAgentConfigImportProgress(
                    ExternalAgentConfigImportProgressNotification { import_id: import_id.clone(), item_type_results: vec![result.clone()] },
                )),
            ).await;
            item_type_results.push(result);
        }
        let mut grouped: Vec<ExternalAgentConfigImportTypeResult> = Vec::new();
        for result in item_type_results {
            if let Some(existing) = grouped.iter_mut().find(|existing| existing.item_type == result.item_type) {
                existing.successes.extend(result.successes);
                existing.failures.extend(result.failures);
            } else {
                grouped.push(result);
            }
        }
        grouped.sort_by_key(|result| match result.item_type {
            ExternalAgentConfigMigrationItemType::Config => 0,
            ExternalAgentConfigMigrationItemType::Skills => 1,
            ExternalAgentConfigMigrationItemType::AgentsMd => 2,
            ExternalAgentConfigMigrationItemType::Plugins => 3,
            ExternalAgentConfigMigrationItemType::McpServerConfig => 4,
            ExternalAgentConfigMigrationItemType::Subagents => 5,
            ExternalAgentConfigMigrationItemType::Hooks => 6,
            ExternalAgentConfigMigrationItemType::Commands => 7,
            ExternalAgentConfigMigrationItemType::Sessions => 8,
        });
        let item_type_results = grouped;
        if let Some(state_db) = self.state_db.as_ref() {
            let successes = item_type_results.iter().flat_map(|result| &result.successes)
                .map(|success| ExternalAgentConfigImportSuccessRecord {
                    item_type: item_type_name(success.item_type),
                    cwd: success.cwd.clone(),
                    source: success.source.clone(),
                    target: success.target.clone(),
                }).collect::<Vec<_>>();
            let failures = item_type_results.iter().flat_map(|result| &result.failures)
                .map(|failure| ExternalAgentConfigImportFailureRecord {
                    item_type: item_type_name(failure.item_type),
                    error_type: failure.error_type.clone(),
                    failure_stage: failure.failure_stage.clone(),
                    message: failure.message.clone(),
                    cwd: failure.cwd.clone(),
                    source: failure.source.clone(),
                }).collect::<Vec<_>>();
            if let Err(err) = state_db.record_external_agent_config_import_completed(
                &import_id, &successes, &failures,
            ).await {
                tracing::warn!(%import_id, error = %err, "failed to record external-agent import completion");
            }
        }
        let _ = tokio::time::timeout(RESOURCE_DELIVERY_TIMEOUT,
            self.outgoing.send_server_notification(
                ServerNotification::ExternalAgentConfigImportCompleted(
                    ExternalAgentConfigImportCompletedNotification { import_id, item_type_results },
                ),
            ),
        ).await;
    }

    pub(crate) async fn read_import_histories(
        &self,
    ) -> Result<ExternalAgentConfigImportHistoriesReadResponse, JSONRPCErrorError> {
        let state_db = self.state_db.as_ref()
            .ok_or_else(|| internal_error("state database is unavailable"))?;
        let records = state_db.external_agent_config_import_history_records().await
            .map_err(|err| internal_error(format!("failed to read import histories: {err}")))?;
        let data = records.into_iter().map(|record| {
            Ok(ExternalAgentConfigImportHistory {
                import_id: record.import_id,
                completed_at_ms: record.completed_at_ms,
                successes: record.successes.into_iter().map(|success| {
                    Ok(ExternalAgentConfigImportItemTypeSuccess {
                        item_type: parse_item_type(success.item_type)?,
                        cwd: success.cwd, source: success.source, target: success.target,
                    })
                }).collect::<Result<Vec<_>, JSONRPCErrorError>>()?,
                failures: record.failures.into_iter().map(|failure| {
                    Ok(ExternalAgentConfigImportItemTypeFailure {
                        item_type: parse_item_type(failure.item_type)?,
                        error_type: failure.error_type, failure_stage: failure.failure_stage,
                        message: failure.message, cwd: failure.cwd, source: failure.source,
                    })
                }).collect::<Result<Vec<_>, JSONRPCErrorError>>()?,
            })
        }).collect::<Result<Vec<_>, JSONRPCErrorError>>()?;
        Ok(ExternalAgentConfigImportHistoriesReadResponse { data })
    }
}

fn item_type_name(item_type: ExternalAgentConfigMigrationItemType) -> String {
    // The enum's serde wire names are also the existing SQLite record contract
    // decoded by `parse_item_type`; keep these strings identical to them.
    match item_type {
        ExternalAgentConfigMigrationItemType::AgentsMd => "AGENTS_MD",
        ExternalAgentConfigMigrationItemType::Config => "CONFIG",
        ExternalAgentConfigMigrationItemType::Skills => "SKILLS",
        ExternalAgentConfigMigrationItemType::Plugins => "PLUGINS",
        ExternalAgentConfigMigrationItemType::McpServerConfig => "MCP_SERVER_CONFIG",
        ExternalAgentConfigMigrationItemType::Subagents => "SUBAGENTS",
        ExternalAgentConfigMigrationItemType::Hooks => "HOOKS",
        ExternalAgentConfigMigrationItemType::Commands => "COMMANDS",
        ExternalAgentConfigMigrationItemType::Sessions => "SESSIONS",
    }
    .to_string()
}

fn parse_item_type(item_type: String) -> Result<ExternalAgentConfigMigrationItemType, JSONRPCErrorError> {
    serde_json::from_value(serde_json::Value::String(item_type.clone()))
        .map_err(|err| internal_error(format!("failed to decode import item type {item_type}: {err}")))
}

fn failure(item: &ExternalAgentConfigMigrationItem, stage: &str, message: String, source: Option<String>) -> ExternalAgentConfigImportItemTypeFailure {
    ExternalAgentConfigImportItemTypeFailure {
        item_type: item.item_type, error_type: None,
        failure_stage: stage.to_string(), message, cwd: item.cwd.clone(), source,
    }
}
