use crate::context::ApprovalPromptContext;
use crate::context::CollaborationModeInstructions;
use crate::context::ContextualUserFragment;
use crate::context::EffectiveMultiAgentMode;
use crate::context::ModelSwitchInstructions;
use crate::context::MultiAgentModeInstructions;
use crate::context::PermissionsInstructions;
use crate::context::PersonalitySpecInstructions;
use crate::session::PreviousTurnSettings;
use crate::session::turn_context::TurnContext;
use codex_execpolicy::Policy;
use codex_features::Feature;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::config_types::Personality;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::TurnContextItem;

fn build_permissions_update_item(
    previous: Option<&TurnContextItem>,
    next: &TurnContext,
    exec_policy: &Policy,
) -> Option<String> {
    if !next.config.include_permissions_instructions {
        return None;
    }

    let prev = previous?;
    if prev.permission_profile() == next.permission_profile()
        && prev.approval_policy == next.approval_policy.value()
        && prev.model == next.model_info.slug
    {
        return None;
    }

    Some(
        PermissionsInstructions::from_permission_profile(
            &next.permission_profile,
            next.approval_policy.value(),
            ApprovalPromptContext::new(
                next.config.approvals_reviewer,
                next.model_info
                    .model_messages
                    .as_ref()
                    .and_then(|messages| messages.approvals.as_ref()),
            ),
            exec_policy,
            next.cwd(),
            next.config
                .features
                .enabled(Feature::ExecPermissionApprovals),
            next.config
                .features
                .enabled(Feature::RequestPermissionsTool),
        )
        .render(),
    )
}

fn build_collaboration_mode_update_item(
    previous: Option<&TurnContextItem>,
    next: &TurnContext,
) -> Option<String> {
    if !next.config.include_collaboration_mode_instructions {
        return None;
    }

    let prev = previous?;
    let previous_instructions = prev
        .collaboration_mode
        .as_ref()
        .and_then(CollaborationModeInstructions::from_collaboration_mode)
        .map(|instructions| instructions.render());
    let next_instructions =
        CollaborationModeInstructions::from_collaboration_mode(&next.collaboration_mode)
            .map(|instructions| instructions.render());
    if previous_instructions == next_instructions {
        return None;
    }
    match next_instructions {
        Some(instructions) => Some(instructions),
        None if previous_instructions.is_some() => {
            Some(CollaborationModeInstructions::reset().render())
        }
        None => None,
    }
}

fn build_multi_agent_mode_update_item(
    previous: Option<&TurnContextItem>,
    next: &TurnContext,
) -> Option<String> {
    let effective_multi_agent_mode = crate::session::multi_agents::effective_multi_agent_mode(next);
    let previous = previous?;
    if previous.multi_agent_mode
        == effective_multi_agent_mode
            .as_ref()
            .map(EffectiveMultiAgentMode::to_persisted_mode)
    {
        return None;
    }

    match effective_multi_agent_mode {
        Some(multi_agent_mode) => Some(MultiAgentModeInstructions::new(multi_agent_mode).render()),
        None if previous.multi_agent_mode == Some(MultiAgentMode::Proactive) => Some(
            MultiAgentModeInstructions::new(EffectiveMultiAgentMode::ExplicitRequestOnly).render(),
        ),
        None => None,
    }
}

fn build_personality_update_item(
    previous: Option<&TurnContextItem>,
    next: &TurnContext,
    personality_feature_enabled: bool,
) -> Option<String> {
    if !personality_feature_enabled {
        return None;
    }
    let previous = previous?;

    let personality = next.personality?;
    if Some(personality) == previous.personality && previous.model == next.model_info.slug {
        return None;
    }

    if personality == Personality::None {
        return previous
            .personality
            .filter(|previous| *previous != Personality::None)
            .map(|_| PersonalitySpecInstructions::reset().render());
    }

    let model_info = &next.model_info;
    let personality_message = personality_message_for(model_info, personality);
    personality_message.map(|message| PersonalitySpecInstructions::new(message).render())
}

pub(crate) fn personality_message_for(
    model_info: &ModelInfo,
    personality: Personality,
) -> Option<String> {
    let catalog_message = model_info
        .model_messages
        .as_ref()
        .and_then(|spec| spec.get_personality_message(Some(personality)))
        .filter(|message| !message.is_empty());
    catalog_message.or_else(|| generic_personality_message(personality).map(str::to_string))
}

fn generic_personality_message(personality: Personality) -> Option<&'static str> {
    match personality {
        Personality::None => None,
        Personality::Friendly => Some(
            "Be warm, collaborative, and candid. Keep momentum while explaining decisions clearly; never trade truthfulness for agreement.",
        ),
        Personality::Pragmatic => Some(
            "Be concise, direct, and engineering-focused. State assumptions, tradeoffs, and next actions plainly; avoid filler.",
        ),
    }
}

