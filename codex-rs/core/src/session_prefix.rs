use codex_context_fragments::ContextualUserFragment;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use codex_utils_output_truncation::truncate_text;
use sha2::Digest;
use sha2::Sha256;

use crate::context::InterAgentCompletionMessage;
use crate::context::SubagentNotification;

const COMPLETION_MESSAGE_MAX_TOKENS: usize = 4_000;
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

pub(crate) fn inter_agent_completion_communication_with_diagnostics(
    task_name: AgentPath,
    sender: AgentPath,
    sender_thread_id: ThreadId,
    status: &AgentStatus,
    receipt: Option<&str>,
    protected_host_summary: Option<&str>,
) -> Option<InterAgentCommunication> {
    let message = format_inter_agent_completion_message_with_diagnostics(
        task_name.clone(),
        sender.clone(),
        status,
        receipt,
        protected_host_summary,
    )?;
    let mut communication =
        InterAgentCommunication::new(sender, task_name, Vec::new(), message, false);
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
            "agent-error-{}",
            hex::encode(digest.finalize()),
        )));
    }
    Some(communication)
}

fn format_inter_agent_completion_message_with_diagnostics(
    task_name: AgentPath,
    sender: AgentPath,
    status: &AgentStatus,
    receipt: Option<&str>,
    protected_host_summary: Option<&str>,
) -> Option<String> {
    let payload = match status {
        AgentStatus::Stopped {
            last_agent_message,
            stop_cause,
        } => {
            let cause = serde_json::to_string(stop_cause).unwrap_or_default();
            return Some(format_bounded_inter_agent_completion_message(
                task_name,
                sender,
                last_agent_message
                    .as_deref()
                    .and_then(|message| {
                        match without_protected_prefix(message, protected_host_summary) {
                            // Host-only text is not an authored report.
                            "" if !message.is_empty() => None,
                            report => Some(report),
                        }
                    })
                    .unwrap_or("No authored report was delivered."),
                TYPED_COMPLETION_NEXT_ACTION,
                receipt,
                Some(&format!("Host execution stop cause: {cause}.")),
            ));
        }
        AgentStatus::Completed(Some(message)) => {
            return Some(format_bounded_inter_agent_completion_message(
                task_name,
                sender,
                without_protected_prefix(message, protected_host_summary),
                TYPED_COMPLETION_NEXT_ACTION,
                receipt,
                None,
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
                without_protected_prefix(message, protected_host_summary),
                TYPED_COMPLETION_NEXT_ACTION,
                receipt,
                None,
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
                &format!(
                    "Agent errored: {}",
                    without_protected_prefix(error, protected_host_summary)
                ),
                ERROR_NEXT_ACTION,
                receipt,
                None,
            ));
        }
        AgentStatus::Shutdown => "Agent shut down.".to_string(),
        AgentStatus::NotFound => "Agent was not found.".to_string(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    Some(
        InterAgentCompletionMessage::new(task_name, sender, payload)
            .with_receipt(receipt)
            .render(),
    )
}

/// Older status projections may carry a host completion summary. Remove only
/// its exact leading copy; completion notifications deliver the authored report.
fn without_protected_prefix<'a>(text: &'a str, protected_host_summary: Option<&str>) -> &'a str {
    match protected_host_summary
        .filter(|summary| !summary.is_empty())
        .and_then(|summary| text.strip_prefix(summary))
    {
        Some("") => "",
        Some(rest) => rest.strip_prefix("\n\n").unwrap_or(text),
        None => text,
    }
}

fn format_bounded_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    payload: &str,
    retrieval_guidance: &str,
    receipt: Option<&str>,
    protected_host_summary: Option<&str>,
) -> String {
    // Only host-owned input gets protected space. Matching authored text in an
    // omitted middle section cannot suppress or replace these facts.
    let mut protected = protected_host_summary.unwrap_or_default().to_string();
    let payload = payload
        .strip_prefix(protected.as_str())
        .filter(|_| !protected.is_empty())
        .map(|payload| payload.strip_prefix("\n\n").unwrap_or(payload))
        .unwrap_or(payload);
    let render = |payload: &str, protected: &str| {
        let body = if protected.is_empty() {
            payload.to_string()
        } else {
            format!("{protected}\n\n{payload}")
        };
        InterAgentCompletionMessage::new(task_name.clone(), sender.clone(), body)
            .with_receipt(receipt)
            .render()
    };
    // The envelope only adds bytes. If its body already cannot fit, do not
    // allocate the complete authored report merely to reject that rendering.
    let body_bytes = payload.len().saturating_add(protected.len());
    if approx_tokens_from_byte_count(body_bytes) < COMPLETION_MESSAGE_MAX_TOKENS as u64 {
        let unabridged = render(payload, &protected);
        if approx_token_count(&unabridged) < COMPLETION_MESSAGE_MAX_TOKENS {
            return unabridged;
        }
    }

    let retrieval_guidance = if receipt.is_some() {
        "The full sealed receipt remains available at the exact receipt locator above; no automatic retrieval was performed."
    } else {
        retrieval_guidance
    };
    if approx_token_count(&render(retrieval_guidance, &protected)) >= COMPLETION_MESSAGE_MAX_TOKENS
    {
        // Preserve an explicit material-omission notice and exact recovery
        // identity rather than silently dropping the tail of host diagnostics.
        protected = format!(
            "{}\nMaterial host diagnostics omitted (completion checks, verification gaps, advisories and/or delivery recovery); retrieve the exact receipt for the complete facts.",
            truncate_text(&protected, TruncationPolicy::Tokens(512))
        );
    }
    let mut payload_budget = ERROR_MAX_TOKENS.saturating_sub(approx_token_count(&protected));
    loop {
        let payload = truncate_text(payload, TruncationPolicy::Tokens(payload_budget));
        let payload = format!("{payload}\n\n{retrieval_guidance}");
        let message = render(&payload, &protected);
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

pub(crate) fn format_subagent_context_line(
    agent_reference: &str,
    agent_nickname: Option<&str>,
) -> String {
    match agent_nickname.filter(|nickname| !nickname.is_empty()) {
        Some(agent_nickname) => format!("- {agent_reference}: {agent_nickname}"),
        None => format!("- {agent_reference}"),
    }
}
