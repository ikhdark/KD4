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
use records::summarize_session;

const SESSION_TITLE_MAX_LEN: usize = 120;

type ExternalAgentSessionMigration = codex_app_server_protocol_types::SessionMigration;

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
    pub imported_thread_id: codex_protocol::ThreadId,
    pub recovery_required: bool,
    pub session: ImportedExternalAgentSession,
}

fn prepare_validated_session_import(
    ledger: &mut ledger::SessionImportLedgerGuard,
    session: ExternalAgentSessionMigration,
    cancellation: &tokio_util::sync::CancellationToken,
) -> io::Result<Option<PendingSessionImport>> {
    if ledger.contains_source(&session.path) {
        let content_sha256 = ledger::session_content_sha256_with_cancellation(&session.path, Some(cancellation))?;
        if ledger.contains_fingerprint(&session.path, &content_sha256) {
            return Ok(None);
        }
    }
    let Some(mut pending) = load_importable_session(&session.path, cancellation)? else {
        return Ok(None);
    };
    if ledger.contains_fingerprint(&pending.source_path, &pending.source_content_sha256) {
        return Ok(None);
    }
    let (thread_id, recovery_required) = ledger.reserve(
        &pending.source_path,
        &pending.source_content_sha256,
    )?;
    pending.imported_thread_id = thread_id;
    pending.recovery_required = recovery_required;
    Ok(Some(pending))
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
            imported_thread_id: codex_protocol::ThreadId::new(),
            recovery_required: false,
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
use codex_app_server_protocol_types::{ExternalAgentConfigImportTypeResult, ExternalAgentConfigMigrationItem, ExternalAgentConfigImportItemTypeSuccess};
use chrono::Utc;
use codex_core::config::ConfigOverrides;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::{MultiAgentVersion, ThreadHistoryMode};
use codex_rollout::is_persisted_rollout_item;
use codex_thread_store::{AppendThreadItemsParams, CreateThreadParams, LocalThreadStore, ThreadMetadataPatch, ThreadPersistenceMetadata, ThreadStore, ThreadStoreError, UpdateThreadMetadataParams};

impl ExternalAgentConfigRequestProcessor {
    pub(super) fn durable_session_import_store(&self) -> Option<&LocalThreadStore> {
        self.thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
            .filter(|store| store.supports_durable_session_import())
    }

    pub(super) async fn import_session_item(&self, item: ExternalAgentConfigMigrationItem) -> ExternalAgentConfigImportTypeResult {
        let mut result = ExternalAgentConfigImportTypeResult { item_type: item.item_type, successes: Vec::new(), failures: Vec::new() };
        let Some(thread_store) = self.durable_session_import_store() else {
            result.failures.push(failure(
                &item,
                "session_prepare",
                "the selected thread store does not support durable external session imports".to_string(),
                None,
            ));
            return result;
        };
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
        let home = self.codex_home.clone();
        let token = cancellation.clone();
        // This task is already owned by complete_import's TaskTracker. Keep the OS
        // lock through creation, durability and ledger commit, even after disconnect.
        let mut ledger = match tokio::task::spawn_blocking(move || ledger::lock_import_ledger(&home, Some(&token))).await {
            Ok(Ok(ledger)) => ledger,
            Ok(Err(err)) => {
                result.failures.push(failure(&item, "session_ledger", err.to_string(), None));
                return result;
            },
            Err(err) => {
                result.failures.push(failure(&item, "session_ledger", format!("session ledger task failed: {err}"), None));
                return result;
            },
        };
        let external_home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join(".claude");
        let mut selected = std::collections::HashSet::new();
        for session in &details.sessions {
            if cancellation.is_cancelled() {
                result.failures.push(failure(&item, "session_prepare", "session import cancelled".to_string(), Some(session.path.display().to_string())));
                continue;
            }
            let session = session.clone();
            let external_home = external_home.clone();
            let source = session.path.display().to_string();
            let token = cancellation.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                let prepared = (|| {
                    let mut session = session;
                    let path = std::fs::canonicalize(&session.path)?;
                    let root = std::fs::canonicalize(external_home.join("projects"))?;
                    if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") || !path.starts_with(root) {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "external agent session was not detected beneath .claude/projects"));
                    }
                    // Request aliases must be rejected before parsing/conversion.
                    if !selected.insert(path.clone()) {
                        return Ok(None);
                    }
                    session.path = path;
                    prepare_validated_session_import(&mut ledger, session, &token)
                })();
                (ledger, selected, prepared)
            }).await;
            let prepared = match prepared {
                Ok((returned_ledger, returned_selected, prepared)) => {
                    ledger = returned_ledger;
                    selected = returned_selected;
                    prepared
                },
                Err(err) => {
                    result.failures.push(failure(&item, "session_prepare", format!("session preparation task failed: {err}"), Some(source)));
                    return result;
                },
            };
            let pending = match prepared {
                Ok(Some(pending)) => pending,
                Ok(None) => continue,
                Err(err) => {
                    result.failures.push(failure(&item, "session_prepare", err.to_string(), Some(source)));
                    continue;
                },
            };
            let persisted = if pending.recovery_required {
                self.persist_or_recover_session(
                    thread_store,
                    pending.imported_thread_id,
                    &pending.source_content_sha256,
                    pending.session,
                )
                .await
            } else {
                self.persist_session(
                    thread_store,
                    pending.imported_thread_id,
                    &pending.source_content_sha256,
                    pending.session,
                )
                .await
            };
            let persisted = match persisted {
                Ok(persisted) => persisted,
                Err(message) => {
                    result.failures.push(failure(&item, "session_persist", message, Some(source)));
                    continue;
                },
            };
            let recorded = tokio::task::spawn_blocking(move || {
                // Once the thread is durable, shutdown cancellation must not
                // turn successful persistence into a rollback or a new identity.
                let recorded = ledger.complete(
                    pending.source_path,
                    pending.source_content_sha256,
                    persisted.thread_id,
                );
                (ledger, recorded, persisted)
            }).await;
            match recorded {
                Ok((returned_ledger, recorded, persisted)) => {
                    ledger = returned_ledger;
                    match recorded {
                        Ok(()) => {
                            result.successes.push(ExternalAgentConfigImportItemTypeSuccess { item_type: item.item_type, cwd: item.cwd.clone(), source: Some(source.clone()), target: Some(persisted.thread_id.to_string()) });
                            if let Some(warning) = persisted.shutdown_warning {
                                result.failures.push(failure(&item, "session_shutdown", warning, Some(source)));
                            }
                        },
                        Err(err) => {
                            // The durable reservation still owns this exact thread.
                            // A retry reconciles it; never delete a published thread.
                            result.failures.push(failure(&item, "session_ledger_update", format!("failed to record imported session {}: {err}; retry will reconcile the reserved thread", persisted.thread_id), Some(source)));
                        },
                    }
                },
                Err(err) => {
                    result.failures.push(failure(&item, "session_ledger_update", format!("session ledger task failed: {err}; the durable reservation is retained"), Some(source)));
                    return result;
                },
            }
        }
        result
    }

    async fn persist_or_recover_session(
        &self,
        thread_store: &LocalThreadStore,
        thread_id: ThreadId,
        content_sha256: &str,
        session: ImportedExternalAgentSession,
    ) -> Result<PersistedSession, String> {
        match thread_store.read_thread(codex_thread_store::ReadThreadParams {
            thread_id,
            include_archived: true,
            include_history: true,
        }).await {
            Err(ThreadStoreError::ThreadNotFound { .. }) => {
                match thread_store.reserved_import_thread_is_absent(thread_id).await {
                    Ok(true) => {
                        self.persist_session(thread_store, thread_id, content_sha256, session)
                            .await
                    }
                    Ok(false) => Err(format!("reserved import thread {thread_id} was not readable but has existing storage; it was preserved and no replacement was created")),
                    Err(err) => Err(format!("failed to establish absence of reserved import thread {thread_id}: {err}; no replacement was created")),
                }
            },
            Err(err) => Err(format!("failed to inspect reserved import thread {thread_id}: {err}; no replacement was created")),
            Ok(thread) => {
                let history = thread.history.ok_or_else(|| format!("reserved import thread {thread_id} has no readable history; it was preserved"))?;
                ensure_session_recovery_proof(
                    thread_store,
                    thread_id,
                    content_sha256,
                    &session.rollout_items,
                    &history.items,
                )
                .await?;
                // A crash can precede the original durability barrier. Reopen via
                // the existing exclusive writer lease, sync, then release only the
                // writer acquired here. Never delete or replace recovered data.
                thread_store.resume_thread(codex_thread_store::ResumeThreadParams {
                    thread_id,
                    rollout_path: thread.rollout_path,
                    history: Some(std::sync::Arc::new(history.items)),
                    include_archived: true,
                    metadata: ThreadPersistenceMetadata {
                        cwd: Some(thread.cwd),
                        model_provider: thread.model_provider,
                    },
                }).await.map_err(|err| format!("failed to acquire reserved import thread {thread_id} for durability: {err}; it was preserved"))?;
                // The earlier inspection was not under the writer lease. Recheck
                // the exact opened rollout before certifying it, for example if
                // another owner truncated history before we acquired the writer.
                let rechecked = async {
                    let current = ThreadStore::load_history(
                        thread_store,
                        codex_thread_store::LoadThreadHistoryParams {
                            thread_id,
                            include_archived: true,
                        },
                    )
                    .await
                    .map_err(|err| format!("failed to recheck reserved import thread {thread_id}: {err}"))?;
                    ensure_session_recovery_proof(
                        thread_store,
                        thread_id,
                        content_sha256,
                        &session.rollout_items,
                        &current.items,
                    )
                    .await
                }
                .await;
                if let Err(err) = rechecked {
                    return Err(preserve_failed_import(thread_store, thread_id, err).await);
                }
                let durable = thread_store.flush_thread_durable(thread_id).await;
                let released = thread_store.discard_thread(thread_id).await;
                if let Err(err) = durable {
                    return Err(format!("failed to sync reserved import thread {thread_id}: {err}; writer release: {released:?}; it was preserved"));
                }
                released.map_err(|err| format!("failed to release reserved import thread {thread_id}: {err}; it was preserved"))?;
                Ok(PersistedSession { thread_id, shutdown_warning: None })
            },
        }
    }

    async fn persist_session(
        &self,
        thread_store: &LocalThreadStore,
        thread_id: ThreadId,
        content_sha256: &str,
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

        thread_store
            .create_thread(create_params)
            .await
            .map_err(|err| format!("failed to import session: {err}"))?;
        if !rollout_items.is_empty()
            && let Err(err) = thread_store
                .append_items(AppendThreadItemsParams {
                    thread_id,
                    items: rollout_items,
                })
                .await
        {
            let failure = format!("failed to import session: {err}");
            return Err(match thread_store.rollback_created_thread(thread_id).await {
                Ok(()) => failure,
                Err(cleanup_error) => format!(
                    "{failure}; failed to roll back imported session: failed to roll back local imported session: {cleanup_error}"
                ),
            });
        }

        // A metadata commit error may leave a durable receipt. From this point
        // forward, release the live writer on failure but do not roll back.
        if let Err(err) = thread_store
            .commit_imported_session_metadata(
                UpdateThreadMetadataParams {
                    thread_id,
                    patch: metadata,
                    include_archived: false,
                },
                content_sha256,
            )
            .await
        {
            return Err(preserve_failed_import(
                thread_store,
                thread_id,
                format!("failed to update imported session {thread_id}: {err}"),
            )
            .await);
        }
        if let Err(err) = thread_store.persist_thread(thread_id).await {
            return Err(preserve_failed_import(
                thread_store,
                thread_id,
                format!("failed to persist imported session {thread_id}: {err}"),
            )
            .await);
        }
        if let Err(err) = thread_store.flush_thread_durable(thread_id).await {
            return Err(preserve_failed_import(
                thread_store,
                thread_id,
                format!("failed to durably persist imported session {thread_id}: {err}"),
            )
            .await);
        }
        let shutdown_warning = match thread_store.shutdown_thread(thread_id).await {
            Ok(()) => None,
            Err(err) => {
                let discard_error = match thread_store.discard_thread(thread_id).await {
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
}

async fn ensure_session_recovery_proof(
    thread_store: &LocalThreadStore,
    thread_id: ThreadId,
    content_sha256: &str,
    expected: &[RolloutItem],
    actual: &[RolloutItem],
) -> Result<(), String> {
    let expected = expected
        .iter()
        .filter(|item| is_persisted_rollout_item(item, ThreadHistoryMode::Legacy))
        .collect::<Vec<_>>();
    // The recorder emits SessionMeta before the imported batch. Later
    // user appends and metadata edits are deliberately permitted.
    let actual = match actual {
        [RolloutItem::SessionMeta(_), items @ ..] => items,
        items => items,
    };
    let conflict = || format!("reserved import thread {thread_id} is incomplete or its imported history changed; it was preserved and no duplicate was created");
    if expected.is_empty() || actual.len() < expected.len() {
        return Err(conflict());
    }
    for (actual, expected) in actual.iter().zip(expected) {
        // RolloutItem intentionally has no PartialEq contract.
        let actual = serde_json::to_value(actual)
            .map_err(|err| format!("failed to inspect reserved import thread {thread_id}: {err}"))?;
        let expected = serde_json::to_value(expected)
            .map_err(|err| format!("failed to inspect imported history: {err}"))?;
        if actual != expected {
            return Err(conflict());
        }
    }
    // Matching history does not prove the metadata committed. Never
    // replay the original patch for a recovered thread.
    let has_receipt = thread_store
        .has_imported_session_metadata_receipt(thread_id, content_sha256)
        .await
        .map_err(|err| format!("failed to inspect the import metadata receipt for reserved thread {thread_id}: {err}; it was preserved"))?;
    if !has_receipt {
        return Err(format!("session import conflict: reserved thread {thread_id} has matching history but no durable import metadata receipt; it was preserved and no duplicate was created"));
    }
    Ok(())
}

async fn preserve_failed_import(
    thread_store: &LocalThreadStore,
    thread_id: ThreadId,
    failure: String,
) -> String {
    let released = thread_store.discard_thread(thread_id).await;
    format!("{failure}; writer release: {released:?}; the reserved thread was preserved for retry")
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



