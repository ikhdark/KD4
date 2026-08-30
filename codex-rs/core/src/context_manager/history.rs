use crate::context::ContextualUserFragment;
use crate::context::PromptProvenanceSidecar;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::normalize;
use crate::event_mapping::has_non_contextual_dev_message_content;
use crate::event_mapping::is_contextual_dev_message_content;
use crate::event_mapping::is_contextual_user_message_content;
use crate::git_workspace::WorkspaceEvidenceIdentity;
use crate::session::turn_context::TurnContext;
use crate::stable_context::StableContextManifest;
use crate::stable_context::StableContextTarget;
use crate::stable_context::project_stable_context;
use crate::tool_history::ModelGenerationId;
#[cfg(test)]
use crate::tool_history::ToolHistoryCandidate;
use crate::tool_history::ToolHistoryProjection;
use crate::tool_history::ToolHistoryState;
use crate::tool_history::ToolHistorySubstitution;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;
use codex_utils_cache::BlockingLruCache;
use codex_utils_cache::sha1_digest;
use codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_bytes_for_tokens;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::approx_tokens_from_byte_count_i64;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;
use codex_utils_output_truncation::truncate_text;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::io::Cursor;
use std::io::Write;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;

const PREPARED_HISTORY_POLICY_VERSION: u16 = 6;
const PREPARED_HISTORY_HASH_DOMAIN: &[u8] = b"codex.pending-turn.prepared-history.v6";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreparedHistoryPolicy {
    version: u16,
    supports_images: bool,
    stable_context_target: StableContextTarget,
}

#[derive(Debug)]
enum PreparedPromptItemsSource {
    Shared(Arc<[ResponseItem]>),
    Appended {
        prefix: PreparedPromptItems,
        suffix: Arc<[ResponseItem]>,
    },
    Prefix {
        source: PreparedPromptItems,
    },
}

#[derive(Debug)]
struct PreparedPromptItemsInner {
    source: PreparedPromptItemsSource,
    len: usize,
    flattened: OnceLock<Arc<[ResponseItem]>>,
}

/// Persistent prompt items. Safe history appends add one shared suffix instead
/// of copying every prepared projection's full prefix. A projection is flattened
/// once, on demand, when a transport-facing caller asks for a shared slice.
#[derive(Clone, Debug)]
struct PreparedPromptItems(Arc<PreparedPromptItemsInner>);

impl PreparedPromptItems {
    fn from_shared(items: Arc<[ResponseItem]>) -> Self {
        let flattened = OnceLock::new();
        let _ = flattened.set(Arc::clone(&items));
        Self(Arc::new(PreparedPromptItemsInner {
            len: items.len(),
            source: PreparedPromptItemsSource::Shared(items),
            flattened,
        }))
    }

    fn appended(&self, suffix: Arc<[ResponseItem]>) -> Self {
        if suffix.is_empty() {
            return self.clone();
        }
        Self(Arc::new(PreparedPromptItemsInner {
            len: self.0.len.saturating_add(suffix.len()),
            source: PreparedPromptItemsSource::Appended {
                prefix: self.clone(),
                suffix,
            },
            flattened: OnceLock::new(),
        }))
    }

    fn truncated(&self, len: usize) -> Self {
        let len = len.min(self.0.len);
        if len == self.0.len {
            return self.clone();
        }
        Self(Arc::new(PreparedPromptItemsInner {
            len,
            source: PreparedPromptItemsSource::Prefix {
                source: self.clone(),
            },
            flattened: OnceLock::new(),
        }))
    }

    fn shared(&self) -> Arc<[ResponseItem]> {
        Arc::clone(self.0.flattened.get_or_init(|| {
            let mut chunks = Vec::new();
            self.collect_prefix_chunks(self.0.len, &mut chunks);

            let mut flattened = Vec::with_capacity(self.0.len);
            for (chunk, len) in chunks {
                flattened.extend_from_slice(&chunk[..len]);
            }
            flattened.into()
        }))
    }

    fn collect_prefix_chunks(&self, len: usize, chunks: &mut Vec<(Arc<[ResponseItem]>, usize)>) {
        let mut current = self.clone();
        let mut remaining = len.min(self.0.len);
        let mut reversed = Vec::new();
        loop {
            match &current.0.source {
                PreparedPromptItemsSource::Shared(items) => {
                    if remaining > 0 {
                        reversed.push((Arc::clone(items), remaining));
                    }
                    break;
                }
                PreparedPromptItemsSource::Appended { prefix, suffix } => {
                    let suffix_len = remaining.saturating_sub(prefix.0.len).min(suffix.len());
                    if suffix_len > 0 {
                        reversed.push((Arc::clone(suffix), suffix_len));
                    }
                    remaining = remaining.min(prefix.0.len);
                    current = prefix.clone();
                }
                PreparedPromptItemsSource::Prefix { source } => {
                    remaining = remaining.min(source.0.len);
                    current = source.clone();
                }
            }
        }
        chunks.extend(reversed.into_iter().rev());
    }

    fn get(&self, index: usize) -> Option<&ResponseItem> {
        if index >= self.0.len {
            return None;
        }
        match &self.0.source {
            PreparedPromptItemsSource::Shared(items) => items.get(index),
            PreparedPromptItemsSource::Appended { prefix, suffix } => {
                if index < prefix.0.len {
                    prefix.get(index)
                } else {
                    suffix.get(index - prefix.0.len)
                }
            }
            PreparedPromptItemsSource::Prefix { source } => source.get(index),
        }
    }

    fn as_slice(&self) -> &[ResponseItem] {
        let _ = self.shared();
        let Some(flattened) = self.0.flattened.get() else {
            unreachable!("shared prompt items must be materialized");
        };
        flattened.as_ref()
    }

    fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[cfg(test)]
    fn is_materialized(&self) -> bool {
        self.0.flattened.get().is_some()
    }
}

pub(crate) type SharedPromptProjections = [Arc<[ResponseItem]>; 4];

#[derive(Clone, Debug)]
struct CompactedPromptProjections([PreparedPromptItems; 4]);

impl CompactedPromptProjections {
    fn shared(&self) -> SharedPromptProjections {
        std::array::from_fn(|index| self.0[index].shared())
    }

