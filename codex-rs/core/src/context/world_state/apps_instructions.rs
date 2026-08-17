use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::AppsInstructions;
use crate::context::AppsInstructionsUnavailable;
use crate::context::ContextualUserFragment;

/// Whether generic Apps usage guidance should be visible to the model.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AppsInstructionsState {
    available: bool,
}

impl AppsInstructionsState {
    pub(crate) fn new(available: bool) -> Self {
        Self { available }
    }
}

impl WorldStateSection for AppsInstructionsState {
    const ID: &'static str = "apps_instructions";
    type Snapshot = bool;

    fn snapshot(&self) -> Self::Snapshot {
        self.available
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && AppsInstructions::matches_text(text)
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
        if self.available {
            match previous {
                PreviousSectionState::Absent => Some(Box::new(AppsInstructions)),
                PreviousSectionState::Known(previous) if !*previous => {
                    Some(Box::new(AppsInstructions))
                }
                PreviousSectionState::Known(_) | PreviousSectionState::Unknown => None,
            }
        } else {
            match previous {
                PreviousSectionState::Known(previous) if *previous => {
                    Some(Box::new(AppsInstructionsUnavailable))
                }
                PreviousSectionState::Unknown => Some(Box::new(AppsInstructionsUnavailable)),
                PreviousSectionState::Absent | PreviousSectionState::Known(_) => None,
            }
        }
    }
}

#[cfg(test)]
#[path = "apps_instructions_tests.rs"]
mod tests;
