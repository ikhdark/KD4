//! Canonical reconstruction of persistent thread settings from rollout history.
//!
//! The reducer is deliberately presence-aware. Older rollout items only supply
//! fields they actually carried, while each `ThreadSettingsApplied` snapshot is
//! replayed in rollout order. Callers pass only the history prefix that belongs
//! to their resume or fork boundary, then remove fields covered by explicit
//! request overrides before applying the result.

use crate::config_types::ApprovalsReviewer;
use crate::config_types::CollaborationMode;
use crate::config_types::Personality;
use crate::config_types::ReasoningSummary;
use crate::config_types::WindowsSandboxLevel;
use crate::models::ActivePermissionProfile;
use crate::models::PermissionProfile;
use crate::openai_models::ReasoningEffort;
use crate::protocol::AskForApproval;
use crate::protocol::EventMsg;
use crate::protocol::RolloutItem;
use crate::protocol::SandboxPolicy;
use crate::protocol::ThreadSettingsSnapshot;
use crate::protocol::TurnContextItem;
use crate::protocol::TurnEnvironmentSelections;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Presence-aware persistent settings reconstructed from a rollout prefix.
///
/// Nullable settings use an outer `Option` for evidence and an inner `Option`
/// for the effective value. For example, `Some(None)` means a persisted event
/// explicitly cleared reasoning effort, while `None` means the history did not
/// supply reasoning effort at all.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersistedThreadSettings {
    pub model: Option<String>,
    pub model_provider_id: Option<String>,
    pub service_tier: Option<Option<String>>,
    pub developer_instructions: Option<Option<String>>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub permission_profile: Option<PermissionProfile>,
    pub active_permission_profile: Option<Option<ActivePermissionProfile>>,
    pub environments: Option<TurnEnvironmentSelections>,
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub profile_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub sandbox_policy: Option<SandboxPolicy>,
    pub windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub reasoning_effort: Option<Option<ReasoningEffort>>,
    pub reasoning_summary: Option<Option<ReasoningSummary>>,
    pub personality: Option<Option<Personality>>,
    pub collaboration_mode: Option<CollaborationMode>,
}

impl PersistedThreadSettings {
    /// Remove reconstructed values for fields supplied explicitly by a caller.
    /// The caller's already-loaded config therefore has final precedence.
    pub fn remove_explicit_overrides(&mut self, mask: &PersistedThreadSettingsOverrideMask) {
        if mask.model {
            self.model = None;
        }
        if mask.model_provider_id {
            self.model_provider_id = None;
        }
        if mask.service_tier {
            self.service_tier = None;
        }
        if mask.developer_instructions {
            self.developer_instructions = None;
        }
        if mask.approval_policy {
            self.approval_policy = None;
        }
        if mask.approvals_reviewer {
            self.approvals_reviewer = None;
        }
        if mask.permission_profile {
            self.permission_profile = None;
        }
        if mask.active_permission_profile {
            self.active_permission_profile = None;
        }
        if mask.environments {
            self.environments = None;
        }
        if mask.workspace_roots {
            self.workspace_roots = None;
        }
        if mask.profile_workspace_roots {
            self.profile_workspace_roots = None;
        }
        if mask.sandbox_policy {
            self.sandbox_policy = None;
        }
        if mask.windows_sandbox_level {
            self.windows_sandbox_level = None;
        }
        if mask.reasoning_effort {
            self.reasoning_effort = None;
        }
        if mask.reasoning_summary {
            self.reasoning_summary = None;
        }
        if mask.personality {
            self.personality = None;
        }
        if mask.collaboration_mode {
            self.collaboration_mode = None;
        }
    }
}

/// Persistent settings that an explicit resume or fork request supplies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PersistedThreadSettingsOverrideMask {
    pub model: bool,
    pub model_provider_id: bool,
    pub service_tier: bool,
    pub developer_instructions: bool,
    pub approval_policy: bool,
    pub approvals_reviewer: bool,
    pub permission_profile: bool,
    pub active_permission_profile: bool,
    pub environments: bool,
    pub workspace_roots: bool,
    pub profile_workspace_roots: bool,
    pub sandbox_policy: bool,
    pub windows_sandbox_level: bool,
    pub reasoning_effort: bool,
    pub reasoning_summary: bool,
    pub personality: bool,
    pub collaboration_mode: bool,
}

