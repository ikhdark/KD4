use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnAborted {
    pub(crate) guidance: String,
}

impl TurnAborted {
    pub(crate) const INTERRUPTED_GUIDANCE: &'static str = "The user interrupted the previous turn on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) const INTERRUPTED_DEVELOPER_GUIDANCE: &'static str = "The previous turn was interrupted on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) const UNFINISHED_GUIDANCE: &'static str = "The previous turn ended before it completed: the session stopped without recording a response or completion for that request, and no tool calls or file changes from it are recorded here. Do not assume any part of that request was carried out. Inspect the workspace and live state before relying on or repeating work, then continue with the user's latest direction.";

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

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!("\n{}\n", self.guidance))
    }
}
