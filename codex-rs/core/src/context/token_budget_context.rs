use super::ContextualUserFragment;
use super::world_state::PreviousSectionState;
use super::world_state::WorldStateSection;
use codex_protocol::AgentPath;
const CONTEXT_WINDOW_CLOSE_TAG: &str = "</context_window>";
const CONTEXT_WINDOW_GUIDANCE_CLOSE_TAG: &str = "</context_window_guidance>";
const CONTEXT_WINDOW_GUIDANCE_OPEN_TAG: &str = "<context_window_guidance>";
const CONTEXT_WINDOW_OPEN_TAG: &str = "<context_window>";
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenBudgetContext {
    agent_path: AgentPath,
    first_window_id: Uuid,
    previous_window_id: Option<Uuid>,
    window_id: Uuid,
    thread_hint: Option<String>,
}

impl TokenBudgetContext {
    pub(crate) fn new(
        agent_path: AgentPath,
        first_window_id: Uuid,
        previous_window_id: Option<Uuid>,
        window_id: Uuid,
        thread_hint: Option<String>,
    ) -> Self {
        Self {
            agent_path,
            first_window_id,
            previous_window_id,
            window_id,
            thread_hint,
        }
    }
}

impl ContextualUserFragment for TokenBudgetContext {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (CONTEXT_WINDOW_OPEN_TAG, CONTEXT_WINDOW_CLOSE_TAG)
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        let first_window_id = self.first_window_id;
        let window_id = self.window_id;
        let mut lines = vec![
            format!("Agent name: {}", self.agent_path),
            format!("First context window id: {first_window_id}"),
            format!("Current context window id: {window_id}"),
        ];
        if let Some(previous_window_id) = self.previous_window_id {
            lines.push(format!("Previous context window id: {previous_window_id}"));
        }
        if let Some(thread_hint) = &self.thread_hint {
            lines.push(thread_hint.clone());
        }
        format!("\n{}\n", lines.join("\n")).into()
    }
}

impl WorldStateSection for TokenBudgetContext {
    const ID: &'static str = "context_window";
    type Snapshot = AgentPath;

    fn snapshot(&self) -> Self::Snapshot {
        self.agent_path.clone()
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        (!matches!(previous, PreviousSectionState::Known(agent_path) if agent_path == &self.agent_path))
            .then(|| Box::new(self.clone()) as Box<dyn ContextualUserFragment>)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextWindowGuidance {
    message: String,
}

impl ContextWindowGuidance {
    pub(crate) fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl ContextualUserFragment for ContextWindowGuidance {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            CONTEXT_WINDOW_GUIDANCE_OPEN_TAG,
            CONTEXT_WINDOW_GUIDANCE_CLOSE_TAG,
        )
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        format!("\n{}\n", self.message).into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenBudgetRemainingContext {
    tokens_left: Option<i64>,
}

impl TokenBudgetRemainingContext {
    pub(crate) fn new(tokens_left: i64) -> Self {
        Self {
            tokens_left: Some(tokens_left),
        }
    }

    pub(crate) fn unknown() -> Self {
        Self { tokens_left: None }
    }
}

impl ContextualUserFragment for TokenBudgetRemainingContext {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        match self.tokens_left {
            Some(tokens_left) => {
                format!("You have {tokens_left} tokens left in this context window.").into()
            }
            None => "You have unknown tokens left in this context window.".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenBudgetReminder {
    message: String,
}

impl TokenBudgetReminder {
    pub(crate) fn new(message_template: &str, n_remaining: i64) -> Self {
        Self {
            message: message_template.replace("{n_remaining}", &n_remaining.to_string()),
        }
    }
}

impl ContextualUserFragment for TokenBudgetReminder {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        self.message.as_str().into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoCompactFallbackPrompt {
    message: String,
}

impl AutoCompactFallbackPrompt {
    pub(crate) fn new(message: &str) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl ContextualUserFragment for AutoCompactFallbackPrompt {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        self.message.as_str().into()
    }
}
