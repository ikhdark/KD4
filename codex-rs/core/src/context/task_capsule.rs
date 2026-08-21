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

    /// Returns the delegated objective carried by a rendered task capsule.
    ///
    /// Task capsules are submitted as structured user input. Consumers that
    /// interpret user intent must inspect the objective rather than the JSON
    /// envelope, whose quotes would otherwise make it look like quoted text.
    pub(crate) fn objective_from_rendered(rendered: &str) -> Option<String> {
        let payload = rendered
            .strip_prefix(TASK_CAPSULE_OPEN_TAG)?
            .strip_suffix(TASK_CAPSULE_CLOSE_TAG)?;
        serde_json::from_str::<serde_json::Value>(payload)
            .ok()?
            .get("objective")?
            .as_str()
            .map(str::to_owned)
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
            format!("{TASK_CAPSULE_OPEN_TAG}{payload}{TASK_CAPSULE_CLOSE_TAG}")
        );
    }

    #[test]
    fn extracts_objective_from_rendered_capsule() {
        let rendered = TaskCapsuleFragment::new(
            r#"{"schema_version":1,"objective":"spawn the second worker"}"#.to_string(),
        )
        .render();

        assert_eq!(
            TaskCapsuleFragment::objective_from_rendered(&rendered).as_deref(),
            Some("spawn the second worker")
        );
        assert_eq!(
            TaskCapsuleFragment::objective_from_rendered("spawn the second worker"),
            None
        );
    }
}
