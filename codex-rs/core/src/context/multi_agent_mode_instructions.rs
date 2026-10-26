use super::ContextualUserFragment;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::MULTI_AGENT_MODE_CLOSE_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;

const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work. Requests for thoroughness or investigation alone do not authorize spawning.";


#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EffectiveMultiAgentMode {
    Custom(String),
    ExplicitRequestOnly,
}

impl EffectiveMultiAgentMode {
    pub(crate) fn to_persisted_mode(&self) -> MultiAgentMode {
        match self {
            Self::Custom(hint_text) => MultiAgentMode::Custom(hint_text.clone()),
            Self::ExplicitRequestOnly => MultiAgentMode::ExplicitRequestOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiAgentModeInstructions {
    multi_agent_mode: EffectiveMultiAgentMode,
}

impl MultiAgentModeInstructions {
    pub(crate) fn new(multi_agent_mode: EffectiveMultiAgentMode) -> Self {
        Self { multi_agent_mode }
    }
}

impl ContextualUserFragment for MultiAgentModeInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (MULTI_AGENT_MODE_OPEN_TAG, MULTI_AGENT_MODE_CLOSE_TAG)
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(match &self.multi_agent_mode {
            EffectiveMultiAgentMode::Custom(hint_text) => hint_text,
            EffectiveMultiAgentMode::ExplicitRequestOnly => {
                EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT
            }
        })
    }
}
