use crate::context::ContextualUserFragment;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::normalize;
use crate::event_mapping::has_non_contextual_dev_message_content;
use crate::event_mapping::is_contextual_dev_message_content;
use crate::event_mapping::is_contextual_user_message_content;
use crate::session::turn_context::TurnContext;
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
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_bytes_for_tokens;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::approx_tokens_from_byte_count_i64;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;
use codex_utils_output_truncation::truncate_text;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::LazyLock;

pub(crate) const PROMPT_HISTORY_CANONICAL_POLICY_VERSION: u16 = 1;
const PROMPT_HISTORY_CANONICAL_HASH_DOMAIN: &[u8] = b"codex.websocket.history.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PromptHistoryCanonicalHash {
    pub(crate) item_count: usize,
    pub(crate) digest: [u8; 32],
}

impl PromptHistoryCanonicalHash {
    pub(crate) fn empty() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PROMPT_HISTORY_CANONICAL_HASH_DOMAIN);
        hasher.update(PROMPT_HISTORY_CANONICAL_POLICY_VERSION.to_be_bytes());
        Self {
            item_count: 0,
            digest: hasher.finalize().into(),
        }
    }

    pub(crate) fn from_items(items: &[ResponseItem]) -> serde_json::Result<Self> {
        let mut prefix = Self::empty();
        prefix.extend_items(items)?;
        Ok(prefix)
    }

    pub(crate) fn extend_items(&mut self, items: &[ResponseItem]) -> serde_json::Result<()> {
        for item in items {
            let mut normalized = item.clone();
            normalized.clear_internal_chat_message_metadata_passthrough();
            let serialized = serde_json::to_vec(&normalized)?;
            let mut hasher = Sha256::new();
            hasher.update(PROMPT_HISTORY_CANONICAL_HASH_DOMAIN);
            hasher.update(self.digest);
            hasher.update((serialized.len() as u64).to_be_bytes());
            hasher.update(serialized);
            self.digest = hasher.finalize().into();
            self.item_count = self.item_count.saturating_add(1);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PromptHistoryIncrementalProof {
    pub(crate) suffix: Vec<ResponseItem>,
    pub(crate) current_prefix: PromptHistoryCanonicalHash,
    pub(crate) mutation_revision: u64,
    pub(crate) rewrite_revision: u64,
}

#[derive(Debug)]
struct PromptHistoryProofNode {
    mutation_revision: u64,
    rewrite_revision: u64,
    canonical_prefix: PromptHistoryCanonicalHash,
    parent: Option<Arc<PromptHistoryProofNode>>,
    appended: Arc<[ResponseItem]>,
}

#[derive(Clone, Debug)]
pub(crate) struct PromptHistorySnapshot {
    mutation_revision: u64,
    rewrite_revision: u64,
    raw_item_count: usize,
    rolling_digest: [u8; 32],
    normalized_token_weight: i64,
    segments: Arc<PromptHistorySegments>,
    proof_tail: Arc<PromptHistoryProofNode>,
    #[cfg(test)]
    normalization_work: usize,
}

impl PromptHistorySnapshot {
    pub(crate) fn mutation_revision(&self) -> u64 {
        self.mutation_revision
    }

    pub(crate) fn rewrite_revision(&self) -> u64 {
        self.rewrite_revision
    }

    pub(crate) fn raw_item_count(&self) -> usize {
        self.raw_item_count
    }

    pub(crate) fn rolling_digest(&self) -> &[u8; 32] {
        &self.rolling_digest
    }

    pub(crate) fn normalized_token_weight(&self) -> i64 {
        self.normalized_token_weight
    }

    pub(crate) fn materialize(&self) -> Vec<ResponseItem> {
        self.segments
            .materialize_segments()
            .into_iter()
            .flat_map(|segment| segment.as_ref().to_vec())
            .collect()
    }

    pub(crate) fn canonical_prefix(&self) -> PromptHistoryCanonicalHash {
        self.proof_tail.canonical_prefix
    }

    /// Proves that this snapshot is an append-only extension of a previously
    /// sampled request and returns only the unsent suffix. The provider response
    /// items are verified as the first appended items, so rollback, compaction,
    /// normalization replacements, and reordered history all fail closed.
    pub(crate) fn incremental_proof(
        &self,
        baseline_mutation_revision: u64,
        baseline_rewrite_revision: u64,
        baseline_prefix: PromptHistoryCanonicalHash,
        response_items: &[ResponseItem],
    ) -> Option<PromptHistoryIncrementalProof> {
        if self.rewrite_revision != baseline_rewrite_revision
            || self.mutation_revision < baseline_mutation_revision
        {
            return None;
        }

        let mut nodes = Vec::new();
        let mut cursor = Arc::clone(&self.proof_tail);
        while cursor.mutation_revision > baseline_mutation_revision {
            nodes.push(Arc::clone(&cursor));
            cursor = Arc::clone(cursor.parent.as_ref()?);
        }
        if cursor.mutation_revision != baseline_mutation_revision
            || cursor.rewrite_revision != baseline_rewrite_revision
            || cursor.canonical_prefix != baseline_prefix
        {
            return None;
        }

        let mut expected_response_prefix = baseline_prefix;
        expected_response_prefix.extend_items(response_items).ok()?;
        let mut running_prefix = baseline_prefix;
        let mut response_prefix_verified = response_items.is_empty();
        let mut suffix = Vec::new();
        for node in nodes.into_iter().rev() {
            for item in node.appended.iter() {
                running_prefix
                    .extend_items(std::slice::from_ref(item))
                    .ok()?;
                if !response_prefix_verified {
                    if running_prefix.item_count < expected_response_prefix.item_count {
                        continue;
                    }
                    if running_prefix != expected_response_prefix {
                        return None;
                    }
                    response_prefix_verified = true;
                    continue;
                }
                suffix.push(item.clone());
            }
        }
        if !response_prefix_verified || running_prefix != self.proof_tail.canonical_prefix {
            return None;
        }

        Some(PromptHistoryIncrementalProof {
            suffix,
            current_prefix: running_prefix,
            mutation_revision: self.mutation_revision,
            rewrite_revision: self.rewrite_revision,
        })
    }

    #[cfg(test)]
    pub(crate) fn normalization_work(&self) -> usize {
        self.normalization_work
    }
}

#[derive(Debug)]
enum PromptHistorySegments {
    Base(Arc<[Arc<[ResponseItem]>]>),
    Delta {
        parent: Arc<PromptHistorySegments>,
        replacements: Arc<[(usize, Arc<[ResponseItem]>)]>,
        appended: Arc<[Arc<[ResponseItem]>]>,
    },
}

impl PromptHistorySegments {
    fn materialize_segments(&self) -> Vec<Arc<[ResponseItem]>> {
        let mut deltas = Vec::new();
        let mut current = self;
        let base = loop {
            match current {
                Self::Base(base) => break base,
                Self::Delta {
                    parent,
                    replacements,
                    appended,
                } => {
                    deltas.push((replacements, appended));
                    current = parent.as_ref();
                }
            }
        };
        let mut segments = base.to_vec();
        for (replacements, appended) in deltas.into_iter().rev() {
            for (position, segment) in replacements.iter() {
                segments[*position] = Arc::clone(segment);
            }
            segments.extend(appended.iter().cloned());
        }
        segments
    }
}

#[derive(Clone, Debug)]
struct PromptHistoryCache {
    supports_images: bool,
    rewrite_revision: u64,
    raw_item_count: usize,
    index: normalize::PromptNormalizationIndex,
    segments: Vec<Arc<[ResponseItem]>>,
    snapshot_segments: Arc<PromptHistorySegments>,
    proof_tail: Arc<PromptHistoryProofNode>,
    normalized_token_weight: i64,
}

/// Transcript of thread history
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextManager {
    /// The oldest items are at the beginning of the vector.
    items: Vec<ResponseItem>,
    /// Bumped whenever history is rewritten, such as compaction or rollback.
    history_version: u64,
    /// Bumped for every model-visible history mutation, including appends.
    mutation_revision: u64,
    /// Append-friendly identity for the complete raw model-visible history.
    rolling_digest: [u8; 32],
    /// Cached prompt normalization. Rewrites discard it; appends update only
    /// newly added and newly paired segments.
    prompt_cache: Option<PromptHistoryCache>,
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
    reference_context_item: Option<TurnContextItem>,
    /// World state most recently appended to model-visible history.
    world_state_baseline: Option<WorldStateSnapshot>,
}

impl ContextManager {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            history_version: 0,
            mutation_revision: 0,
            rolling_digest: [0; 32],
            prompt_cache: None,
            token_info: TokenUsageInfo::new_or_append(
                &None, &None, /*model_context_window*/ None,
            ),
            reference_context_item: None,
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
        self.reference_context_item = item;
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        self.reference_context_item.clone()
    }

    pub(crate) fn update_world_state(
        &mut self,
        world_state: &WorldState,
    ) -> (Vec<Box<dyn ContextualUserFragment>>, Option<WorldStateItem>) {
        let snapshot = world_state.snapshot();
        let fragments =
            world_state.render_history_diff(self.world_state_baseline.as_ref(), &self.items);
        let rollout_item = self.world_state_baseline.as_ref().map_or_else(
            || Some(WorldStateItem::full(snapshot.clone().into_value())),
            |previous| {
                snapshot
                    .merge_patch_from(previous)
                    .map(WorldStateItem::patch)
            },
        );
        self.world_state_baseline = Some(snapshot);
        (fragments, rollout_item)
    }

    pub(crate) fn set_world_state_baseline(&mut self, snapshot: WorldStateSnapshot) {
        self.world_state_baseline = Some(snapshot);
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
        for item in items {
            let item_ref = item.deref();
            if !is_api_message(item_ref) {
                continue;
            }

            let processed = self.process_item(item_ref, policy);
            self.rolling_digest = append_rolling_history_digest(self.rolling_digest, &processed);
            self.items.push(processed);
            self.mutation_revision = self.mutation_revision.saturating_add(1);
        }
    }

    /// Returns the history prepared for sending to the model. This applies a proper
    /// normalization and drops un-suited items. When `input_modalities` does not
    /// include `InputModality::Image`, images are stripped from messages and tool
    /// outputs.
    pub(crate) fn for_prompt(mut self, input_modalities: &[InputModality]) -> Vec<ResponseItem> {
        self.normalize_history(input_modalities);
        self.items
    }

    /// Returns raw items in the history.
    pub(crate) fn raw_items(&self) -> &[ResponseItem] {
        &self.items
    }

    /// Returns raw items in the history and consumes the snapshot.
    pub(crate) fn into_raw_items(self) -> Vec<ResponseItem> {
        self.items
    }

    pub(crate) fn history_version(&self) -> u64 {
        self.history_version
    }

    pub(crate) fn mutation_revision(&self) -> u64 {
        self.mutation_revision
    }

    pub(crate) fn prompt_snapshot(
        &mut self,
        input_modalities: &[InputModality],
    ) -> PromptHistorySnapshot {
        let supports_images = input_modalities.contains(&InputModality::Image);
        let can_extend = self.prompt_cache.as_ref().is_some_and(|cache| {
            cache.supports_images == supports_images
                && cache.rewrite_revision == self.history_version
                && cache.raw_item_count <= self.items.len()
        });

        let normalization_work = if can_extend {
            self.extend_prompt_cache(input_modalities)
        } else {
            self.rebuild_prompt_cache(input_modalities)
        };
        #[cfg(not(test))]
        let _ = normalization_work;
        let cache = self
            .prompt_cache
            .as_ref()
            .expect("prompt cache is initialized above");
        PromptHistorySnapshot {
            mutation_revision: self.mutation_revision,
            rewrite_revision: self.history_version,
            raw_item_count: self.items.len(),
            rolling_digest: self.rolling_digest,
            normalized_token_weight: cache.normalized_token_weight,
            segments: Arc::clone(&cache.snapshot_segments),
            proof_tail: Arc::clone(&cache.proof_tail),
            #[cfg(test)]
            normalization_work,
        }
    }

    // Estimate token usage using byte-based heuristics from the truncation helpers.
    // This is a coarse lower bound, not a tokenizer-accurate count.
    pub(crate) fn estimate_token_count(&self, turn_context: &TurnContext) -> Option<i64> {
        let model_info = &turn_context.model_info;
        let personality = turn_context.personality.or(turn_context.config.personality);
        let base_instructions = BaseInstructions {
            text: model_info.get_model_instructions(personality),
        };
        self.estimate_token_count_with_base_instructions(&base_instructions)
    }

    pub(crate) fn estimate_token_count_with_base_instructions(
        &self,
        base_instructions: &BaseInstructions,
    ) -> Option<i64> {
        let base_tokens = estimate_base_instruction_token_count(base_instructions);

        let items_tokens = self
            .items
            .iter()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add);

        Some(base_tokens.saturating_add(items_tokens))
    }

    pub(crate) fn remove_first_item(&mut self) {
        if !self.items.is_empty() {
            // Remove the oldest item (front of the list). Items are ordered from
            // oldest → newest, so index 0 is the first entry recorded.
            let removed = self.items.remove(0);
            // If the removed item participates in a call/output pair, also remove
            // its corresponding counterpart to keep the invariants intact without
            // running a full normalization pass.
            normalize::remove_corresponding_for(&mut self.items, &removed);
            self.record_rewrite(/*rewrite_count*/ 1);
            self.world_state_baseline = None;
        }
    }

    pub(crate) fn replace(&mut self, items: Vec<ResponseItem>) {
        self.items = items;
        self.record_rewrite(/*rewrite_count*/ 1);
        self.world_state_baseline = None;
    }

    pub(crate) fn replace_with_rewrite_count(
        &mut self,
        items: Vec<ResponseItem>,
        rewrite_count: usize,
    ) {
        self.items = items;
        self.record_rewrite(rewrite_count);
        self.world_state_baseline = None;
    }

    /// Replace image content in the last turn if it originated from a tool output.
    /// Returns true when a tool image was replaced, false otherwise.
    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str) -> bool {
        let Some(index) = self.items.iter().rposition(|item| {
            matches!(item, ResponseItem::FunctionCallOutput { .. }) || is_user_turn_boundary(item)
        }) else {
            return false;
        };

        match &mut self.items[index] {
            ResponseItem::FunctionCallOutput { output, .. } => {
                let Some(content_items) = output.content_items_mut() else {
                    return false;
                };
                let mut replaced = false;
                let placeholder = placeholder.to_string();
                for item in content_items.iter_mut() {
                    if matches!(item, FunctionCallOutputContentItem::InputImage { .. }) {
                        *item = FunctionCallOutputContentItem::InputText {
                            text: placeholder.clone(),
                        };
                        replaced = true;
                    }
                }
                if replaced {
                    self.record_rewrite(/*rewrite_count*/ 1);
                }
                replaced
            }
            ResponseItem::Message { .. } => false,
            _ => false,
        }
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
            self.replace(snapshot);
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
        // Get reasoning items excluding all the ones after the last instruction boundary.
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

    /// When true, the server already accounted for past reasoning tokens and
    /// the client should not re-estimate them.
    pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        let last_tokens = self
            .token_info
            .as_ref()
            .map(|info| info.last_token_usage.total_tokens)
            .unwrap_or(0);
        let items_after_last_model_generated_tokens = self
            .items_after_last_model_generated_item()
            .iter()
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add);
        if server_reasoning_included {
            last_tokens.saturating_add(items_after_last_model_generated_tokens)
        } else {
            last_tokens
                .saturating_add(self.get_non_last_reasoning_items_tokens())
                .saturating_add(items_after_last_model_generated_tokens)
        }
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
        // all function/tool calls must have a corresponding output
        normalize::ensure_call_outputs_present(&mut self.items);

        // all outputs must have a corresponding function/tool call
        normalize::remove_orphan_outputs(&mut self.items);

        // strip images when model does not support them
        normalize::strip_images_when_unsupported(input_modalities, &mut self.items);
    }

    fn rebuild_prompt_cache(&mut self, input_modalities: &[InputModality]) -> usize {
        let index = normalize::PromptNormalizationIndex::from_items(&self.items);
        let segments = self
            .items
            .iter()
            .map(|item| Arc::from(index.normalize_segment(item, input_modalities)))
            .collect::<Vec<Arc<[ResponseItem]>>>();
        let normalized_token_weight = segments
            .iter()
            .flat_map(|segment| segment.iter())
            .map(estimate_item_token_count)
            .fold(0i64, i64::saturating_add);
        let normalization_work = self.items.len();
        let snapshot_segments = Arc::new(PromptHistorySegments::Base(segments.clone().into()));
        let canonical_prefix = canonical_hash_from_segments(&segments)
            .unwrap_or_else(|_| PromptHistoryCanonicalHash::empty());
        let proof_tail = Arc::new(PromptHistoryProofNode {
            mutation_revision: self.mutation_revision,
            rewrite_revision: self.history_version,
            canonical_prefix,
            parent: None,
            appended: Arc::from([]),
        });
        self.prompt_cache = Some(PromptHistoryCache {
            supports_images: input_modalities.contains(&InputModality::Image),
            rewrite_revision: self.history_version,
            raw_item_count: self.items.len(),
            index,
            segments,
            snapshot_segments,
            proof_tail,
            normalized_token_weight,
        });
        normalization_work
    }

    fn extend_prompt_cache(&mut self, input_modalities: &[InputModality]) -> usize {
        let mutation_revision = self.mutation_revision;
        let rewrite_revision = self.history_version;
        let cache = self
            .prompt_cache
            .as_mut()
            .expect("append extension requires an existing prompt cache");
        let start = cache.raw_item_count;
        if start == self.items.len() {
            return 0;
        }

        let mut affected = HashSet::new();
        for item in &self.items[start..] {
            affected.extend(cache.index.affected_positions(item));
        }
        for (position, item) in self.items[start..].iter().enumerate() {
            cache.index.insert(start + position, item);
        }

        let mut affected = affected.into_iter().collect::<Vec<_>>();
        affected.sort_unstable();
        let mut normalization_work = 0usize;
        let mut replacements = Vec::with_capacity(affected.len());
        for position in affected {
            let old_weight = cache.segments[position]
                .iter()
                .map(estimate_item_token_count)
                .fold(0i64, i64::saturating_add);
            let segment: Arc<[ResponseItem]> = cache
                .index
                .normalize_segment(&self.items[position], input_modalities)
                .into();
            let new_weight = segment
                .iter()
                .map(estimate_item_token_count)
                .fold(0i64, i64::saturating_add);
            cache.normalized_token_weight = cache
                .normalized_token_weight
                .saturating_sub(old_weight)
                .saturating_add(new_weight);
            cache.segments[position] = segment;
            replacements.push((position, Arc::clone(&cache.segments[position])));
            normalization_work = normalization_work.saturating_add(1);
        }

        let mut appended = Vec::with_capacity(self.items.len().saturating_sub(start));
        for item in &self.items[start..] {
            let segment: Arc<[ResponseItem]> =
                cache.index.normalize_segment(item, input_modalities).into();
            cache.normalized_token_weight = segment
                .iter()
                .map(estimate_item_token_count)
                .fold(cache.normalized_token_weight, i64::saturating_add);
            appended.push(Arc::clone(&segment));
            cache.segments.push(segment);
            normalization_work = normalization_work.saturating_add(1);
        }
        let appended_items = appended
            .iter()
            .flat_map(|segment| segment.iter().cloned())
            .collect::<Vec<_>>();
        cache.snapshot_segments = Arc::new(PromptHistorySegments::Delta {
            parent: Arc::clone(&cache.snapshot_segments),
            replacements: replacements.into(),
            appended: appended.into(),
        });
        cache.proof_tail = if let PromptHistorySegments::Delta { replacements, .. } =
            cache.snapshot_segments.as_ref()
            && replacements.is_empty()
        {
            let mut canonical_prefix = cache.proof_tail.canonical_prefix;
            if canonical_prefix.extend_items(&appended_items).is_ok() {
                Arc::new(PromptHistoryProofNode {
                    mutation_revision,
                    rewrite_revision,
                    canonical_prefix,
                    parent: Some(Arc::clone(&cache.proof_tail)),
                    appended: appended_items.into(),
                })
            } else {
                rebuild_prompt_history_proof(mutation_revision, rewrite_revision, &cache.segments)
            }
        } else {
            rebuild_prompt_history_proof(mutation_revision, rewrite_revision, &cache.segments)
        };
        cache.raw_item_count = self.items.len();
        normalization_work
    }

    fn record_rewrite(&mut self, rewrite_count: usize) {
        let rewrite_count = u64::try_from(rewrite_count).unwrap_or(u64::MAX);
        self.history_version = self.history_version.saturating_add(rewrite_count);
        self.mutation_revision = self.mutation_revision.saturating_add(rewrite_count.max(1));
        self.rolling_digest = rolling_history_digest(&self.items);
        self.prompt_cache = None;
    }

    fn process_item(&self, item: &ResponseItem, policy: TruncationPolicy) -> ResponseItem {
        let policy_with_serialization_budget = policy * 1.2;
        match item {
            ResponseItem::FunctionCallOutput {
                id,
                call_id,
                output,
                internal_chat_message_metadata_passthrough: metadata,
            } => ResponseItem::FunctionCallOutput {
                id: id.clone(),
                call_id: call_id.clone(),
                output: truncate_function_output_payload(output, policy_with_serialization_budget),
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
                output: truncate_function_output_payload(output, policy_with_serialization_budget),
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
                        self.reference_context_item = None;
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

pub(crate) fn truncate_function_output_payload(
    output: &FunctionCallOutputPayload,
    policy: TruncationPolicy,
) -> FunctionCallOutputPayload {
    let body = match &output.body {
        FunctionCallOutputBody::Text(content) if is_evidence_aware_shell_output(content) => {
            FunctionCallOutputBody::Text(content.clone())
        }
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

fn is_evidence_aware_shell_output(content: &str) -> bool {
    content.contains("Shell output summary:\n")
        || (content.contains("\n[+") && content.contains("B/") && content.contains("L]\n"))
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

pub(crate) fn estimate_base_instruction_token_count(base_instructions: &BaseInstructions) -> i64 {
    i64::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i64::MAX)
}

pub(crate) fn estimate_item_token_count(item: &ResponseItem) -> i64 {
    let model_visible_bytes = estimate_response_item_model_visible_bytes(item);
    approx_tokens_from_byte_count_i64(model_visible_bytes)
}

fn canonical_hash_from_segments(
    segments: &[Arc<[ResponseItem]>],
) -> serde_json::Result<PromptHistoryCanonicalHash> {
    let mut prefix = PromptHistoryCanonicalHash::empty();
    for segment in segments {
        prefix.extend_items(segment)?;
    }
    Ok(prefix)
}

fn rebuild_prompt_history_proof(
    mutation_revision: u64,
    rewrite_revision: u64,
    segments: &[Arc<[ResponseItem]>],
) -> Arc<PromptHistoryProofNode> {
    let canonical_prefix = canonical_hash_from_segments(segments)
        .unwrap_or_else(|_| PromptHistoryCanonicalHash::empty());
    Arc::new(PromptHistoryProofNode {
        mutation_revision,
        rewrite_revision,
        canonical_prefix,
        parent: None,
        appended: Arc::from([]),
    })
}

fn rolling_history_digest(items: &[ResponseItem]) -> [u8; 32] {
    items.iter().fold([0; 32], |digest, item| {
        append_rolling_history_digest(digest, item)
    })
}

fn append_rolling_history_digest(previous: [u8; 32], item: &ResponseItem) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"codex.history.rolling.v1");
    hasher.update(previous);
    match serde_json::to_vec(item) {
        Ok(serialized) => {
            hasher.update((serialized.len() as u64).to_be_bytes());
            hasher.update(serialized);
        }
        Err(error) => {
            hasher.update(b"serialization-error");
            hasher.update(error.to_string().as_bytes());
        }
    }
    hasher.finalize().into()
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
            let raw = serde_json::to_string(item)
                .map(|serialized| i64::try_from(serialized.len()).unwrap_or(i64::MAX))
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
        let bytes = match BASE64_STANDARD.decode(payload) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::trace!("failed to decode original-detail image payload: {error}");
                return None;
            }
        };
        let dynamic = match image::load_from_memory(&bytes) {
            Ok(dynamic) => dynamic,
            Err(error) => {
                tracing::trace!("failed to decode original-detail image bytes: {error}");
                return None;
            }
        };
        let width = i64::from(dynamic.width());
        let height = i64::from(dynamic.height());
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
