use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use codex_arg0::Arg0DispatchPaths;
use codex_core::ThreadManager;
use codex_core::config::ConfigOverrides;
use codex_external_agent_sessions::CompletedExternalAgentSessionImport;
use codex_external_agent_sessions::ExternalAgentSessionMigration;
use codex_external_agent_sessions::ImportedExternalAgentSession;
use codex_external_agent_sessions::PendingSessionImport;
use codex_external_agent_sessions::prepare_validated_session_import;
use codex_external_agent_sessions::record_completed_session_imports;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::StateDbHandle;
use codex_rollout::is_persisted_rollout_item;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::DeleteThreadParams;
use codex_thread_store::ThreadMetadataPatch;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::UpdateThreadMetadataParams;
use futures::StreamExt;
use tokio::sync::Semaphore;

use crate::config::external_agent_config::ExternalAgentConfigImportItemResult;
use crate::config::external_agent_config::record_import_error;
use crate::config_manager::ConfigManager;

const SESSION_IMPORT_CONCURRENCY: usize = 5;

#[derive(Clone)]
pub(super) struct ExternalAgentSessionImporter {
    codex_home: PathBuf,
    permits: Arc<Semaphore>,
    thread_manager: Arc<ThreadManager>,
    thread_store: Arc<dyn ThreadStore>,
    state_db: Option<StateDbHandle>,
    config_manager: ConfigManager,
    arg0_paths: Arg0DispatchPaths,
}

impl ExternalAgentSessionImporter {
    pub(super) fn new(
        codex_home: PathBuf,
        thread_manager: Arc<ThreadManager>,
        thread_store: Arc<dyn ThreadStore>,
        state_db: Option<StateDbHandle>,
        config_manager: ConfigManager,
        arg0_paths: Arg0DispatchPaths,
    ) -> Self {
        Self {
            codex_home,
            permits: Arc::new(Semaphore::new(1)),
            thread_manager,
            thread_store,
            state_db,
            config_manager,
            arg0_paths,
        }
    }

    pub(super) async fn import_sessions(
        &self,
        sessions: Vec<ExternalAgentSessionMigration>,
        mut item_result: ExternalAgentConfigImportItemResult,
    ) -> ExternalAgentConfigImportItemResult {
        if sessions.is_empty() {
            return item_result;
        }
        let Ok(_permit) = self.permits.acquire().await else {
            record_import_error(
                &mut item_result,
                "session_permit",
                "external agent session import permit could not be acquired",
                /*source*/ None,
            );
            return item_result;
        };
        let import_results = futures::stream::iter(sessions)
            .map(|session| {
                let importer = self.clone();
                async move { importer.import_requested_session(session).await }
            })
            .buffer_unordered(SESSION_IMPORT_CONCURRENCY);
        futures::pin_mut!(import_results);

        while let Some(result) = import_results.next().await {
            match result {
                Ok(Some(completed_import)) => {
                    let source = completed_import
                        .ledger_entry
                        .source_path
                        .display()
                        .to_string();
                    let thread_id = completed_import.ledger_entry.imported_thread_id;
                    match self
                        .record_completed_import(completed_import.ledger_entry)
                        .await
                    {
                        Ok(()) => {
                            item_result
                                .record_success(Some(source.clone()), Some(thread_id.to_string()));
                            if let Some(warning) = completed_import.shutdown_warning {
                                record_import_error(
                                    &mut item_result,
                                    "session_shutdown",
                                    warning,
                                    Some(source),
                                );
                            }
                        }
                        Err(message) => {
                            let message = self
                                .rollback_failure(
                                    thread_id,
                                    format!("failed to record imported session: {message}"),
                                )
                                .await;
                            record_import_error(
                                &mut item_result,
                                "session_ledger_update",
                                message,
                                Some(source),
                            );
                        }
                    }
                }
                Ok(None) => {}
                Err(failure) => {
                    record_import_error(
                        &mut item_result,
                        failure.stage,
                        failure.message.clone(),
                        Some(failure.source_path.display().to_string()),
                    );
                }
            }
        }
        item_result
    }

