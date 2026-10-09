//! Context fragments injected into model input.

mod approved_command_prefix_saved;
mod apps_instructions;
mod available_plugins_instructions;
mod available_skills_instructions;
mod collaboration_mode_instructions;
mod contextual_user_message;
mod current_time_reminder;
pub(crate) mod desktop_instructions;
mod environment_context;
mod hook_additional_context;
mod image_generation_instructions;
mod inter_agent_completion_message;
mod internal_model_context;
mod model_switch_instructions;
mod multi_agent_mode_instructions;
mod network_rule_saved;
mod personality_spec_instructions;
mod plugin_instructions;
mod prompt_provenance;
mod recommended_plugins_instructions;
mod subagent_notification;
mod task_capsule;
mod turn_aborted;
mod token_budget_context;
pub(crate) use token_budget_context::{
    AutoCompactFallbackPrompt, ContextWindowGuidance, TokenBudgetContext,
    TokenBudgetReminder,
};
mod user_instructions;
mod user_shell_command;
pub(crate) mod world_state;

pub(crate) use approved_command_prefix_saved::ApprovedCommandPrefixSaved;
pub(crate) use environment_context::is_session_visualization_directory;
pub(crate) use apps_instructions::AppsInstructions;
pub(crate) use apps_instructions::AppsInstructionsUnavailable;
pub(crate) use available_plugins_instructions::AvailablePluginsInstructions;
pub(crate) use available_plugins_instructions::PluginsInstructionsUnavailable;
pub use available_skills_instructions::AvailableSkillsInstructions;
pub(crate) use available_skills_instructions::SKILLS_USAGE_INSTRUCTIONS_OPEN_TAG;
pub(crate) use available_skills_instructions::SkillsUsageInstructions;
pub(crate) use codex_context_fragments::AdditionalContextDeveloperFragment;
pub(crate) use codex_context_fragments::AdditionalContextUserFragment;
pub use codex_context_fragments::ContextualUserFragment;
pub(crate) use codex_context_fragments::FragmentRegistration;
pub(crate) use codex_context_fragments::FragmentRegistrationProxy;
pub(crate) use codex_core_skills::injection::SkillInjection;
pub use codex_prompts::ApprovalPromptContext;
pub use codex_prompts::PermissionsInstructions;
pub(crate) use collaboration_mode_instructions::CollaborationModeInstructions;
pub(crate) use contextual_user_message::is_contextual_user_fragment;
pub(crate) use contextual_user_message::is_legacy_compaction_warning_fragment;
pub(crate) use contextual_user_message::is_startup_contextual_user_fragment;
pub(crate) use contextual_user_message::parse_visible_hook_prompt_message;
pub(crate) use current_time_reminder::CurrentTimeReminder;
pub(crate) use hook_additional_context::HookAdditionalContext;
pub use image_generation_instructions::extension_image_generation_output_hint;
pub(crate) use inter_agent_completion_message::InterAgentCompletionMessage;
pub use internal_model_context::InternalContextSource;
pub use internal_model_context::InternalModelContextFragment;
pub use internal_model_context::InvalidInternalContextSource;
pub(crate) use model_switch_instructions::ModelSwitchInstructions;
pub(crate) use multi_agent_mode_instructions::EffectiveMultiAgentMode;
pub(crate) use multi_agent_mode_instructions::MultiAgentModeInstructions;
pub(crate) use network_rule_saved::NetworkRuleSaved;
pub(crate) use personality_spec_instructions::PersonalitySpecInstructions;
pub(crate) use plugin_instructions::PluginInstructions;
pub(crate) use prompt_provenance::PromptContextBreakdown;
pub(crate) use prompt_provenance::PromptContextCategory;
pub(crate) use prompt_provenance::PromptContextMeasurement;
pub(crate) use prompt_provenance::PromptProvenanceSidecar;
pub(crate) use recommended_plugins_instructions::RecommendedPluginsInstructions;
pub(crate) use subagent_notification::SubagentNotification;
pub(crate) use task_capsule::TaskCapsuleFragment;
pub(crate) use turn_aborted::TurnAborted;
pub(crate) use turn_aborted::lost_turn_recovery;
pub(crate) use user_instructions::UserInstructions;
pub(crate) use user_shell_command::UserShellCommand;

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

#[cfg(test)]
mod escaping_tests {
    use super::escape_fragment_delimiters;

    #[test]
    fn escaping_preserves_ordered_replacement_and_source_bytes() {
        // Literal oracles follow the fragment contract: only each selected
        // delimiter's angle brackets are escaped, in the supplied order. In
        // particular an earlier replacement can remove a later match, while
        // unrelated syntax, existing entities and Unicode remain unchanged.
        for (text, expected) in [
            ("", ["", "", "", "", ""]),
            ("ordinary λ source &lt;tag&gt; <other>", [
                "ordinary λ source &lt;tag&gt; <other>",
                "ordinary λ source &lt;tag&gt; <other>",
                "ordinary λ source &lt;tag&gt; <other>",
                "ordinary λ source &lt;tag&gt; &lt;other>",
                "ordinary λ source &lt;tag&gt; <other>",
            ]),
            ("<INSTRUCTIONS>λ</INSTRUCTIONS><INSTRUCTIONS>", [
                "<INSTRUCTIONS>λ</INSTRUCTIONS><INSTRUCTIONS>",
                "<INSTRUCTIONS>λ</INSTRUCTIONS><INSTRUCTIONS>",
                "&lt;INSTRUCTIONS&gt;λ&lt;/INSTRUCTIONS&gt;&lt;INSTRUCTIONS&gt;",
                "&lt;INSTRUCTIONS>λ&lt;/INSTRUCTIONS>&lt;INSTRUCTIONS>",
                "&lt;INSTRUCTIONS&gt;λ</INSTRUCTIONS>&lt;INSTRUCTIONS&gt;",
            ]),
        ] {
            for (delimiters, expected) in [
                vec![], vec![""], vec!["<INSTRUCTIONS>", "</INSTRUCTIONS>"],
                vec!["<", "<INSTRUCTIONS>"], vec!["<INSTRUCTIONS>", "&lt;"],
            ].into_iter().zip(expected) {
                assert_eq!(escape_fragment_delimiters(text, &delimiters), expected);
            }
        }
    }
}
