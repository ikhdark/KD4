use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::APPS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::COLLABORATION_MODE_OPEN_TAG;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::PLUGINS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_utils_output_truncation::approx_token_count;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

const STABLE_CONTEXT_CONTRACT_VERSION: u16 = 1;
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
    RootCoordinator,
    MultiAgent,
    TaskModelGuidance,
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
            Self::RootCoordinator => "root_coordinator",
            Self::MultiAgent => "multi_agent",
            Self::TaskModelGuidance => "task_model_guidance",
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
        Self::from_components(components, self.projection_enabled, self.fail_open)
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
    SelectedSkill,
    Apps,
    AppContext,
    Plugins,
    RecommendedPlugins,
    Environment,
    TaskModelGuidance,
    Permissions,
    Memory,
    ModelSwitch,
    Personality,
    MultiAgent,
    RootCoordinator,
}

impl StableContextSlot {
    fn kind(self) -> StableContextKind {
        match self {
            Self::Repository => StableContextKind::Repository,
            Self::Collaboration => StableContextKind::Collaboration,
            Self::SkillUsage => StableContextKind::SkillUsage,
            Self::SkillCatalog => StableContextKind::SkillCatalog,
            Self::SelectedSkill => StableContextKind::SelectedSkill,
            Self::Apps => StableContextKind::DesktopApp,
            Self::AppContext => StableContextKind::AppContext,
            Self::Plugins => StableContextKind::Plugins,
            Self::RecommendedPlugins => StableContextKind::RecommendedPlugins,
            Self::Environment => StableContextKind::Environment,
            Self::TaskModelGuidance => StableContextKind::TaskModelGuidance,
            Self::Permissions => StableContextKind::EnvironmentPermissions,
            Self::Memory => StableContextKind::Memory,
            Self::ModelSwitch => StableContextKind::ModelSwitch,
            Self::Personality => StableContextKind::Personality,
            Self::MultiAgent => StableContextKind::MultiAgent,
            Self::RootCoordinator => StableContextKind::RootCoordinator,
        }
    }

    fn semantic_key(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Collaboration => "collaboration",
            Self::SkillUsage => "skill_usage",
            Self::SkillCatalog => "skill_catalog",
            Self::SelectedSkill => "selected_skill",
            Self::Apps => "apps",
            Self::AppContext => "app_context",
            Self::Plugins => "plugins",
            Self::RecommendedPlugins => "recommended_plugins",
            Self::Environment => "environment",
            Self::TaskModelGuidance => "task_model_guidance",
            Self::Permissions => "environment_permissions",
            Self::Memory => "memory",
            Self::ModelSwitch => "model_switch",
            Self::Personality => "personality",
            Self::MultiAgent => "multi_agent",
            Self::RootCoordinator => "root_coordinator",
        }
    }

    /// Canonical ordering for the reusable model-visible prefix. Keep this
    /// independent from producer registration order so concurrent contributors
    /// and reconstructed histories yield the same prompt layout.
    fn canonical_order(self) -> u8 {
        match self {
            Self::Repository => 0,
            Self::Collaboration => 1,
            Self::RootCoordinator => 2,
            Self::MultiAgent => 3,
            Self::TaskModelGuidance => 4,
            Self::Permissions => 5,
            Self::Memory => 6,
            Self::SkillUsage => 7,
            Self::SkillCatalog => 8,
            Self::Apps => 9,
            Self::Plugins => 10,
            Self::Personality => 11,
            Self::SelectedSkill => 12,
            Self::AppContext => 13,
            Self::ModelSwitch => 14,
            Self::Environment => 15,
            Self::RecommendedPlugins => 16,
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
                | Self::RecommendedPlugins
        )
    }
}

#[derive(Clone, Debug)]
struct Occurrence {
    item_index: usize,
    content_index: usize,
    slot: StableContextSlot,
    text: String,
    turn_id: Option<String>,
}

