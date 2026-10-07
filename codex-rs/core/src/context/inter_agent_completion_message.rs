use codex_protocol::AgentPath;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterAgentCompletionMessage {
    task_name: AgentPath,
    sender: AgentPath,
    payload: String,
    receipt: Option<String>,
}

impl InterAgentCompletionMessage {
    pub(crate) fn new(task_name: AgentPath, sender: AgentPath, payload: impl Into<String>) -> Self {
        Self {
            task_name,
            sender,
            payload: payload.into(),
            receipt: None,
        }
    }

    pub(crate) fn with_receipt(mut self, receipt: Option<&str>) -> Self {
        self.receipt = receipt.map(str::to_string);
        self
    }
}

impl ContextualUserFragment for InterAgentCompletionMessage {
    fn role(&self) -> &'static str {
        "assistant"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!(
            "Message Type: FINAL_ANSWER\nTask name: {}\nSender: {}\n{}Payload:\n{}",
            self.task_name, self.sender,
            self.receipt.as_ref().map(|receipt| format!("{receipt}\n")).unwrap_or_default(),
            self.payload,
        ))
    }
}
