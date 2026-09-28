use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::environment_context::push_xml_escaped_text;

/// Agent activity changes independently of filesystem and environment facts.
pub(crate) struct SubagentsState(String);

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SubagentsSnapshot {
    text: String,
}

impl SubagentsState {
    pub(crate) fn new(text: String) -> Self {
        let mut budget = codex_context_fragments::ModelContextBudget::new(1024);
        let mut lines = Vec::new();
        for line in text.lines() {
            if !budget.try_take(line) {
                lines.push("Additional subagents omitted; use list_agents for current details.");
                break;
            }
            lines.push(line);
        }
        Self(lines.join("\n"))
    }
}

impl WorldStateSection for SubagentsState {
    const ID: &'static str = "subagents";
    type Snapshot = SubagentsSnapshot;

    fn snapshot(&self) -> SubagentsSnapshot {
        SubagentsSnapshot {
            text: self.0.clone(),
        }
    }

    fn records_delivery() -> bool {
        true
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && Self::matches_text(text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, SubagentsSnapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        match previous {
            PreviousSectionState::Known(previous) if previous.text == self.0 => None,
            PreviousSectionState::Absent if self.0.is_empty() => None,
            _ => Some(Box::new(Self(self.0.clone()))),
        }
    }
}

impl ContextualUserFragment for SubagentsState {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<subagents_context>", "</subagents_context>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        let text = if self.0.is_empty() { "none" } else { &self.0 };
        let mut rendered = "\nCurrent subagents (replaces previous subagent lists):\n".to_string();
        push_xml_escaped_text(&mut rendered, text);
        rendered.push('\n');
        std::borrow::Cow::Owned(rendered)
    }
}