pub(crate) fn project_stable_context(
    items: Arc<[ResponseItem]>,
    target: StableContextTarget,
) -> StableContextProjection {
    let fallback_items = Arc::clone(&items);
    let mut occurrences = Vec::new();
    let mut ambiguous = false;
    let mut latest_real_user: Option<(usize, Option<String>)> = None;

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
        let mut contains_stable = false;
        let mut contains_unprojectable = false;
        for (content_index, content_item) in content.iter().enumerate() {
            let ContentItem::InputText { text } = content_item else {
                contains_unprojectable = true;
                continue;
            };
            if let Some(slot) = classify_stable_text(role, text) {
                contains_stable = true;
                occurrences.push(Occurrence {
                    item_index,
                    content_index,
                    slot,
                    text: text.clone(),
                    turn_id: item.turn_id().map(str::to_string),
                });
            } else if contains_known_open_marker(text) {
                ambiguous = true;
            }
        }
        // Splitting a registered fragment away from images or output text
        // would alter its model-visible structure. Ordinary assistant/image
        // history remains eligible for projection.
        if contains_stable && contains_unprojectable {
            ambiguous = true;
        }
        if role == "user" && !contains_stable {
            latest_real_user = Some((item_index, item.turn_id().map(str::to_string)));
        }
    }

    let target_fail_open = target == StableContextTarget::FailOpen;
    let enabled = target == StableContextTarget::Sampling && !ambiguous;
    let fail_open = ambiguous || target_fail_open;
    let (projected, components) = if enabled {
        project_items(&items, &occurrences, latest_real_user.as_ref())
    } else {
        (items.to_vec(), analyze_unprojected(&occurrences, fail_open))
    };
    StableContextProjection {
        items: projected.into(),
        fallback_items,
        manifest: StableContextManifest::from_components(components, enabled, fail_open),
    }
}

