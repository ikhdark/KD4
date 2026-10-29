//! Legacy session wire-format conversion through current protocol and ThreadStore owners.
//! Parsing and export helpers for external-agent session histories.

mod detect;
mod export;
mod ledger;
mod records;
mod title;

use codex_protocol::protocol::RolloutItem;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub use detect::detect_recent_sessions;
use export::load_session_for_import_with_content_sha256;
pub use ledger::CompletedExternalAgentSessionImport;
pub use ledger::record_completed_session_imports;
use records::summarize_session;

const SESSION_TITLE_MAX_LEN: usize = 120;

type ExternalAgentSessionMigration = codex_app_server_protocol::SessionMigration;

#[derive(Debug, Clone)]
pub struct ImportedExternalAgentSession {
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub first_user_message: Option<String>,
    pub rollout_items: Vec<RolloutItem>,
}

#[derive(Debug, Clone)]
pub struct PendingSessionImport {
    pub source_path: PathBuf,
    pub source_content_sha256: String,
    pub session: ImportedExternalAgentSession,
}

pub fn prepare_validated_session_import(
    codex_home: &Path,
    session: ExternalAgentSessionMigration,
    cancellation: &tokio_util::sync::CancellationToken,
) -> io::Result<Option<PendingSessionImport>> {
    let Some(pending) = load_importable_session(&session.path, cancellation)? else {
        return Ok(None);
    };
    let ledger = ledger::load_import_ledger_with_cancellation(codex_home, cancellation)?;
    Ok(
        (!ledger.contains_fingerprint(&pending.source_path, &pending.source_content_sha256))
            .then_some(pending),
    )
}

fn load_importable_session(path: &Path, cancellation: &tokio_util::sync::CancellationToken) -> io::Result<Option<PendingSessionImport>> {
    let source_path = std::fs::canonicalize(path)?;
    let Some((imported_session, source_content_sha256)) =
        load_session_for_import_with_content_sha256(&source_path, cancellation)?
    else {
        return Ok(None);
    };
    Ok(imported_session
        .cwd
        .is_dir()
        .then_some(PendingSessionImport {
            source_path,
            source_content_sha256,
            session: imported_session,
        }))
}

