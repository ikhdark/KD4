use super::ContextualUserFragment;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::MULTI_AGENT_MODE_CLOSE_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;

const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
const PROACTIVE_MULTI_AGENT_MODE_TEXT: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.";
const ORCHESTRATOR_TEMPLATE: &str = include_str!("../../templates/agents/orchestrator.md");
const ROOT_ORCHESTRATION_OPEN: &str = "<!-- runtime-root-orchestration:start -->";
const ROOT_ORCHESTRATION_CLOSE: &str = "<!-- runtime-root-orchestration:end -->";

fn root_orchestration_text() -> &'static str {
    ORCHESTRATOR_TEMPLATE
        .split_once(ROOT_ORCHESTRATION_OPEN)
        .and_then(|(_, remainder)| remainder.split_once(ROOT_ORCHESTRATION_CLOSE))
        .map(|(body, _)| body.trim())
        .unwrap_or(ORCHESTRATOR_TEMPLATE.trim())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RootOrchestrationInstructions;

impl ContextualUserFragment for RootOrchestrationInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<root_orchestration_instructions>",
            "</root_orchestration_instructions>",
        )
    }

    fn body(&self) -> String {
        root_orchestration_text().to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiAgentModeInstructions {
    multi_agent_mode: MultiAgentMode,
}

impl MultiAgentModeInstructions {
    pub(crate) fn new(multi_agent_mode: MultiAgentMode) -> Self {
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

    fn body(&self) -> String {
        match &self.multi_agent_mode {
            MultiAgentMode::Custom(hint_text) => hint_text.clone(),
            MultiAgentMode::ExplicitRequestOnly => {
                EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT.to_string()
            }
            MultiAgentMode::Proactive => PROACTIVE_MULTI_AGENT_MODE_TEXT.to_string(),
        }
    }
}