impl PersistedThreadSettingsOverrideMask {
    pub fn any(self) -> bool {
        self != Self::default()
    }
}

/// Stateful form of the canonical reducer, useful for live rollout projection.
#[derive(Clone, Debug, Default)]
pub struct PersistedThreadSettingsReducer {
    settings: PersistedThreadSettings,
    saw_initial_session_meta: bool,
    saw_authoritative_environments: bool,
}

impl PersistedThreadSettingsReducer {
    pub fn new(fallback: PersistedThreadSettings) -> Self {
        Self {
            settings: fallback,
            saw_initial_session_meta: false,
            saw_authoritative_environments: false,
        }
    }

    pub fn settings(&self) -> &PersistedThreadSettings {
        &self.settings
    }

    pub fn into_settings(self) -> PersistedThreadSettings {
        self.settings
    }

    pub fn apply_item(&mut self, item: &RolloutItem) {
        match item {
            RolloutItem::SessionMeta(meta_line) if !self.saw_initial_session_meta => {
                self.saw_initial_session_meta = true;
                if let Some(provider) = meta_line
                    .meta
                    .model_provider
                    .as_ref()
                    .filter(|provider| !provider.is_empty())
                {
                    self.settings.model_provider_id = Some(provider.clone());
                }
                if !meta_line.meta.cwd.as_os_str().is_empty()
                    && let Ok(cwd) = AbsolutePathBuf::from_absolute_path(&meta_line.meta.cwd)
                {
                    self.settings.environments =
                        Some(TurnEnvironmentSelections::new(cwd, Vec::new()));
                }
            }
            RolloutItem::TurnContext(turn_context) => self.apply_turn_context(turn_context),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                self.apply_snapshot(&event.thread_settings);
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::ToolManifest(_)
            | RolloutItem::SamplingBoundary(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::EventMsg(_) => {}
        }
    }

    fn apply_turn_context(&mut self, turn_context: &TurnContextItem) {
        self.settings.model = Some(turn_context.model.clone());
        self.settings.reasoning_effort = Some(turn_context.effort.clone());
        self.settings.approval_policy = Some(turn_context.approval_policy);
        if let Some(approvals_reviewer) = turn_context.approvals_reviewer {
            self.settings.approvals_reviewer = Some(approvals_reviewer);
        }
        self.settings.permission_profile = Some(turn_context.permission_profile());
        self.settings.sandbox_policy = Some(turn_context.sandbox_policy.clone());
        if !self.saw_authoritative_environments {
            self.settings.environments = Some(TurnEnvironmentSelections::new(
                turn_context.cwd.clone(),
                Vec::new(),
            ));
        }
        if let Some(workspace_roots) = turn_context.workspace_roots.clone() {
            self.settings.workspace_roots = Some(workspace_roots);
        }
        self.settings.personality = Some(turn_context.personality);
        if let Some(collaboration_mode) = turn_context.collaboration_mode.clone() {
            self.settings.collaboration_mode = Some(collaboration_mode);
        } else if let Some(collaboration_mode) = self.settings.collaboration_mode.clone() {
            self.settings.collaboration_mode = Some(collaboration_mode.with_updates(
                Some(turn_context.model.clone()),
                Some(turn_context.effort.clone()),
                /*developer_instructions*/ None,
            ));
        }
    }

    fn apply_snapshot(&mut self, snapshot: &ThreadSettingsSnapshot) {
        self.settings.model = Some(snapshot.model.clone());
        self.settings.model_provider_id = Some(snapshot.model_provider_id.clone());
        if let Some(service_tier) = snapshot.service_tier.as_ref() {
            self.settings.service_tier = Some(service_tier.clone());
        }
        if let Some(developer_instructions) = snapshot.developer_instructions.as_ref() {
            self.settings.developer_instructions = Some(developer_instructions.clone());
        }
        self.settings.approval_policy = Some(snapshot.approval_policy);
        self.settings.approvals_reviewer = Some(snapshot.approvals_reviewer);
        self.settings.permission_profile = Some(snapshot.permission_profile.clone());
        if let Some(active_permission_profile) = snapshot.active_permission_profile.as_ref() {
            self.settings.active_permission_profile = Some(active_permission_profile.clone());
        }
        if let Some(environments) = snapshot.environments.clone() {
            self.settings.environments = Some(environments);
            self.saw_authoritative_environments = true;
        } else {
            self.settings.environments = Some(TurnEnvironmentSelections::new(
                snapshot.cwd.clone(),
                Vec::new(),
            ));
            self.saw_authoritative_environments = true;
        }
        if let Some(workspace_roots) = snapshot.workspace_roots.clone() {
            self.settings.workspace_roots = Some(workspace_roots);
        }
        if let Some(profile_workspace_roots) = snapshot.profile_workspace_roots.clone() {
            self.settings.profile_workspace_roots = Some(profile_workspace_roots);
        }
        if let Some(sandbox_policy) = snapshot.sandbox_policy.clone() {
            self.settings.sandbox_policy = Some(sandbox_policy);
        }
        if let Some(windows_sandbox_level) = snapshot.windows_sandbox_level {
            self.settings.windows_sandbox_level = Some(windows_sandbox_level);
        }
        if let Some(reasoning_effort) = snapshot.reasoning_effort.as_ref() {
            self.settings.reasoning_effort = Some(reasoning_effort.clone());
        }
        if let Some(reasoning_summary) = snapshot.reasoning_summary {
            self.settings.reasoning_summary = Some(reasoning_summary);
        }
        if let Some(personality) = snapshot.personality {
            self.settings.personality = Some(personality);
        }
        self.settings.collaboration_mode = Some(snapshot.collaboration_mode.clone());
    }
}

/// Reduce an exact rollout prefix into persistent thread settings.
///
/// `fallback` is used only for fields for which the supplied history has no
/// evidence. History records always overwrite it in rollout order.
pub fn reduce_persisted_thread_settings(
    items: &[RolloutItem],
    fallback: PersistedThreadSettings,
) -> PersistedThreadSettings {
    let mut reducer = PersistedThreadSettingsReducer::new(fallback);
    for item in items {
        reducer.apply_item(item);
    }
    reducer.into_settings()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_types::ModeKind;
    use crate::config_types::Settings;
    use crate::models::PermissionProfile;
    use crate::protocol::ThreadSettingsAppliedEvent;
    use crate::protocol::TurnEnvironmentSelection;
    use codex_utils_path_uri::PathUri;

    fn absolute_path(suffix: &str) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(
            std::env::current_dir()
                .expect("current directory")
                .join(suffix),
        )
        .expect("absolute test path")
    }

