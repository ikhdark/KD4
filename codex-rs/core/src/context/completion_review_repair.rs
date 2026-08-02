use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReviewRepair {
    payload: String,
}

impl CompletionReviewRepair {
    pub(crate) fn new(payload: impl Into<String>) -> Self {
        Self {
            payload: payload.into(),
        }
    }
}

impl ContextualUserFragment for CompletionReviewRepair {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<kd4_completion_repair>", "</kd4_completion_repair>")
    }

    fn body(&self) -> String {
        format!(
            "\nPerform the single KD4 completion repair requested below. Make only the smallest in-scope corrections. Keep the accepted plan and scope unchanged. Run the focused proof when possible. Before finishing, record every unresolved item in plan or risk evidence.\n\n{}\n",
            self.payload
        )
    }
}