    async fn import_requested_session(
        &self,
        session: ExternalAgentSessionMigration,
    ) -> Result<Option<CompletedSessionImport>, SessionImportFailure> {
        let source_path = session.path.clone();
        let Some(pending_import) =
            self.prepare_session_import(session)
                .await
                .map_err(|message| SessionImportFailure {
                    source_path: source_path.clone(),
                    message,
                    stage: "session_prepare",
                })?
        else {
            return Ok(None);
        };
        let persisted_session =
            self.persist_session(pending_import.session)
                .await
                .map_err(|message| SessionImportFailure {
                    source_path: pending_import.source_path.clone(),
                    message,
                    stage: "session_persist",
                })?;
        Ok(Some(CompletedSessionImport {
            ledger_entry: CompletedExternalAgentSessionImport {
                source_path: pending_import.source_path,
                source_content_sha256: pending_import.source_content_sha256,
                source_modified_at: pending_import.source_modified_at,
                imported_thread_id: persisted_session.thread_id,
            },
            shutdown_warning: persisted_session.shutdown_warning,
        }))
    }

    async fn record_completed_import(
        &self,
        completed_import: CompletedExternalAgentSessionImport,
    ) -> Result<(), String> {
        let codex_home = self.codex_home.clone();
        tokio::task::spawn_blocking(move || {
            record_completed_session_imports(&codex_home, vec![completed_import])
        })
        .await
        .map_err(|err| format!("session ledger update task failed: {err}"))?
        .map_err(|err| err.to_string())
    }

    async fn prepare_session_import(
        &self,
        session: ExternalAgentSessionMigration,
    ) -> Result<Option<PendingSessionImport>, String> {
        let codex_home = self.codex_home.clone();
        tokio::task::spawn_blocking(move || prepare_validated_session_import(&codex_home, session))
            .await
            .map_err(|err| format!("external agent session preparation task failed: {err}"))?
            .map_err(|err| format!("failed to prepare external agent session: {err}"))
    }

