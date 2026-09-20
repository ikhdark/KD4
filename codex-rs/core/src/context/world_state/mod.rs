mod agents_md;
mod apps_instructions;
mod environment;
mod plugins_instructions;

use crate::context::ContextualUserFragment;
use codex_context_fragments::ModelContextBudget;
use codex_context_fragments::RenderedContextFragment;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fmt;

pub(crate) use agents_md::AgentsMdState;
pub(crate) use apps_instructions::AppsInstructionsState;
pub(crate) use environment::EnvironmentsState;
pub(crate) use plugins_instructions::PluginsInstructionsState;

trait ErasedWorldStateSection: Send + Sync {
    fn snapshot(&self) -> Option<Value>;

    fn matches_legacy_fragment(&self, role: &str, text: &str) -> bool;

    fn has_retained_fragment_matcher(&self) -> bool;

    fn matches_retained_fragment(&self, role: &str, text: &str) -> bool;

    fn truncate_when_oversized(&self) -> bool;

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Value>,
    ) -> Option<Box<dyn ContextualUserFragment>>;
}

impl<S: WorldStateSection> ErasedWorldStateSection for S {
    fn snapshot(&self) -> Option<Value> {
        let mut snapshot = match serde_json::to_value(WorldStateSection::snapshot(self)) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::error!(
                    section_id = S::ID,
                    %err,
                    "failed to serialize world-state section snapshot"
                );
                return None;
            }
        };
        remove_null_object_fields(&mut snapshot);
        if snapshot.is_null() {
            tracing::error!(
                section_id = S::ID,
                "world-state section snapshot cannot be null"
            );
            return None;
        }
        Some(snapshot)
    }

    fn matches_legacy_fragment(&self, role: &str, text: &str) -> bool {
        S::matches_legacy_fragment(role, text)
    }

    fn has_retained_fragment_matcher(&self) -> bool {
        S::has_retained_fragment_matcher()
    }

    fn matches_retained_fragment(&self, role: &str, text: &str) -> bool {
        S::matches_retained_fragment(role, text)
    }

    fn truncate_when_oversized(&self) -> bool {
        S::truncate_when_oversized()
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Value>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let typed_snapshot;
        let previous = match previous {
            PreviousSectionState::Known(previous) => match S::Snapshot::deserialize(previous) {
                Ok(previous) => {
                    typed_snapshot = previous;
                    PreviousSectionState::Known(&typed_snapshot)
                }
                Err(err) => {
                    tracing::warn!(
                        section_id = S::ID,
                        %err,
                        "failed to restore world-state section snapshot"
                    );
                    PreviousSectionState::Unknown
                }
            },
            PreviousSectionState::Absent => PreviousSectionState::Absent,
            PreviousSectionState::Unknown => PreviousSectionState::Unknown,
        };
        WorldStateSection::render_diff(self, previous)
    }
}

struct ExtensionWorldStateSection(WorldStateSectionContribution);

impl ErasedWorldStateSection for ExtensionWorldStateSection {
    fn snapshot(&self) -> Option<Value> {
        let mut snapshot = self.0.snapshot().clone();
        remove_null_object_fields(&mut snapshot);
        (!snapshot.is_null()).then_some(snapshot)
    }

    fn matches_legacy_fragment(&self, role: &str, text: &str) -> bool {
        self.0.matches_legacy_fragment(role, text)
    }

    fn has_retained_fragment_matcher(&self) -> bool {
        self.0.has_retained_fragment_matcher()
    }

    fn matches_retained_fragment(&self, role: &str, text: &str) -> bool {
        self.0.matches_retained_fragment(role, text)
    }

    fn truncate_when_oversized(&self) -> bool {
        false
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Value>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let previous = match previous {
            PreviousSectionState::Absent => PreviousWorldStateSection::Absent,
            PreviousSectionState::Unknown => PreviousWorldStateSection::Unknown,
            PreviousSectionState::Known(previous) => PreviousWorldStateSection::Known(previous),
        };
        self.0
            .render_diff(previous)
            .map(|fragment| Box::new(WorldStateContextFragment(fragment)) as _)
    }
}