    fn appended(&self, suffix: Arc<[ResponseItem]>) -> Self {
        let compacted_suffix = compact_acknowledged_tool_search_outputs(suffix);
        let mut advanced = Vec::<PreparedPromptItems>::with_capacity(4);
        for (index, projection) in self.0.iter().enumerate() {
            if let Some(prior_index) = self.0[..index]
                .iter()
                .position(|prior| prior.shares_storage_with(projection))
            {
                advanced.push(advanced[prior_index].clone());
                continue;
            }

            let mut prefix = projection.clone();
            if let Some(last @ ResponseItem::ToolSearchOutput { tools, .. }) =
                projection.get(projection.0.len.saturating_sub(1))
                && !tools.is_empty()
            {
                let compacted_last = compact_tool_search_output(last);
                prefix = projection
                    .truncated(projection.0.len - 1)
                    .appended(vec![compacted_last].into());
            }
            advanced.push(prefix.appended(Arc::clone(&compacted_suffix)));
        }

        Self(
            advanced
                .try_into()
                .unwrap_or_else(|_| unreachable!("prompt projections have a fixed width")),
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedPromptInput {
    items: PreparedPromptItems,
    fallback_items: PreparedPromptItems,
    unreplaced_items: PreparedPromptItems,
    unreplaced_fallback_items: PreparedPromptItems,
    tool_history_substitutions: Arc<[ToolHistorySubstitution]>,
    fallback_tool_history_substitutions: Arc<[ToolHistorySubstitution]>,
    stable_context_manifest: StableContextManifest,
    prompt_provenance: PromptProvenanceSidecar,
    fingerprint: Option<PreparedHistoryFingerprint>,
    policy: PreparedHistoryPolicy,
    compacted_tool_search_outputs: Arc<OnceLock<CompactedPromptProjections>>,
}

impl PreparedPromptInput {
    pub(crate) fn items(&self) -> &[ResponseItem] {
        self.items.as_slice()
    }

    pub(crate) fn shared_items(&self) -> Arc<[ResponseItem]> {
        self.items.shared()
    }

    pub(crate) fn shared_fallback_items(&self) -> Arc<[ResponseItem]> {
        self.fallback_items.shared()
    }

    #[cfg(test)]
    pub(crate) fn shared_unreplaced_items(&self) -> Arc<[ResponseItem]> {
        self.unreplaced_items.shared()
    }

    #[cfg(test)]
    pub(crate) fn shared_unreplaced_fallback_items(&self) -> Arc<[ResponseItem]> {
        self.unreplaced_fallback_items.shared()
    }

    pub(crate) fn tool_history_substitutions(&self) -> Arc<[ToolHistorySubstitution]> {
        Arc::clone(&self.tool_history_substitutions)
    }

    pub(crate) fn fallback_tool_history_substitutions(&self) -> Arc<[ToolHistorySubstitution]> {
        Arc::clone(&self.fallback_tool_history_substitutions)
    }

    pub(crate) fn stable_context_manifest(&self) -> &StableContextManifest {
        &self.stable_context_manifest
    }

    pub(crate) fn prompt_provenance(&self) -> &PromptProvenanceSidecar {
        &self.prompt_provenance
    }

    pub(crate) fn fingerprint(&self) -> Option<[u8; 32]> {
        self.fingerprint
            .as_ref()
            .map(|fingerprint| fingerprint.digest)
    }

    pub(crate) fn compacted_tool_search_outputs(
        &self,
        mut build: impl FnMut(Arc<[ResponseItem]>) -> Arc<[ResponseItem]>,
    ) -> SharedPromptProjections {
        self.compacted_tool_search_outputs
            .get_or_init(|| {
                let projections = [
                    &self.items,
                    &self.fallback_items,
                    &self.unreplaced_items,
                    &self.unreplaced_fallback_items,
                ];
                let mut compacted_by_source =
                    Vec::<(PreparedPromptItems, PreparedPromptItems)>::with_capacity(4);
                let compacted = std::array::from_fn(|index| {
                    let source = projections[index];
                    if let Some((_, compacted)) = compacted_by_source
                        .iter()
                        .find(|(candidate, _)| candidate.shares_storage_with(source))
                    {
                        return compacted.clone();
                    }

                    let compacted = PreparedPromptItems::from_shared(build(source.shared()));
                    compacted_by_source.push((source.clone(), compacted.clone()));
                    compacted
                });
                CompactedPromptProjections(compacted)
            })
            .shared()
    }

    #[cfg(test)]
    pub(crate) fn compacted_tool_search_outputs_are_materialized(&self) -> Option<[bool; 4]> {
        self.compacted_tool_search_outputs
            .get()
            .map(|projections| std::array::from_fn(|index| projections.0[index].is_materialized()))
    }
}

pub(crate) fn compact_acknowledged_tool_search_outputs(
    input: Arc<[ResponseItem]>,
) -> Arc<[ResponseItem]> {
    let last_index = input.len().saturating_sub(1);
    if !input.iter().take(last_index).any(
        |item| matches!(item, ResponseItem::ToolSearchOutput { tools, .. } if !tools.is_empty()),
    ) {
        return input;
    }

    input
        .iter()
        .enumerate()
        .map(|(index, item)| {
            if index < last_index {
                compact_tool_search_output(item)
            } else {
                item.clone()
            }
        })
        .collect::<Vec<_>>()
        .into()
}

fn compact_tool_search_output(item: &ResponseItem) -> ResponseItem {
    let mut item = item.clone();
    if let ResponseItem::ToolSearchOutput { tools, .. } = &mut item {
        *tools = tools
            .iter()
            .map(compact_tool_search_tool_identity)
            .collect();
    }
    item
}

fn compact_tool_search_tool_identity(tool: &serde_json::Value) -> serde_json::Value {
    let Some(object) = tool.as_object() else {
        return tool.clone();
    };
    let Some(name) = object.get("name") else {
        return tool.clone();
    };

    let mut identity = serde_json::Map::new();
    if let Some(kind) = object.get("type") {
        identity.insert("type".to_string(), kind.clone());
    }
    identity.insert("name".to_string(), name.clone());
    if let Some(tools) = object.get("tools").and_then(serde_json::Value::as_array) {
        identity.insert(
            "tools".to_string(),
            serde_json::Value::Array(
                tools
                    .iter()
                    .map(compact_tool_search_tool_identity)
                    .collect(),
            ),
        );
    }
    serde_json::Value::Object(identity)
}

#[derive(Clone, Debug)]
struct PreparedHistoryFingerprint {
    hasher: Sha256,
    digest: [u8; 32],
    #[cfg(test)]
    last_update_item_count: usize,
}

impl PreparedHistoryFingerprint {
    fn new(
        items: &[ResponseItem],
        manifest: &StableContextManifest,
        policy: PreparedHistoryPolicy,
    ) -> serde_json::Result<Self> {
        let mut hasher = Sha256::new();
        hasher.update(PREPARED_HISTORY_HASH_DOMAIN);
        hasher.update(policy.version.to_be_bytes());
        hasher.update([u8::from(policy.supports_images)]);
        hasher.update([match policy.stable_context_target {
            StableContextTarget::Sampling => 1,
            StableContextTarget::FailOpen => 0,
        }]);
        hasher.update(manifest.fingerprint());
        hash_prepared_history_items(&mut hasher, items)?;
        let digest = hasher.clone().finalize().into();
        Ok(Self {
            hasher,
            digest,
            #[cfg(test)]
            last_update_item_count: items.len(),
        })
    }

    fn append(&self, items: &[ResponseItem]) -> serde_json::Result<Self> {
        let mut hasher = self.hasher.clone();
        hash_prepared_history_items(&mut hasher, items)?;
        let digest = hasher.clone().finalize().into();
        Ok(Self {
            hasher,
            digest,
            #[cfg(test)]
            last_update_item_count: items.len(),
        })
    }
}

#[derive(Clone, Debug)]
struct PreparedHistoryCacheEntry {
    source_items: Weak<Vec<ResponseItem>>,
    projection_revision: u64,
    prepared: PreparedPromptInput,
    pending_source_items: Option<Weak<Vec<ResponseItem>>>,
    pending_append: Vec<ResponseItem>,
}

#[derive(Clone, Copy)]
enum PreparedAppendSource {
    Prepared,
    Pending,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ItemTokenEstimateCacheNamespace {
    history_version: u64,
    projection_revision: u64,
    projection_kind: u8,
    policy_version: u16,
    supports_images: bool,
    stable_context_target: u8,
}

#[derive(Debug, Clone, Default)]
enum RealizedContextBaseline {
    Known(Arc<TurnContextItem>),
    #[default]
    Unknown,
}

/// Transcript of thread history
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextManager {
    /// The oldest items are at the beginning of the vector.
    /// Snapshots share the vector until a caller needs to mutate it, avoiding
    /// deep copies while session state is locked.
    items: Arc<Vec<ResponseItem>>,
    /// Bumped whenever history is rewritten, such as compaction or rollback.
    history_version: u64,
    projection_revision: u64,
    tool_history: Arc<ToolHistoryState>,
    prepared_history: Arc<StdMutex<Option<PreparedHistoryCacheEntry>>>,
    item_token_estimates:
        Arc<StdMutex<HashMap<ItemTokenEstimateCacheNamespace, HashMap<String, i64>>>>,
    token_info: Option<TokenUsageInfo>,
    /// Reference context snapshot used for diffing and producing model-visible
    /// settings update items.
    ///
    /// This is the baseline for the next regular model turn, and may already
    /// match the current turn after context updates are persisted.
    ///
    /// When this is `None`, settings diffing treats the next turn as having no
    /// baseline and emits a full reinjection of context state. Rollback may
    /// also clear this when it trims a mixed initial-context developer bundle
    /// whose non-diff fragments no longer exist in the surviving history.
    realized_context_baseline: RealizedContextBaseline,
    /// World state most recently appended to model-visible history.
    world_state_baseline: Option<Arc<WorldStateSnapshot>>,
}

impl ContextManager {
    pub(crate) fn new() -> Self {
        Self {
            items: Arc::new(Vec::new()),
            history_version: 0,
            projection_revision: 0,
            tool_history: Arc::new(ToolHistoryState::default()),
            prepared_history: Arc::new(StdMutex::new(None)),
            item_token_estimates: Arc::new(StdMutex::new(HashMap::new())),
            token_info: TokenUsageInfo::new_or_append(
                &None, &None, /*model_context_window*/ None,
            ),
            realized_context_baseline: RealizedContextBaseline::Unknown,
            world_state_baseline: None,
        }
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.token_info.clone()
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.token_info = info;
    }

    pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>) {
        self.realized_context_baseline = item.map_or(RealizedContextBaseline::Unknown, |item| {
            RealizedContextBaseline::Known(Arc::new(item))
        });
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        match &self.realized_context_baseline {
            RealizedContextBaseline::Known(item) => Some(item.as_ref().clone()),
            RealizedContextBaseline::Unknown => None,
        }
    }

    pub(crate) fn has_reference_context_item(&self) -> bool {
        matches!(
            self.realized_context_baseline,
            RealizedContextBaseline::Known(_)
        )
    }

    pub(crate) fn update_world_state(
        &mut self,
        world_state: &WorldState,
    ) -> (Vec<Box<dyn ContextualUserFragment>>, Option<WorldStateItem>) {
        let (fragments, snapshot) = world_state
            .render_history_diff_with_snapshot(self.world_state_baseline.as_deref(), &self.items);
        let rollout_item = self.world_state_baseline.as_ref().map_or_else(
            || Some(WorldStateItem::full(snapshot.clone().into_value())),
            |previous| {
                snapshot
                    .merge_patch_from(previous.as_ref())
                    .map(WorldStateItem::patch)
            },
        );
        self.world_state_baseline = Some(Arc::new(snapshot));
        (fragments, rollout_item)
    }

    pub(crate) fn set_world_state_baseline(&mut self, snapshot: WorldStateSnapshot) {
        self.world_state_baseline = Some(Arc::new(snapshot));
    }

    pub(crate) fn world_state_baseline(&self) -> Option<WorldStateSnapshot> {
        self.world_state_baseline
            .as_ref()
            .map(|snapshot| snapshot.as_ref().clone())
    }

    #[cfg(test)]
    fn baselines_share_storage_with(&self, other: &Self) -> bool {
        let realized_context_is_shared = match (
            &self.realized_context_baseline,
            &other.realized_context_baseline,
        ) {
            (RealizedContextBaseline::Known(left), RealizedContextBaseline::Known(right)) => {
                Arc::ptr_eq(left, right)
            }
            (RealizedContextBaseline::Unknown, RealizedContextBaseline::Unknown) => true,
            _ => false,
        };
        let world_state_is_shared = match (&self.world_state_baseline, &other.world_state_baseline)
        {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        };
        realized_context_is_shared && world_state_is_shared
    }

    pub(crate) fn mark_realized_context_unknown(&mut self) {
        self.realized_context_baseline = RealizedContextBaseline::Unknown;
        self.world_state_baseline = None;
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        match &mut self.token_info {
            Some(info) => info.fill_to_context_window(context_window),
            None => {
                self.token_info = Some(TokenUsageInfo::full_context_window(context_window));
            }
        }
    }

    /// `items` is ordered from oldest to newest.
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator,
        I::Item: std::ops::Deref<Target = ResponseItem>,
    {
        let appended = items
            .into_iter()
            .filter_map(|item| {
                let item = item.deref();
                is_api_message(item).then(|| self.process_item(item, policy))
            })
            .collect::<Vec<_>>();
        if appended.is_empty() {
            return;
        }
        let prepared_append = {
            let mut cache = self
                .prepared_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let source = cache.as_ref().and_then(|entry| {
                if weak_ptr_eq_arc(&entry.source_items, &self.items) {
                    Some(PreparedAppendSource::Prepared)
                } else if entry
                    .pending_source_items
                    .as_ref()
                    .is_some_and(|source| weak_ptr_eq_arc(source, &self.items))
                {
                    Some(PreparedAppendSource::Pending)
                } else {
                    None
                }
            });
            source.and_then(|source| {
                cache.take().map(|mut entry| {
                    // `Arc::make_mut` dissociates weak pointers by moving the Arc
                    // allocation. Drop cache-owned identities first so a unique raw
                    // history allocation can be extended in place.
                    entry.source_items = Weak::new();
                    entry.pending_source_items = None;
                    (entry, source)
                })
            })
        };
        Arc::make_mut(&mut self.items).extend(appended.iter().cloned());
        self.advance_prepared_history_for_append(&appended, prepared_append);
    }

    /// Returns the history prepared for sending to the model. This applies a proper
    /// normalization and drops un-suited items. When `input_modalities` does not
    /// include `InputModality::Image`, images are stripped from messages and tool
    /// outputs.
    pub(crate) fn for_prompt(self, input_modalities: &[InputModality]) -> Vec<ResponseItem> {
        self.prepare_for_prompt(input_modalities).items().to_vec()
    }

    pub(crate) fn prepare_for_prompt(
        self,
        input_modalities: &[InputModality],
    ) -> PreparedPromptInput {
        self.prepare_for_prompt_target(input_modalities, StableContextTarget::FailOpen)
    }

    /// Sampling-only preparation entrypoint. Callers must select the target
    /// explicitly so generic and compaction preparation cannot accidentally
    /// enable logical projection.
    #[cfg(test)]
    pub(crate) fn prepare_for_sampling_prompt(
        self,
        input_modalities: &[InputModality],
        target: StableContextTarget,
    ) -> PreparedPromptInput {
        debug_assert_eq!(target, StableContextTarget::Sampling);
        self.prepare_for_prompt_target(input_modalities, target)
    }

    fn prepare_for_prompt_target(
        mut self,
        input_modalities: &[InputModality],
        stable_context_target: StableContextTarget,
    ) -> PreparedPromptInput {
        let policy = PreparedHistoryPolicy {
            version: PREPARED_HISTORY_POLICY_VERSION,
            supports_images: input_modalities.contains(&InputModality::Image),
            stable_context_target,
        };
        let source_items = Arc::downgrade(&self.items);
        if let Some(prepared) = self
            .prepared_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|entry| {
                weak_ptr_eq_arc(&entry.source_items, &self.items)
                    && entry.projection_revision == self.projection_revision
                    && entry.prepared.policy == policy
            })
            .map(|entry| {
                let mut prepared = entry.prepared.clone();
                prepared.stable_context_manifest = prepared
                    .stable_context_manifest
                    .with_local_reused(/*local_reused*/ true);
                prepared
            })
        {
            return prepared;
        }
        evict_resolved_reasoning(Arc::make_mut(&mut self.items));
        self.normalize_history(input_modalities);
        project_update_plan_history(Arc::make_mut(&mut self.items));
        let normalized_items: Arc<[ResponseItem]> = Arc::from(Arc::unwrap_or_clone(self.items));
        let projection = project_stable_context(normalized_items, stable_context_target);
        let items = projection.items;
        let prompt_provenance =
            PromptProvenanceSidecar::from_assembled_items(&items, &projection.manifest);
        let fingerprint =
            PreparedHistoryFingerprint::new(&items, &projection.manifest, policy).ok();
        let projected_items = items;
        let items = PreparedPromptItems::from_shared(Arc::clone(&projected_items));
        let fallback_items = if Arc::ptr_eq(&projected_items, &projection.fallback_items) {
            items.clone()
        } else {
            PreparedPromptItems::from_shared(projection.fallback_items)
        };
        let prepared = PreparedPromptInput {
            unreplaced_items: items.clone(),
            unreplaced_fallback_items: fallback_items.clone(),
            items,
            fallback_items,
            tool_history_substitutions: Arc::from([]),
            fallback_tool_history_substitutions: Arc::from([]),
            stable_context_manifest: projection.manifest,
            prompt_provenance,
            fingerprint,
            policy,
            compacted_tool_search_outputs: Arc::new(OnceLock::new()),
        };
        if prepared.fingerprint.is_some() {
            *self
                .prepared_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(PreparedHistoryCacheEntry {
                    source_items,
                    projection_revision: self.projection_revision,
                    prepared: prepared.clone(),
                    pending_source_items: None,
                    pending_append: Vec::new(),
                });
        }
        prepared
    }

    pub(crate) fn prepare_for_sampling_prompt_with_completed_tool_projection(
        self,
        input_modalities: &[InputModality],
        target: StableContextTarget,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: &crate::git_workspace::GitWorkspaceCache,
    ) -> PreparedPromptInput {
        debug_assert_eq!(target, StableContextTarget::Sampling);
        self.prepare_for_prompt_with_completed_tool_projection_target(
            input_modalities,
            target,
            workspace_identity,
            Some(git_workspace),
        )
    }

    pub(crate) fn requires_workspace_evidence_validation(&self) -> bool {
        self.tool_history
            .requires_workspace_evidence_validation(self.items.as_slice())
    }

    pub(crate) fn prepare_for_sampling_prompt_with_workspace_freshness(
        self,
        input_modalities: &[InputModality],
        target: StableContextTarget,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: &crate::git_workspace::GitWorkspaceCache,
    ) -> PreparedPromptInput {
        debug_assert_eq!(target, StableContextTarget::Sampling);
        let tool_history = Arc::clone(&self.tool_history);
        let prepared = self.prepare_for_prompt_target(input_modalities, target);
        let projection = tool_history.project_workspace_freshness_with_cache(
            prepared.shared_items(),
            workspace_identity,
            git_workspace,
        );
        let fallback_projection = tool_history.project_workspace_freshness_with_cache(
            prepared.shared_fallback_items(),
            workspace_identity,
            git_workspace,
        );
        apply_tool_history_projection(prepared, projection, fallback_projection)
    }

    /// Compaction also benefits from settled, exactly recoverable tool receipts.
    /// This is explicit so generic prompt preparation remains lossless.
    pub(crate) fn for_compaction_prompt_with_completed_tool_projection(
        self,
        input_modalities: &[InputModality],
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
    ) -> Vec<ResponseItem> {
        self.prepare_for_prompt_with_completed_tool_projection_target(
            input_modalities,
            StableContextTarget::FailOpen,
            workspace_identity,
            None,
        )
        .items()
        .to_vec()
    }

    fn prepare_for_prompt_with_completed_tool_projection_target(
        self,
        input_modalities: &[InputModality],
        target: StableContextTarget,
        workspace_identity: Option<&WorkspaceEvidenceIdentity>,
        git_workspace: Option<&crate::git_workspace::GitWorkspaceCache>,
    ) -> PreparedPromptInput {
        let tool_history = Arc::clone(&self.tool_history);
        let prepared = self.prepare_for_prompt_target(input_modalities, target);
        let project = |items| match git_workspace {
            Some(cache) => {
                tool_history.project_with_workspace_cache(items, workspace_identity, cache)
            }
            None => tool_history.project_with_workspace_identity(items, workspace_identity),
        };
        let projection = project(prepared.shared_items());
        let fallback_projection = project(prepared.shared_fallback_items());
        apply_tool_history_projection(prepared, projection, fallback_projection)
    }

    pub(crate) fn set_tool_history_state(&mut self, state: ToolHistoryState) {
        self.tool_history = Arc::new(state);
        self.invalidate_prepared_history();
    }

    pub(crate) fn tool_history_state(&self) -> ToolHistoryState {
        (*self.tool_history).clone()
    }

    #[cfg(test)]
    pub(crate) fn register_tool_history_candidate(&mut self, candidate: ToolHistoryCandidate) {
        // Candidates are projected after canonical prompt preparation, so they do not invalidate
        // the normalized history cache.
        Arc::make_mut(&mut self.tool_history).register(candidate);
    }

    #[cfg(test)]
    pub(crate) fn register_non_workspace_code_mode_call(&mut self, call_id: String) {
        Arc::make_mut(&mut self.tool_history).register_non_workspace_code_mode_call(call_id);
        self.invalidate_prepared_history();
    }

    pub(crate) fn apply_tool_history_mutation(
        &mut self,
        mutation: &crate::tool_history::ToolHistoryMutation,
    ) -> bool {
        let changed = mutation.apply(Arc::make_mut(&mut self.tool_history));
        if changed {
            self.invalidate_prepared_history();
        }
        changed
    }

    #[cfg(test)]
    pub(crate) fn mark_tool_history_consumed(
        &mut self,
        input: &[ResponseItem],
        generation: ModelGenerationId,
    ) -> bool {
        // Consumption changes receipt eligibility, which is also applied after the cache lookup.
        Arc::make_mut(&mut self.tool_history).mark_consumed(input, generation)
    }

    pub(crate) fn mark_tool_history_consumed_with_delta(
        &mut self,
        input: &[ResponseItem],
        generation: ModelGenerationId,
    ) -> std::collections::BTreeSet<String> {
        // Keep the same post-cache projection boundary while returning the changed call IDs.
        Arc::make_mut(&mut self.tool_history).mark_consumed_with_delta(input, generation)
    }

    /// Returns raw items in the history.
    pub(crate) fn raw_items(&self) -> &[ResponseItem] {
        &self.items
    }

    /// Returns raw items in the history and consumes the snapshot.
    pub(crate) fn into_raw_items(self) -> Vec<ResponseItem> {
        Arc::unwrap_or_clone(self.items)
    }

    /// Returns the immutable raw-item snapshot without cloning its contents.
    pub(crate) fn into_shared_raw_items(self) -> Arc<Vec<ResponseItem>> {
        self.items
    }

    pub(crate) fn history_version(&self) -> u64 {
        self.history_version
    }

    // Estimate token usage using byte-based heuristics from the truncation helpers.
    // This is a coarse lower bound, not a tokenizer-accurate count.
    pub(crate) fn estimate_token_count(&self, turn_context: &TurnContext) -> Option<i64> {
        self.estimate_prepared_token_count_with_base_instructions(
            &turn_context.model_info.input_modalities,
            turn_context.base_instructions.as_ref(),
        )
    }

    pub(crate) fn estimate_prepared_token_count_with_base_instructions(
        &self,
        input_modalities: &[InputModality],
        base_instructions: &BaseInstructions,
    ) -> Option<i64> {
        let prepared = self
            .clone()
            .prepare_for_prompt_target(input_modalities, StableContextTarget::Sampling);
        self.estimate_prepared_items_token_count(
            prepared.items(),
            base_instructions,
            /*pending_user_boundary*/ false,
            prepared.policy,
        )
    }

    #[cfg(test)]
    pub(crate) fn estimate_token_count_with_base_instructions(
        &self,
        base_instructions: &BaseInstructions,
    ) -> Option<i64> {
        Self::estimate_items_token_count_with_base_instructions(self.raw_items(), base_instructions)
    }

    pub(crate) fn estimate_token_count_after_pending_user_boundary(
        &self,
        input_modalities: &[InputModality],
        base_instructions: &BaseInstructions,
    ) -> Option<i64> {
        let prepared = self
            .clone()
            .prepare_for_prompt_target(input_modalities, StableContextTarget::Sampling);
        self.estimate_prepared_items_token_count(
            prepared.items(),
            base_instructions,
            /*pending_user_boundary*/ true,
            prepared.policy,
        )
    }

    fn estimate_prepared_items_token_count(
        &self,
        items: &[ResponseItem],
        base_instructions: &BaseInstructions,
        pending_user_boundary: bool,
        policy: PreparedHistoryPolicy,
    ) -> Option<i64> {
        self.estimate_cached_items_token_count(
            items,
            base_instructions,
            pending_user_boundary,
            policy,
        )
    }

    fn estimate_cached_items_token_count(
        &self,
        items: &[ResponseItem],
        base_instructions: &BaseInstructions,
        pending_user_boundary: bool,
        policy: PreparedHistoryPolicy,
    ) -> Option<i64> {
        let base_tokens =
            i64::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i64::MAX);
        let last_instruction_boundary = pending_user_boundary
            .then_some(items.len())
            .or_else(|| items.iter().rposition(is_user_turn_boundary));
        let items_tokens = items
            .iter()
            .enumerate()
            .filter(|(index, item)| !is_resolved_reasoning(*index, item, last_instruction_boundary))
            .map(|(_, item)| self.estimate_item_token_count_cached(item, policy))
            .fold(0i64, i64::saturating_add);

        Some(base_tokens.saturating_add(items_tokens))
    }

    fn estimate_item_token_count_cached(
        &self,
        item: &ResponseItem,
        policy: PreparedHistoryPolicy,
    ) -> i64 {
        let Some(item_id) = item.id() else {
            return estimate_item_token_count(item);
        };
        let projection_kind = 1;
        let policy_version = policy.version;
        let supports_images = policy.supports_images;
        let stable_context_target = match policy.stable_context_target {
            StableContextTarget::Sampling => 1,
            StableContextTarget::FailOpen => 0,
        };
        let namespace = ItemTokenEstimateCacheNamespace {
            history_version: self.history_version,
            projection_revision: self.projection_revision,
            projection_kind,
            policy_version,
            supports_images,
            stable_context_target,
        };
        let mut estimates = self
            .item_token_estimates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(estimate) = estimates
            .get(&namespace)
            .and_then(|items| items.get(item_id.as_str()))
            .copied()
        {
            return estimate;
        }

        let estimate = estimate_item_token_count(item);
        estimates
            .entry(namespace)
            .or_default()
            .insert(item_id.to_string(), estimate);
        estimate
    }

    #[cfg(test)]
    fn cached_item_token_estimate_count(&self) -> usize {
        self.item_token_estimates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(HashMap::len)
            .sum()
    }

    #[cfg(test)]
    fn cached_item_token_estimate_namespace_count(&self) -> usize {
        self.item_token_estimates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub(crate) fn estimate_items_token_count_with_base_instructions(
        items: &[ResponseItem],
        base_instructions: &BaseInstructions,
    ) -> Option<i64> {
        Self::estimate_items_token_count(
            items,
            base_instructions,
            /*pending_user_boundary*/ false,
        )
    }

    fn estimate_items_token_count(
        items: &[ResponseItem],
        base_instructions: &BaseInstructions,
        pending_user_boundary: bool,
    ) -> Option<i64> {
        let base_tokens =
            i64::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i64::MAX);

        let last_instruction_boundary = pending_user_boundary
            .then_some(items.len())
            .or_else(|| items.iter().rposition(is_user_turn_boundary));
        let items_tokens = items
            .iter()
            .enumerate()
            .filter(|(index, item)| !is_resolved_reasoning(*index, item, last_instruction_boundary))
            .map(|(_, item)| item)
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add);

        Some(base_tokens.saturating_add(items_tokens))
    }