    async fn persist_session(
        &self,
        session: ImportedExternalAgentSession,
    ) -> Result<PersistedSession, String> {
        let ImportedExternalAgentSession {
            cwd,
            title,
            first_user_message,
            mut rollout_items,
        } = session;
        let config = self
            .config_manager
            .load_with_overrides(
                /*request_overrides*/ None,
                ConfigOverrides {
                    cwd: Some(cwd),
                    codex_linux_sandbox_exe: self.arg0_paths.codex_linux_sandbox_exe.clone(),
                    main_execve_wrapper_exe: self.arg0_paths.main_execve_wrapper_exe.clone(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|err| format!("failed to load imported session config: {err}"))?;
        let models_manager = self.thread_manager.get_models_manager();
        let model = models_manager
            .get_default_model(
                &config.model,
                /*allow_provider_model_fallback*/ false,
                RefreshStrategy::Offline,
                config.http_client_factory(),
            )
            .await;
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let thread_id = ThreadId::new();
        let source = self.thread_manager.session_source();
        let cwd = config.cwd.to_path_buf();
        let model_provider = config.model_provider_id.clone();
        let memory_mode = if config.memories.generate_memories {
            ThreadMemoryMode::Enabled
        } else {
            ThreadMemoryMode::Disabled
        };
        let now = Utc::now();
        let create_params = CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: source.clone(),
            thread_source: None,
            originator: codex_login::default_client::originator().value,
            base_instructions: BaseInstructions {
                text: config
                    .base_instructions
                    .clone()
                    .unwrap_or_else(|| model_info.get_model_instructions(config.personality)),
            },
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: Some(MultiAgentVersion::V1),
            history_mode: ThreadHistoryMode::Legacy,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(cwd.clone()),
                model_provider: model_provider.clone(),
                memory_mode,
            },
        };
        rollout_items.retain(|item| is_persisted_rollout_item(item, ThreadHistoryMode::Legacy));
        let title = title
            .as_deref()
            .and_then(codex_core::util::normalize_thread_name);
        let metadata = ThreadMetadataPatch {
            title,
            preview: first_user_message.clone(),
            model_provider: Some(model_provider),
            created_at: Some(now),
            updated_at: Some(now),
            source: Some(source.clone()),
            thread_source: Some(None),
            agent_nickname: Some(source.get_nickname()),
            agent_role: Some(source.get_agent_role()),
            agent_path: Some(source.get_agent_path().map(Into::into)),
            cwd: Some(cwd),
            cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            first_user_message,
            memory_mode: Some(memory_mode),
            ..Default::default()
        };

        self.thread_store
            .create_thread(create_params)
            .await
            .map_err(|err| format!("failed to import session: {err}"))?;
        if !rollout_items.is_empty()
            && let Err(err) = self
                .thread_store
                .append_items(AppendThreadItemsParams {
                    thread_id,
                    items: rollout_items,
                })
                .await
        {
            return Err(self
                .rollback_failure(thread_id, format!("failed to import session: {err}"))
                .await);
        }

        if let Err(err) = self
            .thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id,
                patch: metadata,
                include_archived: false,
            })
            .await
        {
            return Err(self
                .rollback_failure(
                    thread_id,
                    format!("failed to update imported session: {err}"),
                )
                .await);
        }
        if let Err(err) = self.thread_store.persist_thread(thread_id).await {
            return Err(self
                .rollback_failure(
                    thread_id,
                    format!("failed to persist imported session: {err}"),
                )
                .await);
        }
        let shutdown_warning = match self.thread_store.shutdown_thread(thread_id).await {
            Ok(()) => None,
            Err(err) => {
                let discard_error = match self.thread_store.discard_thread(thread_id).await {
                    Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => None,
                    Err(discard_error) => Some(discard_error),
                };
                Some(match discard_error {
                    Some(discard_error) => format!(
                        "failed to shutdown imported session: {err}; failed to release its live writer: {discard_error}"
                    ),
                    None => format!("failed to shutdown imported session: {err}"),
                })
            }
        };
        Ok(PersistedSession {
            thread_id,
            shutdown_warning,
        })
    }

    async fn rollback_failure(&self, thread_id: ThreadId, failure: String) -> String {
        match self.rollback_session(thread_id).await {
            Ok(()) => failure,
            Err(cleanup_error) => {
                format!("{failure}; failed to roll back imported session: {cleanup_error}")
            }
        }
    }

    async fn rollback_session(&self, thread_id: ThreadId) -> Result<(), String> {
        rollback_imported_session(
            self.thread_store.as_ref(),
            self.state_db.as_ref(),
            thread_id,
        )
        .await
    }
}

async fn rollback_imported_session(
    thread_store: &dyn ThreadStore,
    state_db: Option<&StateDbHandle>,
    thread_id: ThreadId,
) -> Result<(), String> {
    let discard_result = thread_store.discard_thread(thread_id).await;
    match thread_store
        .delete_thread(DeleteThreadParams { thread_id })
        .await
    {
        Ok(()) => {}
        Err(ThreadStoreError::ThreadNotFound { .. }) => match discard_result {
            Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
            Err(err) => return Err(format!("failed to discard live writer: {err}")),
        },
        Err(delete_error) => {
            return match discard_result {
                Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {
                    Err(format!("failed to delete durable thread: {delete_error}"))
                }
                Err(discard_error) => Err(format!(
                    "failed to discard live writer: {discard_error}; failed to delete durable thread: {delete_error}"
                )),
            };
        }
    }
    if let Some(state_db) = state_db {
        state_db.delete_thread(thread_id).await.map_err(|err| {
            format!("failed to delete imported thread state for {thread_id}: {err}")
        })?;
    }
    Ok(())
}

struct CompletedSessionImport {
    ledger_entry: CompletedExternalAgentSessionImport,
    shutdown_warning: Option<String>,
}

