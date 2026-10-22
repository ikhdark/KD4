use super::ContextualUserFragment;
use super::escape_fragment_delimiters;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UserInstructions {
    pub(crate) directory: Option<String>,
    pub(crate) text: String,
}

impl ContextualUserFragment for UserInstructions {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (Self::OPEN_MARKER, Self::CLOSE_MARKER)
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        let directory = self
            .directory
            .as_ref()
            .map(|directory| format!(" for {}", escape_instruction_delimiters(directory)))
            .unwrap_or_default();
        std::borrow::Cow::Owned(format!(
            "{directory}\n\n<INSTRUCTIONS>\n{}\n",
            escape_instruction_delimiters(&self.text),
        ))
    }
}

impl UserInstructions {
    pub(crate) const OPEN_MARKER: &str = "# AGENTS.md instructions";
    pub(crate) const CLOSE_MARKER: &str = "</INSTRUCTIONS>";

    /// Exact body size after the same delimiter encoding used by `render`.
    pub(crate) fn escaped_text_len(text: &str) -> usize {
        INSTRUCTION_DELIMITERS.iter().fold(text.len(), |len, tag| {
            len.saturating_add(text.matches(tag).count().saturating_mul(6))
        })
    }
}

fn escape_instruction_delimiters(text: &str) -> String {
    escape_fragment_delimiters(text, INSTRUCTION_DELIMITERS)
}

const INSTRUCTION_DELIMITERS: &[&str] = &[
    "<INSTRUCTIONS>",
    UserInstructions::CLOSE_MARKER,
    "<AGENTS_MD_OBSERVATION>",
    "</AGENTS_MD_OBSERVATION>",
];
