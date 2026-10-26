use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Utc;
use codex_git_utils::RepositoryContext;
use codex_git_utils::discover_repository_context;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::persisted_thread_settings::PersistedThreadSettings;
use codex_protocol::persisted_thread_settings::PersistedThreadSettingsReducer;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_protocol::protocol::user_message_preview;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::CreateThreadParams;
use crate::GitInfoPatch;
use crate::ResumeThreadParams;
use crate::ThreadMetadataPatch;

const THREAD_UPDATED_AT_TOUCH_INTERVAL: Duration = Duration::from_secs(5);

/// Live-thread helper that derives metadata updates from appended rollout items.
///
/// Stores receive raw rollout items plus explicit metadata patches. This helper
/// keeps append-derived metadata observation in the live layer without owning persistence-policy
/// filtering or making `append_items` infer metadata inside a `ThreadStore` implementation.
pub(crate) struct ThreadMetadataSync {
    pub(crate) history_observation_uncertain: bool,
    // An ambiguous append needs one ordering fence, independently of the
    // metadata projection remaining stale until its recovery barrier.
    pub(crate) append_ordering_uncertain: bool,
    thread_id: ThreadId,
    cwd_seen: bool,
    preview_seen: bool,
    first_user_message_seen: bool,
    title_seen: bool,
    settings_reducer: PersistedThreadSettingsReducer,
    pending_update: Option<ThreadMetadataPatch>,
    pending_update_generation: u64,
    last_touch_persisted_at: Option<Instant>,
    defer_create_update_until_history_exists: bool,
    defer_resume_update_until_append: bool,
}

pub(crate) struct PendingThreadMetadataPatch {
    pub(crate) patch: ThreadMetadataPatch,
    generation: u64,
}

impl ThreadMetadataSync {
    pub(crate) async fn collect_repository_context_for_create(
        params: &CreateThreadParams,
    ) -> Option<RepositoryContext> {
        let cwd = params.metadata.cwd.as_deref()?;
        discover_repository_context(cwd).await
    }

    pub(crate) fn git_info_from_repository_context(context: &RepositoryContext) -> GitInfo {
        context.git_info.clone()
    }

    pub(crate) fn for_create_with_git_info(
        params: &CreateThreadParams,
        git_info: Option<GitInfo>,
    ) -> Self {
        let created_at = Utc::now();
        let cwd = params.metadata.cwd.clone().unwrap_or_default();
        let update = ThreadMetadataPatch {
            model_provider: Some(params.metadata.model_provider.clone()),
            created_at: Some(created_at),
            updated_at: Some(created_at),
            source: Some(params.source.clone()),
            thread_source: Some(params.thread_source.clone()),
            agent_nickname: Some(params.source.get_nickname()),
            agent_role: Some(params.source.get_agent_role()),
            agent_path: Some(params.source.get_agent_path().map(Into::into)),
            cwd: Some(cwd.clone()),
            cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            git_info: git_info.map(git_info_patch_from_observation),
            ..Default::default()
        };
        Self {
            thread_id: params.thread_id,
            history_observation_uncertain: false,
            append_ordering_uncertain: false,
            cwd_seen: !cwd.as_os_str().is_empty(),
            preview_seen: false,
            first_user_message_seen: false,
            title_seen: false,
            settings_reducer: settings_reducer_from_persistence_metadata(&params.metadata),
            pending_update: Some(update),
            pending_update_generation: 1,
            last_touch_persisted_at: None,
            defer_create_update_until_history_exists: true,
            defer_resume_update_until_append: false,
        }
    }

