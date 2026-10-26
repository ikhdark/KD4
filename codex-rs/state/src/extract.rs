use crate::model::ThreadMetadata;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::persisted_thread_settings::PersistedThreadSettingsReducer;
use codex_protocol::persisted_thread_settings::reduce_persisted_thread_settings;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_protocol::protocol::user_message_preview;
use serde::Serialize;
use serde_json::Value;

/// Apply a rollout item to the metadata structure.
pub fn apply_rollout_item(
    metadata: &mut ThreadMetadata,
    item: &RolloutItem,
    default_provider: &str,
) {
    apply_non_setting_rollout_item(metadata, item);
    let item_belongs_to_thread = !matches!(
        item,
        RolloutItem::SessionMeta(meta_line) if meta_line.meta.id != metadata.id
    );
    if item_belongs_to_thread {
        apply_persisted_thread_settings(metadata, item);
    }
    if metadata.model_provider.is_empty() {
        metadata.model_provider = default_provider.to_string();
    }
}

/// Apply a rollout batch while reducing persistent settings across the whole
/// supplied history prefix in order.
pub fn apply_rollout_items(
    metadata: &mut ThreadMetadata,
    items: &[RolloutItem],
    default_provider: &str,
) {
    let mut reducer = ThreadMetadataRolloutReducer::default();
    for item in items {
        reducer.apply_item(metadata, item);
    }
    reducer.finish(metadata, default_provider);
}

/// Stateful rollout-to-metadata projection for callers that stream items from disk.
#[derive(Default)]
pub struct ThreadMetadataRolloutReducer {
    settings_reducer: PersistedThreadSettingsReducer,
}

impl ThreadMetadataRolloutReducer {
    pub fn apply_item(&mut self, metadata: &mut ThreadMetadata, item: &RolloutItem) {
        apply_non_setting_rollout_item(metadata, item);
        if !matches!(
            item,
            RolloutItem::SessionMeta(meta_line) if meta_line.meta.id != metadata.id
        ) {
            self.settings_reducer.apply_item(item);
        }
    }

    pub fn finish(self, metadata: &mut ThreadMetadata, default_provider: &str) {
        apply_reduced_persisted_thread_settings(metadata, self.settings_reducer.into_settings());
        if metadata.model_provider.is_empty() {
            metadata.model_provider = default_provider.to_string();
        }
    }
}

/// Return the latest persisted turn-start timestamp in a rollout item batch.
///
/// `TurnStartedEvent::started_at` is the authoritative product-recency fact.
/// Older events may omit it, so callers can retain their existing fallback in
/// that case.
pub fn latest_rollout_recency_at(items: &[RolloutItem]) -> Option<DateTime<Utc>> {
    items.iter().filter_map(rollout_item_recency_at).next_back()
}

fn rollout_item_recency_at(item: &RolloutItem) -> Option<DateTime<Utc>> {
    let RolloutItem::EventMsg(EventMsg::TurnStarted(event)) = item else {
        return None;
    };
    turn_started_recency_at(event.started_at)
}

fn turn_started_recency_at(started_at: Option<i64>) -> Option<DateTime<Utc>> {
    started_at.and_then(|started_at| DateTime::from_timestamp(started_at, 0))
}

fn apply_non_setting_rollout_item(metadata: &mut ThreadMetadata, item: &RolloutItem) {
    match item {
        RolloutItem::SessionMeta(meta_line) => apply_session_meta_from_item(metadata, meta_line),
        RolloutItem::TurnContext(_) => {}
        RolloutItem::EventMsg(event) => apply_event_msg(metadata, event),
        RolloutItem::ResponseItem(item) => apply_response_item(metadata, item),
        RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
        RolloutItem::Compacted(_)
        | RolloutItem::ToolManifest(_)
        | RolloutItem::SamplingBoundary(_) => {}
        RolloutItem::WorldState(_) | RolloutItem::AdditionalContextState { .. } => {}
    }
}

