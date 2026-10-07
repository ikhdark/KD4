// All this file should be replaced by the existing fragment implementation ofc

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromptSlot {
    DeveloperPolicy,
    DeveloperCapabilities,
    ContextualUser,
    SeparateDeveloper,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptFragment {
    slot: PromptSlot,
    text: String,
    unavailable: bool,
}

impl PromptFragment {
    /// Creates a prompt fragment for the given slot.
    pub fn new(slot: PromptSlot, text: impl Into<String>) -> Self {
        Self {
            slot,
            text: text.into(),
            unavailable: false,
        }
    }

    /// Creates a developer-policy prompt fragment.
    pub fn developer_policy(text: impl Into<String>) -> Self {
        Self::new(PromptSlot::DeveloperPolicy, text)
    }

    /// Creates a developer-capabilities prompt fragment.
    pub fn developer_capability(text: impl Into<String>) -> Self {
        Self::new(PromptSlot::DeveloperCapabilities, text)
    }

    /// Creates a separate top-level developer prompt fragment.
    pub fn separate_developer(text: impl Into<String>) -> Self {
        Self::new(PromptSlot::SeparateDeveloper, text)
    }

    /// Returns the target prompt slot.
    pub fn slot(&self) -> PromptSlot {
        self.slot
    }

    /// Returns the model-visible text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The producer could not obtain an authoritative snapshot. This is not an
    /// empty successful contribution and must not revoke its previous values.
    pub fn unavailable() -> Self {
        Self { slot: PromptSlot::SeparateDeveloper, text: String::new(), unavailable: true }
    }

    pub fn is_unavailable(&self) -> bool { self.unavailable }
}