    pub(crate) fn for_resume(params: &ResumeThreadParams) -> Self {
        let mut sync = Self {
            thread_id: params.thread_id,
            history_observation_uncertain: false,
            append_ordering_uncertain: false,
            cwd_seen: params
                .metadata
                .cwd
                .as_ref()
                .is_some_and(|cwd| !cwd.as_os_str().is_empty()),
            preview_seen: false,
            first_user_message_seen: false,
            title_seen: false,
            settings_reducer: settings_reducer_from_persistence_metadata(&params.metadata),
            pending_update: None,
            pending_update_generation: 0,
            last_touch_persisted_at: None,
            defer_create_update_until_history_exists: false,
            defer_resume_update_until_append: false,
        };
        if let Some(history) = params.history.as_deref() {
            sync.record_resume_history(history);
        }
        sync
    }

    pub(crate) fn record_resume_history(&mut self, history: &[RolloutItem]) {
        let update = self.observe_resume_history(history);
        self.merge_pending_update(update);
        self.defer_resume_update_until_append = self.pending_update.is_some();
    }

    pub(crate) fn take_pending_update(&self) -> Option<PendingThreadMetadataPatch> {
        self.pending_update
            .clone()
            .map(|patch| PendingThreadMetadataPatch {
                patch,
                generation: self.pending_update_generation,
            })
    }

    pub(crate) fn take_pending_update_for_existing_history(
        &self,
    ) -> Option<PendingThreadMetadataPatch> {
        if self.defer_create_update_until_history_exists {
            return None;
        }
        if self.defer_resume_update_until_append {
            return None;
        }
        self.take_pending_update()
    }

    pub(crate) fn mark_pending_update_applied(&mut self, update: &PendingThreadMetadataPatch) {
        if self.pending_update_generation == update.generation {
            self.pending_update = None;
        }
        if update.patch.updated_at.is_some() {
            self.last_touch_persisted_at = Some(Instant::now());
        }
    }

