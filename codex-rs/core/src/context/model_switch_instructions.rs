use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelSwitchInstructions;

impl ModelSwitchInstructions {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl ContextualUserFragment for ModelSwitchInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<model_switch>", "</model_switch>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(
            "\nThe user was previously using a different model. Use the active instructions and tool declarations in this request. Preserve the task state and applicable user constraints.\n",
        )
    }
}
