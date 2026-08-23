use super::ContextualUserFragment;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::MULTI_AGENT_MODE_CLOSE_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;

const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
const PROACTIVE_MULTI_AGENT_MODE_TEXT: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.";
const ORCHESTRATOR_TEMPLATE: &str = include_str!("../../templates/agents/orchestrator.md");
const ROOT_ORCHESTRATION_OPEN: &str = "<!-- runtime-root-orchestration:start -->";
const ROOT_ORCHESTRATION_CLOSE: &str = "<!-- runtime-root-orchestration:end -->";

fn extract_root_orchestration_text(template: &str) -> Option<&str> {
    let (before_open, after_open) = template.split_once(ROOT_ORCHESTRATION_OPEN)?;
    let (body, after_close) = after_open.split_once(ROOT_ORCHESTRATION_CLOSE)?;
    if before_open.contains(ROOT_ORCHESTRATION_OPEN)
        || before_open.contains(ROOT_ORCHESTRATION_CLOSE)
        || body.contains(ROOT_ORCHESTRATION_OPEN)
        || body.contains(ROOT_ORCHESTRATION_CLOSE)
        || after_close.contains(ROOT_ORCHESTRATION_OPEN)
        || after_close.contains(ROOT_ORCHESTRATION_CLOSE)
    {
        return None;
    }
    let body = body.trim();
    (!body.is_empty()).then_some(body)
}

fn root_orchestration_text() -> &'static str {
    extract_root_orchestration_text(ORCHESTRATOR_TEMPLATE).unwrap_or_else(|| {
        tracing::error!(
            "orchestrator template must contain exactly one ordered runtime root section"
        );
        ""
    })
}

#[cfg(test)]
mod tests {
    use super::ROOT_ORCHESTRATION_CLOSE;
    use super::ROOT_ORCHESTRATION_OPEN;
    use super::extract_root_orchestration_text;
    use super::root_orchestration_text;

    #[test]
    fn extracts_exactly_one_ordered_root_orchestration_section() {
        let template = format!(
            "preamble\n{ROOT_ORCHESTRATION_OPEN}\n runtime policy \n{ROOT_ORCHESTRATION_CLOSE}\nappendix"
        );

        assert_eq!(
            extract_root_orchestration_text(&template),
            Some("runtime policy")
        );
        assert!(!root_orchestration_text().is_empty());
    }

    #[test]
    fn malformed_root_orchestration_markers_fail_closed() {
        let cases = [
            "no markers".to_string(),
            format!("{ROOT_ORCHESTRATION_OPEN} body"),
            format!("body {ROOT_ORCHESTRATION_CLOSE}"),
            format!("{ROOT_ORCHESTRATION_CLOSE} body {ROOT_ORCHESTRATION_OPEN}"),
            format!(
                "{ROOT_ORCHESTRATION_OPEN} first {ROOT_ORCHESTRATION_OPEN} second {ROOT_ORCHESTRATION_CLOSE}"
            ),
            format!(
                "{ROOT_ORCHESTRATION_OPEN} first {ROOT_ORCHESTRATION_CLOSE} second {ROOT_ORCHESTRATION_CLOSE}"
            ),
            format!("{ROOT_ORCHESTRATION_OPEN}   {ROOT_ORCHESTRATION_CLOSE}"),
        ];

        for template in cases {
            assert_eq!(
                extract_root_orchestration_text(&template),
                None,
                "{template}"
            );
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