    pub(crate) fn replace(&mut self, items: Vec<ResponseItem>) {
        Arc::make_mut(&mut self.tool_history).retain_for_history(&items);
        self.items = Arc::new(items);
        self.history_version = self.history_version.saturating_add(1);
        self.invalidate_prepared_history();
        self.world_state_baseline = None;
    }

    /// Replace image content in the last turn if it originated from a tool output.
    /// Returns true when a tool image was replaced, false otherwise.
    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str) -> bool {
        let Some(turn_start) = self.items.iter().rposition(is_user_turn_boundary) else {
            return false;
        };

        let mut replaced = false;
        let placeholder = placeholder.to_string();
        for item in Arc::make_mut(&mut self.items)
            .iter_mut()
            .skip(turn_start.saturating_add(1))
        {
            let output = match item {
                ResponseItem::FunctionCallOutput { output, .. }
                | ResponseItem::CustomToolCallOutput { output, .. } => output,
                _ => continue,
            };
            let Some(content_items) = output.content_items_mut() else {
                continue;
            };
            for content_item in content_items {
                if matches!(
                    content_item,
                    FunctionCallOutputContentItem::InputImage { .. }
                ) {
                    *content_item = FunctionCallOutputContentItem::InputText {
                        text: placeholder.clone(),
                    };
                    replaced = true;
                }
            }
        }
        if replaced {
            self.history_version = self.history_version.saturating_add(1);
            self.invalidate_prepared_history();
        }
        replaced
    }

    /// Drop the last `num_turns` instruction turns from this history.
    ///
    /// Instruction turns are history messages that should behave like a new prompt boundary:
    /// ordinary user messages and structured assistant inter-agent instructions.
    ///
    /// This mirrors thread-rollback semantics:
    /// - `num_turns == 0` is a no-op
    /// - if there are no user turns, this is a no-op
    /// - if `num_turns` exceeds the number of user turns, all user turns are dropped while
    ///   preserving any items that occurred before the first user message.
    ///
    /// If rollback trims a pre-turn developer message that mixes contextual fragments with
    /// persistent developer text from `build_initial_context`, this also clears
    /// `reference_context_item`. The surviving history no longer contains the full bundle that
    /// established the prior baseline, so future turns must fall back to full reinjection instead
    /// of diffing against stale state.
    pub(crate) fn drop_last_n_user_turns(&mut self, num_turns: u32) {
        if num_turns == 0 {
            return;
        }

        let snapshot = self.items.clone();
        let user_positions = user_message_positions(&snapshot);
        let Some(&first_instruction_turn_idx) = user_positions.first() else {
            return;
        };

        let n_from_end = usize::try_from(num_turns).unwrap_or(usize::MAX);
        let mut cut_idx = if n_from_end >= user_positions.len() {
            first_instruction_turn_idx
        } else {
            user_positions[user_positions.len() - n_from_end]
        };

        cut_idx =
            self.trim_pre_turn_context_updates(&snapshot, first_instruction_turn_idx, cut_idx);

        self.replace(snapshot[..cut_idx].to_vec());
    }

    pub(crate) fn update_token_info(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
    ) {
        self.token_info = TokenUsageInfo::new_or_append(
            &self.token_info,
            &Some(usage.clone()),
            model_context_window,
        );
    }

    fn get_non_last_reasoning_items_tokens(&self) -> i64 {
        // The server snapshot accounts for the current response's reasoning even when it omits
        // resolved reasoning from earlier turns. Re-estimate only reasoning before the latest
        // instruction boundary so the current response is not counted twice.
        let Some(last_user_index) = self.items.iter().rposition(is_user_turn_boundary) else {
            return 0;
        };

        self.items
            .iter()
            .take(last_user_index)
            .filter(|item| {
                matches!(
                    item,
                    ResponseItem::Reasoning {
                        encrypted_content: Some(_),
                        ..
                    }
                )
            })
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add)
    }

    // These are local items added after the most recent model-emitted item.
    // They are not reflected in `last_token_usage.total_tokens`.
    fn items_after_last_model_generated_item(&self) -> &[ResponseItem] {
        let start = self
            .items
            .iter()
            .rposition(is_model_generated_item)
            .map_or(self.items.len(), |index| index.saturating_add(1));
        &self.items[start..]
    }

    /// Returns the active model-visible context estimate. When the server omits resolved
    /// reasoning from its usage snapshot, earlier encrypted reasoning is estimated locally.
    pub(crate) fn get_total_token_usage(
        &self,
        server_reasoning_included: bool,
        base_instructions: &BaseInstructions,
    ) -> i64 {
        let items_after_last_model_generated = self.items_after_last_model_generated_item();
        let local_tail_contains_instruction_boundary = self
            .items
            .iter()
            .rposition(is_model_generated_item)
            .map_or_else(
                || self.items.iter().any(is_user_turn_boundary),
                |last_model_generated| {
                    self.items[last_model_generated.saturating_add(1)..]
                        .iter()
                        .any(is_user_turn_boundary)
                },
            );
        if local_tail_contains_instruction_boundary {
            return Self::estimate_items_token_count_with_base_instructions(
                self.raw_items(),
                base_instructions,
            )
            .unwrap_or(0);
        }

        let last_tokens = self
            .token_info
            .as_ref()
            .map(|info| info.last_token_usage.total_tokens)
            .unwrap_or(0);
        let items_after_last_model_generated_tokens = items_after_last_model_generated
            .iter()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add);
        let earlier_reasoning_tokens = if server_reasoning_included {
            0
        } else {
            self.get_non_last_reasoning_items_tokens()
        };
        last_tokens
            .saturating_add(earlier_reasoning_tokens)
            .saturating_add(items_after_last_model_generated_tokens)
    }

    pub(crate) fn estimated_tokens_after_last_model_generated_item(&self) -> i64 {
        self.items_after_last_model_generated_item()
            .iter()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add)
    }

    /// This function enforces a couple of invariants on the in-memory history:
    /// 1. every call (function/custom) has a corresponding output entry
    /// 2. every output has a corresponding call entry
    /// 3. when images are unsupported, image content is stripped from messages and tool outputs
    fn normalize_history(&mut self, input_modalities: &[InputModality]) {
        let items = Arc::make_mut(&mut self.items);

        // all function/tool calls must have a corresponding output
        normalize::ensure_call_outputs_present(items);

        // all outputs must have a corresponding function/tool call
        normalize::remove_orphan_outputs(items);

        // strip images when model does not support them
        normalize::strip_images_when_unsupported(input_modalities, items);
    }

    fn process_item(&self, item: &ResponseItem, policy: TruncationPolicy) -> ResponseItem {
        match item {
            ResponseItem::FunctionCallOutput {
                id,
                call_id,
                output,
                internal_chat_message_metadata_passthrough: metadata,
            } => ResponseItem::FunctionCallOutput {
                id: id.clone(),
                call_id: call_id.clone(),
                output: truncate_function_output_payload(output, policy),
                internal_chat_message_metadata_passthrough: metadata.clone(),
            },
            ResponseItem::CustomToolCallOutput {
                id,
                call_id,
                name,
                output,
                internal_chat_message_metadata_passthrough: metadata,
            } => ResponseItem::CustomToolCallOutput {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                output: truncate_function_output_payload(output, policy),
                internal_chat_message_metadata_passthrough: metadata.clone(),
            },
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Message { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => item.clone(),
        }
    }

    /// Walk backward from a rollback cut and trim contiguous pre-turn context-update items.
    ///
    /// Returns the adjusted cut index after removing contextual developer/user items immediately
    /// above the rolled-back turn boundary.
    ///
    /// `first_instruction_turn_idx` is the earliest rollback-eligible instruction-turn boundary
    /// in `snapshot`; the trim walk never crosses it so any session-prefix items that predate the
    /// first real turn survive rollback.
    ///
    /// `cut_idx` is the tentative slice boundary after dropping the requested number of
    /// instruction turns, before stripping contextual pre-turn items that sit immediately above
    /// that boundary.
    ///
    /// If any trimmed developer message was a mixed `build_initial_context` bundle containing both
    /// rollback-trimmable contextual fragments and persistent developer text, this also clears the
    /// stored `reference_context_item` baseline so the next real turn falls back to full
    /// reinjection.
    fn trim_pre_turn_context_updates(
        &mut self,
        snapshot: &[ResponseItem],
        first_instruction_turn_idx: usize,
        mut cut_idx: usize,
    ) -> usize {
        while cut_idx > first_instruction_turn_idx {
            match &snapshot[cut_idx - 1] {
                ResponseItem::Message { role, content, .. }
                    if role == "developer" && is_contextual_dev_message_content(content) =>
                {
                    if has_non_contextual_dev_message_content(content) {
                        // Mixed `build_initial_context` bundles are not reconstructible from
                        // steady-state diffs once trimmed, so the next real turn must fully
                        // reinject context instead of diffing against a stale baseline.
                        self.realized_context_baseline = RealizedContextBaseline::Unknown;
                    }
                    cut_idx -= 1;
                }
                ResponseItem::Message { role, content, .. }
                    if role == "user" && is_contextual_user_message_content(content) =>
                {
                    cut_idx -= 1;
                }
                _ => break,
            }
        }
        cut_idx
    }
}

