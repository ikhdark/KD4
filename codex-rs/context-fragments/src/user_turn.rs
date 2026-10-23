//! Shared rollback eligibility for model reconstruction and client history.
use crate::AdditionalContextUserFragment;
use crate::ContextualUserFragment;
use crate::fragment::matches_marked_text;
use codex_protocol::items::parse_hook_prompt_fragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RolloutItem;

pub fn is_user_turn_boundary(item: &ResponseItem) -> bool {
    match item {
        // This durable namespace is reserved at provider ingress and assigned
        // only by trusted context producers. Text alone is not provenance.
        ResponseItem::Message { id: Some(id), .. }
            if id.as_str().starts_with("msg_sctx_") => false,
        ResponseItem::AgentMessage { .. } => true,
        ResponseItem::Message {
            id, role, content, ..
        } if role == "user" => {
            id.as_ref().is_some_and(|id| id.starts_with("msg_user_"))
                || content.is_empty()
                || !content.iter().all(|part| {
                    matches!(part,
                    ContentItem::InputText { text } if is_contextual_user_text(text))
                })
        }
        _ => false,
    }
}

/// Reads inter-agent mail from rollout records written before typed delivery:
/// such writers persisted mail as an assistant message whose only text is the
/// serialized `InterAgentCommunication`, and their rollback markers counted it
/// as an instruction turn. The shape alone is indistinguishable from an
/// assistant's JSON example, so [`is_user_turn_boundary`] never accepts it.
/// A rollout reader accepts it only until the rollout shows a writer that
/// records typed delivery (`inter_agent_communication*`) or stamps turn
/// attribution onto response items; such writers never encode mail as
/// assistant JSON. Typed `agent_message` items do not end the window: older
/// writers emitted encrypted ones next to assistant-encoded plaintext mail.
///
/// Observe every record of one rollout, in order.
#[derive(Clone, Copy, Debug, Default)]
pub struct LegacyInterAgentMailReader {
    typed_delivery_writer_seen: bool,
}

impl LegacyInterAgentMailReader {
    /// Returns the legacy mail `item` carries, if it is legacy mail.
    pub fn observe(&mut self, item: &RolloutItem) -> Option<InterAgentCommunication> {
        match item {
            RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. } => {
                self.typed_delivery_writer_seen = true;
                None
            }
            RolloutItem::ResponseItem(item) if item.turn_id().is_some() => {
                self.typed_delivery_writer_seen = true;
                None
            }
            RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                if !self.typed_delivery_writer_seen && role == "assistant" =>
            {
                InterAgentCommunication::from_message_content(content)
            }
            _ => None,
        }
    }
}

fn is_contextual_user_text(text: &str) -> bool {
    const MARKERS: &[(&str, &str)] = &[
        ("# AGENTS.md instructions", "</INSTRUCTIONS>"),
        ("<environment_context>", "</environment_context>"),
        ("<subagents_context>", "</subagents_context>"),
        ("<skill>", "</skill>"),
        ("<skills_instructions>", "</skills_instructions>"),
        ("<user_shell_command>", "</user_shell_command>"),
        ("<turn_aborted>", "</turn_aborted>"),
        ("<subagent_notification>", "</subagent_notification>"),
        ("<recommended_plugins>", "</recommended_plugins>"),
        ("<task_capsule_v1>", "</task_capsule_v1>"),
        ("<codex_task_state>", "</codex_task_state>"),
    ];
    if MARKERS
        .iter()
        .any(|(open, close)| matches_marked_text(open, close, text))
        || parse_hook_prompt_fragment(text).is_some()
        || AdditionalContextUserFragment::matches_text(text)
        || is_internal_context(text.trim())
    {
        return true;
    }
    let text = text.trim();
    if matches!(
        text,
        "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command."
            | "Warning: Your account was flagged for potentially high-risk cyber activity."
            | "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety"
    ) {
        return true;
    }
    let Some(rest) = text.strip_prefix(
        "Warning: The maximum number of unified exec processes you can keep open is ",
    ) else {
        return false;
    };
    let is_count = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(limit) = rest.strip_suffix('.') {
        return is_count(limit);
    }
    let Some(counts) = rest.strip_suffix(" processes open. Reuse older processes or close them to prevent automatic pruning of old processes") else {
        return false;
    };
    counts
        .split_once(" and you currently have ")
        .is_some_and(|(limit, open)| is_count(limit) && is_count(open))
}

fn is_internal_context(text: &str) -> bool {
    const OPEN: &str = "<codex_internal_context";
    const CLOSE: &str = "</codex_internal_context>";
    if let Some(body) = text
        .strip_prefix("<goal_context>")
        .and_then(|body| body.strip_suffix("</goal_context>"))
    {
        return !body.contains("<goal_context>") && !body.contains("</goal_context>");
    }
    let Some((source, body)) = text
        .strip_prefix(OPEN)
        .and_then(|rest| rest.strip_prefix(" source=\""))
        .and_then(|rest| rest.split_once("\">"))
    else {
        return false;
    };
    let mut chars = source.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_lowercase())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        && body
            .strip_suffix(CLOSE)
            .is_some_and(|body| !body.contains(OPEN) && !body.contains(CLOSE))
}