#[derive(Debug, Clone)]
struct ConversationMessage {
    role: MessageRole,
    text: String,
    timestamp: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageRole {
    Assistant,
    User,
}

fn summarize_for_label(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default().trim();
    truncate(first_line, SESSION_TITLE_MAX_LEN)
}

fn truncate(text: &str, max_len: usize) -> String {
    if text.chars().take(max_len.saturating_add(1)).count() <= max_len {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .nth(max_len.saturating_sub(3))
        .map_or(text.len(), |(index, _)| index);
    let prefix = &text[..end];
    format!("{prefix}...")
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}


use super::{ExternalAgentConfigRequestProcessor, failure};
use codex_app_server_protocol::{ExternalAgentConfigImportTypeResult, ExternalAgentConfigMigrationItem, ExternalAgentConfigImportItemTypeSuccess};
use chrono::Utc;
use codex_core::config::ConfigOverrides;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::{MultiAgentVersion, ThreadHistoryMode};
use codex_rollout::{StateDbHandle, is_persisted_rollout_item};
use codex_thread_store::{AppendThreadItemsParams, CreateThreadParams, DeleteThreadParams, ThreadMetadataPatch, ThreadPersistenceMetadata, ThreadStore, ThreadStoreError, UpdateThreadMetadataParams};

impl ExternalAgentConfigRequestProcessor {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Session imports are single-flight so concurrent imports cannot persist the same source twice"
    )]
    pub(super) async fn import_session_item(&self, item: ExternalAgentConfigMigrationItem) -> ExternalAgentConfigImportTypeResult {
        let mut result = ExternalAgentConfigImportTypeResult { item_type: item.item_type, successes: Vec::new(), failures: Vec::new() };
        let cancellation = self.shutdown.child_token();
        let _permit = tokio::select! {
            _ = cancellation.cancelled() => {
                result.failures.push(failure(&item, "session_permit", "session import cancelled".to_string(), None));
                return result;
            },
            permit = self.session_imports.lock() => permit,
        };
        let Some(details) = item.details.as_ref() else {
            result.failures.push(failure(&item, "session_prepare", "sessions migration item is missing details".to_string(), None));
            return result;
        };
        let external_home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".claude");
        let mut selected = std::collections::HashSet::new();
        for session in &details.sessions {
            if cancellation.is_cancelled() {
                result.failures.push(failure(&item, "session_prepare", "session import cancelled".to_string(), Some(session.path.display().to_string())));
                continue;
            }
            let home = self.codex_home.clone();
            let session = session.clone();
            let external_home = external_home.clone();
            let source = session.path.display().to_string();
            let token = cancellation.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                let mut session = session;
                let path = std::fs::canonicalize(&session.path)?;
                let root = std::fs::canonicalize(external_home.join("projects"))?;
                if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") || !path.starts_with(root) {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "external agent session was not detected beneath .claude/projects"));
                }
                session.path = path;
                prepare_validated_session_import(&home, session, &token)
            }).await;
            let pending = match prepared {
                Ok(Ok(Some(pending))) => pending,
                Ok(Ok(None)) => continue,
                Ok(Err(err)) => { result.failures.push(failure(&item, "session_prepare", err.to_string(), Some(source))); continue; },
                Err(err) => { result.failures.push(failure(&item, "session_prepare", format!("session preparation task failed: {err}"), Some(source))); continue; },
            };
            if !selected.insert((pending.source_path.clone(), pending.source_content_sha256.clone())) { continue; }
            let persisted = match self.persist_session(pending.session).await {
                Ok(persisted) => persisted,
                Err(message) => { result.failures.push(failure(&item, "session_persist", message, Some(source))); continue; },
            };
            let completed = CompletedExternalAgentSessionImport {
                source_path: pending.source_path, source_content_sha256: pending.source_content_sha256, imported_thread_id: persisted.thread_id,
            };
            let home = self.codex_home.clone();
            let token = cancellation.clone();
            let recorded = tokio::task::spawn_blocking(move || record_completed_session_imports(&home, vec![completed], &token)).await;
            match recorded {
                Ok(Ok(())) => {
                    result.successes.push(ExternalAgentConfigImportItemTypeSuccess { item_type: item.item_type, cwd: item.cwd.clone(), source: Some(source.clone()), target: Some(persisted.thread_id.to_string()) });
                    if let Some(warning) = persisted.shutdown_warning { result.failures.push(failure(&item, "session_shutdown", warning, Some(source))); }
                },
                other => {
                    let message = match other { Ok(Err(err)) => err.to_string(), Err(err) => err.to_string(), _ => unreachable!() };
                    let message = self.rollback_failure(persisted.thread_id, format!("failed to record imported session: {message}")).await;
                    result.failures.push(failure(&item, "session_ledger_update", message, Some(source)));
                },
            }
        }
        result
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
            .await
            .map_err(|err| format!("failed to resolve imported session model: {err}"))?;
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let thread_id = ThreadId::new();
        let source = self.thread_manager.session_source();
        let cwd = config.cwd.to_path_buf();
        let model_provider = config.model_provider_id.clone();
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
            base_instructions: imported_base_instructions(
                config.base_instructions.as_deref(),
                &model_info.base_instructions,
            ),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: Some(MultiAgentVersion::V1),
            history_mode: ThreadHistoryMode::Legacy,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(cwd.clone()),
                model_provider: model_provider.clone(),
            },
        };
        rollout_items.retain(|item| is_persisted_rollout_item(item, ThreadHistoryMode::Legacy));
        let title = title
            .as_deref()
            .and_then(codex_thread_store::normalize_thread_name);
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
    if let Some(local_store) = thread_store
        .as_any()
        .downcast_ref::<codex_thread_store::LocalThreadStore>()
    {
        // MessageProcessor constructs the local store and importer with the same state DB.
        // Keep staged files and that DB's commit under the existing cancellation-safe owner.
        return local_store
            .rollback_created_thread(thread_id)
            .await
            .map_err(|err| format!("failed to roll back local imported session: {err}"));
    }
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

struct PersistedSession {
    thread_id: ThreadId,
    shutdown_warning: Option<String>,
}

fn imported_base_instructions(
    configured_base_instructions: Option<&str>,
    model_base_instructions: &str,
) -> BaseInstructions {
    BaseInstructions {
        text: configured_base_instructions
            .unwrap_or(model_base_instructions)
            .to_string(),
    }
}



