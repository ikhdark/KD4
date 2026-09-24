use crate::stable_context::StableContextKind;
use crate::stable_context::StableContextManifest;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;
use serde::Deserialize;
use serde::Serialize;
use serde_json::value::RawValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

const CATEGORY_HASH_DOMAIN: &[u8] = b"codex.prompt-context-category.v1";
// These identities live only in the in-memory sidecar. One JSON message follows
// the domain, so no length prefix or serialized buffer is needed.
const RESPONSE_ITEM_FINGERPRINT_DOMAIN: &[u8] = b"codex.prompt-response-item.v2";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum PromptContextCategory {
    BaseSystem,
    ToolSchemas,
    Repository,
    AgentRole,
    Skills,
    SkillCatalog,
    Plugins,
    PluginCatalog,
    AppDesktop,
    Collaboration,
    EnvironmentPermissions,
    TaskInput,
    History,
    OtherInjected,
}

impl PromptContextCategory {
    pub(crate) const ALL: [Self; 14] = [
        Self::BaseSystem,
        Self::ToolSchemas,
        Self::Repository,
        Self::AgentRole,
        Self::Skills,
        Self::SkillCatalog,
        Self::Plugins,
        Self::PluginCatalog,
        Self::AppDesktop,
        Self::Collaboration,
        Self::EnvironmentPermissions,
        Self::TaskInput,
        Self::History,
        Self::OtherInjected,
    ];

    pub(crate) const FIXED_PREFIX: [Self; 12] = [
        Self::BaseSystem,
        Self::ToolSchemas,
        Self::Repository,
        Self::AgentRole,
        Self::Skills,
        Self::SkillCatalog,
        Self::Plugins,
        Self::PluginCatalog,
        Self::AppDesktop,
        Self::Collaboration,
        Self::EnvironmentPermissions,
        Self::OtherInjected,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::BaseSystem => "base_system",
            Self::ToolSchemas => "tool_schemas",
            Self::Repository => "repository",
            Self::AgentRole => "agent_role",
            Self::Skills => "skills",
            Self::SkillCatalog => "skill_catalog",
            Self::Plugins => "plugins",
            Self::PluginCatalog => "plugin_catalog",
            Self::AppDesktop => "app_desktop",
            Self::Collaboration => "collaboration",
            Self::EnvironmentPermissions => "environment_permissions",
            Self::TaskInput => "task_input",
            Self::History => "history",
            Self::OtherInjected => "other_injected",
        }
    }

    /// Context deliberately selected by the harness for the current request.
    /// This is an operational proxy, not a claim about model attention.
    /// Catalogs, generic extension text, and merely available tool schemas are
    /// excluded; schemas selected by the model are accounted for after the
    /// response completes.
    pub(crate) const fn is_producer_selected_context(self) -> bool {
        matches!(
            self,
            Self::BaseSystem
                | Self::Repository
                | Self::AgentRole
                | Self::Skills
                | Self::Plugins
                | Self::AppDesktop
                | Self::Collaboration
                | Self::EnvironmentPermissions
                | Self::TaskInput
                | Self::History
        )
    }
}

/// Measurement-only category hints keyed by canonical response-item
/// fingerprints. The vectors preserve mixed-message contribution ordering.
type PromptContributionsByItem = Arc<BTreeMap<[u8; 32], Arc<[Option<PromptContextCategory>]>>>;

#[derive(Clone, Debug, Default)]
pub(crate) struct PromptItemProvenance {
    categories: Arc<[Option<PromptContextCategory>]>,
    current_input: bool,
}

