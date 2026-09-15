use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnAborted {
    pub(crate) guidance: String,
}

impl TurnAborted {
    pub(crate) const INTERRUPTED_GUIDANCE: &'static str = "The user interrupted the previous turn on purpose. Any tools, commands, or nested code-mode work may have partially executed or may still be running. Before continuing, inspect the affected state and any live sessions; do not assume pre-interruption evidence is still current or repeat operations whose effects are uncertain.";
    pub(crate) const INTERRUPTED_DEVELOPER_GUIDANCE: &'static str = "The previous turn was interrupted on purpose. Any tools, commands, or nested code-mode work may have partially executed or may still be running. Before continuing, inspect the affected state and any live sessions; do not assume pre-interruption evidence is still current or repeat operations whose effects are uncertain.";

    pub(crate) fn new(guidance: impl Into<String>) -> Self {
        Self {
            guidance: guidance.into(),
        }
    }
}

impl ContextualUserFragment for TurnAborted {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<turn_aborted>", "</turn_aborted>")
    }

    fn body(&self) -> String {
        format!("\n{}\n", self.guidance)
    }
}
