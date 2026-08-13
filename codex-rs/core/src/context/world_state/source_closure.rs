use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::tools::handlers::source_closure::SourceClosureSummary;

#[derive(Clone, Debug, Default)]
pub(crate) struct SourceClosureWorldState {
    summary: SourceClosureSummary,
}

impl SourceClosureWorldState {
    pub(crate) fn new(summary: SourceClosureSummary) -> Self {
        Self { summary }
    }
}

impl WorldStateSection for SourceClosureWorldState {
    const ID: &'static str = "source_closure";
    type Snapshot = SourceClosureSummary;

    fn snapshot(&self) -> Self::Snapshot {
        self.summary.clone()
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        role == "user"
            && text.starts_with("<source_closure>")
            && text.ends_with("</source_closure>")
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if matches!(previous, PreviousSectionState::Known(previous) if previous == &self.summary) {
            return None;
        }
        let empty = self.summary.authoritative_owner.is_none()
            && self.summary.relevant_targets.is_empty()
            && self.summary.unresolved_questions.is_empty();
        if empty && matches!(previous, PreviousSectionState::Absent) {
            return None;
        }
        Some(Box::new(SourceClosureFragment {
            summary: self.summary.clone(),
            replacement: !matches!(previous, PreviousSectionState::Absent),
        }))
    }
}

struct SourceClosureFragment {
    summary: SourceClosureSummary,
    replacement: bool,
}

impl ContextualUserFragment for SourceClosureFragment {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<source_closure>", "</source_closure>")
    }

    fn body(&self) -> String {
        let mut body = String::new();
        if self.replacement {
            body.push_str("This replaces the previous source-closure summary.\n");
        }
        body.push_str(
            &serde_json::to_string(&self.summary)
                .unwrap_or_else(|_| "{\"discovery\":\"gathering\"}".to_string()),
        );
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_summary_contains_no_private_cache_fields() {
        let state = SourceClosureWorldState::new(SourceClosureSummary::default());
        let fragment = WorldStateSection::render_diff(&state, PreviousSectionState::Unknown)
            .expect("replacement fragment");
        let text = fragment.body();
        assert!(!text.contains("artifact_id"));
        assert!(!text.contains("metadata"));
        assert!(!text.contains("watcher"));
    }
}
