use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use serde::Deserialize;
use serde::Serialize;

pub(crate) const TASK_EVIDENCE_STATE_OPEN_TAG: &str = "<kd4_task_state_v1>";
pub(crate) const TASK_EVIDENCE_STATE_CLOSE_TAG: &str = "</kd4_task_state_v1>";
const COMMAND_MUTATION_FOLLOW_UP: &str = "Unresolved command-mutation warnings require immediate follow-up before unrelated work or finalization: inspect a scoped repository status/diff, attribute every changed path and required hash, then reconcile the ledger signal. Do not merely acknowledge or repeatedly carry the warning.";
const TASK_EVIDENCE_STATE_CLEARED: &str =
    "The previously provided KD4 task state no longer applies.";

#[derive(Clone, Debug, Default)]
pub(crate) struct TaskEvidenceState {
    summary: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TaskEvidenceSnapshot {
    summary: Option<String>,
}

impl TaskEvidenceState {
    pub(crate) fn new(summary: Option<String>) -> Self {
        Self { summary }
    }
}

impl WorldStateSection for TaskEvidenceState {
    const ID: &'static str = "kd4_task_state";
    type Snapshot = TaskEvidenceSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        TaskEvidenceSnapshot {
            summary: self.summary.clone(),
        }
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && TaskEvidenceContext::matches_text(text)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if matches!(previous, PreviousSectionState::Known(previous) if previous.summary == self.summary)
        {
            return None;
        }

        let summary = match (&self.summary, previous) {
            (Some(summary), _) => summary.clone(),
            (None, PreviousSectionState::Known(previous)) if previous.summary.is_some() => {
                TASK_EVIDENCE_STATE_CLEARED.to_string()
            }
            (None, PreviousSectionState::Unknown) => TASK_EVIDENCE_STATE_CLEARED.to_string(),
            (None, PreviousSectionState::Absent | PreviousSectionState::Known(_)) => return None,
        };

        Some(Box::new(TaskEvidenceContext::new(summary)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskEvidenceContext {
    summary: String,
}

impl TaskEvidenceContext {
    fn new(summary: String) -> Self {
        Self { summary }
    }
}

impl ContextualUserFragment for TaskEvidenceContext {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn body(&self) -> String {
        if self.summary.contains("unknown-command-mutation-")
            || self.summary.contains("uninspected-command-mutation-")
        {
            format!("{COMMAND_MUTATION_FOLLOW_UP}\n\n{}", self.summary)
        } else {
            self.summary.clone()
        }
    }

    fn type_markers() -> (&'static str, &'static str) {
        (TASK_EVIDENCE_STATE_OPEN_TAG, TASK_EVIDENCE_STATE_CLOSE_TAG)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_is_marker_bounded_and_recognizable() {
        let rendered = TaskEvidenceContext::new("active state".to_string()).render();

        assert!(rendered.starts_with(TASK_EVIDENCE_STATE_OPEN_TAG));
        assert!(rendered.ends_with(TASK_EVIDENCE_STATE_CLOSE_TAG));
        assert!(TaskEvidenceContext::matches_text(&rendered));
    }

    #[test]
    fn command_mutation_warning_requires_scoped_attribution() {
        let rendered = TaskEvidenceContext::new(
            "## Warnings\n- risk unknown-command-mutation-4: workspace changed".to_string(),
        )
        .render();

        assert!(rendered.contains("immediate follow-up before unrelated work or finalization"));
        assert!(rendered.contains("inspect a scoped repository status/diff"));
        assert!(rendered.contains("attribute every changed path and required hash"));
        assert!(rendered.contains("Do not merely acknowledge"));
    }

    #[test]
    fn clearing_task_state_supersedes_previous_once() {
        let previous = TaskEvidenceState::new(Some("active state".to_string()));
        let cleared = TaskEvidenceState::new(None);

        let update = cleared
            .render_diff(PreviousSectionState::Known(&previous.snapshot()))
            .expect("clearing a previously visible task state must emit a supersession");
        let rendered = update.render();

        assert!(rendered.starts_with(TASK_EVIDENCE_STATE_OPEN_TAG));
        assert!(rendered.ends_with(TASK_EVIDENCE_STATE_CLOSE_TAG));
        assert!(rendered.contains(TASK_EVIDENCE_STATE_CLEARED));
        assert!(
            cleared
                .render_diff(PreviousSectionState::Known(&cleared.snapshot()))
                .is_none(),
            "the cleared snapshot must not repeat the supersession"
        );
    }
}
