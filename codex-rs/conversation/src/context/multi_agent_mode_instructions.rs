use super::ContextualUserFragment;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::MULTI_AGENT_MODE_CLOSE_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;

const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = concat!(
    "Default to root-only execution for each new user task. Do not delegate work, including ",
    "spawning sub-agents or assigning work to existing agents through followup_task, send_message ",
    "or other tools, unless the user or applicable AGENTS.md/skill instructions explicitly ",
    "authorize delegation for that task. Task-scoped authorization expires when the overall user ",
    "task is completed. Worker completion alone neither revokes that authorization nor authorizes ",
    "new work: the root may resume the worker for corrections within its original assignment while ",
    "the overall task remains active. Prior delegation and available workers do not authorize ",
    "delegating a subsequent task. Status updates, interruptions and continuations of an unfinished ",
    "task preserve its applicable authorization. Requests for thoroughness or investigation alone ",
    "do not authorize delegation."
);


#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveMultiAgentMode {
    Custom(String),
    ExplicitRequestOnly,
}

impl EffectiveMultiAgentMode {
    pub fn to_persisted_mode(&self) -> MultiAgentMode {
        match self {
            Self::Custom(hint_text) => MultiAgentMode::Custom(hint_text.clone()),
            Self::ExplicitRequestOnly => MultiAgentMode::ExplicitRequestOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiAgentModeInstructions {
    multi_agent_mode: EffectiveMultiAgentMode,
}

impl MultiAgentModeInstructions {
    pub fn new(multi_agent_mode: EffectiveMultiAgentMode) -> Self {
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