fn project_items(
    items: &[ResponseItem],
    occurrences: &[Occurrence],
    latest_real_user: Option<&(usize, Option<String>)>,
) -> (Vec<ResponseItem>, Vec<StableContextComponent>) {
    let selected_skill_indexes = current_selected_skill_indexes(occurrences, latest_real_user);
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
        .is_some_and(|occurrence| occurrence.text.contains(COLLABORATION_RESET_NOTICE));
    let repository_removed = latest_by_slot
        .get(&StableContextSlot::Repository)
        .and_then(|index| occurrences.get(*index))
        .is_some_and(|occurrence| occurrence.text.contains(REPOSITORY_REMOVAL_NOTICE));
    let recommended_plugins_current = latest_by_slot
        .get(&StableContextSlot::RecommendedPlugins)
        .and_then(|index| occurrences.get(*index))
        .is_some_and(|occurrence| {
            latest_real_user.is_none()
                || occurrence_matches_latest_user_turn(occurrence, latest_real_user)
        });

    let mut keep = HashSet::<(usize, usize)>::new();
    let mut replacement_catalog = HashMap::<(usize, usize), String>::new();
    for (slot, occurrence_index) in &latest_by_slot {
        let occurrence = &occurrences[*occurrence_index];
        let should_keep = match slot {
            StableContextSlot::Repository => !repository_removed,
            StableContextSlot::Collaboration => !collaboration_removed,
            StableContextSlot::SkillUsage => !selected_skills_active,
            StableContextSlot::SkillCatalog => true,
            StableContextSlot::RecommendedPlugins => recommended_plugins_current,
            _ => true,
        };
        if should_keep {
            keep.insert((occurrence.item_index, occurrence.content_index));
        }
        if *slot == StableContextSlot::SkillCatalog && selected_skills_active {
            replacement_catalog.insert(
                (occurrence.item_index, occurrence.content_index),
                compact_skill_catalog_reference(&occurrence.text),
            );
        }
    }
    for occurrence_index in &selected_skill_indexes {
        let occurrence = &occurrences[*occurrence_index];
        keep.insert((occurrence.item_index, occurrence.content_index));
    }

    let occurrence_lookup = occurrences
        .iter()
        .map(|occurrence| {
            (
                (occurrence.item_index, occurrence.content_index),
                occurrence,
            )
        })
        .collect::<HashMap<_, _>>();
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
        let mut projected_item = item.clone();
        // A mixed producer message can yield multiple canonical fragments.
        // Do not duplicate a provider item ID across the split messages.
        projected_item.set_id(/*new_id*/ None);
        let content_item = replacement_catalog.get(&key).map_or_else(
            || ContentItem::InputText {
                text: occurrence.text.clone(),
            },
            |replacement| ContentItem::InputText {
                text: replacement.clone(),
            },
        );
        if let ResponseItem::Message { content, .. } = &mut projected_item {
            *content = vec![content_item];
        }
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
            if occurrence_lookup.contains_key(&key) {
                continue;
            }
            next_content.push(content_item.clone());
        }
        if next_content.is_empty() {
            continue;
        }
        let mut next_item = item.clone();
        if let ResponseItem::Message { content, .. } = &mut next_item {
            *content = next_content;
        }
        ordinary.push((item_index, next_item));
    }

    let mut projected = Vec::with_capacity(items.len() + reusable.len() + volatile.len());
    projected.extend(reusable.into_iter().map(|(_, _, item)| item));
    let mut volatile_by_item = HashMap::<usize, Vec<ResponseItem>>::new();
    for (_, occurrence_index, item) in volatile {
        let occurrence = &occurrences[occurrence_index];
        let item_index = volatile_user_insertion_index(occurrence, items, latest_real_user)
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
        let prior = occurrences[..latest_index]
            .iter()
            .filter(|candidate| candidate.slot == slot)
            .collect::<Vec<_>>();
        let replaced = prior
            .iter()
            .any(|candidate| candidate.text != occurrence.text);
        let removed = (slot == StableContextSlot::Repository && repository_removed)
            || (slot == StableContextSlot::Collaboration && collaboration_removed);
        let gated = (matches!(slot, StableContextSlot::SkillUsage) && selected_skills_active)
            || (slot == StableContextSlot::RecommendedPlugins && !recommended_plugins_current);
        let text = if slot == StableContextSlot::SkillCatalog && selected_skills_active {
            compact_skill_catalog_reference(&occurrence.text)
        } else {
            occurrence.text.clone()
        };
        let mut component = component_from_text(
            slot.kind(),
            slot.semantic_key(),
            &text,
            !removed && !gated,
            if removed {
                StableContextDisposition::Removed
            } else if gated || (slot == StableContextSlot::SkillCatalog && selected_skills_active) {
                StableContextDisposition::Gated
            } else if replaced || prior.len() > 1 {
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
            &occurrence.text,
            true,
            StableContextDisposition::Unchanged,
        );
        component.identity.semantic_id = semantic_id(
            StableContextKind::SelectedSkill,
            &[
                occurrence.turn_id.as_deref().unwrap_or_default().as_bytes(),
                &component.identity.content_hash,
            ],
        );
        components.push(component);
    }
    (projected, components)
}

fn current_selected_skill_indexes(
    occurrences: &[Occurrence],
    latest_real_user: Option<&(usize, Option<String>)>,
) -> Vec<usize> {
    let Some((user_index, user_turn_id)) = latest_real_user else {
        return Vec::new();
    };
    occurrences
        .iter()
        .enumerate()
        .filter(|(_, occurrence)| occurrence.slot == StableContextSlot::SelectedSkill)
        .filter(|(_, occurrence)| match user_turn_id {
            Some(turn_id) => occurrence.turn_id.as_ref() == Some(turn_id),
            None => occurrence.item_index > *user_index,
        })
        .map(|(index, _)| index)
        .collect()
}

fn occurrence_matches_latest_user_turn(
    occurrence: &Occurrence,
    latest_real_user: Option<&(usize, Option<String>)>,
) -> bool {
    let Some((_, latest_turn_id)) = latest_real_user else {
        return false;
    };
    match latest_turn_id {
        Some(turn_id) => occurrence.turn_id.as_ref() == Some(turn_id),
        // Legacy histories without turn metadata cannot establish expiry safely.
        None => true,
    }
}

