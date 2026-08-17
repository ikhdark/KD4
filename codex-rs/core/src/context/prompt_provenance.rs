use crate::stable_context::StableContextKind;
use crate::stable_context::StableContextManifest;
use codex_extension_api::PromptFragment;
use codex_extension_api::PromptFragmentKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

const CATEGORY_HASH_DOMAIN: &[u8] = b"codex.prompt-context-category.v1";
const RESPONSE_ITEM_FINGERPRINT_DOMAIN: &[u8] = b"codex.prompt-response-item.v1";

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
    Memory,
    OtherInjected,
}

impl PromptContextCategory {
    pub(crate) const ALL: [Self; 15] = [
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
        Self::Memory,
        Self::OtherInjected,
    ];

    pub(crate) const FIXED_PREFIX: [Self; 13] = [
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
        Self::Memory,
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
            Self::Memory => "memory",
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
                | Self::Memory
        )
    }
}

/// Internal provenance for a public prompt fragment. Public extension values
/// remain constructor-compatible and enter the built-in assembly as
/// `OtherInjected`.
#[derive(Clone, Debug)]
pub(crate) struct CategorizedPromptFragment {
    fragment: PromptFragment,
    category: PromptContextCategory,
}

impl CategorizedPromptFragment {
    pub(crate) fn from_extension(fragment: PromptFragment) -> Self {
        let category = match fragment.kind() {
            PromptFragmentKind::OtherInjected => PromptContextCategory::OtherInjected,
            PromptFragmentKind::Memory => PromptContextCategory::Memory,
        };
        Self { fragment, category }
    }

    pub(crate) fn category(&self) -> PromptContextCategory {
        self.category
    }

    pub(crate) fn into_fragment(self) -> PromptFragment {
        self.fragment
    }
}

/// Measurement-only category hints keyed by canonical response-item
/// fingerprints. The vectors preserve mixed-message contribution ordering.
type PromptContributionsByItem = Arc<BTreeMap<[u8; 32], Arc<[Option<PromptContextCategory>]>>>;

#[derive(Clone, Debug, Default)]
pub(crate) struct PromptProvenanceSidecar {
    contributions_by_item: PromptContributionsByItem,
    current_turn_id: Option<String>,
    current_input_fingerprint: Option<[u8; 32]>,
}

impl PromptProvenanceSidecar {
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

