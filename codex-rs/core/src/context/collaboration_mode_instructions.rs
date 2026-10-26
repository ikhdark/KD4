use super::ContextualUserFragment;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::protocol::COLLABORATION_MODE_CLOSE_TAG;
use codex_protocol::protocol::COLLABORATION_MODE_OPEN_TAG;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CollaborationModeInstructions {
    instructions: String,
}

impl CollaborationModeInstructions {
    pub(crate) const RESET_INSTRUCTIONS: &str = "No collaboration-mode-specific instructions are currently active. Any previously provided collaboration-mode instructions no longer apply.";

    pub(crate) fn from_collaboration_mode(collaboration_mode: &CollaborationMode) -> Option<Self> {
        collaboration_mode
            .settings
            .developer_instructions
            .as_ref()
            .filter(|instructions| !instructions.trim().is_empty())
            .map(|instructions| Self {
                instructions: instructions.clone(),
            })
    }

    pub(crate) fn reset() -> Self {
        Self {
            instructions: Self::RESET_INSTRUCTIONS.to_string(),
        }
    }
}

impl ContextualUserFragment for CollaborationModeInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (COLLABORATION_MODE_OPEN_TAG, COLLABORATION_MODE_CLOSE_TAG)
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.instructions)
    }
}

