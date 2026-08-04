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

    fn body(&self) -> String {
        self.canonical_payload.clone()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (TASK_CAPSULE_OPEN_TAG, TASK_CAPSULE_CLOSE_TAG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_one_contextual_user_fragment_with_canonical_payload_unchanged() {
        let payload = r#"{"schema_version":1,"objective":"inspect"}"#.to_string();

        assert_eq!(
            TaskCapsuleFragment::new(payload.clone()).render(),
            format!("{TASK_CAPSULE_OPEN_TAG}\n{payload}\n{TASK_CAPSULE_CLOSE_TAG}")
        );
    }
}
