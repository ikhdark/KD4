use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnAborted {
    pub(crate) guidance: String,
}

impl TurnAborted {
    pub(crate) const INTERRUPTED_GUIDANCE: &'static str = "The user interrupted the previous turn on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) const INTERRUPTED_DEVELOPER_GUIDANCE: &'static str = "The previous turn was interrupted on purpose. If tools, commands, or nested code-mode work were in flight, inspect only the affected state and live sessions needed to resolve uncertain effects before relying on them or repeating an operation. Reuse unaffected evidence and continue with the user's latest direction.";
    pub(crate) fn unfinished_guidance(tool_calls: usize, tool_results: usize) -> String {
        format!("Recovery detected a lost process, not a user interruption. The previous turn has no recorded completion. Its retained history contains {tool_calls} tool call(s) and {tool_results} tool result(s). Recorded results remain evidence; calls without results may have taken effect, and child commands may still be running. The exact loss time and duration are unknown; this notice records discovery on resume. Inspect only affected live sessions and workspace state before relying on uncertain effects or repeating work, then continue with the user's latest direction.")
    }

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
