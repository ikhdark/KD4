use super::ContextualUserFragment;

pub(crate) struct WorkflowCheckpoint;

impl ContextualUserFragment for WorkflowCheckpoint {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<workflow_checkpoint>", "</workflow_checkpoint>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(
            concat!(
                include_str!("../../templates/workflow_checkpoint.md"),
                "\n",
                include_str!("../../templates/verification_closure_checkpoint.md"),
            )
            .trim(),
        )
    }
}
