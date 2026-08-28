use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;

const AGENT_COMMUNICATION_TARGET: &str = "codex_otel.agent_communication";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentCommunicationKind {
    Spawn,
    Message,
    Followup,
    Result,
}

impl AgentCommunicationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Message => "message",
            Self::Followup => "followup",
            Self::Result => "result",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentCommunicationContext {
    kind: AgentCommunicationKind,
    sender_thread_id: ThreadId,
}

impl AgentCommunicationContext {
    pub(crate) fn new(kind: AgentCommunicationKind, sender_thread_id: ThreadId) -> Self {
        Self {
            kind,
            sender_thread_id,
        }
    }
}

pub(crate) fn logging_enabled() -> bool {
    tracing::enabled!(target: AGENT_COMMUNICATION_TARGET, tracing::Level::INFO)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AgentCommunicationLogMetadata {
    content_type: &'static str,
    content_bytes: usize,
}

pub(crate) fn agent_communication_log_metadata(
    communication: &InterAgentCommunication,
) -> AgentCommunicationLogMetadata {
    if communication.content.is_empty() {
        AgentCommunicationLogMetadata {
            content_type: "encrypted",
            content_bytes: communication
                .encrypted_content
                .as_deref()
                .unwrap_or_default()
                .len(),
        }
    } else {
        AgentCommunicationLogMetadata {
            content_type: "plain",
            content_bytes: communication.content.len(),
        }
    }
}

pub(crate) fn emit_agent_communication_send(
    communication_id: &str,
    context: &AgentCommunicationContext,
    metadata: AgentCommunicationLogMetadata,
    receiver_thread_id: ThreadId,
) {
    tracing::info!(
        target: AGENT_COMMUNICATION_TARGET,
        {
            event.name = "codex.agent_communication",
            communication_id,
            kind = context.kind.as_str(),
            state = "send",
            sender_thread_id = %context.sender_thread_id,
            receiver_thread_id = %receiver_thread_id,
            content_type = metadata.content_type,
            content_bytes = metadata.content_bytes,
        },
        "agent communication"
    );
}

pub(crate) fn emit_agent_communication_receive(communication_id: &str) {
    tracing::info!(
        target: AGENT_COMMUNICATION_TARGET,
        {
            event.name = "codex.agent_communication",
            communication_id,
            state = "receive",
        },
        "agent communication"
    );
}

#[cfg(test)]
mod tests {
    use codex_protocol::AgentPath;

    use super::*;

    #[test]
    fn logging_contract_agent_communication_metadata_omits_content() {
        let plain = InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            Vec::new(),
            "plain secret".to_string(),
            false,
        );
        let encrypted = InterAgentCommunication::new_encrypted(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            Vec::new(),
            "encrypted secret".to_string(),
            false,
        );

        assert_eq!(
            agent_communication_log_metadata(&plain),
            AgentCommunicationLogMetadata {
                content_type: "plain",
                content_bytes: 12,
            }
        );
        assert_eq!(
            agent_communication_log_metadata(&encrypted),
            AgentCommunicationLogMetadata {
                content_type: "encrypted",
                content_bytes: 16,
            }
        );
    }
}