    fn collaboration_mode(model: &str, effort: Option<ReasoningEffort>) -> CollaborationMode {
        CollaborationMode {
            mode: ModeKind::Plan,
            settings: Settings {
                model: model.to_string(),
                reasoning_effort: effort,
                developer_instructions: Some("persisted plan".to_string()),
            },
        }
    }

    fn snapshot(model: &str, cwd_suffix: &str) -> ThreadSettingsSnapshot {
        let cwd = absolute_path(cwd_suffix);
        ThreadSettingsSnapshot {
            model: model.to_string(),
            model_provider_id: format!("provider-{model}"),
            service_tier: Some(Some("flex".to_string())),
            developer_instructions: Some(Some("persisted developer instructions".to_string())),
            approval_policy: AskForApproval::Never,
            approvals_reviewer: ApprovalsReviewer::AutoReview,
            permission_profile: PermissionProfile::workspace_write(),
            active_permission_profile: Some(None),
            environments: Some(TurnEnvironmentSelections::new(
                cwd.clone(),
                vec![TurnEnvironmentSelection {
                    environment_id: "environment-1".to_string(),
                    cwd: PathUri::from_abs_path(&cwd),
                }],
            )),
            workspace_roots: Some(vec![cwd.clone()]),
            profile_workspace_roots: Some(vec![absolute_path("profile-root")]),
            sandbox_policy: Some(SandboxPolicy::new_workspace_write_policy()),
            windows_sandbox_level: Some(WindowsSandboxLevel::RestrictedToken),
            cwd,
            reasoning_effort: Some(Some(ReasoningEffort::High)),
            reasoning_summary: Some(Some(ReasoningSummary::Detailed)),
            personality: Some(Some(Personality::Friendly)),
            collaboration_mode: collaboration_mode(model, Some(ReasoningEffort::High)),
        }
    }

