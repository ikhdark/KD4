use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::APPS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::COLLABORATION_MODE_OPEN_TAG;
use codex_protocol::protocol::ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::EXTENSION_SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::PLUGINS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_utils_output_truncation::approx_token_count;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
std::thread_local! {
    static MANIFEST_FINGERPRINT_CALLS: Cell<usize> = const { Cell::new(0) };
    static COMPACT_CATALOG_CALLS: Cell<usize> = const { Cell::new(0) };
    static CLASSIFY_STABLE_TEXT_CALLS: Cell<usize> = const { Cell::new(0) };
}

const STABLE_CONTEXT_CONTRACT_VERSION: u16 = 2;
const STABLE_CONTEXT_HASH_DOMAIN: &[u8] = b"codex.stable-context.component.v1";
const STABLE_CONTEXT_MANIFEST_HASH_DOMAIN: &[u8] = b"codex.stable-context.manifest.v1";
const SKILLS_USAGE_OPEN_TAG: &str = "<skills_usage_instructions>";
const SKILL_OPEN_TAG: &str = "<skill>";
const REPOSITORY_OPEN_TAG: &str = "# AGENTS.md instructions";
const REPOSITORY_CLOSE_TAG: &str = "</INSTRUCTIONS>";
const REPOSITORY_REMOVAL_NOTICE: &str =
    "The previously provided AGENTS.md instructions no longer apply.";
const COLLABORATION_RESET_NOTICE: &str = "No collaboration-mode-specific instructions are currently active. Any previously provided collaboration-mode instructions no longer apply.";
const ROOT_COORDINATOR_PREFIX: &str =
    "You are `/root`, the primary agent in a team of agents collaborating";
const ROOT_ORCHESTRATION_OPEN_TAG: &str = "<root_orchestration_instructions>";
const ROOT_ORCHESTRATION_CLOSE_TAG: &str = "</root_orchestration_instructions>";
const DEVELOPER_INSTRUCTIONS_PRESENT_MARKER: &str =
    "<configured_developer_instructions state=\"present\" />";
const DEVELOPER_INSTRUCTIONS_REMOVED_MARKER: &str =
    "<configured_developer_instructions state=\"removed\" />";
const MULTI_AGENT_USAGE_HINT_PRESENT_MARKER: &str = "<multi_agent_usage_hint state=\"present\" />";
const MULTI_AGENT_USAGE_HINT_REMOVED_MARKER: &str = "<multi_agent_usage_hint state=\"removed\" />";
const TRUSTED_STABLE_CONTEXT_ITEM_ID_BASE: &str = "msg_sctx";
const TRUSTED_STABLE_CONTEXT_ITEM_ID_PREFIX: &str = "msg_sctx_";

/// Marks a message as stable context emitted by a trusted core producer.
///
/// The marker uses the existing durable response-item ID field so it survives
/// rollout persistence and compaction without extending strongly typed
/// Responses metadata. Ordinary user messages receive `msg_<item-id>` IDs at
/// ingestion and therefore cannot acquire this core-owned prefix from text.
pub(crate) fn mark_trusted_stable_context_item(item: &mut ResponseItem) {
    let ResponseItem::Message { id, .. } = item else {
        return;
    };
    if id.as_ref().is_some_and(is_trusted_stable_context_id) {
        return;
    }
    *id = Some(ResponseItemId::new(TRUSTED_STABLE_CONTEXT_ITEM_ID_BASE));
}

fn is_trusted_stable_context_id(id: &ResponseItemId) -> bool {
    id.as_str()
        .starts_with(TRUSTED_STABLE_CONTEXT_ITEM_ID_PREFIX)
}

