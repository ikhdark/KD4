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
use codex_protocol::protocol::ThreadMemoryMode;
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
        let cwd = params.metadata.cwd.clone().unwrap_or_default();
        discover_repository_context(cwd.as_path()).await
    }

    pub(crate) fn git_info_from_repository_context(context: &RepositoryContext) -> GitInfo {
        GitInfo {
            commit_hash: context.git_info.commit_hash.clone(),
            branch: context.git_info.branch.clone(),
            repository_url: context.git_info.repository_url.clone(),
        }
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
            memory_mode: Some(params.metadata.memory_mode),
            ..Default::default()
        };
        Self {
            thread_id: params.thread_id,
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
            update.advance_recency_at = Some(Utc::now());
        }
        self.merge_pending_update(Some(update));
        if !affects_metadata
            && !self
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
        self.observe_items_with_update(items, ThreadMetadataPatch::default())
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
            let previous_settings = self.settings_reducer.settings().clone();
            if !matches!(
                item,
                RolloutItem::SessionMeta(meta_line) if meta_line.meta.id != self.thread_id
            ) {
                self.settings_reducer.apply_item(item);
            }
            apply_persisted_settings_delta(
                &previous_settings,
                self.settings_reducer.settings(),
                &mut update,
                &mut self.cwd_seen,
            );
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
                    if let Some(memory_mode) = meta_line.meta.memory_mode.as_deref()
                        && let Some(memory_mode) = parse_memory_mode(memory_mode)
                    {
                        update.memory_mode = Some(memory_mode);
                    }
                }
                RolloutItem::TurnContext(_) => {}
                RolloutItem::EventMsg(EventMsg::UserMessage(user)) => {
                    self.observe_user_message(user, &mut update);
                }
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                    if let TurnItem::UserMessage(user) = &event.item {
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
                | RolloutItem::WorldState(_) => {}
            }
        }
        Some(update)
    }

    fn observe_user_message(&mut self, user: &UserMessageEvent, update: &mut ThreadMetadataPatch) {
        if let Some(preview) = user_message_preview(user) {
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

fn parse_memory_mode(value: &str) -> Option<ThreadMemoryMode> {
    match value {
        "enabled" => Some(ThreadMemoryMode::Enabled),
        "disabled" => Some(ThreadMemoryMode::Disabled),
        _ => None,
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
        || update.memory_mode.is_some()
}

fn git_info_patch_from_observation(git_info: GitInfo) -> GitInfoPatch {
    GitInfoPatch {
        sha: git_info.commit_hash.map(|sha| Some(sha.0)),
        branch: git_info.branch.map(Some),
        origin_url: git_info.repository_url.map(Some),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_protocol::config_types::ApprovalsReviewer;
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Personality;
    use codex_protocol::config_types::ReasoningSummary;
    use codex_protocol::config_types::Settings;
    use codex_protocol::items::UserMessageItem;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::CompactedItem;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::ThreadGoal;
    use codex_protocol::protocol::ThreadGoalStatus;
    use codex_protocol::protocol::ThreadGoalUpdatedEvent;
    use codex_protocol::protocol::ThreadSettingsAppliedEvent;
    use codex_protocol::protocol::ThreadSettingsSnapshot;
    use codex_protocol::protocol::TurnEnvironmentSelections;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::protocol::UserMessageEvent;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::ThreadPersistenceMetadata;

    #[test]
    fn resume_history_keeps_derived_metadata_pending_until_applied() {
        let thread_id = ThreadId::new();
        let mut sync = ThreadMetadataSync::for_resume(&resume_params(
            thread_id,
            vec![
                RolloutItem::SessionMeta(session_meta(thread_id)),
                RolloutItem::EventMsg(EventMsg::UserMessage(user_message("hello metadata"))),
            ],
        ));

        let update = sync.take_pending_update().expect("pending metadata update");
        assert_eq!(
            update
                .patch
                .created_at
                .expect("created_at should come from session metadata")
                .to_rfc3339(),
            "2025-01-03T12:00:00+00:00"
        );
        assert_eq!(update.patch.preview.as_deref(), Some("hello metadata"));
        assert_eq!(update.patch.title.as_deref(), Some("hello metadata"));
        assert_eq!(
            update.patch.first_user_message.as_deref(),
            Some("hello metadata")
        );
        assert_eq!(update.patch.updated_at, None);
        assert!(
            sync.take_pending_update().is_some(),
            "taking the pending update should not drop retry state"
        );

        sync.mark_pending_update_applied(&update);
        assert!(sync.take_pending_update().is_none());
    }

    #[test]
    fn settings_snapshot_updates_live_thread_read_metadata_projection() {
        let thread_id = ThreadId::new();
        let cwd = AbsolutePathBuf::from_absolute_path(
            std::env::current_dir()
                .expect("current directory")
                .join("persisted-settings-cwd"),
        )
        .expect("absolute settings cwd");
        let permission_profile = PermissionProfile::read_only();
        let snapshot = ThreadSettingsSnapshot {
            model: "persisted-model".to_string(),
            model_provider_id: "persisted-provider".to_string(),
            service_tier: Some(Some("flex".to_string())),
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::User,
            permission_profile: permission_profile.clone(),
            active_permission_profile: Some(None),
            environments: Some(TurnEnvironmentSelections::new(cwd.clone(), Vec::new())),
            workspace_roots: Some(vec![cwd.clone()]),
            profile_workspace_roots: Some(Vec::new()),
            sandbox_policy: None,
            windows_sandbox_level: None,
            cwd: cwd.clone(),
            reasoning_effort: Some(Some(ReasoningEffort::High)),
            reasoning_summary: Some(Some(ReasoningSummary::Detailed)),
            personality: Some(Some(Personality::Pragmatic)),
            collaboration_mode: CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: "persisted-model".to_string(),
                    reasoning_effort: Some(ReasoningEffort::High),
                    developer_instructions: None,
                },
            },
        };
        let sync = ThreadMetadataSync::for_resume(&resume_params(
            thread_id,
            vec![
                RolloutItem::SessionMeta(session_meta(thread_id)),
                RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                    ThreadSettingsAppliedEvent {
                        thread_settings: snapshot,
                    },
                )),
            ],
        ));

        let update = sync
            .take_pending_update()
            .expect("settings metadata update");
        assert_eq!(update.patch.model.as_deref(), Some("persisted-model"));
        assert_eq!(
            update.patch.model_provider.as_deref(),
            Some("persisted-provider")
        );
        assert_eq!(
            update.patch.reasoning_effort,
            Some(Some(ReasoningEffort::High))
        );
        assert_eq!(update.patch.cwd, Some(cwd.into_path_buf()));
        assert_eq!(update.patch.approval_mode, Some(AskForApproval::Never));
        assert_eq!(update.patch.permission_profile, Some(permission_profile));
    }

    #[test]
    fn goal_update_sets_preview_without_overriding_existing_preview() {
        let thread_id = ThreadId::new();
        let sync = ThreadMetadataSync::for_resume(&resume_params(
            thread_id,
            vec![
                RolloutItem::EventMsg(EventMsg::ThreadGoalUpdated(goal_update(
                    thread_id,
                    "ship the refactor",
                ))),
                RolloutItem::EventMsg(EventMsg::UserMessage(user_message("first user text"))),
            ],
        ));

        let update = sync.take_pending_update().expect("pending metadata update");
        assert_eq!(update.patch.preview.as_deref(), Some("ship the refactor"));
        assert_eq!(
            update.patch.first_user_message.as_deref(),
            Some("first user text")
        );
        assert_eq!(update.patch.title.as_deref(), Some("first user text"));
    }

    #[test]
    fn later_user_messages_do_not_emit_existing_preview_fields() {
        let thread_id = ThreadId::new();
        let mut sync = ThreadMetadataSync::for_resume(&resume_params(
            thread_id,
            vec![RolloutItem::EventMsg(EventMsg::UserMessage(user_message(
                "first user text",
            )))],
        ));
        let pending = sync.take_pending_update().expect("pending resume metadata");
        sync.mark_pending_update_applied(&pending);

        let update = sync
            .observe_appended_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(user_message(
                "later user text",
            )))])
            .expect("updated_at touch");

        assert_eq!(update.patch.preview, None);
        assert_eq!(update.patch.title, None);
        assert_eq!(update.patch.first_user_message, None);
        assert!(update.patch.updated_at.is_some());
    }

    #[test]
    fn completed_user_message_items_emit_metadata_fields() {
        let thread_id = ThreadId::new();
        let mut sync = ThreadMetadataSync::for_resume(&resume_params(thread_id, Vec::new()));
        let update = sync
            .observe_appended_items(&[RolloutItem::EventMsg(EventMsg::ItemCompleted(
                ItemCompletedEvent {
                    thread_id,
                    turn_id: "turn-1".to_string(),
                    item: TurnItem::UserMessage(UserMessageItem::new(&[UserInput::Text {
                        text: "first user text".to_string(),
                        text_elements: Vec::new(),
                    }])),
                    completed_at_ms: 0,
                },
            ))])
            .expect("completed user message metadata update");

        assert_eq!(update.patch.preview.as_deref(), Some("first user text"));
        assert_eq!(update.patch.title.as_deref(), Some("first user text"));
        assert_eq!(
            update.patch.first_user_message.as_deref(),
            Some("first user text")
        );
    }

    #[test]
    fn metadata_irrelevant_items_coalesce_updated_at_touches() {
        let thread_id = ThreadId::new();
        let mut sync = ThreadMetadataSync::for_resume(&resume_params(thread_id, Vec::new()));
        let item = RolloutItem::Compacted(CompactedItem {
            message: "compacted".to_string(),
            replacement_history: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
        });

        let first = sync
            .observe_appended_items(std::slice::from_ref(&item))
            .expect("first touch should apply immediately");
        assert!(first.patch.updated_at.is_some());
        sync.mark_pending_update_applied(&first);

        assert!(
            sync.observe_appended_items(std::slice::from_ref(&item))
                .is_none(),
            "second touch inside the coalescing window should wait for a barrier"
        );
        assert!(
            sync.take_pending_update().is_some(),
            "coalesced touches still flush at the next barrier"
        );
    }

    #[test]
    fn turn_start_advances_recency_at_without_changing_updated_at_behavior() {
        let thread_id = ThreadId::new();
        let mut sync = ThreadMetadataSync::for_resume(&resume_params(thread_id, Vec::new()));

        let update = sync
            .observe_appended_items(&[RolloutItem::EventMsg(EventMsg::TurnStarted(
                TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                },
            ))])
            .expect("turn start metadata update");

        assert!(update.patch.updated_at.is_some());
        assert!(update.patch.advance_recency_at.is_some());
    }

    #[test]
    fn resume_history_waits_for_append_before_flushing_metadata() {
        let thread_id = ThreadId::new();
        let mut sync = ThreadMetadataSync::for_resume(&resume_params(
            thread_id,
            vec![
                RolloutItem::SessionMeta(session_meta(thread_id)),
                RolloutItem::EventMsg(EventMsg::UserMessage(user_message("hello metadata"))),
            ],
        ));

        assert!(
            sync.take_pending_update_for_existing_history().is_none(),
            "resume-only metadata should not flush without a new append"
        );
        assert!(
            sync.observe_appended_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(
                user_message("new append"),
            ))])
            .is_some(),
            "the first append should flush resume metadata together with append metadata"
        );
    }

    fn resume_params(thread_id: ThreadId, history: Vec<RolloutItem>) -> ResumeThreadParams {
        ResumeThreadParams {
            thread_id,
            rollout_path: None,
            history: Some(Arc::new(history)),
            include_archived: false,
            metadata: ThreadPersistenceMetadata {
                cwd: None,
                model_provider: "test-provider".to_string(),
                memory_mode: ThreadMemoryMode::Enabled,
            },
        }
    }

    fn user_message(message: &str) -> UserMessageEvent {
        UserMessageEvent {
            client_id: None,
            message: message.to_string(),
            images: None,
            local_images: Vec::new(),
            text_elements: Vec::new(),
            ..Default::default()
        }
    }

    fn session_meta(thread_id: ThreadId) -> SessionMetaLine {
        SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                timestamp: "2025-01-03T12:00:00Z".to_string(),
                source: SessionSource::Exec,
                ..Default::default()
            },
            git: None,
        }
    }

    fn goal_update(thread_id: ThreadId, objective: &str) -> ThreadGoalUpdatedEvent {
        ThreadGoalUpdatedEvent {
            thread_id,
            turn_id: None,
            goal: ThreadGoal {
                thread_id,
                objective: objective.to_string(),
                status: ThreadGoalStatus::Active,
                token_budget: None,
                tokens_used: 0,
                time_used_seconds: 0,
                created_at: 0,
                updated_at: 0,
            },
        }
    }
}