/// A last accepted extension snapshot retained while its current observation is unavailable.
/// Preserved sections never render; a successful contributor result restores its render and
/// retained-fragment policies on the next step.
struct PreservedWorldStateSection(Value);

impl ErasedWorldStateSection for PreservedWorldStateSection {
    fn snapshot(&self) -> Option<Value> {
        Some(self.0.clone())
    }

    fn matches_legacy_fragment(&self, _role: &str, _text: &str) -> bool {
        false
    }

    fn has_retained_fragment_matcher(&self) -> bool {
        false
    }

    fn matches_retained_fragment(&self, _role: &str, _text: &str) -> bool {
        false
    }

    fn truncate_when_oversized(&self) -> bool {
        false
    }

    fn render_diff(
        &self,
        _previous: PreviousSectionState<'_, Value>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        None
    }
}

struct WorldStateContextFragment(RenderedWorldStateFragment);

impl ContextualUserFragment for WorldStateContextFragment {
    fn role(&self) -> &'static str {
        self.0.role()
    }

    fn markers(&self) -> (&'static str, &'static str) {
        self.0.markers()
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(self.0.body())
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }
}

/// What is known about a section's previously model-visible state.
pub(crate) enum PreviousSectionState<'a, T> {
    /// No persisted snapshot or matching fragment exists in retained history.
    Absent,
    /// Retained history contains the section, but its typed snapshot is unavailable.
    Unknown,
    /// The exact persisted snapshot is available.
    Known(&'a T),
}

/// A typed portion of the state visible to the model.
///
/// Implementations own how their current state is rendered relative to an
/// earlier snapshot of the same section. `ID` is persisted in rollouts and
/// must remain stable. `Snapshot` should contain only the comparison data
/// needed to decide what the model must be told next, and must not serialize
/// to null because merge-patch nulls represent deletion. Sections migrated
/// from older context can recognize their previous fragments through
/// `matches_legacy_fragment`.
pub(crate) trait WorldStateSection: Send + Sync + 'static {
    const ID: &'static str;
    type Snapshot: DeserializeOwned + Serialize;

    fn snapshot(&self) -> Self::Snapshot;

    fn matches_legacy_fragment(_role: &str, _text: &str) -> bool {
        false
    }

    /// Whether retained history must still contain this section's rendered fragment.
    fn has_retained_fragment_matcher() -> bool {
        false
    }

    /// Recognizes this section's rendered fragment in retained model history.
    fn matches_retained_fragment(_role: &str, _text: &str) -> bool {
        false
    }

    /// Whether this section is mandatory enough to admit a bounded rendering instead of
    /// silently dropping the whole structured fragment when it exceeds the shared budget.
    fn truncate_when_oversized() -> bool {
        false
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>>;
}

/// Live model-visible state, keyed by the same stable section IDs used in rollouts.
#[derive(Default)]
pub(crate) struct WorldState {
    sections: IndexMap<&'static str, Box<dyn ErasedWorldStateSection>>,
}

/// Compact comparison state for each model-visible world-state section.
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct WorldStateSnapshot {
    sections: BTreeMap<String, Value>,
}

/// A bounded delivery is not a typed, fully accepted section snapshot. Keep its
/// source identity separate from the text actually sent, including across resume.
#[derive(Serialize, serde::Deserialize)]
struct PartialDelivery<'a> {
    source_digest: &'a str,
    role: &'a str,
    rendered: &'a str,
}

fn partial_delivery(snapshot: &Value) -> Option<PartialDelivery<'_>> {
    PartialDelivery::deserialize(snapshot.get("partial_delivery")?).ok()
}