        let mut contributions_by_item = BTreeMap::new();
        for item in items {
            let ResponseItem::Message { content, .. } = item else {
                continue;
            };
            let categories = content
                .iter()
                .map(|content| match content {
                    ContentItem::InputText { text } => {
                        let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
                        categories_by_content_hash.get(&hash).copied().flatten()
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if categories.iter().any(Option::is_some)
                && let Ok(fingerprint) = response_item_fingerprint(item)
            {
                contributions_by_item.insert(fingerprint, categories.into());
            }
        }

        let current_input = items.iter().rev().find(|item| {
            let ResponseItem::Message { role, content, .. } = item else {
                return false;
            };
            role == "user"
                && content.iter().any(|content| match content {
                    ContentItem::InputText { text } => {
                        let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
                        !categories_by_content_hash.contains_key(&hash)
                    }
                    _ => true,
                })
        });
        Self {
            contributions_by_item: Arc::new(contributions_by_item),
            current_turn_id: current_input
                .and_then(ResponseItem::turn_id)
                .map(str::to_string),
            current_input_fingerprint: current_input
                .and_then(|item| response_item_fingerprint(item).ok()),
        }
    }

    /// Adds provenance for an exact fragment supplied by a built-in assembly
    /// site. This deliberately compares the already-rendered contribution and
    /// does not infer categories from markers or prompt prose.
    pub(crate) fn with_exact_fragment(
        &self,
        items: &[ResponseItem],
        fragment: &str,
        category: PromptContextCategory,
    ) -> Self {
        let mut contributions_by_item = self.contributions_by_item.as_ref().clone();
        for item in items {
            let ResponseItem::Message { content, .. } = item else {
                continue;
            };
            let Ok(fingerprint) = response_item_fingerprint(item) else {
                continue;
            };
            let mut categories = contributions_by_item
                .get(&fingerprint)
                .map(|categories| categories.to_vec())
                .unwrap_or_else(|| vec![None; content.len()]);
            let mut changed = false;
            for (index, content_item) in content.iter().enumerate() {
                if matches!(content_item, ContentItem::InputText { text } if text == fragment) {
                    categories[index] = Some(category);
                    changed = true;
                }
            }
            if changed {
                contributions_by_item.insert(fingerprint, categories.into());
            }
        }
        Self {
            contributions_by_item: Arc::new(contributions_by_item),
            current_turn_id: self.current_turn_id.clone(),
            current_input_fingerprint: self.current_input_fingerprint,
        }
    }

    /// Assigns one already-rendered response item to a category without
    /// matching equal text in any other item.
    pub(crate) fn with_response_item_category(
        &self,
        item: &ResponseItem,
        category: PromptContextCategory,
    ) -> Self {
        let mut contributions_by_item = self.contributions_by_item.as_ref().clone();
        if let ResponseItem::Message { content, .. } = item
            && let Ok(fingerprint) = response_item_fingerprint(item)
        {
            contributions_by_item.insert(fingerprint, vec![Some(category); content.len()].into());
        }
        Self {
            contributions_by_item: Arc::new(contributions_by_item),
            current_turn_id: self.current_turn_id.clone(),
            current_input_fingerprint: self.current_input_fingerprint,
        }
    }

    fn contributions(&self, item: &ResponseItem) -> Option<&[Option<PromptContextCategory>]> {
        let fingerprint = response_item_fingerprint(item).ok()?;
        self.contributions_by_item
            .get(&fingerprint)
            .map(AsRef::as_ref)
    }

    fn is_current_input(&self, item: &ResponseItem) -> bool {
        if let Some(current_turn_id) = self.current_turn_id.as_deref() {
            return item.turn_id() == Some(current_turn_id);
        }
        self.current_input_fingerprint
            .zip(response_item_fingerprint(item).ok())
            .is_some_and(|(expected, actual)| expected == actual)
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
    categories: BTreeMap<PromptContextCategory, CategoryAccumulator>,
}

impl PromptContextBreakdown {
    pub(crate) fn from_response_items(
        items: &[ResponseItem],
        sidecar: &PromptProvenanceSidecar,
    ) -> serde_json::Result<Self> {
        let mut breakdown = Self::default();
        let mut item_bytes = 0_u64;
        for item in items {
            let serialized_item = serde_json::to_vec(item)?;
            item_bytes =
                item_bytes.saturating_add(u64::try_from(serialized_item.len()).unwrap_or(u64::MAX));
            breakdown.record_response_item(item, &serialized_item, sidecar)?;
        }
        let serialized_input = serde_json::to_vec(items)?;
        breakdown.record_overhead(
            PromptContextCategory::OtherInjected,
            u64::try_from(serialized_input.len())
                .unwrap_or(u64::MAX)
                .saturating_sub(item_bytes),
            b"response_input_array_envelope",
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
    ) -> serde_json::Result<()> {
        if matches!(item, ResponseItem::AdditionalTools { .. }) {
            self.record_serialized(PromptContextCategory::ToolSchemas, serialized_item);
            return Ok(());
        }
        let ResponseItem::Message { role, content, .. } = item else {
            self.record_serialized(PromptContextCategory::History, serialized_item);
            return Ok(());
        };
        let fallback = if role == "user" && sidecar.is_current_input(item) {
            PromptContextCategory::TaskInput
        } else if role == "developer" {
            PromptContextCategory::OtherInjected
        } else {
            PromptContextCategory::History
        };
        let categories = sidecar
            .contributions(item)
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

    fn record(
        &mut self,
        category: PromptContextCategory,
        serialized_bytes: u64,
        estimated_tokens: u64,
        stable_source: &[u8],
    ) {
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

fn response_item_fingerprint(item: &ResponseItem) -> serde_json::Result<[u8; 32]> {
    let mut normalized = item.clone();
    normalized.clear_internal_chat_message_metadata_passthrough();
    let serialized = serde_json::to_vec(&normalized)?;
    let mut hasher = Sha256::new();
    hasher.update(RESPONSE_ITEM_FINGERPRINT_DOMAIN);
    hasher.update((serialized.len() as u64).to_be_bytes());
    hasher.update(serialized);
    Ok(hasher.finalize().into())
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
        StableContextKind::Memory => PromptContextCategory::Memory,
        StableContextKind::RootCoordinator | StableContextKind::MultiAgent => {
            PromptContextCategory::AgentRole
        }
        StableContextKind::RequestUserInput
        | StableContextKind::Wait
        | StableContextKind::DynamicHistory
        | StableContextKind::ModelSwitch
        | StableContextKind::Personality
        | StableContextKind::Realtime => PromptContextCategory::OtherInjected,
    }
}

fn hex_hash(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_extension_api::PromptSlot;
    use codex_protocol::models::InternalChatMessageMetadataPassthrough;

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
    fn public_extension_fragments_are_other_injected() {
        let fragment = PromptFragment::new(PromptSlot::DeveloperPolicy, "extension text");
        let categorized = CategorizedPromptFragment::from_extension(fragment.clone());
        assert_eq!(categorized.category(), PromptContextCategory::OtherInjected);
        assert_eq!(categorized.into_fragment(), fragment);
    }

    #[test]
    fn memory_extension_fragments_keep_explicit_provenance() {
        let fragment =
            PromptFragment::developer_policy("memory text").with_kind(PromptFragmentKind::Memory);
        let categorized = CategorizedPromptFragment::from_extension(fragment.clone());
        assert_eq!(categorized.category(), PromptContextCategory::Memory);
        assert_eq!(categorized.into_fragment(), fragment);
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
}
