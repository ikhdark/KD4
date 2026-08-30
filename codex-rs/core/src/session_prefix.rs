use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

use crate::context::ContextualUserFragment;
use crate::context::InterAgentCompletionMessage;
use crate::context::SubagentNotification;

const COMPLETION_MESSAGE_MAX_TOKENS: usize = 1_000;
const COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE: usize = 100;
const ERROR_MAX_TOKENS: usize =
    COMPLETION_MESSAGE_MAX_TOKENS - COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE;
const ERROR_NEXT_ACTION: &str = "This agent's turn failed. The full sealed error remains available through get_agent_task; retrieve it with the assignment id returned by spawn_agent before deciding whether to retry. If you still need this agent, use the available collaboration tools to give it another task.";
const TYPED_COMPLETION_NEXT_ACTION: &str = "The full sealed receipt remains available through get_agent_task; retrieve it with the assignment id returned by spawn_agent.";

// Helpers for model-visible session state markers that are stored in user-role
// messages but are not user intent.

// TODO(jif) unify with structured schema
pub(crate) fn format_subagent_notification_message(
    agent_reference: &str,
    status: &AgentStatus,
) -> String {
    match status {
        AgentStatus::Errored(error) => {
            format_bounded_subagent_error_notification(agent_reference, error)
        }
        status => SubagentNotification::new(agent_reference, status.clone()).render(),
    }
}

fn format_bounded_subagent_error_notification(agent_reference: &str, error: &str) -> String {
    let mut error_budget = ERROR_MAX_TOKENS.min(approx_token_count(error));
    loop {
        let error = truncate_text(error, TruncationPolicy::Tokens(error_budget));
        let message =
            SubagentNotification::new(agent_reference, AgentStatus::Errored(error)).render();
        let message_tokens = approx_token_count(&message);
        if message_tokens < COMPLETION_MESSAGE_MAX_TOKENS || error_budget == 0 {
            return message;
        }

        // JSON escaping can expand control characters after raw-text truncation, so tighten the
        // source budget until the rendered notification itself fits the completion envelope.
        let next_budget = error_budget
            .saturating_mul(COMPLETION_MESSAGE_MAX_TOKENS.saturating_sub(1))
            / message_tokens;
        error_budget = next_budget.min(error_budget.saturating_sub(1));
    }
}

pub(crate) fn format_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    status: &AgentStatus,
) -> Option<String> {
    let payload = match status {
        AgentStatus::Completed(Some(message)) => {
            return Some(format_bounded_inter_agent_completion_message(
                task_name, sender, message,
            ));
        }
        AgentStatus::Completed(None) => String::new(),
        AgentStatus::CompletedWithSurface {
            last_agent_message: Some(message),
            ..
        } => {
            return Some(format_bounded_inter_agent_completion_message(
                task_name, sender, message,
            ));
        }
        AgentStatus::CompletedWithSurface {
            last_agent_message: None,
            ..
        } => String::new(),
        AgentStatus::Errored(error) => {
            let error = truncate_text(error, TruncationPolicy::Tokens(ERROR_MAX_TOKENS));
            format!("Agent errored: {error}\n\n{ERROR_NEXT_ACTION}")
        }
        AgentStatus::Shutdown => "Agent shut down.".to_string(),
        AgentStatus::NotFound => "Agent was not found.".to_string(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    Some(InterAgentCompletionMessage::new(task_name, sender, payload).render())
}

fn format_bounded_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    payload: &str,
) -> String {
    let unabridged =
        InterAgentCompletionMessage::new(task_name.clone(), sender.clone(), payload.to_string())
            .render();
    if approx_token_count(&unabridged) < COMPLETION_MESSAGE_MAX_TOKENS {
        return unabridged;
    }

    let mut payload_budget = ERROR_MAX_TOKENS;
    loop {
        let payload = truncate_text(payload, TruncationPolicy::Tokens(payload_budget));
        let payload = format!("{payload}\n\n{TYPED_COMPLETION_NEXT_ACTION}");
        let message =
            InterAgentCompletionMessage::new(task_name.clone(), sender.clone(), payload).render();
        let message_tokens = approx_token_count(&message);
        if message_tokens < COMPLETION_MESSAGE_MAX_TOKENS || payload_budget == 0 {
            return message;
        }

        // Rendering adds the typed envelope and can expand escaped control characters. Tighten
        // the source budget until the complete model-visible notification fits the ceiling.
        let next_budget = payload_budget
            .saturating_mul(COMPLETION_MESSAGE_MAX_TOKENS.saturating_sub(1))
            / message_tokens;
        payload_budget = next_budget.min(payload_budget.saturating_sub(1));
    }
}

#[cfg(test)]
#[path = "session_prefix_tests.rs"]
mod tests;

pub(crate) fn format_subagent_context_line(
    agent_reference: &str,
    agent_nickname: Option<&str>,
) -> String {
    match agent_nickname.filter(|nickname| !nickname.is_empty()) {
        Some(agent_nickname) => format!("- {agent_reference}: {agent_nickname}"),
        None => format!("- {agent_reference}"),
    }
}
