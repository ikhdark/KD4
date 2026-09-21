use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::AvailablePluginsInstructions;
use crate::context::ContextualUserFragment;
use crate::context::PluginsInstructionsUnavailable;

/// Whether generic plugin usage guidance should be visible to the model.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PluginsInstructionsState {
    available: bool,
}

impl PluginsInstructionsState {
    pub(crate) fn new(available: bool) -> Self {
        Self { available }
    }
}

impl WorldStateSection for PluginsInstructionsState {
    const ID: &'static str = "plugins_instructions";
    type Snapshot = bool;

    fn snapshot(&self) -> Self::Snapshot {
        self.available
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && AvailablePluginsInstructions::matches_text(text)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn required() -> bool {
        true
    }

    fn retained_state_supported(
        previous: &Self::Snapshot,
        items: &[codex_protocol::models::ResponseItem],
    ) -> bool {
        let latest = super::retained_texts(items, "developer")
            .rev()
            .find(|text| Self::matches_legacy_fragment("developer", text));
        let expected = if *previous {
            AvailablePluginsInstructions.render()
        } else {
            PluginsInstructionsUnavailable.render()
        };
        latest.map_or(!*previous, |text| text.contains(&expected))
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if self.available {
            match previous {
                PreviousSectionState::Absent | PreviousSectionState::Unknown => {
                    Some(Box::new(AvailablePluginsInstructions))
                }
                PreviousSectionState::Known(previous) if !*previous => {
                    Some(Box::new(AvailablePluginsInstructions))
                }
                PreviousSectionState::Known(_) => None,
            }
        } else {
            match previous {
                PreviousSectionState::Known(previous) if *previous => {
                    Some(Box::new(PluginsInstructionsUnavailable))
                }
                PreviousSectionState::Unknown => Some(Box::new(PluginsInstructionsUnavailable)),
                PreviousSectionState::Absent | PreviousSectionState::Known(_) => None,
            }
        }
    }
}

#[cfg(test)]
#[path = "plugins_instructions_tests.rs"]
mod tests;
