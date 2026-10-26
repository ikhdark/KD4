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
    let settings_unchanged = prev.permission_profile() == next.permission_profile()
        && prev.approval_policy == next.approval_policy.value()
        && prev.model == next.model_info.slug;
    if settings_unchanged && prev.cwd == *next.cwd() {
        return None;
    }

    let render = |cwd: &std::path::Path| {
        PermissionsInstructions::from_permission_profile(
            &next.permission_profile,
            next.approval_policy.value(),
            ApprovalPromptContext::new(
                next.model_info
                    .model_messages
                    .as_ref()
                    .and_then(|messages| messages.approvals.as_ref()),
            ),
            exec_policy,
            cwd,
            next.config
                .features
                .enabled(Feature::ExecPermissionApprovals),
            next.config
                .features
                .enabled(Feature::RequestPermissionsTool),
        )
        .render()
    };
    let instructions = render(next.cwd().as_path());
    // Relative deny globs and root exclusions can change with cwd even when the
    // stored profile does not. Avoid reinjecting cwd-independent instructions.
    if settings_unchanged && instructions == render(prev.cwd.as_path()) {
        return None;
    }
    Some(instructions)
}

pub(crate) fn build_collaboration_mode_update_item(
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

pub(crate) fn build_personality_update_item(
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
            "Be concise, direct, and engineering-focused. State assumptions, tradeoffs, and next actions only when they change what the user should do; avoid filler.",
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
    let mut items = Vec::new();
    for fragment in fragments {
        let role = fragment.role();
        let item = fragment.into_boxed_response_item();
        let ResponseItem::Message { content, .. } = item else {
            continue;
        };
        let text = content
            .into_iter()
            .filter_map(|part| match part {
                ContentItem::InputText { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        match messages.last_mut() {
            Some((previous_role, text_sections)) if *previous_role == role => {
                text_sections.push(text);
            }
            _ => messages.push((role, vec![text])),
        }
    }
    items.extend(
        messages
            .into_iter()
            .filter_map(|(role, text_sections)| build_text_message(role, text_sections)),
    );
    items
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
        if previous.is_some_and(|previous| previous.model == next.model_info.slug) {
            None
        } else {
            build_model_switch_update_item(previous_turn_settings, next)
        },
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

