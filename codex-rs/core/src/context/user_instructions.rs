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
        std::borrow::Cow::Owned(self.body_with_observation(None))
    }
}

impl UserInstructions {
    pub(crate) const OPEN_MARKER: &str = "# AGENTS.md instructions";
    pub(crate) const CLOSE_MARKER: &str = "</INSTRUCTIONS>";

    pub(crate) fn with_observation(
        &self,
        observation: &str,
    ) -> codex_context_fragments::RenderedContextFragment {
        let (start, end) = Self::type_markers();
        codex_context_fragments::RenderedContextFragment::new(
            self.role(),
            format!(
                "{start}{}{end}",
                self.body_with_observation(Some(observation))
            ),
        )
    }

    fn body_with_observation(&self, observation: Option<&str>) -> String {
        let directory = self
            .directory
            .as_ref()
            .map(|directory| format!(" for {}", escape_instruction_delimiters(directory)))
            .unwrap_or_default();
        let observation = observation
            .map(|text| {
                format!(
                    "<AGENTS_MD_OBSERVATION>\n{}\n</AGENTS_MD_OBSERVATION>\n\n",
                    escape_instruction_delimiters(text)
                )
            })
            .unwrap_or_default();
        format!(
            "{directory}\n\n{observation}<INSTRUCTIONS>\n{}\n",
            escape_instruction_delimiters(&self.text)
        )
    }
}

fn escape_instruction_delimiters(text: &str) -> String {
    escape_fragment_delimiters(
        text,
        &[
            "<INSTRUCTIONS>",
            UserInstructions::CLOSE_MARKER,
            "<AGENTS_MD_OBSERVATION>",
            "</AGENTS_MD_OBSERVATION>",
        ],
    )
}