    pub(crate) fn observe_appended_items(
        &mut self,
        items: &[RolloutItem],
    ) -> Option<PendingThreadMetadataPatch> {
        self.defer_create_update_until_history_exists = false;
        self.defer_resume_update_until_append = false;
        let affects_metadata = items
            .iter()
            .any(codex_state::rollout_item_affects_thread_metadata);
        let advances_recency = items
            .iter()
            .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))));
        let mut update = if affects_metadata {
            self.observe_items(items)?
        } else {
            thread_updated_at_touch()
        };
        if advances_recency {
            update.advance_recency_at =
                Some(codex_state::latest_rollout_recency_at(items).unwrap_or_else(Utc::now));
        }
        self.merge_pending_update(Some(update));
        if !self
            .pending_update
            .as_ref()
            .is_some_and(update_has_metadata_facts)
            && self.last_touch_persisted_at.is_some_and(|last_touch| {
                Instant::now().duration_since(last_touch) < THREAD_UPDATED_AT_TOUCH_INTERVAL
            })
        {
            return None;
        }
        self.take_pending_update()
    }

    fn observe_items(&mut self, items: &[RolloutItem]) -> Option<ThreadMetadataPatch> {
        self.observe_items_with_update(
            items,
            ThreadMetadataPatch {
                updated_at: Some(Utc::now()),
                ..Default::default()
            },
        )
    }

    fn observe_resume_history(&mut self, items: &[RolloutItem]) -> Option<ThreadMetadataPatch> {
        let mut update = self.observe_items_with_update(items, ThreadMetadataPatch::default())?;
        // History establishes whether a first message was already seen, but is not
        // a rename operation. Replaying its default title would overwrite a custom
        // SQLite title when the next append commits this repair patch.
        update.title = None;
        Some(update)
    }

    fn observe_items_with_update(
        &mut self,
        items: &[RolloutItem],
        mut update: ThreadMetadataPatch,
    ) -> Option<ThreadMetadataPatch> {
        if items.is_empty() {
            return None;
        }
        for item in items {
            // Resume histories and mixed append batches also contain tool payloads and
            // sampling records. Reuse the live-path classifier before cloning settings;
            // these records cannot change either settings or metadata.
            if !codex_state::rollout_item_affects_thread_metadata(item) {
                continue;
            }
            if matches!(item, RolloutItem::TurnContext(_)
                | RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(_)))
                || matches!(item, RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == self.thread_id)
            {
                let previous_settings = self.settings_reducer.settings().clone();
                self.settings_reducer.apply_item(item);
                apply_persisted_settings_delta(
                    &previous_settings,
                    self.settings_reducer.settings(),
                    &mut update,
                    &mut self.cwd_seen,
                );
            }
            match item {
                RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == self.thread_id => {
                    update.created_at = parse_session_timestamp(meta_line.meta.timestamp.as_str());
                    update.source = Some(meta_line.meta.source.clone());
                    update.thread_source = Some(meta_line.meta.thread_source.clone());
                    update.agent_nickname = Some(meta_line.meta.agent_nickname.clone());
                    update.agent_role = Some(meta_line.meta.agent_role.clone());
                    update.agent_path = Some(meta_line.meta.agent_path.clone());
                    if !meta_line.meta.cli_version.is_empty() {
                        update.cli_version = Some(meta_line.meta.cli_version.clone());
                    }
                    if let Some(git_info) = meta_line.git.clone() {
                        update.git_info = Some(git_info_patch_from_observation(git_info));
                    }
                }
                RolloutItem::TurnContext(_) => {}
                RolloutItem::EventMsg(EventMsg::UserMessage(user)) => {
                    self.observe_user_message(user, &mut update);
                }
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                    if let TurnItem::UserMessage(user) = &event.item
                        && !(self.preview_seen && self.first_user_message_seen && self.title_seen)
                    {
                        self.observe_user_message(
                            &user.as_legacy_user_message_event(),
                            &mut update,
                        );
                    }
                }
                RolloutItem::EventMsg(EventMsg::TokenCount(token_count)) => {
                    if let Some(info) = token_count.info.as_ref() {
                        update.token_usage = Some(info.total_token_usage.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadGoalUpdated(event)) => {
                    if !self.preview_seen {
                        let objective = event.goal.objective.trim();
                        if !objective.is_empty() {
                            self.preview_seen = true;
                            update.preview = Some(objective.to_string());
                        }
                    }
                }
                RolloutItem::SessionMeta(_)
                | RolloutItem::ToolManifest(_)
                | RolloutItem::SamplingBoundary(_)
                | RolloutItem::EventMsg(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::Compacted(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::AdditionalContextState { .. } => {}
            }
        }
        Some(update)
    }

    fn observe_user_message(&mut self, user: &UserMessageEvent, update: &mut ThreadMetadataPatch) {
        if !(self.preview_seen && self.first_user_message_seen)
            && let Some(preview) = user_message_preview(user)
        {
            if !self.first_user_message_seen {
                self.first_user_message_seen = true;
                update.first_user_message = Some(preview.clone());
            }
            if !self.preview_seen {
                self.preview_seen = true;
                update.preview = Some(preview);
            }
        }
        if !self.title_seen {
            let title = strip_user_message_prefix(user.message.as_str());
            if !title.is_empty() {
                self.title_seen = true;
                update.title = Some(title.to_string());
            }
        }
    }

    pub(crate) fn record_explicit_update(&mut self, patch: ThreadMetadataPatch) {
        if patch.name.is_some() || patch.title.is_some() {
            self.title_seen = true;
            if let Some(pending) = self.pending_update.as_mut() {
                pending.title = None;
            }
        }
        if let Some(pending) = self.pending_update.as_mut() {
            // These fields have already committed through the explicit owner.
            // Retrying older observations must not restore them or re-submit
            // an acknowledged name/project operation as a durability condition.
            macro_rules! superseded {
                ($($field:ident),* $(,)?) => { $(if patch.$field.is_some() { pending.$field = None; })* };
            }
            superseded!(
                name,
                rollout_path,
                preview,
                title,
                model_provider,
                model,
                reasoning_effort,
                created_at,
                updated_at,
                source,
                thread_source,
                agent_nickname,
                agent_role,
                agent_path,
                cwd,
                cli_version,
                approval_mode,
                permission_profile,
                token_usage,
                first_user_message,
                git_info,
                project_id
            );
            self.pending_update_generation = self.pending_update_generation.wrapping_add(1);
        }
    }

    fn merge_pending_update(&mut self, update: Option<ThreadMetadataPatch>) {
        let Some(update) = update else {
            return;
        };
        match self.pending_update.as_mut() {
            Some(pending_update) => pending_update.merge(update),
            None => self.pending_update = Some(update),
        }
        self.pending_update_generation = self.pending_update_generation.wrapping_add(1);
    }
}

fn settings_reducer_from_persistence_metadata(
    metadata: &crate::ThreadPersistenceMetadata,
) -> PersistedThreadSettingsReducer {
    let environments = metadata
        .cwd
        .as_ref()
        .and_then(|cwd| AbsolutePathBuf::from_absolute_path(cwd).ok())
        .map(|cwd| TurnEnvironmentSelections::new(cwd, Vec::new()));
    PersistedThreadSettingsReducer::new(PersistedThreadSettings {
        model_provider_id: (!metadata.model_provider.is_empty())
            .then(|| metadata.model_provider.clone()),
        environments,
        ..Default::default()
    })
}

fn apply_persisted_settings_delta(
    previous: &PersistedThreadSettings,
    current: &PersistedThreadSettings,
    update: &mut ThreadMetadataPatch,
    cwd_seen: &mut bool,
) {
    if previous.model_provider_id != current.model_provider_id
        && let Some(model_provider_id) = current.model_provider_id.clone()
    {
        update.model_provider = Some(model_provider_id);
    }
    if previous.model != current.model
        && let Some(model) = current.model.clone()
    {
        update.model = Some(model);
    }
    if previous.reasoning_effort != current.reasoning_effort
        && let Some(reasoning_effort) = current.reasoning_effort.clone()
    {
        update.reasoning_effort = Some(reasoning_effort);
    }
    if previous.environments != current.environments
        && let Some(environments) = current.environments.as_ref()
    {
        *cwd_seen = true;
        update.cwd = Some(environments.legacy_fallback_cwd.to_path_buf());
    }
    if previous.approval_policy != current.approval_policy
        && let Some(approval_policy) = current.approval_policy
    {
        update.approval_mode = Some(approval_policy);
    }
    if previous.permission_profile != current.permission_profile
        && let Some(permission_profile) = current.permission_profile.clone()
    {
        update.permission_profile = Some(permission_profile);
    }
}

fn parse_session_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H-%M-%S")
                .map(|timestamp| DateTime::from_naive_utc_and_offset(timestamp, Utc))
        })
        .ok()
}

fn thread_updated_at_touch() -> ThreadMetadataPatch {
    ThreadMetadataPatch {
        updated_at: Some(Utc::now()),
        ..Default::default()
    }
}

fn update_has_metadata_facts(update: &ThreadMetadataPatch) -> bool {
    update.rollout_path.is_some()
        || update.preview.is_some()
        || update.title.is_some()
        || update.model_provider.is_some()
        || update.model.is_some()
        || update.reasoning_effort.is_some()
        || update.created_at.is_some()
        || update.advance_recency_at.is_some()
        || update.source.is_some()
        || update.thread_source.is_some()
        || update.agent_nickname.is_some()
        || update.agent_role.is_some()
        || update.agent_path.is_some()
        || update.cwd.is_some()
        || update.cli_version.is_some()
        || update.approval_mode.is_some()
        || update.permission_profile.is_some()
        || update.token_usage.is_some()
        || update.first_user_message.is_some()
        || update.git_info.is_some()
}

fn git_info_patch_from_observation(git_info: GitInfo) -> GitInfoPatch {
    GitInfoPatch {
        sha: git_info.commit_hash.map(|sha| Some(sha.0)),
        branch: git_info.branch.map(Some),
        origin_url: git_info.repository_url.map(Some),
    }
}