    #[test]
    fn snapshots_replay_in_rollout_order_and_keep_authoritative_environments() {
        let first = snapshot("first", "first-cwd");
        let second = snapshot("second", "second-cwd");
        let later_turn_cwd = absolute_path("selected-environment-cwd");
        let later_turn = TurnContextItem {
            turn_id: Some("turn-1".to_string()),
            cwd: later_turn_cwd,
            workspace_roots: None,
            current_date: None,
            timezone: None,
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: None,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            permission_profile: Some(PermissionProfile::read_only()),
            network: None,
            file_system_sandbox_policy: None,
            model: "turn-model".to_string(),
            comp_hash: None,
            personality: None,
            collaboration_mode: None,
            multi_agent_version: None,
            multi_agent_mode: None,
            effort: None,
            context_provenance: None,
        };
        let mut history = vec![
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_settings: first,
                },
            )),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_settings: second.clone(),
                },
            )),
        ];
        let snapshot_settings =
            reduce_persisted_thread_settings(&history, PersistedThreadSettings::default());
        assert_eq!(snapshot_settings.model.as_deref(), Some("second"));
        assert_eq!(&snapshot_settings.service_tier, &second.service_tier);
        assert_eq!(
            &snapshot_settings.developer_instructions,
            &second.developer_instructions
        );
        assert_eq!(
            snapshot_settings.permission_profile,
            Some(second.permission_profile.clone())
        );
        assert_eq!(
            &snapshot_settings.active_permission_profile,
            &second.active_permission_profile
        );
        assert_eq!(&snapshot_settings.workspace_roots, &second.workspace_roots);
        assert_eq!(
            &snapshot_settings.profile_workspace_roots,
            &second.profile_workspace_roots
        );
        assert_eq!(&snapshot_settings.sandbox_policy, &second.sandbox_policy);
        assert_eq!(
            snapshot_settings.windows_sandbox_level,
            second.windows_sandbox_level
        );
        assert_eq!(
            &snapshot_settings.reasoning_effort,
            &second.reasoning_effort
        );
        assert_eq!(
            snapshot_settings.reasoning_summary,
            second.reasoning_summary
        );
        assert_eq!(snapshot_settings.personality, second.personality);

        history.push(RolloutItem::TurnContext(later_turn));
        let settings =
            reduce_persisted_thread_settings(&history, PersistedThreadSettings::default());

        assert_eq!(settings.model.as_deref(), Some("turn-model"));
        assert_eq!(
            settings.model_provider_id.as_deref(),
            Some("provider-second")
        );
        assert_eq!(
            settings
                .environments
                .as_ref()
                .map(|environments| &environments.legacy_fallback_cwd),
            Some(&second.cwd)
        );
        assert_eq!(settings.reasoning_effort, Some(None));
    }

    #[test]
    fn fallback_only_supplies_fields_missing_from_history() {
        let event = snapshot("history-model", "history-cwd");
        let fallback_cwd = absolute_path("fallback-cwd");
        let settings = reduce_persisted_thread_settings(
            &[RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_settings: event,
                },
            ))],
            PersistedThreadSettings {
                model: Some("metadata-model".to_string()),
                model_provider_id: Some("metadata-provider".to_string()),
                environments: Some(TurnEnvironmentSelections::new(fallback_cwd, Vec::new())),
                ..Default::default()
            },
        );

        assert_eq!(settings.model.as_deref(), Some("history-model"));
        assert_eq!(
            settings.model_provider_id.as_deref(),
            Some("provider-history-model")
        );
    }

    #[test]
    fn old_snapshot_keeps_missing_fields_unsupplied_and_uses_legacy_cwd() {
        let cwd = absolute_path("old-cwd");
        let value = serde_json::json!({
            "model": "old-model",
            "model_provider_id": "old-provider",
            "approval_policy": "never",
            "approvals_reviewer": "user",
            "permission_profile": PermissionProfile::read_only(),
            "cwd": cwd,
            "collaboration_mode": collaboration_mode("old-model", None),
        });
        let snapshot: ThreadSettingsSnapshot =
            serde_json::from_value(value).expect("deserialize old snapshot");
        let fallback_profile = ActivePermissionProfile::new("fallback-profile");
        let settings = reduce_persisted_thread_settings(
            &[RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_settings: snapshot,
                },
            ))],
            PersistedThreadSettings {
                service_tier: Some(Some("metadata-tier".to_string())),
                developer_instructions: Some(Some("metadata instructions".to_string())),
                active_permission_profile: Some(Some(fallback_profile.clone())),
                reasoning_effort: Some(Some(ReasoningEffort::Medium)),
                reasoning_summary: Some(Some(ReasoningSummary::Concise)),
                personality: Some(Some(Personality::Pragmatic)),
                ..Default::default()
            },
        );

        assert_eq!(
            settings
                .environments
                .as_ref()
                .map(|environments| &environments.legacy_fallback_cwd),
            Some(&cwd)
        );
        assert!(settings.workspace_roots.is_none());
        assert!(settings.profile_workspace_roots.is_none());
        assert!(settings.sandbox_policy.is_none());
        assert!(settings.windows_sandbox_level.is_none());
        assert_eq!(
            settings.service_tier,
            Some(Some("metadata-tier".to_string()))
        );
        assert_eq!(
            settings.developer_instructions,
            Some(Some("metadata instructions".to_string()))
        );
        assert_eq!(
            settings.active_permission_profile,
            Some(Some(fallback_profile))
        );
        assert_eq!(
            settings.reasoning_effort,
            Some(Some(ReasoningEffort::Medium))
        );
        assert_eq!(
            settings.reasoning_summary,
            Some(Some(ReasoningSummary::Concise))
        );
        assert_eq!(settings.personality, Some(Some(Personality::Pragmatic)));
    }

    #[test]
    fn explicit_null_snapshot_fields_clear_fallback_values() {
        let cwd = absolute_path("null-cwd");
        let value = serde_json::json!({
            "model": "null-model",
            "model_provider_id": "null-provider",
            "service_tier": null,
            "developer_instructions": null,
            "approval_policy": "never",
            "approvals_reviewer": "user",
            "permission_profile": PermissionProfile::read_only(),
            "active_permission_profile": null,
            "cwd": cwd,
            "reasoning_effort": null,
            "reasoning_summary": null,
            "personality": null,
            "collaboration_mode": collaboration_mode("null-model", None),
        });
        let snapshot: ThreadSettingsSnapshot =
            serde_json::from_value(value).expect("deserialize explicit null snapshot");
        let settings = reduce_persisted_thread_settings(
            &[RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_settings: snapshot,
                },
            ))],
            PersistedThreadSettings {
                service_tier: Some(Some("metadata-tier".to_string())),
                developer_instructions: Some(Some("metadata instructions".to_string())),
                active_permission_profile: Some(Some(ActivePermissionProfile::new(
                    "metadata-profile",
                ))),
                reasoning_effort: Some(Some(ReasoningEffort::High)),
                reasoning_summary: Some(Some(ReasoningSummary::Detailed)),
                personality: Some(Some(Personality::Friendly)),
                ..Default::default()
            },
        );

        assert_eq!(settings.service_tier, Some(None));
        assert_eq!(settings.developer_instructions, Some(None));
        assert_eq!(settings.active_permission_profile, Some(None));
        assert_eq!(settings.reasoning_effort, Some(None));
        assert_eq!(settings.reasoning_summary, Some(None));
        assert_eq!(settings.personality, Some(None));
    }
}