fn project_update_plan_history(items: &mut Vec<ResponseItem>) {
    let update_calls = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            ResponseItem::FunctionCall { name, call_id, .. } if name == "update_plan" => {
                Some((index, call_id.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some((latest_call_index, latest_call_id)) = update_calls.last() else {
        return;
    };
    let Some((latest_output_index, latest_output)) =
        items.iter().enumerate().rev().find(|(_, item)| {
            matches!(
                item,
                ResponseItem::FunctionCallOutput { call_id, .. } if call_id == latest_call_id
            )
        })
    else {
        return;
    };
    let ResponseItem::FunctionCallOutput { output, .. } = latest_output else {
        return;
    };
    let FunctionCallOutputBody::Text(text) = &output.body else {
        return;
    };
    let Ok(mut authoritative_output) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if authoritative_output.get("current_plan").is_none() {
        return;
    }
    let Some(output_object) = authoritative_output.as_object_mut() else {
        return;
    };
    output_object.remove("normalized_plan");
    output_object.insert(
        "superseded_updates".to_string(),
        serde_json::json!(update_calls.len().saturating_sub(1)),
    );

    let mut projected_call = items[*latest_call_index].clone();
    let ResponseItem::FunctionCall { arguments, .. } = &mut projected_call else {
        return;
    };
    *arguments = serde_json::json!({
        "projected": "authoritative current plan is in the tool output"
    })
    .to_string();
    let mut projected_output = items[latest_output_index].clone();
    let ResponseItem::FunctionCallOutput { output, .. } = &mut projected_output else {
        return;
    };
    output.body = FunctionCallOutputBody::Text(authoritative_output.to_string());

    let update_call_ids = update_calls
        .iter()
        .map(|(_, call_id)| call_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut projected = Vec::with_capacity(items.len());
    for (index, item) in items.drain(..).enumerate() {
        if index == *latest_call_index {
            projected.push(projected_call.clone());
            projected.push(projected_output.clone());
        }
        let belongs_to_update_plan = matches!(
            &item,
            ResponseItem::FunctionCall { name, .. } if name == "update_plan"
        ) || matches!(
            &item,
            ResponseItem::FunctionCallOutput { call_id, .. }
                if update_call_ids.contains(call_id.as_str())
        );
        if !belongs_to_update_plan {
            projected.push(item);
        }
    }
    *items = projected;
}

fn prepared_append_can_be_completed(items: &[ResponseItem], supports_images: bool) -> bool {
    let mut calls = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for item in items {
        match item {
            ResponseItem::Reasoning { .. } => {}
            ResponseItem::FunctionCall { name, call_id, .. } if name != "update_plan" => {
                if !calls.insert(call_id.as_str()) {
                    return false;
                }
            }
            ResponseItem::CustomToolCall { call_id, .. } => {
                if !calls.insert(call_id.as_str()) {
                    return false;
                }
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            }
            | ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                if !supports_images
                    && matches!(
                        &output.body,
                        FunctionCallOutputBody::ContentItems(content)
                            if content.iter().any(|item| matches!(
                                item,
                                FunctionCallOutputContentItem::InputImage { .. }
                            ))
                    )
                {
                    return false;
                }
                if !outputs.insert(call_id.as_str()) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    outputs.is_subset(&calls)
}

fn prepared_append_is_complete_and_safe(items: &[ResponseItem], supports_images: bool) -> bool {
    if !prepared_append_can_be_completed(items, supports_images) {
        return false;
    }
    let calls = items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let outputs = items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput { call_id, .. }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    calls == outputs
}

impl ContextManager {
    fn clear_item_token_estimates(&self) {
        self.item_token_estimates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn advance_item_token_estimates_for_append(
        &self,
        previous_revision: u64,
        preserved_policy: PreparedHistoryPolicy,
    ) {
        let mut estimates = self
            .item_token_estimates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut current = HashMap::with_capacity(estimates.len());
        for (mut namespace, item_estimates) in estimates.drain() {
            let prepared_projection_is_preserved = namespace.projection_kind == 1
                && namespace.policy_version == preserved_policy.version
                && namespace.supports_images == preserved_policy.supports_images
                && namespace.stable_context_target
                    == match preserved_policy.stable_context_target {
                        StableContextTarget::Sampling => 1,
                        StableContextTarget::FailOpen => 0,
                    };
            if namespace.history_version == self.history_version
                && namespace.projection_revision == previous_revision
                && prepared_projection_is_preserved
            {
                namespace.projection_revision = self.projection_revision;
                current.insert(namespace, item_estimates);
            }
        }
        *estimates = current;
    }

    fn advance_prepared_history_for_append(
        &mut self,
        appended: &[ResponseItem],
        prepared_append: Option<(PreparedHistoryCacheEntry, PreparedAppendSource)>,
    ) {
        let previous_revision = self.projection_revision;
        self.projection_revision = self.projection_revision.saturating_add(1);
        let Some((mut entry, pre_append_source)) = prepared_append else {
            *self
                .prepared_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            self.clear_item_token_estimates();
            return;
        };
        if entry.projection_revision != previous_revision {
            self.clear_item_token_estimates();
            return;
        }
        let append = match pre_append_source {
            PreparedAppendSource::Prepared if entry.pending_append.is_empty() => appended.to_vec(),
            PreparedAppendSource::Pending => {
                let mut pending = std::mem::take(&mut entry.pending_append);
                pending.extend_from_slice(appended);
                pending
            }
            PreparedAppendSource::Prepared => {
                self.clear_item_token_estimates();
                return;
            }
        };
        if !prepared_append_is_complete_and_safe(&append, entry.prepared.policy.supports_images) {
            const MAX_PENDING_PREPARED_APPEND_ITEMS: usize = 64;
            if append.len() <= MAX_PENDING_PREPARED_APPEND_ITEMS
                && prepared_append_can_be_completed(&append, entry.prepared.policy.supports_images)
            {
                entry.projection_revision = self.projection_revision;
                entry.pending_source_items = Some(Arc::downgrade(&self.items));
                entry.pending_append = append;
                *self
                    .prepared_history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(entry);
            }
            self.clear_item_token_estimates();
            return;
        }
        let append: Arc<[ResponseItem]> = append.into();
        let items = entry.prepared.items.appended(Arc::clone(&append));
        let fallback_items = if entry
            .prepared
            .fallback_items
            .shares_storage_with(&entry.prepared.items)
        {
            items.clone()
        } else {
            entry.prepared.fallback_items.appended(Arc::clone(&append))
        };
        let unreplaced_items = if entry
            .prepared
            .unreplaced_items
            .shares_storage_with(&entry.prepared.items)
        {
            items.clone()
        } else if entry
            .prepared
            .unreplaced_items
            .shares_storage_with(&entry.prepared.fallback_items)
        {
            fallback_items.clone()
        } else {
            entry
                .prepared
                .unreplaced_items
                .appended(Arc::clone(&append))
        };
        let unreplaced_fallback_items = if entry
            .prepared
            .unreplaced_fallback_items
            .shares_storage_with(&entry.prepared.items)
        {
            items.clone()
        } else if entry
            .prepared
            .unreplaced_fallback_items
            .shares_storage_with(&entry.prepared.fallback_items)
        {
            fallback_items.clone()
        } else if entry
            .prepared
            .unreplaced_fallback_items
            .shares_storage_with(&entry.prepared.unreplaced_items)
        {
            unreplaced_items.clone()
        } else {
            entry
                .prepared
                .unreplaced_fallback_items
                .appended(Arc::clone(&append))
        };
        let Some(fingerprint) = entry
            .prepared
            .fingerprint
            .as_ref()
            .and_then(|fingerprint| fingerprint.append(&append).ok())
        else {
            self.clear_item_token_estimates();
            return;
        };
        let preserved_policy = entry.prepared.policy;
        let prompt_provenance = entry.prepared.prompt_provenance.clone();
        let compacted_tool_search_outputs = Arc::new(OnceLock::new());
        if let Some(compacted) = entry.prepared.compacted_tool_search_outputs.get() {
            let _ = compacted_tool_search_outputs.set(compacted.appended(Arc::clone(&append)));
        }
        *self
            .prepared_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PreparedHistoryCacheEntry {
            source_items: Arc::downgrade(&self.items),
            projection_revision: self.projection_revision,
            prepared: PreparedPromptInput {
                items,
                fallback_items,
                unreplaced_items,
                unreplaced_fallback_items,
                tool_history_substitutions: entry.prepared.tool_history_substitutions,
                fallback_tool_history_substitutions: entry
                    .prepared
                    .fallback_tool_history_substitutions,
                stable_context_manifest: entry.prepared.stable_context_manifest,
                prompt_provenance,
                fingerprint: Some(fingerprint),
                policy: entry.prepared.policy,
                compacted_tool_search_outputs,
            },
            pending_source_items: None,
            pending_append: Vec::new(),
        });
        self.advance_item_token_estimates_for_append(previous_revision, preserved_policy);
    }

    fn invalidate_prepared_history(&mut self) {
        self.projection_revision = self.projection_revision.saturating_add(1);
        *self
            .prepared_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.clear_item_token_estimates();
    }
}

fn apply_tool_history_projection(
    mut prepared: PreparedPromptInput,
    projection: ToolHistoryProjection,
    fallback_projection: ToolHistoryProjection,
) -> PreparedPromptInput {
    let prepared_items = prepared.items.shared();
    let prepared_unreplaced_items = if prepared
        .unreplaced_items
        .shares_storage_with(&prepared.items)
    {
        Arc::clone(&prepared_items)
    } else {
        prepared.unreplaced_items.shared()
    };
    let prepared_fallback_items = prepared.fallback_items.shared();
    let prepared_unreplaced_fallback_items = if prepared
        .unreplaced_fallback_items
        .shares_storage_with(&prepared.fallback_items)
    {
        Arc::clone(&prepared_fallback_items)
    } else {
        prepared.unreplaced_fallback_items.shared()
    };
    if Arc::ptr_eq(&projection.items, &prepared_items)
        && Arc::ptr_eq(&projection.unreplaced_items, &prepared_unreplaced_items)
        && projection.substitutions.as_ref() == prepared.tool_history_substitutions.as_ref()
        && Arc::ptr_eq(&fallback_projection.items, &prepared_fallback_items)
        && Arc::ptr_eq(
            &fallback_projection.unreplaced_items,
            &prepared_unreplaced_fallback_items,
        )
        && fallback_projection.substitutions.as_ref()
            == prepared.fallback_tool_history_substitutions.as_ref()
    {
        return prepared;
    }

    let projected_items = projection.items;
    let projected_unreplaced_items = projection.unreplaced_items;
    let fallback_projected_items = fallback_projection.items;
    let fallback_projected_unreplaced_items = fallback_projection.unreplaced_items;

    let items = PreparedPromptItems::from_shared(Arc::clone(&projected_items));
    let unreplaced_items = if Arc::ptr_eq(&projected_unreplaced_items, &projected_items) {
        items.clone()
    } else {
        PreparedPromptItems::from_shared(Arc::clone(&projected_unreplaced_items))
    };
    let fallback_items = if Arc::ptr_eq(&fallback_projected_items, &projected_items) {
        items.clone()
    } else if Arc::ptr_eq(&fallback_projected_items, &projected_unreplaced_items) {
        unreplaced_items.clone()
    } else {
        PreparedPromptItems::from_shared(Arc::clone(&fallback_projected_items))
    };
    let unreplaced_fallback_items =
        if Arc::ptr_eq(&fallback_projected_unreplaced_items, &projected_items) {
            items.clone()
        } else if Arc::ptr_eq(
            &fallback_projected_unreplaced_items,
            &projected_unreplaced_items,
        ) {
            unreplaced_items.clone()
        } else if Arc::ptr_eq(
            &fallback_projected_unreplaced_items,
            &fallback_projected_items,
        ) {
            fallback_items.clone()
        } else {
            PreparedPromptItems::from_shared(Arc::clone(&fallback_projected_unreplaced_items))
        };

    prepared.items = items;
    prepared.unreplaced_items = unreplaced_items;
    prepared.tool_history_substitutions = projection.substitutions;
    prepared.fallback_items = fallback_items;
    prepared.unreplaced_fallback_items = unreplaced_fallback_items;
    prepared.fallback_tool_history_substitutions = fallback_projection.substitutions;
    prepared.prompt_provenance = PromptProvenanceSidecar::from_assembled_items(
        prepared.items(),
        &prepared.stable_context_manifest,
    );
    prepared.fingerprint = PreparedHistoryFingerprint::new(
        prepared.items(),
        &prepared.stable_context_manifest,
        prepared.policy,
    )
    .ok();
    prepared.compacted_tool_search_outputs = Arc::new(OnceLock::new());
    prepared
}

#[cfg(test)]
fn prepared_history_fingerprint(
    items: &[ResponseItem],
    manifest: &StableContextManifest,
    policy: PreparedHistoryPolicy,
) -> serde_json::Result<[u8; 32]> {
    PreparedHistoryFingerprint::new(items, manifest, policy).map(|fingerprint| fingerprint.digest)
}

fn hash_prepared_history_items(
    hasher: &mut Sha256,
    items: &[ResponseItem],
) -> serde_json::Result<()> {
    for item in items {
        // Turn IDs are rollout bookkeeping, not model-visible history. Normalize
        // them out so reconstruction and compaction do not churn an otherwise
        // unchanged prompt prefix.
        let mut normalized = item.clone();
        normalized.clear_internal_chat_message_metadata_passthrough();
        let encoded = serde_json::to_vec(&normalized)?;
        hasher.update((encoded.len() as u64).to_be_bytes());
        hasher.update(encoded);
    }
    Ok(())
}

fn weak_ptr_eq_arc<T>(weak: &Weak<T>, strong: &Arc<T>) -> bool {
    std::ptr::eq(weak.as_ptr(), Arc::as_ptr(strong))
}

pub(crate) fn truncate_function_output_payload(
    output: &FunctionCallOutputPayload,
    policy: TruncationPolicy,
) -> FunctionCallOutputPayload {
    let body = match &output.body {
        FunctionCallOutputBody::Text(content) => {
            FunctionCallOutputBody::Text(truncate_text(content, policy))
        }
        FunctionCallOutputBody::ContentItems(items) => FunctionCallOutputBody::ContentItems(
            truncate_function_output_items_with_policy(items, policy),
        ),
    };

    FunctionCallOutputPayload {
        body,
        success: output.success,
    }
}

/// API messages include every non-system item (user/assistant messages, reasoning,
/// tool calls, tool outputs, shell calls, web-search calls, and image-generation
/// calls).
fn is_api_message(message: &ResponseItem) -> bool {
    match message {
        ResponseItem::Message { role, .. } => role.as_str() != "system",
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::CompactionTrigger { .. } => false,
        ResponseItem::Other => false,
    }
}

fn estimate_reasoning_length(encoded_len: usize) -> usize {
    encoded_len
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650)
}

fn estimate_encrypted_function_output_length(encoded_len: usize) -> usize {
    encoded_len.saturating_mul(9).div_ceil(16)
}

pub(crate) fn estimate_item_token_count(item: &ResponseItem) -> i64 {
    let model_visible_bytes = estimate_response_item_model_visible_bytes(item);
    approx_tokens_from_byte_count_i64(model_visible_bytes)
}

/// Approximate model-visible byte cost for one image input.
///
/// The estimator later converts bytes to tokens using a 4-bytes/token heuristic
/// with ceiling division, so 7,373 bytes maps to approximately 1,844 tokens.
const RESIZED_IMAGE_BYTES_ESTIMATE: i64 = 7373;
// See https://platform.openai.com/docs/guides/images-vision#calculating-costs.
// Use a direct 32px patch count only for `detail: "original"`;
// all other image inputs continue to use `RESIZED_IMAGE_BYTES_ESTIMATE`.
const ORIGINAL_IMAGE_PATCH_SIZE: u32 = 32;
// See https://platform.openai.com/docs/guides/images-vision#model-sizing-behavior.
// Keep this hard-coded for now; move it into model capabilities if the patch
// budget starts changing often across model releases.
const ORIGINAL_IMAGE_MAX_PATCHES: usize = 10_000;
const ORIGINAL_IMAGE_ESTIMATE_CACHE_SIZE: usize = 32;

static ORIGINAL_IMAGE_ESTIMATE_CACHE: LazyLock<BlockingLruCache<[u8; 20], Option<i64>>> =
    LazyLock::new(|| {
        BlockingLruCache::new(
            NonZeroUsize::new(ORIGINAL_IMAGE_ESTIMATE_CACHE_SIZE).unwrap_or(NonZeroUsize::MIN),
        )
    });

fn estimate_response_item_model_visible_bytes(item: &ResponseItem) -> i64 {
    match item {
        ResponseItem::Reasoning {
            encrypted_content: Some(content),
            ..
        }
        | ResponseItem::Compaction {
            encrypted_content: content,
            ..
        }
        | ResponseItem::ContextCompaction {
            encrypted_content: Some(content),
            ..
        } => i64::try_from(estimate_reasoning_length(content.len())).unwrap_or(i64::MAX),
        item => {
            let raw = serialized_json_len(item)
                .map(|serialized_len| i64::try_from(serialized_len).unwrap_or(i64::MAX))
                .unwrap_or_default();
            let (image_payload_bytes, image_replacement_bytes) =
                image_data_url_estimate_adjustment(item);
            let (encrypted_payload_bytes, encrypted_replacement_bytes) =
                encrypted_function_output_estimate_adjustment(item);
            // Replace raw base64 payload bytes with a per-image estimate.
            // We intentionally preserve the data URL prefix and JSON
            // wrapper bytes already included in `raw`.
            let raw = raw
                .saturating_sub(image_payload_bytes)
                .saturating_add(image_replacement_bytes);
            raw.saturating_sub(encrypted_payload_bytes)
                .saturating_add(encrypted_replacement_bytes)
        }
    }
}

#[derive(Default)]
struct SerializedByteCounter {
    bytes: usize,
}

impl Write for SerializedByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_json_len<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<usize> {
    let mut counter = SerializedByteCounter::default();
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}

/// Returns the base64 payload byte length for inline image data URLs that are
/// eligible for token-estimation discounting.
///
/// We only discount payloads for `data:image/...;base64,...` URLs (case
/// insensitive markers) and leave everything else at raw serialized size.
fn parse_base64_image_data_url(url: &str) -> Option<&str> {
    if !url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return None;
    }
    let comma_index = url.find(',')?;
    let metadata = &url[..comma_index];
    let payload = &url[comma_index + 1..];
    // Parse the media type and parameters without decoding. This keeps the
    // estimator cheap while ensuring we only apply the fixed-cost image
    // heuristic to image-typed base64 data URLs.
    let metadata_without_scheme = &metadata["data:".len()..];
    let mut metadata_parts = metadata_without_scheme.split(';');
    let mime_type = metadata_parts.next().unwrap_or_default();
    let has_base64_marker = metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type
        .get(.."image/".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    {
        return None;
    }
    if !has_base64_marker {
        return None;
    }
    Some(payload)
}

fn estimate_original_image_bytes(image_url: &str) -> Option<i64> {
    let key = sha1_digest(image_url.as_bytes());
    ORIGINAL_IMAGE_ESTIMATE_CACHE.get_or_insert_with(key, || {
        let payload = match parse_base64_image_data_url(image_url) {
            Some(payload) => payload,
            None => {
                tracing::trace!("skipping original-detail estimate for non-base64 image data URL");
                return None;
            }
        };
        let max_encoded_len = MAX_PROMPT_IMAGE_SOURCE_BYTES
            .saturating_add(2)
            .saturating_div(3)
            .saturating_mul(4);
        if payload.len() > max_encoded_len {
            tracing::trace!(
                payload_bytes = payload.len(),
                "skipping oversized original-detail image estimate"
            );
            return None;
        }
        let bytes = match BASE64_STANDARD.decode(payload) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::trace!("failed to decode original-detail image payload: {error}");
                return None;
            }
        };
        if bytes.len() > MAX_PROMPT_IMAGE_SOURCE_BYTES {
            return None;
        }
        let reader = match image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
            Ok(reader) => reader,
            Err(error) => {
                tracing::trace!("failed to identify original-detail image bytes: {error}");
                return None;
            }
        };
        let dimensions = match reader.into_dimensions() {
            Ok(dimensions) => dimensions,
            Err(error) => {
                tracing::trace!("failed to inspect original-detail image bytes: {error}");
                return None;
            }
        };
        let width = i64::from(dimensions.0);
        let height = i64::from(dimensions.1);
        let patch_size = i64::from(ORIGINAL_IMAGE_PATCH_SIZE);
        let patches_wide = width.saturating_add(patch_size.saturating_sub(1)) / patch_size;
        let patches_high = height.saturating_add(patch_size.saturating_sub(1)) / patch_size;
        let patch_count = patches_wide.saturating_mul(patches_high);
        let patch_count = usize::try_from(patch_count).unwrap_or(usize::MAX);
        let patch_count = patch_count.min(ORIGINAL_IMAGE_MAX_PATCHES);
        Some(i64::try_from(approx_bytes_for_tokens(patch_count)).unwrap_or(i64::MAX))
    })
}

/// Scans one response item for discount-eligible inline image data URLs and
/// returns:
/// - total base64 payload bytes to subtract from raw serialized size
/// - total replacement byte estimate for those images
fn image_data_url_estimate_adjustment(item: &ResponseItem) -> (i64, i64) {
    let mut payload_bytes = 0i64;
    let mut replacement_bytes = 0i64;

    let mut accumulate = |image_url: &str, detail: Option<ImageDetail>| {
        if let Some(payload_len) = parse_base64_image_data_url(image_url).map(str::len) {
            payload_bytes =
                payload_bytes.saturating_add(i64::try_from(payload_len).unwrap_or(i64::MAX));
            replacement_bytes = replacement_bytes.saturating_add(match detail {
                Some(ImageDetail::Original) => {
                    estimate_original_image_bytes(image_url).unwrap_or(RESIZED_IMAGE_BYTES_ESTIMATE)
                }
                _ => RESIZED_IMAGE_BYTES_ESTIMATE,
            });
        }
    };

    match item {
        ResponseItem::Message { content, .. } => {
            for content_item in content {
                if let ContentItem::InputImage { image_url, detail } = content_item {
                    accumulate(image_url, *detail);
                }
            }
        }
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            if let FunctionCallOutputBody::ContentItems(items) = &output.body {
                for content_item in items {
                    if let FunctionCallOutputContentItem::InputImage { image_url, detail } =
                        content_item
                    {
                        accumulate(image_url, *detail);
                    }
                }
            }
        }
        _ => {}
    }

    (payload_bytes, replacement_bytes)
}

fn encrypted_function_output_estimate_adjustment(item: &ResponseItem) -> (i64, i64) {
    let ResponseItem::FunctionCallOutput { output, .. } = item else {
        return (0, 0);
    };
    let FunctionCallOutputBody::ContentItems(items) = &output.body else {
        return (0, 0);
    };

    items.iter().fold((0i64, 0i64), |acc, item| {
        let FunctionCallOutputContentItem::EncryptedContent { encrypted_content } = item else {
            return acc;
        };
        let payload_bytes = acc
            .0
            .saturating_add(i64::try_from(encrypted_content.len()).unwrap_or(i64::MAX));
        let replacement_bytes = acc.1.saturating_add(
            i64::try_from(estimate_encrypted_function_output_length(
                encrypted_content.len(),
            ))
            .unwrap_or(i64::MAX),
        );
        (payload_bytes, replacement_bytes)
    })
}

fn is_model_generated_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role == "assistant",
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::CompactionTrigger { .. } => false,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Other => false,
    }
}

