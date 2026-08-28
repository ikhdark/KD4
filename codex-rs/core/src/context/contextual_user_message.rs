use codex_protocol::items::HookPromptItem;
use codex_protocol::items::parse_hook_prompt_fragment;
use codex_protocol::models::ContentItem;

use super::AdditionalContextUserFragment;
use super::CompletionCheckpointContext;
use super::CompletionReviewRepair;
use super::FragmentRegistration;
use super::FragmentRegistrationProxy;
use super::InternalModelContextFragment;
use super::RecommendedPluginsInstructions;
use super::SkillInjection;
use super::SubagentNotification;
use super::TaskCapsuleFragment;
use super::TaskModelGuidance;
use super::TurnAborted;
use super::UserInstructions;
use super::UserShellCommand;
use super::world_state::EnvironmentsState;
use super::world_state::TaskEvidenceContext;

// These warnings are no longer produced. The fragment definitions remain here so compaction can
// recognize messages restored from old sessions.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegacyApplyPatchExecCommandWarning;

impl super::ContextualUserFragment for LegacyApplyPatchExecCommandWarning {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn matches_text(text: &str) -> bool {
        let trimmed = text.trim();
        trimmed.starts_with("Warning: apply_patch was requested via ")
            && trimmed.ends_with("Use the apply_patch tool instead of exec_command.")
    }

