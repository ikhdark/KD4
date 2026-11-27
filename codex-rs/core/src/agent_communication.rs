use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;

const AGENT_COMMUNICATION_TARGET: &str = "codex_otel.agent_communication";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentCommunicationKind {
    Spawn,
    Message,
    SettledSourceAdvisory,
    Followup,
    Result,
}

impl AgentCommunicationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Message | Self::SettledSourceAdvisory => "message",
            Self::Followup => "followup",
            Self::Result => "result",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentCommunicationContext {
    kind: AgentCommunicationKind,
    sender_thread_id: ThreadId,
    sender_turn_id: Option<String>,
    sender_validation_scope: Option<(String, String)>,
}

impl AgentCommunicationContext {
    pub(crate) fn new(kind: AgentCommunicationKind, sender_thread_id: ThreadId) -> Self {
        Self {
            kind,
            sender_thread_id,
            sender_turn_id: None,
            sender_validation_scope: None,
        }
    }

    pub(crate) fn with_sender_turn(mut self, turn_id: String) -> Self {
        self.sender_turn_id = Some(turn_id);
        self
    }

    /// The root validation turn the sending turn acts for.
    pub(crate) fn with_sender_validation_scope(mut self, scope: (String, String)) -> Self {
        self.sender_validation_scope = Some(scope);
        self
    }

    /// Only a deliberate follow-up delegates its sender's validation scope.
    /// Terminal results and host nudges never move a receiver between scopes.
    pub(crate) fn followup_validation_scope(&self) -> Option<&(String, String)> {
        (self.kind == AgentCommunicationKind::Followup)
            .then_some(self.sender_validation_scope.as_ref())
            .flatten()
    }

    pub(crate) fn followup_turn_owner(&self) -> Option<(ThreadId, &str)> {
        (self.kind == AgentCommunicationKind::Followup)
            .then(|| self.sender_turn_id.as_deref().map(|turn| (self.sender_thread_id, turn)))
            .flatten()
    }

    pub(crate) fn tracks_last_task_message(&self) -> bool {
        matches!(
            self.kind,
            AgentCommunicationKind::Spawn
                | AgentCommunicationKind::Message
                | AgentCommunicationKind::Followup
        )
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