impl WorldStateSnapshot {
    pub(crate) fn section(&self, id: &str) -> Option<&Value> {
        self.sections.get(id)
    }

    pub(crate) fn into_value(self) -> Value {
        Value::Object(self.sections.into_iter().collect())
    }

    /// Returns the RFC 7386 merge patch that advances `previous` to `self`.
    pub(crate) fn merge_patch_from(&self, previous: &Self) -> Option<Value> {
        let mut patch = Map::new();
        for key in previous.sections.keys() {
            if !self.sections.contains_key(key) {
                patch.insert(key.clone(), Value::Null);
            }
        }
        for (key, current) in &self.sections {
            let change = match previous.sections.get(key) {
                Some(previous) => create_merge_patch(previous, current),
                None => Some(current.clone()),
            };
            if let Some(change) = change {
                patch.insert(key.clone(), change);
            }
        }
        (!patch.is_empty()).then_some(Value::Object(patch))
    }

    pub(crate) fn apply_merge_patch(&mut self, patch: &Value) -> serde_json::Result<()> {
        let Value::Object(patch) = patch else {
            // Preserve deserialization errors and leave the snapshot untouched for
            // invalid top-level replacements, as the previous round trip did.
            *self = serde_json::from_value(patch.clone())?;
            return Ok(());
        };
        for (key, value) in patch {
            if value.is_null() {
                self.sections.remove(key);
            } else {
                apply_merge_patch_value(
                    self.sections.entry(key.clone()).or_insert(Value::Null),
                    value,
                );
            }
        }
        Ok(())
    }
}

impl fmt::Debug for WorldState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorldState")
            .field("section_count", &self.sections.len())
            .finish()
    }
}

impl WorldState {
    pub(crate) fn add_section<S: WorldStateSection>(&mut self, section: S) {
        let id = S::ID;
        assert!(
            !self.sections.contains_key(id),
            "duplicate world-state section ID: {id}"
        );
        self.sections.insert(id, Box::new(section));
    }

    pub(crate) fn add_extension_section(&mut self, section: WorldStateSectionContribution) {
        let id = section.id();
        assert!(
            !self.sections.contains_key(id),
            "duplicate world-state section ID: {id}"
        );
        self.sections
            .insert(id, Box::new(ExtensionWorldStateSection(section)));
    }

    pub(crate) fn add_preserved_extension_section(&mut self, id: &'static str, snapshot: Value) {
        assert!(
            !self.sections.contains_key(id),
            "duplicate world-state section ID: {id}"
        );
        self.sections
            .insert(id, Box::new(PreservedWorldStateSection(snapshot)));
    }

    pub(crate) fn snapshot(&self) -> WorldStateSnapshot {
        WorldStateSnapshot {
            sections: self
                .sections
                .iter()
                .filter_map(|(id, section)| {
                    section
                        .snapshot()
                        .map(|snapshot| ((*id).to_string(), snapshot))
                })
                .collect(),
        }
    }

    /// Renders every section as new, without any known previous state.
    #[cfg(test)]
    pub(crate) fn render_full(&self) -> Vec<Box<dyn ContextualUserFragment>> {
        self.render_full_with_snapshot().0
    }

    pub(crate) fn render_full_with_snapshot(
        &self,
    ) -> (Vec<Box<dyn ContextualUserFragment>>, WorldStateSnapshot) {
        self.render_with(|_, _| PreviousSectionState::Absent)
    }

    /// Renders each section against the exact persisted snapshot when available.
    #[cfg(test)]
    pub(crate) fn render_diff(
        &self,
        previous: &WorldStateSnapshot,
    ) -> Vec<Box<dyn ContextualUserFragment>> {
        self.render_diff_with_snapshot(previous).0
    }