struct PersistedSession {
    thread_id: ThreadId,
    shutdown_warning: Option<String>,
}

struct SessionImportFailure {
    source_path: PathBuf,
    message: String,
    stage: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::SessionSource;
    use codex_state::ThreadMetadataBuilder;
    use codex_thread_store::InMemoryThreadStore;
    use codex_thread_store::LocalThreadStore;
    use codex_thread_store::LocalThreadStoreConfig;
    use tempfile::TempDir;

    #[tokio::test]
    async fn rollback_removes_imported_thread_state() {
        let root = TempDir::new().expect("tempdir");
        let state_db =
            codex_state::StateRuntime::init(root.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state db");
        let thread_id = ThreadId::new();
        let mut builder = ThreadMetadataBuilder::new(
            thread_id,
            root.path().join("rollout.jsonl"),
            Utc::now(),
            SessionSource::default(),
        );
        builder.model_provider = Some("test-provider".to_string());
        state_db
            .upsert_thread(&builder.build("test-provider"))
            .await
            .expect("insert thread state");
        let thread_store = InMemoryThreadStore::default();

        rollback_imported_session(&thread_store, Some(&state_db), thread_id)
            .await
            .expect("rollback");

        assert!(
            state_db
                .get_thread(thread_id)
                .await
                .expect("read thread state")
                .is_none()
        );
        let calls = thread_store.calls().await;
        assert_eq!(calls.discard_thread, 1);
        assert_eq!(calls.delete_thread, 1);
    }

    #[tokio::test]
    async fn rollback_removes_materialized_local_rollout_and_state() {
        let root = TempDir::new().expect("tempdir");
        let codex_home = root.path().join("codex-home");
        let sqlite_home = root.path().join("sqlite-home");
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("workspace");
        let state_db =
            codex_state::StateRuntime::init(sqlite_home.clone(), "test-provider".to_string())
                .await
                .expect("state db");
        let thread_store = LocalThreadStore::new(
            LocalThreadStoreConfig {
                codex_home,
                sqlite_home,
                default_model_provider_id: "test-provider".to_string(),
            },
            Some(state_db.clone()),
        );
        let thread_id = ThreadId::new();
        thread_store
            .create_thread(CreateThreadParams {
                session_id: thread_id.into(),
                thread_id,
                extra_config: None,
                forked_from_id: None,
                parent_thread_id: None,
                source: SessionSource::default(),
                thread_source: None,
                originator: "test-originator".to_string(),
                base_instructions: BaseInstructions {
                    text: "test instructions".to_string(),
                },
                dynamic_tools: Vec::new(),
                selected_capability_roots: Vec::new(),
                multi_agent_version: None,
                history_mode: ThreadHistoryMode::Legacy,
                initial_window_id: uuid::Uuid::now_v7().to_string(),
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(cwd),
                    model_provider: "test-provider".to_string(),
                    memory_mode: ThreadMemoryMode::Disabled,
                },
            })
            .await
            .expect("create local thread");
        thread_store
            .persist_thread(thread_id)
            .await
            .expect("materialize local thread");
        let rollout_path = thread_store
            .live_rollout_path(thread_id)
            .await
            .expect("rollout path");
        assert!(rollout_path.is_file());
        let mut builder = ThreadMetadataBuilder::new(
            thread_id,
            rollout_path.clone(),
            Utc::now(),
            SessionSource::default(),
        );
        builder.model_provider = Some("test-provider".to_string());
        state_db
            .upsert_thread(&builder.build("test-provider"))
            .await
            .expect("insert thread state");

        rollback_imported_session(&thread_store, Some(&state_db), thread_id)
            .await
            .expect("rollback");

        assert!(!rollout_path.exists());
        assert!(
            state_db
                .get_thread(thread_id)
                .await
                .expect("read thread state")
                .is_none()
        );
        assert!(matches!(
            thread_store.live_rollout_path(thread_id).await,
            Err(ThreadStoreError::ThreadNotFound { .. })
        ));
    }
}