pub(crate) fn build_model_switch_update_item(
    previous_turn_settings: Option<&PreviousTurnSettings>,
    next: &TurnContext,
) -> Option<String> {
    let previous_turn_settings = previous_turn_settings?;
    if previous_turn_settings.model == next.model_info.slug {
        return None;
    }

    Some(ModelSwitchInstructions::new().render())
}

pub(crate) fn build_developer_update_item(text_sections: Vec<String>) -> Option<ResponseItem> {
    build_text_message("developer", text_sections)
}

pub(crate) fn build_contextual_user_message(text_sections: Vec<String>) -> Option<ResponseItem> {
    build_text_message("user", text_sections)
}

pub(crate) fn merge_contextual_fragments(
    fragments: Vec<Box<dyn ContextualUserFragment>>,
) -> Vec<ResponseItem> {
    let mut messages: Vec<(&str, Vec<String>)> = Vec::with_capacity(fragments.len());
    for fragment in fragments {
        let role = fragment.role();
        let text = fragment.render();
        match messages.last_mut() {
            Some((previous_role, text_sections)) if *previous_role == role => {
                text_sections.push(text);
            }
            _ => messages.push((role, vec![text])),
        }
    }
    messages
        .into_iter()
        .filter_map(|(role, text_sections)| build_text_message(role, text_sections))
        .collect()
}

fn build_text_message(role: &str, text_sections: Vec<String>) -> Option<ResponseItem> {
    if text_sections.is_empty() {
        return None;
    }

    let content = text_sections
        .into_iter()
        .map(|text| ContentItem::InputText { text })
        .collect();

    let mut item = ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    crate::stable_context::mark_trusted_stable_context_item(&mut item);
    Some(item)
}

pub(crate) fn build_settings_update_items(
    previous: Option<&TurnContextItem>,
    previous_turn_settings: Option<&PreviousTurnSettings>,
    next: &TurnContext,
    exec_policy: &Policy,
    personality_feature_enabled: bool,
) -> Vec<ResponseItem> {
    // TODO(ccunningham): build_settings_update_items still does not cover every
    // model-visible item emitted by build_initial_context. Persist the remaining
    // inputs or add explicit replay events so fork/resume can diff everything
    // deterministically.
    let developer_update_sections = [
        // Keep the model-switch compatibility note first, followed by independent setting
        // deltas that remain relevant to the new model.
        build_model_switch_update_item(previous_turn_settings, next),
        build_permissions_update_item(previous, next, exec_policy),
        build_collaboration_mode_update_item(previous, next),
        build_multi_agent_mode_update_item(previous, next),
        build_personality_update_item(previous, next, personality_feature_enabled),
    ]
    .into_iter()
    .flatten()
    .collect();

    build_developer_update_item(developer_update_sections)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::build_collaboration_mode_update_item;
    use super::build_personality_update_item;
    use super::generic_personality_message;
    use crate::session::tests::make_session_and_context;
    use codex_protocol::config_types::Personality;

    #[test]
    fn generic_personality_fallback_covers_template_less_models() {
        assert!(
            generic_personality_message(Personality::Friendly)
                .is_some_and(|message| message.contains("warm, collaborative"))
        );
        assert!(
            generic_personality_message(Personality::Pragmatic)
                .is_some_and(|message| message.contains("engineering-focused"))
        );
        assert_eq!(generic_personality_message(Personality::None), None);
    }

    #[tokio::test]
    async fn model_switch_reinjects_an_unchanged_personality() {
        let (_session, mut next) = make_session_and_context().await;
        next.personality = Some(Personality::Friendly);
        next.model_info.model_messages = None;
        let mut previous = next.to_turn_context_item();
        previous.personality = Some(Personality::Friendly);
        previous.model = "previous-model".to_string();

        let update = build_personality_update_item(Some(&previous), &next, true)
            .expect("model switch should refresh model-visible personality wording");

        assert!(update.contains("<personality_spec>"));
        assert!(update.contains("warm, collaborative"));
    }

    #[tokio::test]
    async fn collaboration_metadata_change_without_prompt_change_emits_no_delta() {
        let (_session, next) = make_session_and_context().await;
        let mut previous = next.to_turn_context_item();
        let previous_mode = previous
            .collaboration_mode
            .as_mut()
            .expect("test turn should persist collaboration mode");
        previous_mode.settings.model = "previous-model".to_string();

        assert_eq!(
            build_collaboration_mode_update_item(Some(&previous), &next),
            None
        );
    }
}
