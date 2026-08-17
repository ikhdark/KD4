// All this file should be replaced by the existing fragment implementation ofc

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromptSlot {
    DeveloperPolicy,
    DeveloperCapabilities,
    ContextualUser,
    SeparateDeveloper,
}

/// Stable provenance for extension-owned prompt fragments.
///
/// The default remains `OtherInjected` so existing extensions keep their
/// current behavior. Built-in extensions should select a more specific kind
/// when the host exposes dedicated context accounting for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PromptFragmentKind {
    #[default]
    OtherInjected,
    Memory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptFragment {
    slot: PromptSlot,
    text: String,
    kind: PromptFragmentKind,
}

impl PromptFragment {
    /// Creates a prompt fragment for the given slot.
    pub fn new(slot: PromptSlot, text: impl Into<String>) -> Self {
        Self {
            slot,
            text: text.into(),
            kind: PromptFragmentKind::OtherInjected,
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

    /// Assigns stable measurement provenance to this fragment.
    pub fn with_kind(mut self, kind: PromptFragmentKind) -> Self {
        self.kind = kind;
        self
    }

    /// Returns the target prompt slot.
    pub fn slot(&self) -> PromptSlot {
        self.slot
    }

    /// Returns the model-visible text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the measurement provenance selected by the extension.
    pub fn kind(&self) -> PromptFragmentKind {
        self.kind
    }
}
