use crate::digest_writer::DigestWriter;
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

/// byte_div_4_image_budget_v1: retain JSON envelopes and text, replace image
/// URLs with empty strings and add the history owner's detail-aware image
/// estimate. Transport byte counts and hashes always use the original bytes.
pub(crate) fn content_aware_serialized_tokens(serialized: &[u8]) -> u64 {
    let ordinary = || u64::try_from(approx_token_count(
        std::str::from_utf8(serialized).unwrap_or_default())).unwrap_or(u64::MAX);
    if !serialized.windows(b"\"input_image\"".len()).any(|part| part == b"\"input_image\"") {
        return ordinary();
    }
    fn strip_images(value: &mut serde_json::Value) -> u64 {
        match value {
            serde_json::Value::Array(items) => items.iter_mut().fold(0u64, |sum, item|
                sum.saturating_add(strip_images(item))),
            serde_json::Value::Object(object) => {
                if object.get("type").and_then(serde_json::Value::as_str) == Some("input_image") {
                    let detail = object.get("detail").cloned()
                        .and_then(|detail| serde_json::from_value(detail).ok());
                    if let Some(url) = object.get_mut("image_url")
                        && let Some(text) = url.as_str()
                    {
                        let bytes = crate::context_manager::estimate_image_bytes(text, detail);
                        *url = serde_json::Value::String(String::new());
                        return (bytes.max(0) as u64).div_ceil(4);
                    }
                    return 0;
                }
                // Only actual request/content envelopes, never schema examples
                // or a tool's argument JSON, can confer modality semantics.
                let kind = object.get("type").and_then(serde_json::Value::as_str);
                let fields: &[&str] = match kind {
                    Some("message") => &["content"],
                    Some("function_call_output" | "custom_tool_call_output") => &["output"],
                    None if object.contains_key("input") => &["input"],
                    _ => &[],
                };
                fields.iter().fold(0u64, |sum, key| sum.saturating_add(
                    object.get_mut(*key).map_or(0, strip_images)))
            }
            _ => 0,
        }
    }
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(serialized) else {
        return ordinary();
    };
    let images = strip_images(&mut value);
    if images == 0 { return ordinary(); }
    (approx_token_count(&value.to_string()) as u64).saturating_add(images)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum PromptContextCategory {
    BaseSystem,
    ToolSchemas,
    DiscoveredToolSchemas,
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
    /// Client-executed tool results returned to the model (command output,
    /// file bodies, MCP results). Their calls and other items stay `History`.
    ToolOutputs,
    OtherInjected,
}

impl PromptContextCategory {
    pub(crate) const ALL: [Self; 16] = [
        Self::BaseSystem,
        Self::ToolSchemas,
        Self::DiscoveredToolSchemas,
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
        Self::ToolOutputs,
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
            Self::DiscoveredToolSchemas => "discovered_tool_schemas",
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
            Self::ToolOutputs => "tool_outputs",
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
                | Self::ToolOutputs
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


    pub(crate) fn from_assembled_items(
        items: &[ResponseItem],
        manifest: &StableContextManifest,
    ) -> Self {
        let mut categories_by_source = HashMap::new();
        for component in manifest
            .components()
            .iter()
            .filter(|component| component.active)
        {
            let category = category_for_stable_kind(component.kind);
            categories_by_source
                .insert((component.kind, component.identity.content_hash), category);
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
            let mut categories = vec![None; content.len()];
            if !categories_by_source.is_empty() {
                // Bind accounting to the trusted source occurrence, including a
                // presence marker's following payload. Equal prose is not origin.
                for (range, kind, payload) in crate::stable_context::stable_content_sections(item) {
                    if let Some(category) =
                        categories_by_source.get(&(kind, category_lookup_hash(payload)))
                    {
                        for index in range {
                            categories[index] = Some(*category);
                        }
                    }
                }
            }
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
    /// Length of the leading run of input items whose contributions are all
    /// fixed-prefix categories: the provider-cacheable fixed prefix.
    pub(crate) fixed_prefix_item_count: usize,
    fixed_contribution_count: usize,
    /// Every recorded contribution, fixed or not. Compared with the fixed
    /// count, it shows whether one item contributed only fixed categories.
    contribution_count: usize,
    /// Set by the first input item with any non-fixed contribution. Fixed
    /// categories after it (for example a notice appended after history) are
    /// appended context and cannot extend or invalidate the cacheable prefix.
    prefix_closed: bool,
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
            let before = breakdown.contribution_counts();
            breakdown.record_response_item(item, &serialized_item, sidecar, aligned)?;
            breakdown.observe_prefix_item(index, before);
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
            let before = breakdown.contribution_counts();
            if embedded_base_index == Some(index) {
                breakdown.record_serialized(PromptContextCategory::BaseSystem, serialized_item);
                breakdown.observe_prefix_item(index, before);
                continue;
            }
            let aligned = index
                .checked_sub(aligned_offset)
                .and_then(|index| sidecar.aligned_item(index));
            breakdown.record_serialized_response_item(item, serialized_item, raw_item, aligned)?;
            breakdown.observe_prefix_item(index, before);
        }
        breakdown.record_overhead_unhashed(
            PromptContextCategory::OtherInjected,
            sequence_envelope_bytes(items.len()),
        );
        Ok(breakdown)
    }

    fn contribution_counts(&self) -> (usize, usize) {
        (self.fixed_contribution_count, self.contribution_count)
    }

    /// Extends the fixed prefix through the leading run of items whose every
    /// contribution is a fixed-prefix category; the first other item closes it.
    fn observe_prefix_item(&mut self, index: usize, (fixed_before, total_before): (usize, usize)) {
        if self.prefix_closed {
            return;
        }
        let fixed = self.fixed_contribution_count - fixed_before;
        let total = self.contribution_count - total_before;
        if fixed > 0 && fixed == total {
            self.fixed_prefix_item_count = index + 1;
        } else {
            self.prefix_closed = true;
        }
    }

    pub(crate) fn record_serialized(&mut self, category: PromptContextCategory, serialized: &[u8]) {
        let serialized_bytes = u64::try_from(serialized.len()).unwrap_or(u64::MAX);
        let estimated_tokens = content_aware_serialized_tokens(serialized);
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
        if contains_discovered_tool_schemas(item) {
            self.record_serialized(
                PromptContextCategory::DiscoveredToolSchemas,
                serialized_item,
            );
            return Ok(());
        }
        let ResponseItem::Message { role, content, .. } = item else {
            let category = if is_tool_output_item(item) {
                PromptContextCategory::ToolOutputs
            } else {
                PromptContextCategory::History
            };
            self.record_serialized(category, serialized_item);
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
        if contains_discovered_tool_schemas(item) {
            self.record_serialized(
                PromptContextCategory::DiscoveredToolSchemas,
                serialized_item,
            );
            return Ok(());
        }
        let ResponseItem::Message { role, content, .. } = item else {
            let category = if is_tool_output_item(item) {
                PromptContextCategory::ToolOutputs
            } else {
                PromptContextCategory::History
            };
            self.record_serialized(category, serialized_item);
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
        self.contribution_count += 1;
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

fn contains_discovered_tool_schemas(item: &ResponseItem) -> bool {
    // Retired discovery receipts are history, not callable definitions. Keep
    // definitions separate from the request-level schema prefix so appending a
    // search result cannot falsely invalidate that unchanged prefix.
    matches!(item, ResponseItem::ToolSearchOutput { tools, .. }
        if tools.iter().any(|tool|
            codex_tools::LoadableToolSpec::callable_names_from_value(tool).is_some()))
}

/// Results of client-executed tools. Their calls, reasoning and retired
/// discovery receipts stay history, so repeated tool output is attributable.
fn is_tool_output_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
    )
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
    let mut hasher = Sha256::new();
    hasher.update(RESPONSE_ITEM_FINGERPRINT_DOMAIN);
    serde_json::to_writer(
        DigestWriter(&mut hasher),
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
        StableContextKind::Repository | StableContextKind::RepositoryObservation => {
            PromptContextCategory::Repository
        }
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
        | StableContextKind::TaskState
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


