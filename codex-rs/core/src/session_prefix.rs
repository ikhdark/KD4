use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use sha2::Digest;
use sha2::Sha256;
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

/// Runtime error completions are idempotent within a producer attempt. Reuse
/// mailbox admission's bounded, resume-seeded ID deduplication before history
/// insertion. Ordinary messages and successful answers remain distinct.
pub(crate) fn inter_agent_completion_communication(
    task_name: AgentPath,
    sender: AgentPath,
    sender_thread_id: ThreadId,
    status: &AgentStatus,
    receipt: Option<&str>,
) -> Option<InterAgentCommunication> {
    let message = format_inter_agent_completion_message(
        task_name.clone(), sender.clone(), status, receipt,
    )?;
    let mut communication = InterAgentCommunication::new(
        sender, task_name, Vec::new(), message, false,
    );
    if let AgentStatus::Errored(error) = status {
        let mut digest = Sha256::new();
        // Hash the full error, not its bounded display. Length framing avoids
        // ambiguous concatenations; the receipt distinguishes explicit retries.
        for part in [
            sender_thread_id.to_string().as_str(),
            communication.author.as_str(),
            communication.recipient.as_str(),
            receipt.unwrap_or_default(),
            error.as_str(),
        ] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part.as_bytes());
        }
        communication.id = Some(codex_protocol::ResponseItemId::from_server(format!(
            "agent-error-{:x}", digest.finalize(),
        )));
    }
    Some(communication)
}

pub(crate) fn format_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    status: &AgentStatus,
    receipt: Option<&str>,
) -> Option<String> {
    let payload = match status {
        AgentStatus::Completed(Some(message)) => {
            return Some(format_bounded_inter_agent_completion_message(
                task_name,
                sender,
                message,
                TYPED_COMPLETION_NEXT_ACTION,
                receipt,
            ));
        }
        AgentStatus::Completed(None) => String::new(),
        AgentStatus::CompletedWithSurface {
            last_agent_message: Some(message),
            ..
        } => {
            return Some(format_bounded_inter_agent_completion_message(
                task_name,
                sender,
                message,
                TYPED_COMPLETION_NEXT_ACTION,
                receipt,
            ));
        }
        AgentStatus::CompletedWithSurface {
            last_agent_message: None,
            ..
        } => String::new(),
        AgentStatus::Errored(error) => {
            return Some(format_bounded_inter_agent_completion_message(
                task_name,
                sender,
                &format!("Agent errored: {error}"),
                ERROR_NEXT_ACTION,
                receipt,
            ));
        }
        AgentStatus::Shutdown => "Agent shut down.".to_string(),
        AgentStatus::NotFound => "Agent was not found.".to_string(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    Some(InterAgentCompletionMessage::new(task_name, sender, payload).with_receipt(receipt).render())
}

fn format_bounded_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    payload: &str,
    retrieval_guidance: &str,
    receipt: Option<&str>,
) -> String {
    let unabridged =
        InterAgentCompletionMessage::new(task_name.clone(), sender.clone(), payload.to_string())
            .with_receipt(receipt)
            .render();
    if approx_token_count(&unabridged) < COMPLETION_MESSAGE_MAX_TOKENS {
        return unabridged;
    }

    let mut payload_budget = ERROR_MAX_TOKENS;
    loop {
        let payload = truncate_text(payload, TruncationPolicy::Tokens(payload_budget));
        let retrieval_guidance = if receipt.is_some() {
            "The full sealed receipt remains available at the exact receipt locator above; no automatic retrieval was performed."
        } else { retrieval_guidance };
        let payload = format!("{payload}\n\n{retrieval_guidance}");
        let message =
            InterAgentCompletionMessage::new(task_name.clone(), sender.clone(), payload).with_receipt(receipt).render();
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