fn is_resolved_reasoning(
    index: usize,
    item: &ResponseItem,
    last_instruction_boundary: Option<usize>,
) -> bool {
    last_instruction_boundary.is_some_and(|boundary| index < boundary)
        && matches!(item, ResponseItem::Reasoning { .. })
}

fn evict_resolved_reasoning(items: &mut Vec<ResponseItem>) {
    let last_instruction_boundary = items.iter().rposition(is_user_turn_boundary);
    let mut index = 0usize;
    items.retain(|item| {
        let retain = !is_resolved_reasoning(index, item, last_instruction_boundary);
        index = index.saturating_add(1);
        retain
    });
}

pub(crate) fn is_user_turn_boundary(item: &ResponseItem) -> bool {
    if matches!(item, ResponseItem::AgentMessage { .. }) {
        return true;
    }
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };

    (role == "user" && !is_contextual_user_message_content(content))
        || (role == "assistant" && is_inter_agent_instruction_content(content))
}

fn is_inter_agent_instruction_content(content: &[ContentItem]) -> bool {
    InterAgentCommunication::is_message_content(content)
}

fn user_message_positions(items: &[ResponseItem]) -> Vec<usize> {
    let mut positions = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if is_user_turn_boundary(item) {
            positions.push(idx);
        }
    }
    positions
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