fn is_trusted_stable_context_item(item: &ResponseItem) -> bool {
    item.id().is_some_and(is_trusted_stable_context_id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum StableContextKind {
    BaseModel,
    Repository,
    Collaboration,
    SkillUsage,
    SkillCatalog,
    SelectedSkill,
    DesktopApp,
    AppContext,
    Plugins,
    RecommendedPlugins,
    Environment,
    EnvironmentPermissions,
    Memory,
    ModelSwitch,
    Personality,
    DeveloperInstructions,
    RootCoordinator,
    MultiAgent,
    MultiAgentUsageHint,
    TaskModelGuidance,
    TaskEvidence,
    ToolSchemas,
    RequestUserInput,
    Wait,
    DynamicHistory,
}

impl StableContextKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::BaseModel => "base_model",
            Self::Repository => "repository",
            Self::Collaboration => "collaboration",
            Self::SkillUsage => "skill_usage",
            Self::SkillCatalog => "skill_catalog",
            Self::SelectedSkill => "selected_skill",
            Self::DesktopApp => "desktop_app",
            Self::AppContext => "app_context",
            Self::Plugins => "plugins",
            Self::RecommendedPlugins => "recommended_plugins",
            Self::Environment => "environment",
            Self::EnvironmentPermissions => "environment_permissions",
            Self::Memory => "memory",
            Self::ModelSwitch => "model_switch",
            Self::Personality => "personality",
            Self::DeveloperInstructions => "developer_instructions",
            Self::RootCoordinator => "root_coordinator",
            Self::MultiAgent => "multi_agent",
            Self::MultiAgentUsageHint => "multi_agent_usage_hint",
            Self::TaskModelGuidance => "task_model_guidance",
            Self::TaskEvidence => "task_evidence",
            Self::ToolSchemas => "tool_schemas",
            Self::RequestUserInput => "request_user_input",
            Self::Wait => "wait",
            Self::DynamicHistory => "dynamic_history",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StableContextDisposition {
    Unchanged,
    Replaced,
    Removed,
    Gated,
    RetainedFallback,
}

/// Selects whether normalized history is being prepared for an actual model
/// sampling request or for a compatibility-sensitive generic caller.
///
/// Generic callers deliberately retain the complete normalized history. This
/// keeps compaction, finalization, and other non-sampling paths fail-open while
/// allowing sampling to opt into canonical stable-context projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StableContextTarget {
    Sampling,
    FailOpen,
}

impl StableContextDisposition {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Replaced => "replaced",
            Self::Removed => "removed",
            Self::Gated => "gated",
            Self::RetainedFallback => "retained_fallback",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StableContextIdentity {
    pub(crate) contract_version: u16,
    pub(crate) semantic_id: String,
    pub(crate) content_hash: [u8; 32],
    pub(crate) serialized_bytes: u64,
    pub(crate) approx_tokens: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StableContextComponent {
    pub(crate) kind: StableContextKind,
    pub(crate) identity: StableContextIdentity,
    pub(crate) active: bool,
    pub(crate) disposition: StableContextDisposition,
    pub(crate) local_reused: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StableContextManifest {
    components: Arc<[StableContextComponent]>,
    fingerprint: [u8; 32],
    projection_enabled: bool,
    fail_open: bool,
}

impl StableContextManifest {
    pub(crate) fn components(&self) -> &[StableContextComponent] {
        &self.components
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub(crate) fn projection_enabled(&self) -> bool {
        self.projection_enabled
    }

    pub(crate) fn fail_open(&self) -> bool {
        self.fail_open
    }

    pub(crate) fn active_content_hash(&self, kind: StableContextKind) -> Option<[u8; 32]> {
        let mut matches = self
            .components
            .iter()
            .filter(|component| component.active && component.kind == kind);
        let hash = matches.next()?.identity.content_hash;
        matches.next().is_none().then_some(hash)
    }

    pub(crate) fn projected_bytes(&self) -> u64 {
        self.components
            .iter()
            .filter(|component| component.active)
            .map(|component| component.identity.serialized_bytes)
            .fold(0_u64, u64::saturating_add)
    }

    pub(crate) fn projected_tokens(&self) -> i64 {
        self.components
            .iter()
            .filter(|component| component.active)
            .map(|component| component.identity.approx_tokens)
            .fold(0_i64, i64::saturating_add)
    }

    pub(crate) fn with_local_reused(&self, local_reused: bool) -> Self {
        let mut components = self.components.to_vec();
        for component in &mut components {
            component.local_reused = local_reused;
        }
        Self {
            components: components.into(),
            fingerprint: self.fingerprint,
            projection_enabled: self.projection_enabled,
            fail_open: self.fail_open,
        }
    }

    pub(crate) fn with_base_model(&self, model_slug: &str, base_instructions: &str) -> Self {
        let mut components = self.components.to_vec();
        components.push(base_component(model_slug, base_instructions));
        Self::from_components(components, self.projection_enabled, self.fail_open)
    }

    pub(crate) fn with_repository_identity(
        &self,
        repository: Option<([u8; 32], bool, bool)>,
    ) -> Self {
        let Some((semantic_id, local_reused, semantic_replacement)) = repository else {
            return self.clone();
        };
        let mut components = self.components.to_vec();
        let semantic_id = format!("repository:v1:{}", short_hash(&semantic_id));
        for component in &mut components {
            if component.kind == StableContextKind::Repository && component.active {
                component.identity.semantic_id.clone_from(&semantic_id);
                component.local_reused = local_reused;
                if semantic_replacement {
                    component.disposition = StableContextDisposition::Replaced;
                }
            }
        }
        Self::from_components(components, self.projection_enabled, self.fail_open)
    }

    pub(crate) fn add_component_bytes(
        &self,
        kind: StableContextKind,
        semantic_key: &str,
        bytes: &[u8],
    ) -> Self {
        let mut components = self.components.to_vec();
        components.push(component_from_bytes(
            kind,
            semantic_key,
            bytes,
            true,
            StableContextDisposition::Unchanged,
        ));
        Self::from_components(components, self.projection_enabled, self.fail_open)
    }

    pub(crate) fn add_measured_component(
        &self,
        kind: StableContextKind,
        semantic_key: &str,
        identity_bytes: &[u8],
        serialized_bytes: u64,
        approx_tokens: i64,
    ) -> Self {
        let mut components = self.components.to_vec();
        let mut component = component_from_bytes(
            kind,
            semantic_key,
            identity_bytes,
            true,
            StableContextDisposition::Unchanged,
        );
        component.identity.serialized_bytes = serialized_bytes;
        component.identity.approx_tokens = approx_tokens;
        components.push(component);
        Self::from_components(components, self.projection_enabled, self.fail_open)
    }

    /// Appends the request-dynamic history measurement without rebuilding the
    /// immutable stable-prefix identity. `DynamicHistory` is deliberately
    /// excluded from [`manifest_fingerprint`], and is the final sorted kind,
    /// so retaining the already-computed fingerprint is equivalent to
    /// `add_measured_component` while avoiding another stable-manifest hash.
    pub(crate) fn add_dynamic_history(
        &self,
        identity_bytes: &[u8],
        serialized_bytes: u64,
        approx_tokens: i64,
    ) -> Self {
        debug_assert!(
            self.components
                .iter()
                .all(|component| component.kind != StableContextKind::DynamicHistory),
            "request scaffold must not contain dynamic history"
        );
        let mut component = component_from_bytes(
            StableContextKind::DynamicHistory,
            "dynamic_history",
            identity_bytes,
            true,
            StableContextDisposition::Unchanged,
        );
        component.identity.serialized_bytes = serialized_bytes;
        component.identity.approx_tokens = approx_tokens;
        let mut components = self.components.to_vec();
        components.push(component);
        Self {
            components: components.into(),
            fingerprint: self.fingerprint,
            projection_enabled: self.projection_enabled,
            fail_open: self.fail_open,
        }
    }

    fn from_components(
        mut components: Vec<StableContextComponent>,
        projection_enabled: bool,
        fail_open: bool,
    ) -> Self {
        components.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.identity.semantic_id.cmp(&right.identity.semantic_id))
                .then_with(|| left.identity.content_hash.cmp(&right.identity.content_hash))
                .then_with(|| left.active.cmp(&right.active))
        });
        let fingerprint = manifest_fingerprint(&components, projection_enabled, fail_open);
        Self {
            components: components.into(),
            fingerprint,
            projection_enabled,
            fail_open,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StableContextProjection {
    pub(crate) items: Arc<[ResponseItem]>,
    pub(crate) fallback_items: Arc<[ResponseItem]>,
    pub(crate) manifest: StableContextManifest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum StableContextSlot {
    Repository,
    Collaboration,
    SkillUsage,
    SkillCatalog,
    ExtensionSkillCatalog,
    EnvironmentSkillCatalog,
    SelectedSkill,
    Apps,
    AppContext,
    Plugins,
    RecommendedPlugins,
    Environment,
    TaskModelGuidance,
    TaskEvidence,
    Permissions,
    Memory,
    ModelSwitch,
    Personality,
    DeveloperInstructions,
    MultiAgent,
    MultiAgentUsageHint,
    RootCoordinator,
}

impl StableContextSlot {
    fn kind(self) -> StableContextKind {
        match self {
            Self::Repository => StableContextKind::Repository,
            Self::Collaboration => StableContextKind::Collaboration,
            Self::SkillUsage => StableContextKind::SkillUsage,
            Self::SkillCatalog | Self::ExtensionSkillCatalog | Self::EnvironmentSkillCatalog => {
                StableContextKind::SkillCatalog
            }
            Self::SelectedSkill => StableContextKind::SelectedSkill,
            Self::Apps => StableContextKind::DesktopApp,
            Self::AppContext => StableContextKind::AppContext,
            Self::Plugins => StableContextKind::Plugins,
            Self::RecommendedPlugins => StableContextKind::RecommendedPlugins,
            Self::Environment => StableContextKind::Environment,
            Self::TaskModelGuidance => StableContextKind::TaskModelGuidance,
            Self::TaskEvidence => StableContextKind::TaskEvidence,
            Self::Permissions => StableContextKind::EnvironmentPermissions,
            Self::Memory => StableContextKind::Memory,
            Self::ModelSwitch => StableContextKind::ModelSwitch,
            Self::Personality => StableContextKind::Personality,
            Self::DeveloperInstructions => StableContextKind::DeveloperInstructions,
            Self::MultiAgent => StableContextKind::MultiAgent,
            Self::MultiAgentUsageHint => StableContextKind::MultiAgentUsageHint,
            Self::RootCoordinator => StableContextKind::RootCoordinator,
        }
    }

    fn semantic_key(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Collaboration => "collaboration",
            Self::SkillUsage => "skill_usage",
            Self::SkillCatalog => "skill_catalog",
            Self::ExtensionSkillCatalog => "extension_skill_catalog",
            Self::EnvironmentSkillCatalog => "environment_skill_catalog",
            Self::SelectedSkill => "selected_skill",
            Self::Apps => "apps",
            Self::AppContext => "app_context",
            Self::Plugins => "plugins",
            Self::RecommendedPlugins => "recommended_plugins",
            Self::Environment => "environment",
            Self::TaskModelGuidance => "task_model_guidance",
            Self::TaskEvidence => "task_evidence",
            Self::Permissions => "environment_permissions",
            Self::Memory => "memory",
            Self::ModelSwitch => "model_switch",
            Self::Personality => "personality",
            Self::DeveloperInstructions => "developer_instructions",
            Self::MultiAgent => "multi_agent",
            Self::MultiAgentUsageHint => "multi_agent_usage_hint",
            Self::RootCoordinator => "root_coordinator",
        }
    }

    /// Canonical ordering for the reusable model-visible prefix. Keep this
    /// independent from producer registration order so concurrent contributors
    /// and reconstructed histories yield the same prompt layout.
    fn canonical_order(self) -> u8 {
        match self {
            Self::Repository => 0,
            Self::DeveloperInstructions => 1,
            Self::Collaboration => 2,
            Self::RootCoordinator => 3,
            Self::MultiAgent => 4,
            Self::MultiAgentUsageHint => 5,
            Self::TaskModelGuidance => 6,
            Self::TaskEvidence => 7,
            Self::Permissions => 8,
            Self::Memory => 9,
            Self::SkillUsage => 10,
            Self::SkillCatalog => 11,
            Self::ExtensionSkillCatalog => 12,
            Self::EnvironmentSkillCatalog => 13,
            Self::Apps => 14,
            Self::Plugins => 15,
            Self::Personality => 16,
            Self::SelectedSkill => 17,
            Self::AppContext => 18,
            Self::ModelSwitch => 19,
            Self::Environment => 20,
            Self::RecommendedPlugins => 21,
        }
    }

    /// Turn-scoped manifests and runtime observations remain attached to the
    /// turn that introduced them. This preserves an unchanged request prefix
    /// across turns while a replacement still appears immediately before its
    /// corresponding user input.
    fn is_volatile(self) -> bool {
        matches!(
            self,
            Self::SelectedSkill
                | Self::AppContext
                | Self::ModelSwitch
                | Self::Environment
                | Self::TaskEvidence
                | Self::RecommendedPlugins
        )
    }

    fn is_skill_catalog(self) -> bool {
        matches!(
            self,
            Self::SkillCatalog | Self::ExtensionSkillCatalog | Self::EnvironmentSkillCatalog
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct Occurrence {
    item_index: usize,
    content_index: usize,
    payload_content_index: Option<usize>,
    slot: StableContextSlot,
    explicitly_removed: bool,
}

impl Occurrence {
    fn text<'a>(&self, items: &'a [ResponseItem]) -> &'a str {
        let ResponseItem::Message { content, .. } = &items[self.item_index] else {
            unreachable!("stable context occurrences only reference messages");
        };
        let content_index = self.payload_content_index.unwrap_or(self.content_index);
        let ContentItem::InputText { text } = &content[content_index] else {
            unreachable!("stable context occurrences only reference input text");
        };
        text
    }

    fn turn_id<'a>(&self, items: &'a [ResponseItem]) -> Option<&'a str> {
        items[self.item_index].turn_id()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StableItemSignatureEntry {
    slot: StableContextSlot,
    role: String,
    payload: String,
}

fn stable_item_signature(item: &ResponseItem) -> Option<Vec<StableItemSignatureEntry>> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if !is_trusted_stable_context_item(item) {
        return None;
    }
    let mut signature = Vec::new();
    let mut content_index = 0;
    while let Some(content_item) = content.get(content_index) {
        let ContentItem::InputText { text } = content_item else {
            return None;
        };
        let classification = classify_stable_text(role, text)?;
        let (payload, consumed) = match classification.payload {
            StablePayload::Inline | StablePayload::Removed => (text.clone(), 1),
            StablePayload::FollowingText => {
                let Some(ContentItem::InputText { text }) = content.get(content_index + 1) else {
                    return None;
                };
                (text.clone(), 2)
            }
        };
        signature.push(StableItemSignatureEntry {
            slot: classification.slot,
            role: role.clone(),
            payload,
        });
        content_index += consumed;
    }
    (!signature.is_empty()).then_some(signature)
}

/// Removes an unchanged non-volatile stable injection that is already the
/// latest value in history. Volatile or ambiguous items remain turn-scoped.
pub(crate) fn filter_unchanged_stable_context_items(
    history: &[ResponseItem],
    candidates: Vec<ResponseItem>,
) -> Vec<ResponseItem> {
    let mut latest = HashMap::<StableContextSlot, StableItemSignatureEntry>::new();
    for item in history {
        if let Some(signature) = stable_item_signature(item) {
            for entry in signature {
                latest.insert(entry.slot, entry);
            }
        }
    }

    let mut retained = Vec::with_capacity(candidates.len());
    for item in candidates {
        let Some(signature) = stable_item_signature(&item) else {
            retained.push(item);
            continue;
        };
        let unchanged = signature
            .iter()
            .all(|entry| !entry.slot.is_volatile() && latest.get(&entry.slot) == Some(entry));
        if unchanged {
            continue;
        }
        for entry in signature {
            latest.insert(entry.slot, entry);
        }
        retained.push(item);
    }
    retained
}

pub(crate) fn project_stable_context(
    items: Arc<[ResponseItem]>,
    target: StableContextTarget,
) -> StableContextProjection {
    let fallback_items = Arc::clone(&items);
    let mut occurrences = Vec::new();
    let mut ambiguous = false;
    let mut latest_real_user = None;
    let mut user_insertion_by_turn = HashMap::<&str, usize>::new();

    for (item_index, item) in items.iter().enumerate() {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: _,
            ..
        } = item
        else {
            continue;
        };
        let trusted_stable_context = is_trusted_stable_context_item(item);
        let mut contains_stable = false;
        let mut contains_ordinary_user_content = false;
        let mut contains_unprojectable = false;
        let mut content_index = 0;
        while let Some(content_item) = content.get(content_index) {
            let ContentItem::InputText { text } = content_item else {
                contains_unprojectable = true;
                contains_ordinary_user_content = true;
                content_index += 1;
                continue;
            };
            if trusted_stable_context && let Some(classification) = classify_stable_text(role, text)
            {
                contains_stable = true;
                let payload_content_index = match classification.payload {
                    StablePayload::Inline | StablePayload::Removed => None,
                    StablePayload::FollowingText => {
                        let Some(ContentItem::InputText { .. }) = content.get(content_index + 1)
                        else {
                            ambiguous = true;
                            contains_ordinary_user_content = true;
                            content_index += 1;
                            continue;
                        };
                        Some(content_index + 1)
                    }
                };
                occurrences.push(Occurrence {
                    item_index,
                    content_index,
                    payload_content_index,
                    slot: classification.slot,
                    explicitly_removed: classification.payload == StablePayload::Removed,
                });
                content_index += 1 + usize::from(payload_content_index.is_some());
            } else if trusted_stable_context && contains_known_open_marker(text) {
                ambiguous = true;
                contains_ordinary_user_content = true;
                content_index += 1;
            } else {
                contains_ordinary_user_content = true;
                content_index += 1;
            }
        }
        // Splitting a registered fragment away from images or output text
        // would alter its model-visible structure. Ordinary assistant/image
        // history remains eligible for projection.
        if contains_stable && contains_unprojectable {
            ambiguous = true;
        }
        if role == "user" && contains_ordinary_user_content {
            latest_real_user = Some(item_index);
            if let Some(turn_id) = item.turn_id() {
                user_insertion_by_turn.entry(turn_id).or_insert(item_index);
            }
        }
    }

    let target_fail_open = target == StableContextTarget::FailOpen;
    let enabled = target == StableContextTarget::Sampling && !ambiguous;
    let fail_open = ambiguous || target_fail_open;
    let (projected, components) = if enabled {
        project_items(
            &items,
            &occurrences,
            latest_real_user,
            &user_insertion_by_turn,
        )
    } else {
        (
            items.to_vec(),
            analyze_unprojected(&items, &occurrences, fail_open),
        )
    };
    let projected: Arc<[ResponseItem]> = projected.into();
    let fallback_items = if enabled {
        Arc::clone(&projected)
    } else {
        fallback_items
    };
    StableContextProjection {
        items: projected,
        fallback_items,
        manifest: StableContextManifest::from_components(components, enabled, fail_open),
    }
}

fn project_items(
    items: &[ResponseItem],
    occurrences: &[Occurrence],
    latest_real_user: Option<usize>,
    user_insertion_by_turn: &HashMap<&str, usize>,
) -> (Vec<ResponseItem>, Vec<StableContextComponent>) {
    let selected_skill_indexes =
        current_selected_skill_indexes(items, occurrences, latest_real_user);
    let selected_skills_active = !selected_skill_indexes.is_empty();
    let mut latest_by_slot = HashMap::<StableContextSlot, usize>::new();
    for (index, occurrence) in occurrences.iter().enumerate() {
        if occurrence.slot != StableContextSlot::SelectedSkill {
            latest_by_slot.insert(occurrence.slot, index);
        }
    }

    let collaboration_removed = latest_by_slot
        .get(&StableContextSlot::Collaboration)
        .and_then(|index| occurrences.get(*index))
        .is_some_and(|occurrence| occurrence.text(items).contains(COLLABORATION_RESET_NOTICE));
    let repository_removed = latest_by_slot
        .get(&StableContextSlot::Repository)
        .and_then(|index| occurrences.get(*index))
        .is_some_and(|occurrence| occurrence.text(items).contains(REPOSITORY_REMOVAL_NOTICE));
    let developer_instructions_removed = latest_by_slot
        .get(&StableContextSlot::DeveloperInstructions)
        .and_then(|index| occurrences.get(*index))
        .is_some_and(|occurrence| occurrence.explicitly_removed);
    let multi_agent_usage_hint_removed = latest_by_slot
        .get(&StableContextSlot::MultiAgentUsageHint)
        .and_then(|index| occurrences.get(*index))
        .is_some_and(|occurrence| occurrence.explicitly_removed);
    let recommended_plugins_current = latest_by_slot
        .get(&StableContextSlot::RecommendedPlugins)
        .and_then(|index| occurrences.get(*index))
        .is_some_and(|occurrence| {
            latest_real_user.is_none()
                || occurrence_matches_latest_user_turn(items, occurrence, latest_real_user)
        });

    let compact_catalogs = if selected_skills_active {
        [
            StableContextSlot::SkillCatalog,
            StableContextSlot::ExtensionSkillCatalog,
            StableContextSlot::EnvironmentSkillCatalog,
        ]
        .into_iter()
        .filter_map(|slot| {
            latest_by_slot
                .get(&slot)
                .map(|occurrence_index| (slot, occurrence_index))
        })
        .map(|(slot, occurrence_index)| {
            (
                *occurrence_index,
                compact_skill_catalog_reference(slot, occurrences[*occurrence_index].text(items)),
            )
        })
        .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };

    let mut keep = HashSet::<(usize, usize)>::new();
    for (slot, occurrence_index) in &latest_by_slot {
        let occurrence = &occurrences[*occurrence_index];
        let should_keep = match slot {
            StableContextSlot::Repository => !repository_removed,
            StableContextSlot::Collaboration => !collaboration_removed,
            StableContextSlot::DeveloperInstructions => !developer_instructions_removed,
            StableContextSlot::MultiAgentUsageHint => !multi_agent_usage_hint_removed,
            StableContextSlot::SkillUsage => !selected_skills_active,
            slot if slot.is_skill_catalog() => true,
            StableContextSlot::RecommendedPlugins => recommended_plugins_current,
            _ => true,
        };
        if should_keep {
            keep.insert((occurrence.item_index, occurrence.content_index));
        }
    }
    for occurrence_index in &selected_skill_indexes {
        let occurrence = &occurrences[*occurrence_index];
        keep.insert((occurrence.item_index, occurrence.content_index));
    }

    let occurrence_content = occurrences
        .iter()
        .flat_map(|occurrence| {
            std::iter::once((occurrence.item_index, occurrence.content_index)).chain(
                occurrence
                    .payload_content_index
                    .map(|content_index| (occurrence.item_index, content_index)),
            )
        })
        .collect::<HashSet<_>>();
    let mut reusable = Vec::<(StableContextSlot, usize, ResponseItem)>::new();
    let mut volatile = Vec::<(StableContextSlot, usize, ResponseItem)>::new();
    for (occurrence_index, occurrence) in occurrences.iter().enumerate() {
        let key = (occurrence.item_index, occurrence.content_index);
        if !keep.contains(&key) {
            continue;
        }
        let Some(item) = items.get(occurrence.item_index) else {
            continue;
        };
        let text = compact_catalogs
            .get(&occurrence_index)
            .map_or_else(|| occurrence.text(items).to_string(), Clone::clone);
        let Some(projected_item) =
            projected_message(item, false, vec![ContentItem::InputText { text }])
        else {
            continue;
        };
        let target = if occurrence.slot.is_volatile() {
            &mut volatile
        } else {
            &mut reusable
        };
        target.push((occurrence.slot, occurrence_index, projected_item));
    }
    let sort_fragments = |fragments: &mut Vec<(StableContextSlot, usize, ResponseItem)>| {
        fragments
            .sort_by_key(|(slot, occurrence_index, _)| (slot.canonical_order(), *occurrence_index));
    };
    sort_fragments(&mut reusable);
    sort_fragments(&mut volatile);

    let mut ordinary = Vec::<(usize, ResponseItem)>::with_capacity(items.len());
    for (item_index, item) in items.iter().enumerate() {
        let ResponseItem::Message { content, .. } = item else {
            ordinary.push((item_index, item.clone()));
            continue;
        };
        let mut next_content = Vec::with_capacity(content.len());
        for (content_index, content_item) in content.iter().enumerate() {
            let key = (item_index, content_index);
            if occurrence_content.contains(&key) {
                continue;
            }
            next_content.push(content_item.clone());
        }
        if next_content.is_empty() {
            continue;
        }
        if let Some(next_item) = projected_message(item, true, next_content) {
            ordinary.push((item_index, next_item));
        }
    }

    let mut projected = Vec::with_capacity(items.len() + reusable.len() + volatile.len());
    projected.extend(reusable.into_iter().map(|(_, _, item)| item));
    let mut volatile_by_item = HashMap::<usize, Vec<ResponseItem>>::new();
    for (_, occurrence_index, item) in volatile {
        let occurrence = &occurrences[occurrence_index];
        let item_index = volatile_user_insertion_index(
            occurrence,
            items,
            latest_real_user,
            user_insertion_by_turn,
        )
        .unwrap_or(occurrence.item_index);
        volatile_by_item.entry(item_index).or_default().push(item);
    }
    let mut ordinary = ordinary.into_iter().peekable();
    for item_index in 0..items.len() {
        if let Some(items) = volatile_by_item.remove(&item_index) {
            projected.extend(items);
        }
        while ordinary
            .peek()
            .is_some_and(|(ordinary_index, _)| *ordinary_index == item_index)
        {
            let Some((_, item)) = ordinary.next() else {
                break;
            };
            projected.push(item);
        }
    }

    let mut components = Vec::new();
    for (slot, latest_index) in latest_by_slot {
        let occurrence = &occurrences[latest_index];
        let (prior_count, replaced) =
            prior_occurrence_summary(items, occurrences, latest_index, slot);
        let removed = (slot == StableContextSlot::Repository && repository_removed)
            || (slot == StableContextSlot::Collaboration && collaboration_removed)
            || (slot == StableContextSlot::DeveloperInstructions && developer_instructions_removed)
            || (slot == StableContextSlot::MultiAgentUsageHint && multi_agent_usage_hint_removed);
        let gated = (matches!(slot, StableContextSlot::SkillUsage) && selected_skills_active)
            || (slot == StableContextSlot::RecommendedPlugins && !recommended_plugins_current);
        let text = compact_catalogs
            .get(&latest_index)
            .map(String::as_str)
            .unwrap_or_else(|| occurrence.text(items));
        let mut component = component_from_text(
            slot.kind(),
            slot.semantic_key(),
            text,
            !removed && !gated,
            if removed {
                StableContextDisposition::Removed
            } else if gated || (slot.is_skill_catalog() && selected_skills_active) {
                StableContextDisposition::Gated
            } else if replaced || prior_count > 1 {
                StableContextDisposition::Replaced
            } else {
                StableContextDisposition::Unchanged
            },
        );
        component.identity.semantic_id = semantic_id(
            slot.kind(),
            &[
                slot.semantic_key().as_bytes(),
                &component.identity.content_hash,
            ],
        );
        components.push(component);
    }
    for occurrence_index in selected_skill_indexes {
        let occurrence = &occurrences[occurrence_index];
        let mut component = component_from_text(
            StableContextKind::SelectedSkill,
            "selected_skill",
            occurrence.text(items),
            true,
            StableContextDisposition::Unchanged,
        );
        component.identity.semantic_id = semantic_id(
            StableContextKind::SelectedSkill,
            &[
                occurrence.turn_id(items).unwrap_or_default().as_bytes(),
                &component.identity.content_hash,
            ],
        );
        components.push(component);
    }
    (projected, components)
}

fn projected_message(
    item: &ResponseItem,
    preserve_id: bool,
    content: Vec<ContentItem>,
) -> Option<ResponseItem> {
    let ResponseItem::Message {
        id,
        role,
        phase,
        internal_chat_message_metadata_passthrough,
        ..
    } = item
    else {
        return None;
    };
    Some(ResponseItem::Message {
        id: if preserve_id { id.clone() } else { None },
        role: role.clone(),
        content,
        phase: phase.clone(),
        internal_chat_message_metadata_passthrough: internal_chat_message_metadata_passthrough
            .clone(),
    })
}

fn prior_occurrence_summary(
    items: &[ResponseItem],
    occurrences: &[Occurrence],
    latest_index: usize,
    slot: StableContextSlot,
) -> (usize, bool) {
    let latest_text = occurrences[latest_index].text(items);
    occurrences[..latest_index]
        .iter()
        .filter(|candidate| candidate.slot == slot)
        .fold((0, false), |(prior_count, replaced), candidate| {
            (
                prior_count + 1,
                replaced || candidate.text(items) != latest_text,
            )
        })
}

fn current_selected_skill_indexes(
    items: &[ResponseItem],
    occurrences: &[Occurrence],
    latest_real_user: Option<usize>,
) -> Vec<usize> {
    let Some(user_index) = latest_real_user else {
        return Vec::new();
    };
    let user_turn_id = items[user_index].turn_id();
    occurrences
        .iter()
        .enumerate()
        .filter(|(_, occurrence)| occurrence.slot == StableContextSlot::SelectedSkill)
        .filter(|(_, occurrence)| match user_turn_id {
            Some(turn_id) => occurrence.turn_id(items) == Some(turn_id),
            None => occurrence.item_index > user_index,
        })
        .map(|(index, _)| index)
        .collect()
}

fn occurrence_matches_latest_user_turn(
    items: &[ResponseItem],
    occurrence: &Occurrence,
    latest_real_user: Option<usize>,
) -> bool {
    let Some(latest_real_user) = latest_real_user else {
        return false;
    };
    match items[latest_real_user].turn_id() {
        Some(turn_id) => occurrence.turn_id(items) == Some(turn_id),
        // Legacy histories without turn metadata cannot establish expiry safely.
        None => true,
    }
}

fn volatile_user_insertion_index(
    occurrence: &Occurrence,
    items: &[ResponseItem],
    latest_real_user: Option<usize>,
    user_insertion_by_turn: &HashMap<&str, usize>,
) -> Option<usize> {
    if let Some(turn_id) = occurrence.turn_id(items)
        && let Some(item_index) = user_insertion_by_turn.get(turn_id)
    {
        return Some(*item_index);
    }

    latest_real_user
}

fn analyze_unprojected(
    items: &[ResponseItem],
    occurrences: &[Occurrence],
    retained_fallback: bool,
) -> Vec<StableContextComponent> {
    occurrences
        .iter()
        .map(|occurrence| {
            component_from_text(
                occurrence.slot.kind(),
                occurrence.slot.semantic_key(),
                occurrence.text(items),
                true,
                if retained_fallback {
                    StableContextDisposition::RetainedFallback
                } else {
                    StableContextDisposition::Unchanged
                },
            )
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StablePayload {
    Inline,
    FollowingText,
    Removed,
}

#[derive(Clone, Copy, Debug)]
struct StableTextClassification {
    slot: StableContextSlot,
    payload: StablePayload,
}

impl StableTextClassification {
    const fn inline(slot: StableContextSlot) -> Self {
        Self {
            slot,
            payload: StablePayload::Inline,
        }
    }
}

fn classify_stable_text(role: &str, text: &str) -> Option<StableTextClassification> {
    #[cfg(test)]
    CLASSIFY_STABLE_TEXT_CALLS.with(|calls| calls.set(calls.get() + 1));

    if role == "user" && marked(text, REPOSITORY_OPEN_TAG, REPOSITORY_CLOSE_TAG) {
        return Some(StableTextClassification::inline(
            StableContextSlot::Repository,
        ));
    }
    if matches!(role, "system" | "developer" | "user") && marked(text, SKILL_OPEN_TAG, "</skill>") {
        return Some(StableTextClassification::inline(
            StableContextSlot::SelectedSkill,
        ));
    }
    if role == "user" && marked(text, "<environment_context>", "</environment_context>") {
        return Some(StableTextClassification::inline(
            StableContextSlot::Environment,
        ));
    }
    if role == "user" && marked(text, "<task_model_guidance>", "</task_model_guidance>") {
        return Some(StableTextClassification::inline(
            StableContextSlot::TaskModelGuidance,
        ));
    }
    if role == "user" && marked(text, "<kd4_task_state_v1>", "</kd4_task_state_v1>") {
        return Some(StableTextClassification::inline(
            StableContextSlot::TaskEvidence,
        ));
    }
    if role == "user" && marked(text, "<recommended_plugins>", "</recommended_plugins>") {
        return Some(StableTextClassification::inline(
            StableContextSlot::RecommendedPlugins,
        ));
    }
    if role != "developer" {
        return None;
    }
    let trimmed = text.trim();
    if trimmed == DEVELOPER_INSTRUCTIONS_PRESENT_MARKER {
        return Some(StableTextClassification {
            slot: StableContextSlot::DeveloperInstructions,
            payload: StablePayload::FollowingText,
        });
    }
    if trimmed == DEVELOPER_INSTRUCTIONS_REMOVED_MARKER {
        return Some(StableTextClassification {
            slot: StableContextSlot::DeveloperInstructions,
            payload: StablePayload::Removed,
        });
    }
    if trimmed == MULTI_AGENT_USAGE_HINT_PRESENT_MARKER {
        return Some(StableTextClassification {
            slot: StableContextSlot::MultiAgentUsageHint,
            payload: StablePayload::FollowingText,
        });
    }
    if trimmed == MULTI_AGENT_USAGE_HINT_REMOVED_MARKER {
        return Some(StableTextClassification {
            slot: StableContextSlot::MultiAgentUsageHint,
            payload: StablePayload::Removed,
        });
    }
    if text.trim_start().starts_with(ROOT_COORDINATOR_PREFIX) {
        return Some(StableTextClassification::inline(
            StableContextSlot::RootCoordinator,
        ));
    }
    [
        (
            ROOT_ORCHESTRATION_OPEN_TAG,
            ROOT_ORCHESTRATION_CLOSE_TAG,
            StableContextSlot::RootCoordinator,
        ),
        (
            COLLABORATION_MODE_OPEN_TAG,
            "</collaboration_mode>",
            StableContextSlot::Collaboration,
        ),
        (
            SKILLS_USAGE_OPEN_TAG,
            "</skills_usage_instructions>",
            StableContextSlot::SkillUsage,
        ),
        (
            SKILLS_INSTRUCTIONS_OPEN_TAG,
            "</skills_instructions>",
            StableContextSlot::SkillCatalog,
        ),
        (
            EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG,
            EXTENSION_SKILLS_INSTRUCTIONS_CLOSE_TAG,
            StableContextSlot::ExtensionSkillCatalog,
        ),
        (
            ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG,
            ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG,
            StableContextSlot::EnvironmentSkillCatalog,
        ),
        (
            APPS_INSTRUCTIONS_OPEN_TAG,
            "</apps_instructions>",
            StableContextSlot::Apps,
        ),
        (
            PLUGINS_INSTRUCTIONS_OPEN_TAG,
            "</plugins_instructions>",
            StableContextSlot::Plugins,
        ),
        (
            "<permissions instructions>",
            "</permissions instructions>",
            StableContextSlot::Permissions,
        ),
        (
            "<memory_context>",
            "</memory_context>",
            StableContextSlot::Memory,
        ),
        (
            MULTI_AGENT_MODE_OPEN_TAG,
            "</multi_agent_mode>",
            StableContextSlot::MultiAgent,
        ),
        (
            "<app-context>",
            "</app-context>",
            StableContextSlot::AppContext,
        ),
        (
            "<model_switch>",
            "</model_switch>",
            StableContextSlot::ModelSwitch,
        ),
        (
            "<personality_spec>",
            "</personality_spec>",
            StableContextSlot::Personality,
        ),
    ]
    .into_iter()
    .find_map(|(open, close, slot)| {
        marked(text, open, close).then_some(StableTextClassification::inline(slot))
    })
}

fn contains_known_open_marker(text: &str) -> bool {
    [
        REPOSITORY_OPEN_TAG,
        ROOT_ORCHESTRATION_OPEN_TAG,
        COLLABORATION_MODE_OPEN_TAG,
        SKILLS_USAGE_OPEN_TAG,
        SKILLS_INSTRUCTIONS_OPEN_TAG,
        EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG,
        ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG,
        SKILL_OPEN_TAG,
        "<environment_context>",
        "<task_model_guidance>",
        "<kd4_task_state_v1>",
        "<recommended_plugins>",
        APPS_INSTRUCTIONS_OPEN_TAG,
        "<app-context>",
        PLUGINS_INSTRUCTIONS_OPEN_TAG,
        "<permissions instructions>",
        "<memory_context>",
        MULTI_AGENT_MODE_OPEN_TAG,
        "<configured_developer_instructions",
        "<multi_agent_usage_hint",
        "<model_switch>",
        "<personality_spec>",
    ]
    .iter()
    .any(|marker| text.trim_start().starts_with(marker))
}

fn stable_identity_sections(
    text: Option<&str>,
    present_marker: &str,
    removed_marker: &str,
) -> Vec<String> {
    match text.filter(|text| !text.is_empty()) {
        Some(text) => vec![present_marker.to_string(), text.to_string()],
        None => vec![removed_marker.to_string()],
    }
}

pub(crate) fn configured_developer_instructions_sections(text: Option<&str>) -> Vec<String> {
    stable_identity_sections(
        text,
        DEVELOPER_INSTRUCTIONS_PRESENT_MARKER,
        DEVELOPER_INSTRUCTIONS_REMOVED_MARKER,
    )
}

pub(crate) fn multi_agent_usage_hint_sections(text: Option<&str>) -> Vec<String> {
    stable_identity_sections(
        text.map(|text| {
            format!(
                "{text}\n\nTool availability does not authorize spawning agents. The active <multi_agent_mode> and its applicable instructions govern whether delegation is allowed."
            )
        })
        .as_deref(),
        MULTI_AGENT_USAGE_HINT_PRESENT_MARKER,
        MULTI_AGENT_USAGE_HINT_REMOVED_MARKER,
    )
}

#[cfg(test)]
pub(crate) fn multi_agent_usage_hint_payload(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    let [
        ContentItem::InputText { text: marker },
        ContentItem::InputText { text },
    ] = content.as_slice()
    else {
        return None;
    };
    (role == "developer" && marker.trim() == MULTI_AGENT_USAGE_HINT_PRESENT_MARKER)
        .then_some(text.as_str())
}

pub(crate) fn is_multi_agent_usage_hint_item(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    let Some(ContentItem::InputText { text }) = content.first() else {
        return false;
    };
    role == "developer"
        && matches!(
            text.trim(),
            MULTI_AGENT_USAGE_HINT_PRESENT_MARKER | MULTI_AGENT_USAGE_HINT_REMOVED_MARKER
        )
}

fn marked(text: &str, open: &str, close: &str) -> bool {
    let text = text.trim();
    text.starts_with(open) && text.ends_with(close)
}

fn compact_skill_catalog_reference(slot: StableContextSlot, catalog: &str) -> String {
    #[cfg(test)]
    COMPACT_CATALOG_CALLS.with(|calls| calls.set(calls.get() + 1));

    let digest: [u8; 32] = Sha256::digest(catalog.as_bytes()).into();
    let (open_tag, close_tag) = match slot {
        StableContextSlot::SkillCatalog => {
            (SKILLS_INSTRUCTIONS_OPEN_TAG, SKILLS_INSTRUCTIONS_CLOSE_TAG)
        }
        StableContextSlot::ExtensionSkillCatalog => (
            EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG,
            EXTENSION_SKILLS_INSTRUCTIONS_CLOSE_TAG,
        ),
        StableContextSlot::EnvironmentSkillCatalog => (
            ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG,
            ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG,
        ),
        _ => unreachable!("only skill catalog slots can be compacted"),
    };
    format!(
        "{open_tag}\n<active_catalog version=\"v1\" sha256=\"{}\" state=\"selected\" />\nThe full catalog is inactive while explicitly selected skill instructions are active. It will be restored for a later capability-selection turn.\n{close_tag}",
        short_hash(&digest)
    )
}

fn component_from_text(
    kind: StableContextKind,
    semantic_key: &str,
    text: &str,
    active: bool,
    disposition: StableContextDisposition,
) -> StableContextComponent {
    component_from_bytes(kind, semantic_key, text.as_bytes(), active, disposition)
}

fn base_component(model_slug: &str, base_instructions: &str) -> StableContextComponent {
    let mut component = component_from_text(
        StableContextKind::BaseModel,
        "base_model",
        base_instructions,
        true,
        StableContextDisposition::Unchanged,
    );
    component.identity.semantic_id = semantic_id(
        StableContextKind::BaseModel,
        &[model_slug.as_bytes(), &component.identity.content_hash],
    );
    component
}

fn component_from_bytes(
    kind: StableContextKind,
    semantic_key: &str,
    bytes: &[u8],
    active: bool,
    disposition: StableContextDisposition,
) -> StableContextComponent {
    let content_hash: [u8; 32] = Sha256::digest(bytes).into();
    StableContextComponent {
        kind,
        identity: StableContextIdentity {
            contract_version: STABLE_CONTEXT_CONTRACT_VERSION,
            semantic_id: semantic_id(kind, &[semantic_key.as_bytes(), &content_hash]),
            content_hash,
            serialized_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            approx_tokens: i64::try_from(approx_token_count(
                std::str::from_utf8(bytes).unwrap_or_default(),
            ))
            .unwrap_or(i64::MAX),
        },
        active,
        disposition,
        local_reused: false,
    }
}

fn semantic_id(kind: StableContextKind, parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(STABLE_CONTEXT_HASH_DOMAIN);
    hasher.update(STABLE_CONTEXT_CONTRACT_VERSION.to_be_bytes());
    hasher.update(kind.as_str().as_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    format!("{}:v1:{}", kind.as_str(), short_hash(&digest))
}

fn manifest_fingerprint(
    components: &[StableContextComponent],
    projection_enabled: bool,
    fail_open: bool,
) -> [u8; 32] {
    #[cfg(test)]
    MANIFEST_FINGERPRINT_CALLS.with(|calls| calls.set(calls.get() + 1));

    let mut hasher = Sha256::new();
    hasher.update(STABLE_CONTEXT_MANIFEST_HASH_DOMAIN);
    hasher.update([u8::from(projection_enabled), u8::from(fail_open)]);
    for component in components {
        if component.kind == StableContextKind::DynamicHistory {
            continue;
        }
        hasher.update(component.kind.as_str().as_bytes());
        hasher.update(component.identity.semantic_id.as_bytes());
        hasher.update(component.identity.content_hash);
        hasher.update([u8::from(component.active)]);
    }
    hasher.finalize().into()
}

pub(crate) fn short_hash(hash: &[u8; 32]) -> String {
    hash[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests_optimization {
    use super::*;
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::MessagePhase;

    fn text_message(role: &str, text: &str) -> ResponseItem {
        let mut item = ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        mark_trusted_stable_context_item(&mut item);
        item
    }

    fn text_message_for_turn(role: &str, text: &str, turn_id: &str) -> ResponseItem {
        let mut item = text_message(role, text);
        item.set_turn_id_if_missing(turn_id);
        item
    }

    fn skill(name: &str) -> String {
        format!("<skill>\n<name>{name}</name>\n<body>{name} body</body>\n</skill>")
    }

    fn content_text(content: &ContentItem) -> Option<&str> {
        match content {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text),
            _ => None,
        }
    }

    #[test]
    fn unchanged_nonvolatile_injection_is_reused_without_history_growth() {
        let guidance = text_message(
            "user",
            "<task_model_guidance>\nkeep the task focused\n</task_model_guidance>",
        );

        let retained = filter_unchanged_stable_context_items(
            std::slice::from_ref(&guidance),
            vec![guidance.clone()],
        );

        assert!(retained.is_empty());
    }

    #[test]
    fn wrapped_root_orchestration_is_classified_and_reused() {
        let old = text_message(
            "developer",
            "<root_orchestration_instructions>old orchestration</root_orchestration_instructions>",
        );
        let current = text_message(
            "developer",
            "<root_orchestration_instructions>current orchestration</root_orchestration_instructions>",
        );

        let projected = project_stable_context(
            vec![old, current.clone()].into(),
            StableContextTarget::Sampling,
        );
        let root_items = projected
            .manifest
            .components
            .iter()
            .filter(|component| component.kind == StableContextKind::RootCoordinator)
            .count();

        assert_eq!(root_items, 1);
        assert_eq!(projected.items.as_ref(), &[current]);
    }

    #[test]
    fn changed_and_volatile_injections_remain_turn_scoped() {
        let old_guidance = text_message(
            "user",
            "<task_model_guidance>\nold guidance\n</task_model_guidance>",
        );
        let new_guidance = text_message(
            "user",
            "<task_model_guidance>\nnew guidance\n</task_model_guidance>",
        );
        let selected_skill = text_message("user", &skill("one"));

        assert_eq!(
            filter_unchanged_stable_context_items(&[old_guidance], vec![new_guidance.clone()]),
            vec![new_guidance]
        );
        assert_eq!(
            filter_unchanged_stable_context_items(
                std::slice::from_ref(&selected_skill),
                vec![selected_skill.clone()]
            ),
            vec![selected_skill]
        );
    }

    #[test]
    fn local_reuse_preserves_order_and_reuses_fingerprint() {
        let manifest = StableContextManifest::from_components(
            vec![
                component_from_text(
                    StableContextKind::Repository,
                    "repository",
                    "repository",
                    true,
                    StableContextDisposition::Unchanged,
                ),
                component_from_text(
                    StableContextKind::Collaboration,
                    "collaboration",
                    "collaboration",
                    true,
                    StableContextDisposition::Unchanged,
                ),
            ],
            true,
            false,
        );
        MANIFEST_FINGERPRINT_CALLS.with(|calls| calls.set(0));

        let reused = manifest.with_local_reused(true);

        assert_eq!(
            MANIFEST_FINGERPRINT_CALLS.with(Cell::get),
            0,
            "local reuse does not affect ordering or the manifest fingerprint"
        );
        assert_eq!(reused.fingerprint(), manifest.fingerprint());
        assert_eq!(reused.components().len(), manifest.components().len());
        for (reused, original) in reused.components().iter().zip(manifest.components()) {
            assert_eq!(reused.kind, original.kind);
            assert_eq!(reused.identity, original.identity);
            assert_eq!(reused.active, original.active);
            assert_eq!(reused.disposition, original.disposition);
            assert!(reused.local_reused);
        }
    }

    #[test]
    fn occurrence_records_only_source_indexes_and_slot() {
        assert_eq!(
            std::mem::size_of::<Occurrence>(),
            std::mem::size_of::<usize>() * 3
        );
    }

    #[test]
    fn projected_messages_clone_the_shell_and_selected_content_only() {
        let id = ResponseItemId::with_suffix("msg", "stable-context");
        let mut source = ResponseItem::Message {
            id: Some(id.clone()),
            role: "developer".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "first".to_string(),
                },
                ContentItem::InputText {
                    text: "second".to_string(),
                },
            ],
            phase: Some(MessagePhase::Commentary),
            internal_chat_message_metadata_passthrough: None,
        };
        source.set_turn_id_if_missing("turn-1");

        let stable = projected_message(
            &source,
            false,
            vec![ContentItem::InputText {
                text: "second".to_string(),
            }],
        )
        .expect("message shell");
        let ordinary = projected_message(
            &source,
            true,
            vec![ContentItem::InputText {
                text: "first".to_string(),
            }],
        )
        .expect("message shell");

        assert_eq!(stable.turn_id(), Some("turn-1"));
        assert_eq!(ordinary.turn_id(), Some("turn-1"));

        let ResponseItem::Message {
            id: stable_id,
            role: stable_role,
            content: stable_content,
            phase: stable_phase,
            ..
        } = stable
        else {
            panic!("projected stable fragment must remain a message");
        };
        assert_eq!(stable_id, None);
        assert_eq!(stable_role, "developer");
        assert_eq!(stable_phase, Some(MessagePhase::Commentary));
        assert_eq!(stable_content.len(), 1);
        assert_eq!(content_text(&stable_content[0]), Some("second"));

        let ResponseItem::Message {
            id: ordinary_id,
            role: ordinary_role,
            content: ordinary_content,
            phase: ordinary_phase,
            ..
        } = ordinary
        else {
            panic!("projected ordinary content must remain a message");
        };
        assert_eq!(ordinary_id, Some(id));
        assert_eq!(ordinary_role, "developer");
        assert_eq!(ordinary_phase, Some(MessagePhase::Commentary));
        assert_eq!(ordinary_content.len(), 1);
        assert_eq!(content_text(&ordinary_content[0]), Some("first"));
    }

    #[test]
    fn selected_skill_compacts_the_catalog_once() {
        let catalog = "<skills_instructions>\nfull catalog\n</skills_instructions>";
        let selected = skill("one");
        COMPACT_CATALOG_CALLS.with(|calls| calls.set(0));

        let projection = project_stable_context(
            vec![
                text_message("developer", catalog),
                text_message("user", "use one"),
                text_message("user", &selected),
            ]
            .into(),
            StableContextTarget::Sampling,
        );

        assert_eq!(COMPACT_CATALOG_CALLS.with(Cell::get), 1);
        assert!(projection.manifest.components().iter().any(|component| {
            component.kind == StableContextKind::SkillCatalog
                && component.disposition == StableContextDisposition::Gated
        }));
    }

    #[test]
    fn volatile_context_uses_the_preindexed_user_insertion() {
        let items = vec![
            text_message_for_turn(
                "user",
                "<environment_context>\nenvironment\n</environment_context>",
                "turn-1",
            ),
            text_message_for_turn("developer", "<app-context>\napp\n</app-context>", "turn-1"),
            text_message_for_turn(
                "developer",
                "<model_switch>\nmodel\n</model_switch>",
                "turn-1",
            ),
            text_message_for_turn(
                "user",
                "<recommended_plugins>\nplugins\n</recommended_plugins>",
                "turn-1",
            ),
            text_message_for_turn("user", "do the work", "turn-1"),
        ];
        CLASSIFY_STABLE_TEXT_CALLS.with(|calls| calls.set(0));

        let projection = project_stable_context(items.into(), StableContextTarget::Sampling);

        assert_eq!(
            CLASSIFY_STABLE_TEXT_CALLS.with(Cell::get),
            5,
            "projection classifies each source fragment exactly once"
        );
        let prompt_index = projection
            .items
            .iter()
            .position(|item| {
                matches!(
                    item,
                    ResponseItem::Message { content, .. }
                        if content.iter().any(|content| content_text(content) == Some("do the work"))
                )
            })
            .expect("ordinary user prompt");
        assert_eq!(prompt_index, 4);
    }

    #[test]
    fn prior_occurrence_summary_folds_count_and_replacement() {
        let old = "# AGENTS.md instructions\n<INSTRUCTIONS>old</INSTRUCTIONS>";
        let current = "# AGENTS.md instructions\n<INSTRUCTIONS>current</INSTRUCTIONS>";
        let items = vec![
            text_message("user", old),
            text_message("user", old),
            text_message("user", current),
        ];
        let occurrences = vec![
            Occurrence {
                item_index: 0,
                content_index: 0,
                payload_content_index: None,
                slot: StableContextSlot::Repository,
                explicitly_removed: false,
            },
            Occurrence {
                item_index: 1,
                content_index: 0,
                payload_content_index: None,
                slot: StableContextSlot::Repository,
                explicitly_removed: false,
            },
            Occurrence {
                item_index: 2,
                content_index: 0,
                payload_content_index: None,
                slot: StableContextSlot::Repository,
                explicitly_removed: false,
            },
        ];

        assert_eq!(
            prior_occurrence_summary(&items, &occurrences, 1, StableContextSlot::Repository),
            (1, false)
        );
        assert_eq!(
            prior_occurrence_summary(&items, &occurrences, 2, StableContextSlot::Repository),
            (2, true)
        );
    }
}

#[cfg(test)]
#[path = "stable_context_tests.rs"]
mod tests;
