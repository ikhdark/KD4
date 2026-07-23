use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PersonalitySpecInstructions {
    body: String,
}

impl PersonalitySpecInstructions {
    pub(crate) fn new(spec: impl Into<String>) -> Self {
        let spec = spec.into();
        Self {
            body: format!(
                " The user has requested a new communication style. Future messages should adhere to the following personality: \n{spec} "
            ),
        }
    }

    pub(crate) fn reset() -> Self {
        Self {
            body: "The previously requested personality no longer applies. No personality-specific communication style is currently active."
                .to_string(),
        }
    }
}

impl ContextualUserFragment for PersonalitySpecInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<personality_spec>", "</personality_spec>")
    }

    fn body(&self) -> String {
        self.body.clone()
    }
}