    #[cfg(test)]
    pub(crate) fn render_diff_with_snapshot(
        &self,
        previous: &WorldStateSnapshot,
    ) -> (Vec<Box<dyn ContextualUserFragment>>, WorldStateSnapshot) {
        self.render_with(|id, _| match previous.sections.get(id) {
            Some(previous) => PreviousSectionState::Known(previous),
            None => PreviousSectionState::Absent,
        })
    }

    /// Falls back to retained model history when no exact persisted snapshot is available.
    #[cfg(test)]
    pub(crate) fn render_history_diff(
        &self,
        previous: Option<&WorldStateSnapshot>,
        items: &[ResponseItem],
    ) -> Vec<Box<dyn ContextualUserFragment>> {
        self.render_history_diff_with_snapshot(previous, items).0
    }

    pub(crate) fn render_history_diff_with_snapshot(
        &self,
        previous: Option<&WorldStateSnapshot>,
        items: &[ResponseItem],
    ) -> (Vec<Box<dyn ContextualUserFragment>>, WorldStateSnapshot) {
        self.render_with(|id, section| {
            if let Some(previous) = previous.and_then(|previous| previous.sections.get(id)) {
                if let Some(partial) = partial_delivery(previous)
                    && section.truncate_when_oversized()
                {
                    if has_delivered_text(items, partial.role, partial.rendered) {
                        PreviousSectionState::Known(previous)
                    } else {
                        PreviousSectionState::Unknown
                    }
                } else if section.has_retained_fragment_matcher()
                    && !has_retained_fragment(items, section)
                {
                    PreviousSectionState::Absent
                } else {
                    PreviousSectionState::Known(previous)
                }
            } else if has_legacy_fragment(items, section) {
                PreviousSectionState::Unknown
            } else {
                PreviousSectionState::Absent
            }
        })
    }

    fn render_with<'a>(
        &self,
        mut previous: impl FnMut(&str, &dyn ErasedWorldStateSection) -> PreviousSectionState<'a, Value>,
    ) -> (Vec<Box<dyn ContextualUserFragment>>, WorldStateSnapshot) {
        let mut budget = ModelContextBudget::default();
        let mut fragments = Vec::new();
        let mut sections = BTreeMap::new();
        for (id, section) in &self.sections {
            let _render_span =
                tracing::trace_span!("world_state.render_section", section_id = *id).entered();
            let previous = previous(id, section.as_ref());
            let rejected_snapshot = match &previous {
                PreviousSectionState::Known(previous) => Some(*previous),
                PreviousSectionState::Absent | PreviousSectionState::Unknown => None,
            };
            let partial = rejected_snapshot
                .filter(|_| section.truncate_when_oversized())
                .and_then(partial_delivery);
            let fragment = section.render_diff(if partial.is_some() {
                PreviousSectionState::Unknown
            } else {
                previous
            });
            let snapshot_advanced = match fragment {
                Some(fragment) if !matches!(fragment.role(), "developer" | "user") => {
                    tracing::warn!(
                        section_id = *id,
                        role = fragment.role(),
                        "world-state section used an unsupported model-context role"
                    );
                    false
                }
                Some(fragment) => {
                    let rendered = fragment.render();
                    if !budget.try_take(&rendered) {
                        if section.truncate_when_oversized() {
                            // Always bound an authoritative replacement, so the same source
                            // produces the same bounded text on initial and subsequent passes.
                            if let Some(replacement) =
                                section.render_diff(PreviousSectionState::Unknown)
                                && matches!(replacement.role(), "developer" | "user")
                                && let Some(source) = section.snapshot()
                            {
                                let source_digest =
                                    format!("{:x}", Sha256::digest(source.to_string().as_bytes()));
                                let mut candidate_budget = budget.clone();
                                let replacement_text = replacement.render();
                                if let Some(rendered) = candidate_budget.take(&replacement_text) {
                                    let role = replacement.role();
                                    if partial.as_ref().is_some_and(|previous| {
                                        previous.source_digest == source_digest
                                            && previous.role == role
                                            && previous.rendered.len() >= rendered.len()
                                    }) && let Some(rejected_snapshot) = rejected_snapshot
                                    {
                                        sections
                                            .insert((*id).to_string(), rejected_snapshot.clone());
                                        continue;
                                    }
                                    let snapshot = if rendered == replacement_text {
                                        source
                                    } else {
                                        tracing::warn!(
                                            section_id = *id,
                                            "mandatory world-state section exceeded its context budget; admitted a bounded rendering"
                                        );
                                        serde_json::json!({"partial_delivery": PartialDelivery {
                                            source_digest: &source_digest,
                                            role,
                                            rendered: &rendered,
                                        }})
                                    };
                                    budget = candidate_budget;
                                    fragments.push(Box::new(RenderedContextFragment::new(
                                        role,
                                        rendered.into_owned(),
                                    ))
                                        as Box<dyn ContextualUserFragment>);
                                    sections.insert((*id).to_string(), snapshot);
                                    continue;
                                }
                            }
                        }
                        false
                    } else {
                        let role = fragment.role();
                        fragments.push(Box::new(RenderedContextFragment::new(role, rendered))
                            as Box<dyn ContextualUserFragment>);
                        true
                    }
                }
                None => true,
            };

            let snapshot = if snapshot_advanced {
                section.snapshot()
            } else {
                rejected_snapshot.cloned()
            };
            if let Some(snapshot) = snapshot {
                sections.insert((*id).to_string(), snapshot);
            }
        }

        (fragments, WorldStateSnapshot { sections })
    }
}

