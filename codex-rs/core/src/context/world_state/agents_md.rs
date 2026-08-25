use super::PreviousSectionState;
use super::WorldStateSection;
use crate::agents_md::AgentsMdFreshness;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::RepositoryStableContextBundle;
use crate::context::ContextualUserFragment;
use crate::context::UserInstructions;
use serde::Deserialize;
use serde::Serialize;

const REPLACEMENT_NOTICE: &str =
    "These AGENTS.md instructions replace all previously provided AGENTS.md instructions.";
const REMOVAL_NOTICE: &str = "The previously provided AGENTS.md instructions no longer apply.";

/// The AGENTS.md instructions currently visible to the model.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentsMdState {
    instructions: Option<UserInstructions>,
    freshness: AgentsMdFreshness,
}

/// Persisted model-visible AGENTS.md state and the freshness of its filesystem observation.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AgentsMdSnapshot {
    directory: Option<String>,
    text: Option<String>,
    #[serde(default)]
    freshness: AgentsMdFreshness,
}

impl AgentsMdState {
    #[cfg(test)]
    pub(crate) fn new(loaded: Option<&LoadedAgentsMd>) -> Self {
        Self::from_instructions(
            loaded.map(LoadedAgentsMd::contextual_user_fragment),
            AgentsMdFreshness::Refreshed,
        )
    }

    pub(crate) fn new_cached(
        loaded: Option<&LoadedAgentsMd>,
        stable_context: Option<&RepositoryStableContextBundle>,
        freshness: AgentsMdFreshness,
    ) -> Self {
        Self::from_instructions(
            loaded.map(|loaded| {
                let rendered = stable_context
                    .map(|bundle| bundle.rendered.to_string())
                    .unwrap_or_else(|| loaded.text());
                loaded.contextual_user_fragment_with_text(rendered)
            }),
            freshness,
        )
    }

    fn from_instructions(
        instructions: Option<UserInstructions>,
        freshness: AgentsMdFreshness,
    ) -> Self {
        Self {
            instructions: instructions.map(|mut instructions| {
                instructions.text = format!(
                    "{}\n\n{}",
                    freshness.model_visible_description(),
                    instructions.text
                );
                instructions
            }),
            freshness,
        }
    }
}

impl WorldStateSection for AgentsMdState {
    const ID: &'static str = "agents_md";
    type Snapshot = AgentsMdSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        match &self.instructions {
            Some(instructions) => AgentsMdSnapshot {
                directory: instructions.directory.clone(),
                text: Some(instructions.text.clone()),
                freshness: self.freshness,
            },
            None => AgentsMdSnapshot {
                freshness: self.freshness,
                ..AgentsMdSnapshot::default()
            },
        }
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && UserInstructions::matches_text(text)
    }

    fn truncate_when_oversized() -> bool {
        true
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let current = self.snapshot();
        if matches!(previous, PreviousSectionState::Known(previous) if previous == &current) {
            return None;
        }

        let previous_may_contain_instructions = match previous {
            PreviousSectionState::Known(previous) => previous.text.is_some(),
            PreviousSectionState::Unknown => true,
            PreviousSectionState::Absent => false,
        };
        let instructions = match (&self.instructions, previous_may_contain_instructions) {
            (Some(instructions), true) => UserInstructions {
                directory: instructions.directory.clone(),
                text: format!("{REPLACEMENT_NOTICE}\n\n{}", instructions.text),
            },
            (Some(instructions), false) => instructions.clone(),
            (None, true) => UserInstructions {
                directory: None,
                text: format!(
                    "{}\n\n{REMOVAL_NOTICE}",
                    self.freshness.model_visible_description()
                ),
            },
            (None, false) => return None,
        };
        Some(Box::new(instructions))
    }
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
