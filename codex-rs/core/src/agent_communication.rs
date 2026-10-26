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

    pub(crate) fn is_terminal_result(&self) -> bool {
        self.kind == AgentCommunicationKind::Result
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
