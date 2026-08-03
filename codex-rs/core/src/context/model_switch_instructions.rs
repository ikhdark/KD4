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

    fn body(&self) -> String {
        "\nThe user was previously using a different model. Continue following the session's existing base instructions. Adapt only to the capabilities and tools available in the current request.\n"
            .to_string()
    }
}