fn has_delivered_text(items: &[ResponseItem], delivered_role: &str, rendered: &str) -> bool {
    items.iter().any(|item| {
        matches!(item, ResponseItem::Message { role, content, .. }
        if role == delivered_role && content.iter().any(|content| {
            matches!(content, ContentItem::InputText { text } if text.contains(rendered))
        }))
    })
}

fn has_retained_fragment(items: &[ResponseItem], section: &dyn ErasedWorldStateSection) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if content.iter().any(|content| {
                    matches!(
                        content,
                        ContentItem::InputText { text }
                            if section.matches_retained_fragment(role, text)
                    )
                })
        )
    })
}

fn has_legacy_fragment(items: &[ResponseItem], section: &dyn ErasedWorldStateSection) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if content.iter().any(|content| {
                    matches!(
                        content,
                        ContentItem::InputText { text }
                            if section.matches_legacy_fragment(role, text)
                    )
                })
        )
    })
}

fn remove_null_object_fields(value: &mut Value) {
    // RFC 7386 reserves object-valued nulls for deletion, but arrays are replaced whole.
    match value {
        Value::Object(values) => {
            values.retain(|_, value| !value.is_null());
            values.values_mut().for_each(remove_null_object_fields);
        }
        Value::Array(_) => {}
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn create_merge_patch(previous: &Value, current: &Value) -> Option<Value> {
    let Value::Object(current) = current else {
        return (previous != current).then(|| current.clone());
    };
    let previous = previous.as_object();
    let mut patch = Map::new();

    if let Some(previous) = previous {
        for key in previous.keys() {
            if !current.contains_key(key) {
                patch.insert(key.clone(), Value::Null);
            }
        }
    }

    for (key, current_value) in current {
        let Some(previous_value) = previous.and_then(|previous| previous.get(key)) else {
            patch.insert(key.clone(), current_value.clone());
            continue;
        };
        if let Some(value_patch) = create_merge_patch(previous_value, current_value) {
            patch.insert(key.clone(), value_patch);
        }
    }

    // An empty object still replaces a previous scalar or array.
    (previous.is_none() || !patch.is_empty()).then_some(Value::Object(patch))
}

fn apply_merge_patch_value(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        target.clone_from(patch);
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(target) = target {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                apply_merge_patch_value(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }
}

#[cfg(test)]
#[path = "world_state_tests.rs"]
mod tests;
