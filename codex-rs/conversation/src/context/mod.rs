//! Context fragments injected into model input.

mod approved_command_prefix_saved;
mod apps_instructions;
mod available_plugins_instructions;
mod available_skills_instructions;
mod collaboration_mode_instructions;
mod contextual_user_message;
pub mod desktop_instructions;
pub mod environment_context;
mod hook_additional_context;
mod image_generation_instructions;
mod inter_agent_completion_message;
mod internal_model_context;
mod model_switch_instructions;
mod multi_agent_mode_instructions;
mod personality_spec_instructions;
mod plugin_instructions;
mod prompt_provenance;
mod recommended_plugins_instructions;
mod subagent_notification;
mod task_capsule;
mod turn_aborted;
mod user_instructions;
mod user_shell_command;
mod workflow_checkpoint;
pub mod world_state;

pub use approved_command_prefix_saved::ApprovedCommandPrefixSaved;
pub use apps_instructions::AppsInstructions;
pub use apps_instructions::AppsInstructionsUnavailable;
pub use available_plugins_instructions::AvailablePluginsInstructions;
pub use available_plugins_instructions::PluginsInstructionsUnavailable;
pub use available_skills_instructions::AvailableSkillsInstructions;
pub use available_skills_instructions::SKILLS_USAGE_INSTRUCTIONS_OPEN_TAG;
pub use available_skills_instructions::SkillsUsageInstructions;
pub use codex_context_fragments::AdditionalContextDeveloperFragment;
pub use codex_context_fragments::AdditionalContextUserFragment;
pub use codex_context_fragments::ContextualUserFragment;
pub use codex_context_fragments::FragmentRegistration;
pub use codex_context_fragments::FragmentRegistrationProxy;
pub use codex_core_skills::injection::SkillInjection;
pub use codex_prompts::ApprovalPromptContext;
pub use codex_prompts::PermissionsInstructions;
pub use collaboration_mode_instructions::CollaborationModeInstructions;
pub use contextual_user_message::is_contextual_user_fragment;
pub use contextual_user_message::is_legacy_compaction_warning_fragment;
pub use contextual_user_message::is_startup_contextual_user_fragment;
pub use contextual_user_message::parse_visible_hook_prompt_message;
pub use environment_context::is_session_visualization_directory;
pub use hook_additional_context::HookAdditionalContext;
pub use image_generation_instructions::extension_image_generation_output_hint;
pub use inter_agent_completion_message::InterAgentCompletionMessage;
pub use internal_model_context::InternalContextSource;
pub use internal_model_context::InternalModelContextFragment;
pub use internal_model_context::InvalidInternalContextSource;
pub use model_switch_instructions::ModelSwitchInstructions;
pub use multi_agent_mode_instructions::EffectiveMultiAgentMode;
pub use multi_agent_mode_instructions::MultiAgentModeInstructions;
pub use personality_spec_instructions::PersonalitySpecInstructions;
pub use plugin_instructions::PluginInstructions;
pub use prompt_provenance::PromptContextBreakdown;
pub use prompt_provenance::PromptContextCategory;
pub use prompt_provenance::PromptContextMeasurement;
pub use prompt_provenance::PromptProvenanceSidecar;
pub use prompt_provenance::content_aware_serialized_tokens;
pub use recommended_plugins_instructions::RecommendedPluginsInstructions;
pub use subagent_notification::SubagentNotification;
pub use task_capsule::TaskCapsuleFragment;
pub use turn_aborted::{TurnAborted, append_retained_work};
pub use user_instructions::UserInstructions;
pub use user_shell_command::UserShellCommand;
pub use workflow_checkpoint::WorkflowCheckpoint;

/// Preserve source syntax in model-facing prose while keeping embedded copies of
/// the fragment's own delimiters from closing or reopening its sections.
fn escape_fragment_delimiters(text: &str, delimiters: &[&str]) -> String {
    delimiters.iter().fold(text.to_string(), |text, delimiter| {
        // Most source text contains none of its wrapper's delimiters. Keep the
        // existing allocation instead of copying the entire body for every tag.
        if !text.contains(delimiter) {
            return text;
        }
        text.replace(
            delimiter,
            &delimiter.replace('<', "&lt;").replace('>', "&gt;"),
        )
    })
}
