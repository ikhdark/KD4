use super::ContextualUserFragment;

pub(crate) const COMPLETION_CHECKPOINT_OPEN_TAG: &str = "<kd4_completion_checkpoint_v1>";
pub(crate) const COMPLETION_CHECKPOINT_CLOSE_TAG: &str = "</kd4_completion_checkpoint_v1>";

/// Model-visible rendering of the completion checkpoint persisted by task evidence.
///
/// This is a normal contextual fragment. It deliberately owns neither history nor
/// persistence; finalization projects it through the existing `ContextManager`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionCheckpointContext {
    canonical_payload: String,
}

impl CompletionCheckpointContext {
    pub(crate) fn new(canonical_payload: impl Into<String>) -> Self {
        Self {
            canonical_payload: canonical_payload.into(),
        }
    }
}

impl ContextualUserFragment for CompletionCheckpointContext {
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
        (
            COMPLETION_CHECKPOINT_OPEN_TAG,
            COMPLETION_CHECKPOINT_CLOSE_TAG,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_canonical_payload_without_rewriting_it() {
        let payload = r#"{"schema_version":1,"candidate_id":"candidate"}"#;
        assert_eq!(
            CompletionCheckpointContext::new(payload).render(),
            format!("{COMPLETION_CHECKPOINT_OPEN_TAG}{payload}{COMPLETION_CHECKPOINT_CLOSE_TAG}")
        );
    }
}
