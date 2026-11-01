use codex_protocol::items::HookPromptItem;
use codex_protocol::items::parse_hook_prompt_fragment;
use codex_protocol::models::ContentItem;

use super::AdditionalContextUserFragment;
use super::FragmentRegistration;
use super::FragmentRegistrationProxy;
use super::InternalModelContextFragment;
use super::RecommendedPluginsInstructions;
use super::SkillInjection;
use super::SubagentNotification;
use super::TaskCapsuleFragment;
use super::TurnAborted;
use super::UserInstructions;
use super::UserShellCommand;
use super::world_state::EnvironmentsState;

static USER_INSTRUCTIONS_REGISTRATION: FragmentRegistrationProxy<UserInstructions> =
    FragmentRegistrationProxy::new();
static ENVIRONMENT_CONTEXT_REGISTRATION: FragmentRegistrationProxy<EnvironmentsState> =
    FragmentRegistrationProxy::new();
static SUBAGENTS_CONTEXT_REGISTRATION: FragmentRegistrationProxy<
    super::world_state::SubagentsState,
> = FragmentRegistrationProxy::new();
static ADDITIONAL_CONTEXT_REGISTRATION: FragmentRegistrationProxy<AdditionalContextUserFragment> =
    FragmentRegistrationProxy::new();
static SKILL_INSTRUCTIONS_REGISTRATION: FragmentRegistrationProxy<SkillInjection> =
    FragmentRegistrationProxy::new();
static AVAILABLE_SKILLS_REGISTRATION: FragmentRegistrationProxy<
    super::AvailableSkillsInstructions,
> = FragmentRegistrationProxy::new();
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
static TASK_CAPSULE_REGISTRATION: FragmentRegistrationProxy<TaskCapsuleFragment> =
    FragmentRegistrationProxy::new();
static TASK_STATE_REGISTRATION: FragmentRegistrationProxy<super::world_state::TaskState> =
    FragmentRegistrationProxy::new();

static CONTEXTUAL_USER_FRAGMENTS: &[&dyn FragmentRegistration] = &[
    &USER_INSTRUCTIONS_REGISTRATION,
    &ENVIRONMENT_CONTEXT_REGISTRATION,
    &SUBAGENTS_CONTEXT_REGISTRATION,
    &ADDITIONAL_CONTEXT_REGISTRATION,
    &SKILL_INSTRUCTIONS_REGISTRATION,
    &AVAILABLE_SKILLS_REGISTRATION,
    &USER_SHELL_COMMAND_REGISTRATION,
    &TURN_ABORTED_REGISTRATION,
    &SUBAGENT_NOTIFICATION_REGISTRATION,
    &INTERNAL_MODEL_CONTEXT_REGISTRATION,
    &RECOMMENDED_PLUGINS_REGISTRATION,
    &TASK_CAPSULE_REGISTRATION,
    &TASK_STATE_REGISTRATION,
];

static STARTUP_CONTEXTUAL_USER_FRAGMENTS: &[&dyn FragmentRegistration] = &[
    &USER_INSTRUCTIONS_REGISTRATION,
    &ENVIRONMENT_CONTEXT_REGISTRATION,
    &SUBAGENTS_CONTEXT_REGISTRATION,
    &SKILL_INSTRUCTIONS_REGISTRATION,
    &AVAILABLE_SKILLS_REGISTRATION,
    &RECOMMENDED_PLUGINS_REGISTRATION,
];

fn is_standard_contextual_user_text(text: &str) -> bool {
    codex_context_fragments::is_legacy_compaction_warning_text(text)
        || CONTEXTUAL_USER_FRAGMENTS
            .iter()
            .any(|fragment| fragment.matches_text(text))
}

pub fn is_contextual_user_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    parse_hook_prompt_fragment(text).is_some() || is_standard_contextual_user_text(text)
}

pub fn is_startup_contextual_user_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    STARTUP_CONTEXTUAL_USER_FRAGMENTS
        .iter()
        .any(|fragment| fragment.matches_text(text))
}

pub fn is_legacy_compaction_warning_fragment(content_item: &ContentItem) -> bool {
    let ContentItem::InputText { text } = content_item else {
        return false;
    };
    codex_context_fragments::is_legacy_compaction_warning_text(text)
}

pub fn parse_visible_hook_prompt_message(
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