/// Return whether this rollout item can mutate thread metadata stored in SQLite.
pub fn rollout_item_affects_thread_metadata(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::SessionMeta(_) | RolloutItem::TurnContext(_) => true,
        RolloutItem::EventMsg(
            EventMsg::TokenCount(_)
            | EventMsg::UserMessage(_)
            | EventMsg::ThreadGoalUpdated(_)
            | EventMsg::ThreadSettingsApplied(_)
            | EventMsg::TurnStarted(_),
        ) => true,
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
            if matches!(event.item, TurnItem::UserMessage(_)) =>
        {
            true
        }
        RolloutItem::EventMsg(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::ToolManifest(_)
        | RolloutItem::SamplingBoundary(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::AdditionalContextState { .. } => false,
    }
}

fn apply_session_meta_from_item(metadata: &mut ThreadMetadata, meta_line: &SessionMetaLine) {
    if metadata.id != meta_line.meta.id {
        // Ignore session_meta lines that don't match the canonical thread ID,
        // e.g., forked rollouts that embed the source session metadata.
        return;
    }
    metadata.id = meta_line.meta.id;
    metadata.session_id.get_or_insert(meta_line.meta.session_id);
    metadata.source = enum_to_string(&meta_line.meta.source);
    // Later SessionMeta lines do not redefine the canonical history_mode.
    metadata.thread_source = meta_line.meta.thread_source.clone();
    metadata.agent_nickname = meta_line.meta.agent_nickname.clone();
    metadata.agent_role = meta_line.meta.agent_role.clone();
    metadata.agent_path = meta_line.meta.agent_path.clone();
    if let Some(provider) = meta_line.meta.model_provider.as_deref() {
        metadata.model_provider = provider.to_string();
    }
    if !meta_line.meta.cli_version.is_empty() {
        metadata.cli_version = meta_line.meta.cli_version.clone();
    }
    if !meta_line.meta.cwd.as_os_str().is_empty() {
        metadata.cwd = meta_line.meta.cwd.clone();
    }
    if let Some(git) = meta_line.git.as_ref() {
        metadata.git_sha = git.commit_hash.as_ref().map(|sha| sha.0.clone());
        metadata.git_branch = git.branch.clone();
        metadata.git_origin_url = git.repository_url.clone();
    }
}

fn apply_persisted_thread_settings(metadata: &mut ThreadMetadata, item: &RolloutItem) {
    let settings = reduce_persisted_thread_settings(std::slice::from_ref(item), Default::default());
    apply_reduced_persisted_thread_settings(metadata, settings);
}

fn apply_reduced_persisted_thread_settings(
    metadata: &mut ThreadMetadata,
    settings: codex_protocol::persisted_thread_settings::PersistedThreadSettings,
) {
    if let Some(model_provider_id) = settings.model_provider_id {
        metadata.model_provider = model_provider_id;
    }
    if let Some(model) = settings.model {
        metadata.model = Some(model);
    }
    if let Some(reasoning_effort) = settings.reasoning_effort {
        metadata.reasoning_effort = reasoning_effort;
    }
    if let Some(environments) = settings.environments {
        metadata.cwd = environments.legacy_fallback_cwd.into_path_buf();
    }
    if let Some(permission_profile) = settings.permission_profile {
        metadata.sandbox_policy = serde_json::to_string(&permission_profile).unwrap_or_default();
    }
    if let Some(approval_policy) = settings.approval_policy {
        metadata.approval_mode = enum_to_string(&approval_policy);
    }
}

fn apply_event_msg(metadata: &mut ThreadMetadata, event: &EventMsg) {
    match event {
        EventMsg::TokenCount(token_count) => {
            if let Some(info) = token_count.info.as_ref() {
                metadata.tokens_used = info.total_token_usage.total_tokens.max(0);
            }
        }
        EventMsg::UserMessage(user) => {
            apply_user_message(metadata, user);
        }
        EventMsg::ItemCompleted(event) => {
            if let TurnItem::UserMessage(user) = &event.item
                && (metadata.first_user_message.is_none()
                    || metadata.preview.is_none()
                    || metadata.title.is_empty())
            {
                apply_user_message(metadata, &user.as_legacy_user_message_event());
            }
        }
        EventMsg::ThreadGoalUpdated(event) => {
            let objective = event.goal.objective.trim();
            if !objective.is_empty() {
                set_preview_if_empty(metadata, Some(objective.to_string()));
            }
        }
        EventMsg::TurnStarted(event) => {
            if let Some(recency_at) = turn_started_recency_at(event.started_at) {
                metadata.recency_at = recency_at;
            }
        }
        _ => {}
    }
}

fn apply_response_item(_metadata: &mut ThreadMetadata, _item: &ResponseItem) {}

fn apply_user_message(metadata: &mut ThreadMetadata, user: &UserMessageEvent) {
    if metadata.first_user_message.is_none() || metadata.preview.is_none() {
        let preview = user_message_preview(user);
        if metadata.first_user_message.is_none() {
            metadata.first_user_message = preview.clone();
        }
        set_preview_if_empty(metadata, preview);
    }
    if metadata.title.is_empty() {
        let title = strip_user_message_prefix(user.message.as_str());
        if !title.is_empty() {
            metadata.title = title.to_string();
        }
    }
}

fn set_preview_if_empty(metadata: &mut ThreadMetadata, preview: Option<String>) {
    if metadata.preview.is_none() {
        metadata.preview = preview;
    }
}

pub(crate) fn enum_to_string<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

