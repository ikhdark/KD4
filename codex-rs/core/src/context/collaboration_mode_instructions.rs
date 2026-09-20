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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Settings;
    use pretty_assertions::assert_eq;

    #[test]
    fn constructor_omits_blank_instructions_and_preserves_nonblank_body() {
        for (instructions, expected) in [
            (None, None),
            (Some(""), None),
            (Some(" \t\r\n"), None),
            (Some("\u{2003}\u{a0}"), None),
            (
                Some("\n  Keep this formatting.\t\n"),
                Some("\n  Keep this formatting.\t\n"),
            ),
        ] {
            let mode = CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: "test-model".to_string(),
                    reasoning_effort: None,
                    developer_instructions: instructions.map(str::to_string),
                },
            };
            let fragment = CollaborationModeInstructions::from_collaboration_mode(&mode);
            assert_eq!(
                fragment
                    .as_ref()
                    .map(ContextualUserFragment::body)
                    .as_deref(),
                expected
            );
            if let Some(fragment) = fragment {
                assert_eq!(fragment.role(), "developer");
                assert_eq!(
                    fragment.render(),
                    "<collaboration_mode>\n  Keep this formatting.\t\n</collaboration_mode>"
                );
            }
        }
    }
}
