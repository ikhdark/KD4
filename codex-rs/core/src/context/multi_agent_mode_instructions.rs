use super::ContextualUserFragment;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::MULTI_AGENT_MODE_CLOSE_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;

const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
const ROOT_ORCHESTRATION_TEXT: &str = include_str!("../../templates/agents/root_orchestration.md");

#[cfg(test)]
mod tests {
    use super::EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT;
    use super::EffectiveMultiAgentMode;
    use super::MultiAgentModeInstructions;
    use crate::context::ContextualUserFragment;
    use codex_protocol::config_types::MultiAgentMode;

    #[test]
    fn root_orchestration_renders_only_the_bounded_runtime_policy() {
        let fragment = super::RootOrchestrationInstructions;
        let rendered = fragment.render();
        assert_eq!(fragment.role(), "developer");
        assert!(
            rendered.starts_with(
                "<root_orchestration_instructions>When several independent tool calls"
            )
        );
        assert!(rendered.contains("Do not run shared-state mutations concurrently."));
        assert!(
            rendered
                .contains("Continue yielded commands through their existing wait or session path")
        );
        assert!(
            rendered
                .ends_with("scope clippy to changed packages.</root_orchestration_instructions>")
        );
        assert!(!rendered.contains("<!--"));
        assert!(
            rendered.len() <= 1_200,
            "runtime policy grew to {} bytes",
            rendered.len()
        );
    }

    #[test]
    fn effective_modes_only_persist_and_render_current_policies() {
        let cases = [
            (
                EffectiveMultiAgentMode::Custom("custom policy".to_string()),
                MultiAgentMode::Custom("custom policy".to_string()),
                "custom policy",
            ),
            (
                EffectiveMultiAgentMode::ExplicitRequestOnly,
                MultiAgentMode::ExplicitRequestOnly,
                EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT,
            ),
        ];

        for (mode, persisted, rendered) in cases {
            assert_eq!(mode.to_persisted_mode(), persisted);
            assert_eq!(MultiAgentModeInstructions::new(mode).body(), rendered);
        }
    }
}

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
        ROOT_ORCHESTRATION_TEXT.trim().to_string()
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

    fn body(&self) -> String {
        match &self.multi_agent_mode {
            EffectiveMultiAgentMode::Custom(hint_text) => hint_text.clone(),
            EffectiveMultiAgentMode::ExplicitRequestOnly => {
                EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT.to_string()
            }
        }
    }
}
