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
    pub(crate) const REMOVAL_NOTICE: &str =
        "The previously provided AGENTS.md instructions no longer apply.";

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
            instructions,
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
                // Preserve the persisted format used by existing rollout baselines.
                text: Some(format!(
                    "{}\n\n{}",
                    self.freshness.model_visible_description(),
                    instructions.text
                )),
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

    fn required() -> bool {
        true
    }

    fn retained_state_supported(
        previous: &Self::Snapshot,
        items: &[codex_protocol::models::ResponseItem],
    ) -> bool {
        // Freshness-only notices do not replace the last substantive instruction body.
        let latest = super::retained_texts(items, "user").rev().find(|text| {
            UserInstructions::matches_text(text) && !text.contains("<INSTRUCTIONS>\nThe previously provided instruction body is unchanged.\n</INSTRUCTIONS>")
        });
        let Some(text) = latest else {
            return previous.text.is_none();
        };
        let expected_body = instruction_body(previous).unwrap_or(Self::REMOVAL_NOTICE);
        let instructions = UserInstructions {
            directory: previous.directory.clone(),
            text: expected_body.to_string(),
        };
        let rendered = instructions.render();
        // Unframed renders cannot be matched against retained text; resend them.
        let Some((header, body)) = rendered.split_once("<INSTRUCTIONS>\n") else {
            return false;
        };
        let header = header.trim_end();
        text.starts_with(&format!("{header}\n\n"))
            && text
                .split_once("<INSTRUCTIONS>\n")
                .is_some_and(|(_, actual)| {
                    actual == body
                        || actual.strip_prefix(&format!("{REPLACEMENT_NOTICE}\n\n")) == Some(body)
                })
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let current = self.snapshot();
        if matches!(previous, PreviousSectionState::Known(previous) if previous == &current) {
            return None;
        }
        if let PreviousSectionState::Known(previous) = previous
            && current.directory == previous.directory
            && current.freshness != previous.freshness
            && let Some(current_body) = instruction_body(&current)
            && instruction_body(previous) == Some(current_body)
        {
            let instructions = UserInstructions {
                directory: current.directory,
                text: "The previously provided instruction body is unchanged.".to_string(),
            };
            return Some(Box::new(
                instructions.with_observation(current.freshness.model_visible_description()),
            ));
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
                text: Self::REMOVAL_NOTICE.to_string(),
            },
            (None, false) => return None,
        };
        Some(Box::new(instructions.with_observation(
            self.freshness.model_visible_description(),
        )))
    }
}

fn instruction_body(snapshot: &AgentsMdSnapshot) -> Option<&str> {
    snapshot
        .text
        .as_deref()?
        .strip_prefix(snapshot.freshness.model_visible_description())?
        .strip_prefix("\n\n")
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
