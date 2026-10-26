use super::ContextualUserFragment;

pub(crate) const TASK_CAPSULE_OPEN_TAG: &str = "<task_capsule_v1>";
pub(crate) const TASK_CAPSULE_CLOSE_TAG: &str = "</task_capsule_v1>";

/// The sole model-visible bootstrap for a typed TaskCapsule child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskCapsuleFragment {
    canonical_payload: String,
}

impl TaskCapsuleFragment {
    pub(crate) fn new(canonical_payload: String) -> Self {
        Self { canonical_payload }
    }
}

impl ContextualUserFragment for TaskCapsuleFragment {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.canonical_payload)
    }

    fn type_markers() -> (&'static str, &'static str) {
        (TASK_CAPSULE_OPEN_TAG, TASK_CAPSULE_CLOSE_TAG)
    }
}