impl PromptItemProvenance {
    fn new(categories: Vec<Option<PromptContextCategory>>, current_input: bool) -> Self {
        Self {
            categories: categories.into(),
            current_input,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PromptProvenanceSidecar {
    contributions_by_item: PromptContributionsByItem,
    aligned_items: Arc<[PromptItemProvenance]>,
    current_turn_id: Option<String>,
    current_input_fingerprint: Option<[u8; 32]>,
}

impl PromptProvenanceSidecar {
    /// Extend attribution for a normalization-safe suffix without shifting the prefix.
    pub(crate) fn with_appended_items(
        &self,
        items: &[ResponseItem],
        manifest: &StableContextManifest,
    ) -> Self {
        let suffix = Self::from_assembled_items(items, manifest);
        let mut aligned_items = self.aligned_items.to_vec();
        aligned_items.extend(suffix.aligned_items.iter().cloned().zip(items).map(
            |(mut provenance, item)| {
                if matches!(item, ResponseItem::Message { .. }) {
                    provenance.current_input = self
                        .current_turn_id
                        .as_deref()
                        .is_some_and(|turn_id| item.turn_id() == Some(turn_id));
                }
                provenance
            },
        ));
        let mut contributions_by_item = Arc::clone(&self.contributions_by_item);
        if !suffix.contributions_by_item.is_empty() {
            Arc::make_mut(&mut contributions_by_item).extend(
                suffix
                    .contributions_by_item
                    .iter()
                    .map(|(key, value)| (*key, Arc::clone(value))),
            );
        }
        Self {
            aligned_items: aligned_items.into(),
            contributions_by_item,
            ..self.clone()
        }
    }

    #[cfg(test)]
    pub(crate) fn shares_contributions_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.contributions_by_item, &other.contributions_by_item)
    }

    pub(crate) fn from_assembled_items(
        items: &[ResponseItem],
        manifest: &StableContextManifest,
    ) -> Self {
        let mut categories_by_content_hash =
            HashMap::<[u8; 32], Option<PromptContextCategory>>::new();
        for component in manifest
            .components()
            .iter()
            .filter(|component| component.active)
        {
            let category = category_for_stable_kind(component.kind);
            categories_by_content_hash
                .entry(component.identity.content_hash)
                .and_modify(|existing| {
                    if *existing != Some(category) {
                        *existing = None;
                    }
                })
                .or_insert(Some(category));
        }

        let current_input_index = items.iter().rposition(|item| {
            matches!(item, ResponseItem::Message { role, .. } if role == "user")
                && crate::context_manager::is_user_turn_boundary(item)
        });
        let mut contributions_by_item = BTreeMap::new();
        let mut aligned_items = Vec::with_capacity(items.len());
        let mut current_input_fingerprint = None;
        let current_turn_id = current_input_index
            .and_then(|index| items.get(index))
            .and_then(ResponseItem::turn_id);
        for (index, item) in items.iter().enumerate() {
            let ResponseItem::Message { content, .. } = item else {
                aligned_items.push(PromptItemProvenance::default());
                continue;
            };
            let categories = content
                .iter()
                .map(|content| match content {
                    ContentItem::InputText { text } => {
                        if categories_by_content_hash.is_empty()
                            || crate::context_manager::is_user_turn_boundary(item)
                        {
                            return None;
                        }
                        let hash = category_lookup_hash(text);
                        categories_by_content_hash.get(&hash).copied().flatten()
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let current_input = current_input_index == Some(index)
                || current_turn_id.is_some_and(|turn_id| item.turn_id() == Some(turn_id));
            let fingerprint = if prompt_item_requires_fingerprint(
                &categories,
                current_input_index == Some(index),
            ) {
                response_item_fingerprint(item)
            } else {
                None
            };
            if categories.iter().any(Option::is_some)
                && let Some(fingerprint) = fingerprint
            {
                contributions_by_item.insert(fingerprint, categories.clone().into());
            }
            if current_input_index == Some(index) {
                current_input_fingerprint = fingerprint;
            }
            aligned_items.push(PromptItemProvenance::new(categories, current_input));
        }

        Self {
            contributions_by_item: Arc::new(contributions_by_item),
            aligned_items: aligned_items.into(),
            current_turn_id: current_turn_id.map(str::to_string),
            current_input_fingerprint,
        }
    }

    /// Adds provenance for an exact fragment supplied by a built-in assembly
    /// site. This deliberately compares the already-rendered contribution and
    /// does not infer categories from markers or prompt prose.
    #[cfg(test)]
    pub(crate) fn with_exact_fragment(
        &self,
        items: &[ResponseItem],
        fragment: &str,
        category: PromptContextCategory,
    ) -> Self {
        self.with_exact_fragments(items, std::iter::once((fragment, category)))
    }

    /// Adds provenance for multiple built-in fragments in one history pass.
    /// Message positions and contents must still match the assembled sequence;
    /// tool-output compaction may change non-message items in place.
    /// If assembly supplied the same text more than once, the last category
    /// wins, matching repeated `with_exact_fragment` calls.
    pub(crate) fn with_exact_fragments<'a>(
        &self,
        items: &[ResponseItem],
        fragments: impl IntoIterator<Item = (&'a str, PromptContextCategory)>,
    ) -> Self {
        let fragments = fragments.into_iter().collect::<Vec<_>>();
        if fragments.is_empty() {
            return self.clone();
        }
        let mut occurrences = HashMap::<&str, usize>::new();
        for item in items {
            if let ResponseItem::Message { role, content, .. } = item
                && role == "developer"
            {
                for part in content {
                    if let ContentItem::InputText { text } = part {
                        *occurrences.entry(text.as_str()).or_default() += 1;
                    }
                }
            }
        }
        let mut contributions_by_item = Arc::clone(&self.contributions_by_item);
        let mut aligned_items = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let ResponseItem::Message { role, content, .. } = item else {
                aligned_items.push(PromptItemProvenance::default());
                continue;
            };
            let aligned = self.aligned_item(index);
            let mut categories = aligned
                .map(|item| item.categories.to_vec())
                .or_else(|| self.contributions(item).map(<[_]>::to_vec))
                .unwrap_or_else(|| vec![None; content.len()]);
            let mut changed = false;
            for (index, content_item) in content.iter().enumerate() {
                let ContentItem::InputText { text } = content_item else {
                    continue;
                };
                if role == "developer"
                    && occurrences.get(text.as_str()) == Some(&1)
                    && let Some((_, category)) = fragments
                        .iter()
                        .rev()
                        .find(|(fragment, _)| text.as_str() == *fragment)
                {
                    categories[index] = Some(*category);
                    changed = true;
                }
            }
            if changed && let Some(fingerprint) = response_item_fingerprint(item) {
                Arc::make_mut(&mut contributions_by_item)
                    .insert(fingerprint, categories.clone().into());
            }
            aligned_items.push(PromptItemProvenance::new(
                categories,
                aligned.map_or_else(|| self.is_current_input(item), |item| item.current_input),
            ));
        }
        Self {
            contributions_by_item,
            aligned_items: aligned_items.into(),
            current_turn_id: self.current_turn_id.clone(),
            current_input_fingerprint: self.current_input_fingerprint,
        }
    }

    /// Assigns one already-rendered response item to a category without
    /// matching equal text in any other item. The assembled sequence must be
    /// an unchanged suffix of `items`, optionally preceded by transport context.
    pub(crate) fn with_response_item_category(
        &self,
        items: &[ResponseItem],
        index: usize,
        category: PromptContextCategory,
    ) -> Self {
        let Some(item @ ResponseItem::Message { content, .. }) = items.get(index) else {
            return self.clone();
        };
        let offset = items.len().saturating_sub(self.aligned_items.len());
        let mut aligned_items = vec![PromptItemProvenance::default(); offset];
        aligned_items.extend(self.aligned_items.iter().cloned());
        let Some(aligned) = aligned_items.get_mut(index) else {
            return self.clone();
        };
        aligned.categories = vec![Some(category); content.len()].into();
        let mut contributions_by_item = self.contributions_by_item.as_ref().clone();
        if let Some(fingerprint) = response_item_fingerprint(item) {
            contributions_by_item.insert(fingerprint, vec![Some(category); content.len()].into());
        }
        Self {
            contributions_by_item: Arc::new(contributions_by_item),
            aligned_items: aligned_items.into(),
            current_turn_id: self.current_turn_id.clone(),
            current_input_fingerprint: self.current_input_fingerprint,
        }
    }

    fn aligned_item(&self, index: usize) -> Option<&PromptItemProvenance> {
        self.aligned_items.get(index)
    }

    /// Rebuild alignment only when transport fallback replay changes the
    /// assembled sequence. Canonical fingerprints are recovery hints here;
    /// ordinary requests retain occurrence-specific aligned attribution.
    pub(crate) fn for_reprojected_items(
        &self,
        original: &[ResponseItem],
        items: &[ResponseItem],
    ) -> Self {
        if self.aligned_items.is_empty() {
            return Self {
                aligned_items: vec![PromptItemProvenance::default(); items.len()].into(),
                ..self.clone()
            };
        }
        // Fallback projection retains the order of equal messages. Carry each occurrence's
        // aligned metadata from the actual source sequence, never a category per text value.
        let offset = original.len().saturating_sub(self.aligned_items.len());
        let mut origins = HashMap::<_, Vec<PromptItemProvenance>>::new();
        for (index, item) in original.iter().enumerate() {
            if let Some(fingerprint) = response_item_fingerprint(item) {
                let provenance = index
                    .checked_sub(offset)
                    .and_then(|index| self.aligned_item(index))
                    .cloned()
                    .unwrap_or_default();
                origins.entry(fingerprint).or_default().push(provenance);
            }
        }
        let fingerprints = items
            .iter()
            .map(response_item_fingerprint)
            .collect::<Vec<_>>();
        let mut counts = HashMap::<_, usize>::new();
        for fingerprint in fingerprints.iter().flatten() {
            *counts.entry(*fingerprint).or_default() += 1;
        }
        let mut used = HashMap::<_, usize>::new();
        let aligned_items = fingerprints
            .into_iter()
            .map(|fingerprint| {
                let Some(fingerprint) = fingerprint else {
                    return PromptItemProvenance::default();
                };
                let Some(matches) = origins.get(&fingerprint) else {
                    return PromptItemProvenance::default();
                };
                // If projection removed or added an indistinguishable occurrence, provenance is
                // unresolved. Do not turn an older identical user request into current input.
                if counts.get(&fingerprint) != Some(&matches.len()) {
                    return PromptItemProvenance::default();
                }
                let index = used.entry(fingerprint).or_default();
                let provenance = matches[*index].clone();
                *index += 1;
                provenance
            })
            .collect::<Vec<_>>();
        Self {
            aligned_items: aligned_items.into(),
            ..self.clone()
        }
    }

    fn contributions(&self, item: &ResponseItem) -> Option<&[Option<PromptContextCategory>]> {
        if self.contributions_by_item.is_empty() {
            return None;
        }
        let fingerprint = response_item_fingerprint(item)?;
        self.contributions_by_item
            .get(&fingerprint)
            .map(AsRef::as_ref)
    }

    fn is_current_input(&self, item: &ResponseItem) -> bool {
        if let Some(current_turn_id) = self.current_turn_id.as_deref() {
            return item.turn_id() == Some(current_turn_id);
        }
        let Some(expected) = self.current_input_fingerprint else {
            return false;
        };
        response_item_fingerprint(item).is_some_and(|actual| expected == actual)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PromptContextMeasurement {
    pub(crate) category: &'static str,
    pub(crate) serialized_bytes: u64,
    pub(crate) estimated_tokens: u64,
    pub(crate) sha256: String,
    pub(crate) unchanged_from_previous_request: bool,
    #[serde(skip_serializing)]
    pub(crate) hash: [u8; 32],
}

#[derive(Debug)]
struct CategoryAccumulator {
    serialized_bytes: u64,
    estimated_tokens: u64,
    hasher: Sha256,
}

impl CategoryAccumulator {
    fn new(category: PromptContextCategory) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(CATEGORY_HASH_DOMAIN);
        hasher.update(category.as_str().as_bytes());
        Self {
            serialized_bytes: 0,
            estimated_tokens: 0,
            hasher,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PromptContextBreakdown {
    pub(crate) fixed_prefix_item_count: usize,
    fixed_contribution_count: usize,
    categories: BTreeMap<PromptContextCategory, CategoryAccumulator>,
}

impl PromptContextBreakdown {
    pub(crate) fn from_response_items(
        items: &[ResponseItem],
        sidecar: &PromptProvenanceSidecar,
    ) -> serde_json::Result<Self> {
        let mut breakdown = Self::default();
        let aligned_offset = items.len().saturating_sub(sidecar.aligned_items.len());
        for (index, item) in items.iter().enumerate() {
            let serialized_item = serde_json::to_vec(item)?;
            let aligned = index
                .checked_sub(aligned_offset)
                .and_then(|index| sidecar.aligned_item(index));
            let before = breakdown.fixed_contribution_count;
            breakdown.record_response_item(item, &serialized_item, sidecar, aligned)?;
            if breakdown.fixed_contribution_count != before {
                breakdown.fixed_prefix_item_count = index + 1;
            }
        }
        // The array envelope grows with the item count. Keep its bytes in the
        // injected category for reconciliation, but leave it out of the
        // category hash: hashing it flipped fixed-prefix reuse eligibility on
        // every request that merely appended history.
        breakdown.record_overhead_unhashed(
            PromptContextCategory::OtherInjected,
            sequence_envelope_bytes(items.len()),
        );
        Ok(breakdown)
    }

    /// Measures response items from the bytes emitted by the transport
    /// serializer. The sidecar's aligned provenance was established while the
    /// prompt was assembled, so the diagnostic path does not serialize items
    /// again merely to recover category identities.
    pub(crate) fn from_serialized_response_items(
        items: &[ResponseItem],
        serialized_items: &[&RawValue],
        sidecar: &PromptProvenanceSidecar,
        embedded_base_index: Option<usize>,
    ) -> serde_json::Result<Self> {
        if items.len() != serialized_items.len() {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "serialized request input does not match logical input",
            )));
        }
        let aligned_offset = items.len().saturating_sub(sidecar.aligned_items.len());
        let mut breakdown = Self::default();
        for (index, (item, raw_item)) in items.iter().zip(serialized_items.iter()).enumerate() {
            let serialized_item = raw_item.get().as_bytes();
            if embedded_base_index == Some(index) {
                breakdown.record_serialized(PromptContextCategory::BaseSystem, serialized_item);
                breakdown.fixed_prefix_item_count = index + 1;
                continue;
            }
            let aligned = index
                .checked_sub(aligned_offset)
                .and_then(|index| sidecar.aligned_item(index));
            let before = breakdown.fixed_contribution_count;
            breakdown.record_serialized_response_item(item, serialized_item, raw_item, aligned)?;
            if breakdown.fixed_contribution_count != before {
                breakdown.fixed_prefix_item_count = index + 1;
            }
        }
        breakdown.record_overhead_unhashed(
            PromptContextCategory::OtherInjected,
            sequence_envelope_bytes(items.len()),
        );
        Ok(breakdown)
    }

    pub(crate) fn record_serialized(&mut self, category: PromptContextCategory, serialized: &[u8]) {
        let serialized_bytes = u64::try_from(serialized.len()).unwrap_or(u64::MAX);
        let estimated_tokens = u64::try_from(approx_token_count(
            std::str::from_utf8(serialized).unwrap_or_default(),
        ))
        .unwrap_or(u64::MAX);
        self.record(category, serialized_bytes, estimated_tokens, serialized);
    }

    pub(crate) fn bytes(&self, category: PromptContextCategory) -> u64 {
        self.categories
            .get(&category)
            .map_or(0, |entry| entry.serialized_bytes)
    }

    pub(crate) fn estimated_tokens(&self, category: PromptContextCategory) -> u64 {
        self.categories
            .get(&category)
            .map_or(0, |entry| entry.estimated_tokens)
    }

    pub(crate) fn total_estimated_tokens(&self) -> u64 {
        self.categories.values().fold(0_u64, |total, entry| {
            total.saturating_add(entry.estimated_tokens)
        })
    }

    pub(crate) fn record_sequence_envelope(
        &mut self,
        category: PromptContextCategory,
        item_count: usize,
        stable_source: &[u8],
    ) {
        self.record_overhead(category, sequence_envelope_bytes(item_count), stable_source);
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        self.categories.values().fold(0_u64, |total, entry| {
            total.saturating_add(entry.serialized_bytes)
        })
    }

    pub(crate) fn measurements(&self) -> Vec<PromptContextMeasurement> {
        PromptContextCategory::ALL
            .into_iter()
            .map(|category| {
                let (serialized_bytes, estimated_tokens, hash) =
                    if let Some(entry) = self.categories.get(&category) {
                        let digest: [u8; 32] = entry.hasher.clone().finalize().into();
                        (entry.serialized_bytes, entry.estimated_tokens, digest)
                    } else {
                        let digest: [u8; 32] =
                            CategoryAccumulator::new(category).hasher.finalize().into();
                        (0, 0, digest)
                    };
                PromptContextMeasurement {
                    category: category.as_str(),
                    serialized_bytes,
                    estimated_tokens,
                    sha256: hex_hash(&hash),
                    unchanged_from_previous_request: false,
                    hash,
                }
            })
            .collect()
    }

    fn record_response_item(
        &mut self,
        item: &ResponseItem,
        serialized_item: &[u8],
        sidecar: &PromptProvenanceSidecar,
        provenance: Option<&PromptItemProvenance>,
    ) -> serde_json::Result<()> {
        if matches!(item, ResponseItem::AdditionalTools { .. }) {
            self.record_serialized(PromptContextCategory::ToolSchemas, serialized_item);
            return Ok(());
        }
        let ResponseItem::Message { role, content, .. } = item else {
            self.record_serialized(PromptContextCategory::History, serialized_item);
            return Ok(());
        };
        let fallback = if role == "user"
            && provenance.map_or_else(|| sidecar.is_current_input(item), |item| item.current_input)
        {
            PromptContextCategory::TaskInput
        } else if role == "developer" {
            PromptContextCategory::OtherInjected
        } else {
            PromptContextCategory::History
        };
        let categories = provenance
            .map(|item| item.categories.as_ref())
            .or_else(|| sidecar.contributions(item))
            .map(|categories| {
                categories
                    .iter()
                    .map(|category| category.unwrap_or(fallback))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![fallback; content.len()]);
        let first = categories.first().copied().unwrap_or(fallback);
        if categories.iter().all(|category| *category == first) {
            self.record_serialized(first, serialized_item);
            return Ok(());
        }

        let mut content_bytes = 0_u64;
        for (content_item, category) in content.iter().zip(categories) {
            let serialized_content = serde_json::to_vec(content_item)?;
            content_bytes = content_bytes
                .saturating_add(u64::try_from(serialized_content.len()).unwrap_or(u64::MAX));
            self.record_serialized(category, &serialized_content);
        }
        self.record_overhead(
            PromptContextCategory::OtherInjected,
            u64::try_from(serialized_item.len())
                .unwrap_or(u64::MAX)
                .saturating_sub(content_bytes),
            b"mixed_message_envelope",
        );
        Ok(())
    }

    fn record_serialized_response_item(
        &mut self,
        item: &ResponseItem,
        serialized_item: &[u8],
        raw_item: &RawValue,
        provenance: Option<&PromptItemProvenance>,
    ) -> serde_json::Result<()> {
        if matches!(item, ResponseItem::AdditionalTools { .. }) {
            self.record_serialized(PromptContextCategory::ToolSchemas, serialized_item);
            return Ok(());
        }
        let ResponseItem::Message { role, content, .. } = item else {
            self.record_serialized(PromptContextCategory::History, serialized_item);
            return Ok(());
        };
        let fallback = if role == "user" && provenance.is_some_and(|value| value.current_input) {
            PromptContextCategory::TaskInput
        } else if role == "developer" {
            PromptContextCategory::OtherInjected
        } else {
            PromptContextCategory::History
        };
        let categories = provenance
            .map(|value| {
                value
                    .categories
                    .iter()
                    .map(|category| category.unwrap_or(fallback))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![fallback; content.len()]);
        let first = categories.first().copied().unwrap_or(fallback);
        if categories.iter().all(|category| *category == first) {
            self.record_serialized(first, serialized_item);
            return Ok(());
        }

        #[derive(Deserialize)]
        struct RawMessage<'a> {
            #[serde(borrow)]
            content: Vec<&'a RawValue>,
        }

        let raw_message: RawMessage<'_> = serde_json::from_str(raw_item.get())?;
        if raw_message.content.len() != content.len() {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "serialized message content does not match logical content",
            )));
        }
        let mut content_bytes = 0_u64;
        for (serialized_content, category) in raw_message.content.iter().zip(categories) {
            let serialized_content = serialized_content.get().as_bytes();
            content_bytes = content_bytes
                .saturating_add(u64::try_from(serialized_content.len()).unwrap_or(u64::MAX));
            self.record_serialized(category, serialized_content);
        }
        self.record_overhead(
            PromptContextCategory::OtherInjected,
            u64::try_from(serialized_item.len())
                .unwrap_or(u64::MAX)
                .saturating_sub(content_bytes),
            b"mixed_message_envelope",
        );
        Ok(())
    }

    fn record_overhead(
        &mut self,
        category: PromptContextCategory,
        serialized_bytes: u64,
        stable_source: &[u8],
    ) {
        if serialized_bytes == 0 {
            return;
        }
        self.record(
            category,
            serialized_bytes,
            serialized_bytes.saturating_add(3) / 4,
            stable_source,
        );
    }

    /// Charges envelope bytes to a category without contributing to its
    /// identity hash, so item-count-dependent overhead cannot make an
    /// otherwise unchanged category look changed.
    fn record_overhead_unhashed(&mut self, category: PromptContextCategory, serialized_bytes: u64) {
        if serialized_bytes == 0 {
            return;
        }
        let entry = self
            .categories
            .entry(category)
            .or_insert_with(|| CategoryAccumulator::new(category));
        entry.serialized_bytes = entry.serialized_bytes.saturating_add(serialized_bytes);
        entry.estimated_tokens = entry
            .estimated_tokens
            .saturating_add(serialized_bytes.saturating_add(3) / 4);
    }

    fn record(
        &mut self,
        category: PromptContextCategory,
        serialized_bytes: u64,
        estimated_tokens: u64,
        stable_source: &[u8],
    ) {
        if PromptContextCategory::FIXED_PREFIX.contains(&category) {
            self.fixed_contribution_count += 1;
        }
        let entry = self
            .categories
            .entry(category)
            .or_insert_with(|| CategoryAccumulator::new(category));
        entry.serialized_bytes = entry.serialized_bytes.saturating_add(serialized_bytes);
        entry.estimated_tokens = entry.estimated_tokens.saturating_add(estimated_tokens);
        entry.hasher.update(serialized_bytes.to_be_bytes());
        entry.hasher.update(stable_source);
    }
}

fn sequence_envelope_bytes(item_count: usize) -> u64 {
    let separators = item_count.saturating_sub(1);
    u64::try_from(2_usize.saturating_add(separators)).unwrap_or(u64::MAX)
}

fn response_item_fingerprint(item: &ResponseItem) -> Option<[u8; 32]> {
    // Only messages have provenance contributions. Borrow their serialized
    // fields while excluding provider-specific turn metadata from identity.
    #[derive(Serialize)]
    struct MessageIdentity<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<&'a ResponseItemId>,
        role: &'a str,
        content: &'a [ContentItem],
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<&'a MessagePhase>,
    }

    let ResponseItem::Message {
        id,
        role,
        content,
        phase,
        internal_chat_message_metadata_passthrough: _,
    } = item
    else {
        return None;
    };
    #[cfg(test)]
    tests::FINGERPRINT_CALLS.with(|calls| calls.set(calls.get() + 1));
    let mut hasher = Sha256::new();
    hasher.update(RESPONSE_ITEM_FINGERPRINT_DOMAIN);
    serde_json::to_writer(
        &mut hasher,
        &MessageIdentity {
            kind: "message",
            id: id.as_ref(),
            role,
            content,
            phase: phase.as_ref(),
        },
    )
    .ok()?;
    Some(hasher.finalize().into())
}

fn category_lookup_hash(text: &str) -> [u8; 32] {
    #[cfg(test)]
    tests::CATEGORY_LOOKUP_BYTES.with(|count| count.set(count.get() + text.len()));
    Sha256::digest(text.as_bytes()).into()
}

fn prompt_item_requires_fingerprint(
    categories: &[Option<PromptContextCategory>],
    current_input: bool,
) -> bool {
    current_input || categories.iter().any(Option::is_some)
}

fn category_for_stable_kind(kind: StableContextKind) -> PromptContextCategory {
    match kind {
        StableContextKind::BaseModel => PromptContextCategory::BaseSystem,
        StableContextKind::ToolSchemas => PromptContextCategory::ToolSchemas,
        StableContextKind::Repository => PromptContextCategory::Repository,
        StableContextKind::Collaboration => PromptContextCategory::Collaboration,
        StableContextKind::SkillUsage | StableContextKind::SelectedSkill => {
            PromptContextCategory::Skills
        }
        StableContextKind::SkillCatalog => PromptContextCategory::SkillCatalog,
        StableContextKind::DesktopApp | StableContextKind::AppContext => {
            PromptContextCategory::AppDesktop
        }
        StableContextKind::Plugins => PromptContextCategory::Plugins,
        StableContextKind::RecommendedPlugins => PromptContextCategory::PluginCatalog,
        StableContextKind::Environment | StableContextKind::EnvironmentPermissions => {
            PromptContextCategory::EnvironmentPermissions
        }
        StableContextKind::RootCoordinator
        | StableContextKind::MultiAgent
        | StableContextKind::MultiAgentUsageHint => PromptContextCategory::AgentRole,
        StableContextKind::RequestUserInput
        | StableContextKind::Wait
        | StableContextKind::TurnContribution
        | StableContextKind::DynamicHistory
        | StableContextKind::TaskModelGuidance
        | StableContextKind::ModelSwitch
        | StableContextKind::Personality
        | StableContextKind::DeveloperInstructions => PromptContextCategory::OtherInjected,
    }
}

fn hex_hash(hash: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in hash {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::InternalChatMessageMetadataPassthrough;

    // Synchronous tests count real fingerprint work without timing thresholds
    // or interference from tests running on other threads.
    thread_local! {
        pub(super) static CATEGORY_LOOKUP_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        pub(super) static FINGERPRINT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn measure_both(
        items: &[ResponseItem],
        sidecar: &PromptProvenanceSidecar,
    ) -> [PromptContextBreakdown; 2] {
        let encoded = serde_json::to_string(items).unwrap();
        let raw_items: Vec<&RawValue> = serde_json::from_str(&encoded).unwrap();
        let logical = PromptContextBreakdown::from_response_items(items, sidecar).unwrap();
        let wire = PromptContextBreakdown::from_serialized_response_items(
            items, &raw_items, sidecar, None,
        )
        .unwrap();
        assert_eq!(logical.measurements(), wire.measurements());
        assert_eq!(logical.total_bytes(), encoded.len() as u64);
        [logical, wire]
    }

    fn item_bytes(item: &ResponseItem) -> u64 {
        serde_json::to_vec(item).unwrap().len() as u64
    }

    fn message(role: &str, text: &str, turn_id: Option<&str>) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: turn_id.map(|turn_id| {
                InternalChatMessageMetadataPassthrough {
                    turn_id: Some(turn_id.to_string()),
                }
            }),
        }
    }

    #[test]
    fn fingerprint_stream_matches_protocol_message_without_turn_metadata() {
        for phase in [
            None,
            Some(MessagePhase::Commentary),
            Some(MessagePhase::FinalAnswer),
        ] {
            let mut item = message(
                "assistant",
                &"large \"text\" & Unicode 😀\n".repeat(1024),
                Some("turn"),
            );
            if let ResponseItem::Message {
                id,
                phase: item_phase,
                content,
                ..
            } = &mut item
            {
                *id = Some(ResponseItemId::from_server("message-id".to_string()));
                *item_phase = phase;
                content.push(ContentItem::InputImage {
                    image_url: "data:image/png;base64,abc".to_string(),
                    detail: None,
                });
            }
            let fingerprint = response_item_fingerprint(&item).unwrap();
            item.clear_internal_chat_message_metadata_passthrough();
            let mut expected = Sha256::new();
            expected.update(RESPONSE_ITEM_FINGERPRINT_DOMAIN);
            expected.update(serde_json::to_vec(&item).unwrap());
            assert_eq!(fingerprint, <[u8; 32]>::from(expected.finalize()));
            assert_eq!(response_item_fingerprint(&item), Some(fingerprint));
        }
        let bytes = std::array::from_fn(|index| (index * 8) as u8);
        assert_eq!(
            hex_hash(&bytes),
            "0008101820283038404850586068707880889098a0a8b0b8c0c8d0d8e0e8f0f8"
        );
    }

    #[test]
    fn replay_matches_injected_context_after_provider_metadata_is_removed() {
        let original = vec![
            message("developer", "remember café & quotes \"here\"", Some("turn")),
            message("user", "request", None),
        ];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &original,
            &StableContextManifest::default(),
        )
        .with_exact_fragment(
            &original,
            "remember café & quotes \"here\"",
            PromptContextCategory::AgentRole,
        );
        let mut replay = original.clone();
        replay[0].clear_internal_chat_message_metadata_passthrough();
        replay.push(message("developer", "different context", None));
        let recovered = sidecar.for_reprojected_items(&original, &replay);
        for measured in measure_both(&replay, &recovered) {
            assert_eq!(
                measured.bytes(PromptContextCategory::AgentRole),
                item_bytes(&replay[0])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&replay[1])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::OtherInjected),
                item_bytes(&replay[2]) + 4 // Two array brackets and two item separators.
            );
        }
    }

    #[test]
    fn replay_keeps_duplicate_occurrences_and_input_after_metadata_removal() {
        let original = vec![
            message("developer", "same", None),
            message("developer", "same", None),
            message("user", "continue", Some("old")),
            message("user", "continue", Some("current")),
        ];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &original,
            &StableContextManifest::default(),
        )
        .with_response_item_category(&original, 1, PromptContextCategory::AgentRole);
        let mut replay = vec![message("developer", "transport context", None)];
        replay.extend(original.iter().cloned());
        for item in &mut replay {
            item.clear_internal_chat_message_metadata_passthrough();
        }
        let before = serde_json::to_vec(&replay).unwrap();
        let recovered = sidecar.for_reprojected_items(&original, &replay);
        for measured in measure_both(&replay, &recovered) {
            assert_eq!(
                measured.bytes(PromptContextCategory::AgentRole),
                item_bytes(&replay[2])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::History),
                item_bytes(&replay[3])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&replay[4])
            );
        }
        assert_eq!(serde_json::to_vec(&replay).unwrap(), before);
        replay.remove(3);
        let ambiguous = sidecar.for_reprojected_items(&original, &replay);
        assert_eq!(
            measure_both(&replay, &ambiguous)[0].bytes(PromptContextCategory::TaskInput),
            0
        );
    }

    #[test]
    fn empty_manifest_skips_category_hashing_and_injected_user_context_is_not_task_input() {
        let items = vec![
            message("user", "current request", None),
            crate::context::ContextualUserFragment::into(
                crate::context::InternalModelContextFragment::new(
                    crate::context::InternalContextSource::from_static("extension"),
                    "steering",
                ),
            ),
        ];
        CATEGORY_LOOKUP_BYTES.set(0);
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        );
        assert_eq!(CATEGORY_LOOKUP_BYTES.get(), 0);
        let sidecar = sidecar.with_appended_items(&items[1..], &StableContextManifest::default());
        for measured in measure_both(&items, &sidecar) {
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&items[0])
            );
        }
    }

    #[test]
    fn compatibility_fragment_matching_does_not_claim_quotes_or_ambiguous_occurrences() {
        let items = vec![
            message("developer", "memory", None),
            message("user", "memory", None),
        ];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        )
        .with_exact_fragment(&items, "memory", PromptContextCategory::AgentRole);
        assert_eq!(
            measure_both(&items, &sidecar)[0].bytes(PromptContextCategory::AgentRole),
            item_bytes(&items[0])
        );
        let repeated = vec![items[0].clone(), items[0].clone()];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &repeated,
            &StableContextManifest::default(),
        )
        .with_exact_fragment(&repeated, "memory", PromptContextCategory::AgentRole);
        assert_eq!(
            measure_both(&repeated, &sidecar)[0].bytes(PromptContextCategory::AgentRole),
            0
        );
    }

    #[test]
    fn queued_agent_message_does_not_displace_current_user_input() {
        let items = vec![
            message("user", "current user input", None),
            ResponseItem::AgentMessage {
                id: None,
                author: "worker".into(),
                recipient: "root".into(),
                content: vec![
                    codex_protocol::models::AgentMessageInputContent::InputText {
                        text: "child result".into(),
                    },
                ],
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        );
        for measured in measure_both(&items, &sidecar) {
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&items[0])
            );
        }
    }

    #[test]
    fn user_quote_of_stable_component_keeps_task_input_identity() {
        let text = "exact stable policy text";
        let items = vec![
            message("developer", text, None),
            message("user", text, None),
        ];
        let manifest = StableContextManifest::default().add_component_bytes(
            StableContextKind::Repository,
            "repository",
            text.as_bytes(),
        );
        let provenance = PromptProvenanceSidecar::from_assembled_items(&items, &manifest);
        for measured in measure_both(&items, &provenance) {
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&items[1])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::Repository),
                item_bytes(&items[0])
            );
        }
    }

    #[test]
    fn appended_non_messages_preserve_current_input_alignment() {
        let mut items = vec![message("user", "current request", None)];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        );
        items.push(ResponseItem::FunctionCall {
            id: None,
            call_id: "call".into(),
            namespace: None,
            name: "read_file".into(),
            arguments: "{}".into(),
            internal_chat_message_metadata_passthrough: None,
        });
        let sidecar = sidecar.with_appended_items(&items[1..], &StableContextManifest::default());
        for measured in measure_both(&items, &sidecar) {
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&items[0])
            );
        }
    }

    #[test]
    fn ordinary_history_does_not_require_a_provenance_fingerprint() {
        for history_len in [1, 64] {
            let mut items = (0..history_len)
                .map(|index| message("user", &format!("prior {index}"), Some("old-turn")))
                .collect::<Vec<_>>();
            items.push(message("developer", "memory", Some("current-turn")));
            items.push(message("user", "first contribution", Some("current-turn")));
            items.push(message("user", "second contribution", Some("current-turn")));

            FINGERPRINT_CALLS.set(0);
            let sidecar = PromptProvenanceSidecar::from_assembled_items(
                &items,
                &StableContextManifest::default(),
            );
            assert_eq!(
                FINGERPRINT_CALLS.replace(0),
                1,
                "only the last input needs identity recovery"
            );
            let augmented =
                sidecar.with_exact_fragment(&items, "memory", PromptContextCategory::AgentRole);
            assert_eq!(
                FINGERPRINT_CALLS.replace(0),
                1,
                "only the matching fragment needs a fingerprint"
            );
            let unmatched =
                augmented.with_exact_fragment(&items, "absent", PromptContextCategory::Skills);
            assert_eq!(FINGERPRINT_CALLS.replace(0), 0);
            assert!(unmatched.shares_contributions_with(&augmented));

            for measured in measure_both(&items, &unmatched) {
                assert_eq!(
                    measured.bytes(PromptContextCategory::AgentRole),
                    item_bytes(&items[history_len])
                );
                assert_eq!(
                    measured.bytes(PromptContextCategory::History),
                    items[..history_len].iter().map(item_bytes).sum::<u64>()
                );
                assert_eq!(
                    measured.bytes(PromptContextCategory::TaskInput),
                    items[history_len + 1..].iter().map(item_bytes).sum::<u64>()
                );
                assert_eq!(measured.bytes(PromptContextCategory::Skills), 0);
            }
            assert_eq!(
                FINGERPRINT_CALLS.get(),
                0,
                "aligned measurement must not fingerprint history again"
            );
        }
    }

    #[test]
    fn absent_provenance_does_not_fingerprint_messages_for_lookup_or_replay() {
        let items = vec![
            message("user", "history", None),
            message("developer", "context", None),
        ];
        let sidecar = PromptProvenanceSidecar::default();
        FINGERPRINT_CALLS.set(0);
        let logical = PromptContextBreakdown::from_response_items(&items, &sidecar).unwrap();
        assert_eq!(
            logical.bytes(PromptContextCategory::History),
            item_bytes(&items[0])
        );
        assert_eq!(logical.bytes(PromptContextCategory::TaskInput), 0);
        assert_eq!(
            FINGERPRINT_CALLS.replace(0),
            0,
            "no identity or category hints exist to look up"
        );

        let recovered = sidecar.for_reprojected_items(&items, &items);
        for measured in measure_both(&items, &recovered) {
            assert_eq!(measured.measurements(), logical.measurements());
        }
        assert_eq!(
            FINGERPRINT_CALLS.get(),
            0,
            "empty provenance needs no recovery fingerprints"
        );
    }

    #[test]
    fn category_override_updates_every_content_block_of_only_the_selected_occurrence() {
        let mut repeated = message("user", "first block", None);
        if let ResponseItem::Message { content, .. } = &mut repeated {
            content.push(ContentItem::InputText {
                text: "second block".to_string(),
            });
        }
        let original = vec![repeated.clone(), repeated];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &original,
            &StableContextManifest::default(),
        );
        let mut prefixed = vec![message("developer", "transport base", None)];
        prefixed.extend(original.iter().cloned());
        let overridden = sidecar
            .with_response_item_category(&prefixed, 0, PromptContextCategory::BaseSystem)
            .with_response_item_category(&prefixed, 1, PromptContextCategory::AgentRole);
        for measured in measure_both(&prefixed, &overridden) {
            assert_eq!(
                measured.bytes(PromptContextCategory::BaseSystem),
                item_bytes(&prefixed[0])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::AgentRole),
                item_bytes(&prefixed[1])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&prefixed[2])
            );
            assert_eq!(measured.bytes(PromptContextCategory::History), 0);
        }
        for measured in measure_both(&original, &sidecar) {
            assert_eq!(measured.bytes(PromptContextCategory::AgentRole), 0);
            assert_eq!(
                measured.bytes(PromptContextCategory::History),
                item_bytes(&original[0])
            );
            assert_eq!(
                measured.bytes(PromptContextCategory::TaskInput),
                item_bytes(&original[1])
            );
        }
    }

    #[test]
    fn replay_recovers_categories_and_current_input_after_reordering_and_removal() {
        for turn_id in [Some("current-turn"), None] {
            let original = vec![
                message("user", "discarded history", Some("old-turn")),
                message("developer", "memory", turn_id),
                message("user", "current", turn_id),
            ];
            let sidecar = PromptProvenanceSidecar::from_assembled_items(
                &original,
                &StableContextManifest::default(),
            )
            .with_exact_fragment(&original, "memory", PromptContextCategory::AgentRole);
            let replay = vec![
                original[2].clone(),
                message("user", "restored history", Some("old-turn")),
                original[1].clone(),
            ];
            FINGERPRINT_CALLS.set(0);
            let recovered = sidecar.for_reprojected_items(&original, &replay);
            assert_eq!(
                FINGERPRINT_CALLS.replace(0),
                6,
                "match the source and replay occurrences only during fallback"
            );
            for measured in measure_both(&replay, &recovered) {
                assert_eq!(
                    measured.bytes(PromptContextCategory::TaskInput),
                    item_bytes(&replay[0])
                );
                assert_eq!(
                    measured.bytes(PromptContextCategory::History),
                    item_bytes(&replay[1])
                );
                assert_eq!(
                    measured.bytes(PromptContextCategory::AgentRole),
                    item_bytes(&replay[2])
                );
                assert_eq!(measured.bytes(PromptContextCategory::OtherInjected), 4);
            }
            assert_eq!(FINGERPRINT_CALLS.get(), 0);
        }
    }

    #[test]
    fn unmatched_turn_identity_separates_task_input_from_history() {
        let items = vec![
            message("user", "prior", Some("turn-1")),
            message("assistant", "answer", Some("turn-1")),
            message("user", "current", Some("turn-2")),
        ];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        );
        let breakdown = PromptContextBreakdown::from_response_items(&items, &sidecar)
            .expect("breakdown should build");
        assert!(breakdown.bytes(PromptContextCategory::TaskInput) > 0);
        assert!(breakdown.bytes(PromptContextCategory::History) > 0);
        assert_eq!(
            breakdown.total_bytes(),
            serde_json::to_vec(&items).unwrap().len() as u64
        );
    }

    #[test]
    fn category_measurements_are_hash_only_and_domain_stable() {
        let items = vec![message("user", "secret prompt text", Some("turn-2"))];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        );
        let breakdown = PromptContextBreakdown::from_response_items(&items, &sidecar)
            .expect("breakdown should build");
        let serialized = serde_json::to_string(&breakdown.measurements()).unwrap();
        assert!(serialized.contains("estimated_tokens"));
        assert!(serialized.contains("sha256"));
        assert!(!serialized.contains("secret prompt text"));
    }

    #[test]
    fn assembly_site_exact_fragments_override_unknown_history_without_parsing() {
        let permissions = "opaque rendered permissions contribution";
        let items = vec![message("developer", permissions, Some("turn-2"))];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        )
        .with_exact_fragment(
            &items,
            permissions,
            PromptContextCategory::EnvironmentPermissions,
        );
        let breakdown = PromptContextBreakdown::from_response_items(&items, &sidecar)
            .expect("breakdown should build");

        assert!(breakdown.bytes(PromptContextCategory::EnvironmentPermissions) > 0);
        assert_eq!(breakdown.bytes(PromptContextCategory::History), 0);
    }

    #[test]
    fn assembly_site_exact_fragments_classify_multiple_categories_together() {
        let permissions = "opaque rendered permissions contribution";
        let role = "opaque rendered role contribution";
        let items = vec![
            message("developer", permissions, Some("turn-2")),
            message("developer", role, Some("turn-2")),
        ];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        )
        .with_exact_fragments(
            &items,
            [
                (permissions, PromptContextCategory::EnvironmentPermissions),
                (role, PromptContextCategory::AgentRole),
            ],
        );
        let breakdown = PromptContextBreakdown::from_response_items(&items, &sidecar)
            .expect("breakdown should build");

        assert!(breakdown.bytes(PromptContextCategory::EnvironmentPermissions) > 0);
        assert!(breakdown.bytes(PromptContextCategory::AgentRole) > 0);
        assert_eq!(breakdown.bytes(PromptContextCategory::History), 0);
    }

    #[test]
    fn unrelated_fragments_preserve_occurrence_categories_and_shared_fingerprints() {
        let items = vec![
            message("developer", "same content", None),
            message("developer", "same content", None),
            message("user", "current", None),
        ];
        let sidecar = PromptProvenanceSidecar::from_assembled_items(
            &items,
            &StableContextManifest::default(),
        )
        .with_response_item_category(&items, 1, PromptContextCategory::AgentRole);
        let augmented =
            sidecar.with_exact_fragment(&items, "absent", PromptContextCategory::Skills);
        assert!(augmented.shares_contributions_with(&sidecar));
        let measured = PromptContextBreakdown::from_response_items(&items, &augmented).unwrap();
        assert_eq!(
            measured.bytes(PromptContextCategory::AgentRole),
            serde_json::to_vec(&items[1]).unwrap().len() as u64
        );
        assert_eq!(measured.bytes(PromptContextCategory::Skills), 0);
        assert_eq!(
            measured.bytes(PromptContextCategory::TaskInput),
            serde_json::to_vec(&items[2]).unwrap().len() as u64
        );
    }
}