fn volatile_user_insertion_index(
    occurrence: &Occurrence,
    items: &[ResponseItem],
    latest_real_user: Option<&(usize, Option<String>)>,
) -> Option<usize> {
    if let Some(turn_id) = occurrence.turn_id.as_deref()
        && let Some((item_index, _)) = items.iter().enumerate().find(|(_, item)| {
            let ResponseItem::Message { role, content, .. } = item else {
                return false;
            };
            role == "user"
                && item.turn_id() == Some(turn_id)
                && content.iter().any(|content_item| {
                    let ContentItem::InputText { text } = content_item else {
                        return false;
                    };
                    classify_stable_text(role, text).is_none()
                })
        })
    {
        return Some(item_index);
    }

    latest_real_user.map(|(item_index, _)| *item_index)
}

fn analyze_unprojected(
    occurrences: &[Occurrence],
    retained_fallback: bool,
) -> Vec<StableContextComponent> {
    occurrences
        .iter()
        .map(|occurrence| {
            component_from_text(
                occurrence.slot.kind(),
                occurrence.slot.semantic_key(),
                &occurrence.text,
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

fn classify_stable_text(role: &str, text: &str) -> Option<StableContextSlot> {
    if role == "user" && marked(text, REPOSITORY_OPEN_TAG, REPOSITORY_CLOSE_TAG) {
        return Some(StableContextSlot::Repository);
    }
    if role == "user" && marked(text, SKILL_OPEN_TAG, "</skill>") {
        return Some(StableContextSlot::SelectedSkill);
    }
    if role == "user" && marked(text, "<environment_context>", "</environment_context>") {
        return Some(StableContextSlot::Environment);
    }
    if role == "user" && marked(text, "<task_model_guidance>", "</task_model_guidance>") {
        return Some(StableContextSlot::TaskModelGuidance);
    }
    if role == "user" && marked(text, "<recommended_plugins>", "</recommended_plugins>") {
        return Some(StableContextSlot::RecommendedPlugins);
    }
    if role != "developer" {
        return None;
    }
    if text.trim_start().starts_with(ROOT_COORDINATOR_PREFIX) {
        return Some(StableContextSlot::RootCoordinator);
    }
    [
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
    .find_map(|(open, close, slot)| marked(text, open, close).then_some(slot))
}

fn contains_known_open_marker(text: &str) -> bool {
    [
        REPOSITORY_OPEN_TAG,
        COLLABORATION_MODE_OPEN_TAG,
        SKILLS_USAGE_OPEN_TAG,
        SKILLS_INSTRUCTIONS_OPEN_TAG,
        SKILL_OPEN_TAG,
        "<environment_context>",
        "<task_model_guidance>",
        "<recommended_plugins>",
        APPS_INSTRUCTIONS_OPEN_TAG,
        "<app-context>",
        PLUGINS_INSTRUCTIONS_OPEN_TAG,
        "<permissions instructions>",
        "<memory_context>",
        MULTI_AGENT_MODE_OPEN_TAG,
        "<model_switch>",
        "<personality_spec>",
    ]
    .iter()
    .any(|marker| text.trim_start().starts_with(marker))
}

fn marked(text: &str, open: &str, close: &str) -> bool {
    let text = text.trim();
    text.starts_with(open) && text.ends_with(close)
}

fn compact_skill_catalog_reference(catalog: &str) -> String {
    let digest: [u8; 32] = Sha256::digest(catalog.as_bytes()).into();
    format!(
        "<skills_instructions>\n<active_catalog version=\"v1\" sha256=\"{}\" state=\"selected\" />\nThe full catalog is inactive while explicitly selected skill instructions are active. It will be restored for a later capability-selection turn.\n</skills_instructions>",
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
#[path = "stable_context_tests.rs"]
mod tests;