    fn body(&self) -> String {
        String::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegacyModelMismatchWarning;

impl super::ContextualUserFragment for LegacyModelMismatchWarning {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn matches_text(text: &str) -> bool {
        text.trim().starts_with(
            "Warning: Your account was flagged for potentially high-risk cyber activity",
        )
    }

    fn body(&self) -> String {
        String::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegacyUnifiedExecProcessLimitWarning;

impl super::ContextualUserFragment for LegacyUnifiedExecProcessLimitWarning {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn matches_text(text: &str) -> bool {
        text.trim().starts_with(
            "Warning: The maximum number of unified exec processes you can keep open is",
        )
    }

    fn body(&self) -> String {
        String::new()
    }
}

static USER_INSTRUCTIONS_REGISTRATION: FragmentRegistrationProxy<UserInstructions> =
    FragmentRegistrationProxy::new();
static ENVIRONMENT_CONTEXT_REGISTRATION: FragmentRegistrationProxy<EnvironmentsState> =
    FragmentRegistrationProxy::new();
static ADDITIONAL_CONTEXT_REGISTRATION: FragmentRegistrationProxy<AdditionalContextUserFragment> =
    FragmentRegistrationProxy::new();
static COMPLETION_REVIEW_REPAIR_REGISTRATION: FragmentRegistrationProxy<CompletionReviewRepair> =
    FragmentRegistrationProxy::new();
static COMPLETION_CHECKPOINT_REGISTRATION: FragmentRegistrationProxy<CompletionCheckpointContext> =
    FragmentRegistrationProxy::new();
static SKILL_INSTRUCTIONS_REGISTRATION: FragmentRegistrationProxy<SkillInjection> =
    FragmentRegistrationProxy::new();
static USER_SHELL_COMMAND_REGISTRATION: FragmentRegistrationProxy<UserShellCommand> =
    FragmentRegistrationProxy::new();
static TURN_ABORTED_REGISTRATION: FragmentRegistrationProxy<TurnAborted> =
    FragmentRegistrationProxy::new();
static SUBAGENT_NOTIFICATION_REGISTRATION: FragmentRegistrationProxy<SubagentNotification> =
    FragmentRegistrationProxy::new();
static INTERNAL_MODEL_CONTEXT_REGISTRATION: FragmentRegistrationProxy<
    InternalModelContextFragment,
> = FragmentRegistrationProxy::new();
static RECOMMENDED_PLUGINS_REGISTRATION: FragmentRegistrationProxy<RecommendedPluginsInstructions> =
    FragmentRegistrationProxy::new();
static LEGACY_UNIFIED_EXEC_PROCESS_LIMIT_WARNING_REGISTRATION: FragmentRegistrationProxy<
    LegacyUnifiedExecProcessLimitWarning,
> = FragmentRegistrationProxy::new();
static LEGACY_APPLY_PATCH_EXEC_COMMAND_WARNING_REGISTRATION: FragmentRegistrationProxy<
    LegacyApplyPatchExecCommandWarning,
> = FragmentRegistrationProxy::new();
static LEGACY_MODEL_MISMATCH_WARNING_REGISTRATION: FragmentRegistrationProxy<
    LegacyModelMismatchWarning,
> = FragmentRegistrationProxy::new();
static TASK_CAPSULE_REGISTRATION: FragmentRegistrationProxy<TaskCapsuleFragment> =
    FragmentRegistrationProxy::new();
static TASK_MODEL_GUIDANCE_REGISTRATION: FragmentRegistrationProxy<TaskModelGuidance> =
    FragmentRegistrationProxy::new();
static TASK_EVIDENCE_STATE_REGISTRATION: FragmentRegistrationProxy<TaskEvidenceContext> =
    FragmentRegistrationProxy::new();

static CONTEXTUAL_USER_FRAGMENTS: &[&dyn FragmentRegistration] = &[
    &USER_INSTRUCTIONS_REGISTRATION,
    &ENVIRONMENT_CONTEXT_REGISTRATION,
    &ADDITIONAL_CONTEXT_REGISTRATION,
    &COMPLETION_CHECKPOINT_REGISTRATION,
    &COMPLETION_REVIEW_REPAIR_REGISTRATION,
    &SKILL_INSTRUCTIONS_REGISTRATION,
    &USER_SHELL_COMMAND_REGISTRATION,
    &TURN_ABORTED_REGISTRATION,
    &SUBAGENT_NOTIFICATION_REGISTRATION,
    &INTERNAL_MODEL_CONTEXT_REGISTRATION,
    &RECOMMENDED_PLUGINS_REGISTRATION,
    &LEGACY_UNIFIED_EXEC_PROCESS_LIMIT_WARNING_REGISTRATION,
    &LEGACY_APPLY_PATCH_EXEC_COMMAND_WARNING_REGISTRATION,
    &LEGACY_MODEL_MISMATCH_WARNING_REGISTRATION,
    &TASK_CAPSULE_REGISTRATION,
    &TASK_MODEL_GUIDANCE_REGISTRATION,
    &TASK_EVIDENCE_STATE_REGISTRATION,
];

static STARTUP_CONTEXTUAL_USER_FRAGMENTS: &[&dyn FragmentRegistration] = &[
    &USER_INSTRUCTIONS_REGISTRATION,
    &ENVIRONMENT_CONTEXT_REGISTRATION,
    &SKILL_INSTRUCTIONS_REGISTRATION,
    &RECOMMENDED_PLUGINS_REGISTRATION,
    &TASK_MODEL_GUIDANCE_REGISTRATION,
    &TASK_EVIDENCE_STATE_REGISTRATION,
];

fn is_standard_contextual_user_text(text: &str) -> bool {
    CONTEXTUAL_USER_FRAGMENTS
        .iter()
        .any(|fragment| fragment.matches_text(text))
}

pub(crate) fn is_contextual_user_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    parse_hook_prompt_fragment(text).is_some() || is_standard_contextual_user_text(text)
}

pub(crate) fn is_startup_contextual_user_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    STARTUP_CONTEXTUAL_USER_FRAGMENTS
        .iter()
        .any(|fragment| fragment.matches_text(text))
}

pub(crate) fn is_task_evidence_context_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    TASK_EVIDENCE_STATE_REGISTRATION.matches_text(text)
}

pub(crate) fn is_legacy_compaction_warning_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    [
        &LEGACY_UNIFIED_EXEC_PROCESS_LIMIT_WARNING_REGISTRATION as &dyn FragmentRegistration,
        &LEGACY_APPLY_PATCH_EXEC_COMMAND_WARNING_REGISTRATION,
        &LEGACY_MODEL_MISMATCH_WARNING_REGISTRATION,
    ]
    .iter()
    .any(|fragment| fragment.matches_text(text))
}

pub(crate) fn parse_visible_hook_prompt_message(
    id: Option<&str>,
    content: &[ContentItem],
) -> Option<HookPromptItem> {
    let mut fragments = Vec::new();

    for content_item in content {
        let ContentItem::InputText { text } = content_item else {
            return None;
        };
        if let Some(fragment) = parse_hook_prompt_fragment(text) {
            fragments.push(fragment);
            continue;
        }
        if is_standard_contextual_user_text(text) {
            continue;
        }
        return None;
    }

    if fragments.is_empty() {
        return None;
    }

    Some(HookPromptItem::from_fragments(id, fragments))
}

#[cfg(test)]
#[path = "contextual_user_message_tests.rs"]
mod tests;
