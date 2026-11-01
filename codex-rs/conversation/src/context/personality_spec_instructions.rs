use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub struct PersonalitySpecInstructions {
    body: String,
}

impl PersonalitySpecInstructions {
    pub fn new(spec: impl Into<String>) -> Self {
        let spec = spec.into();
        Self {
            body: format!("Use the following communication style for future messages:\n{spec}"),
        }
    }

    pub fn reset() -> Self {
        Self {
            body: "The previous personality no longer applies. No personality-specific communication style is currently active."
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

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.body)
    }
}
