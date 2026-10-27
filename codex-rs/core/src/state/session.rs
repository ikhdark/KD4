//! Session-wide mutable state.

use codex_protocol::models::ResponseItem;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_sandboxing::policy_transforms::merge_uri_permission_profiles;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use super::AdditionalContextStore;
use super::auto_compact_window::AutoCompactWindow;
use super::auto_compact_window::AutoCompactWindowIds;
use super::auto_compact_window::AutoCompactWindowSnapshot;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::ContextManager;
use crate::session::PreviousTurnSettings;
use crate::session::session::SessionConfiguration;
use crate::session::time_reminder::CurrentTimeReminderState;
use crate::session_startup_prewarm::SessionStartupPrewarmHandle;
use crate::session_startup_prewarm::SessionStartupTransportHandle;
use crate::tool_history::ModelGenerationId;
use crate::tool_history::ToolHistoryState;
use codex_protocol::protocol::ContextFragmentDigest;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;

#[derive(Clone)]
pub(crate) struct ContextBaselineCandidate {
    pub(crate) turn_context_item: TurnContextItem,
    pub(crate) world_state_snapshot: WorldStateSnapshot,
    pub(crate) world_state_item: Option<WorldStateItem>,
    pub(crate) fragment_digests: Vec<ContextFragmentDigest>,
    pub(crate) bound_sampling_request_id: Option<String>,
    pub(crate) bound_physical_attempt_id: Option<String>,
}
use codex_utils_output_truncation::TruncationPolicy;

/// Ephemeral calibration: resume starts with the local estimator until a fresh
/// completed request supplies a matching provider measurement. The largest of
/// four recent ratios avoids reacting optimistically to one cheap prompt.
#[derive(Default)]
pub(crate) struct PromptTokenCalibration {
    basis: Option<(String, String)>,
    samples: VecDeque<(u64, u64)>,
    covered_prompt: Option<crate::client_common::PromptCalibrationBasis>,
    observed_input_tokens: Option<i64>,
}

impl PromptTokenCalibration {
    pub(crate) fn observe_prompt(
        &mut self,
        provider: &str,
        model: &str,
        prompt: crate::client_common::PromptCalibrationBasis,
        measured: Option<u64>,
        observed_input: Option<u64>,
    ) {
        if self
            .covered_prompt
            .as_ref()
            .is_none_or(|previous| !prompt.extends(previous))
        {
            self.samples.clear();
        }
        self.observe(
            provider,
            model,
            measured.map(|measured| (prompt.local_tokens.max(0) as u64, measured)),
        );
        self.observed_input_tokens =
            observed_input.map(|tokens| i64::try_from(tokens).unwrap_or(i64::MAX));
        self.covered_prompt = Some(prompt);
    }

    pub(crate) fn estimate_prompt(
        &self,
        provider: &str,
        model: &str,
        prompt: &crate::client_common::PromptCalibrationBasis,
    ) -> i64 {
        let Some(previous) = self
            .covered_prompt
            .as_ref()
            .filter(|previous| prompt.extends(previous))
        else {
            return prompt.local_tokens;
        };
        // Only the observed prefix is calibrated. Newly appended content has
        // independent cost, and changed schemas/projections cannot reuse it.
        let observed = self
            .basis
            .as_ref()
            .filter(|(p, m)| p == provider && m == model)
            .and(self.observed_input_tokens)
            .unwrap_or(0);
        self.estimate_ratio(provider, model, previous.local_tokens)
            .max(observed)
            .saturating_add(
                prompt
                    .local_tokens
                    .saturating_sub(previous.local_tokens)
                    .max(0),
            )
    }

    pub(crate) fn observe(&mut self, provider: &str, model: &str, sample: Option<(u64, u64)>) {
        if self
            .basis
            .as_ref()
            .is_none_or(|(p, m)| p != provider || m != model)
        {
            self.samples.clear();
            self.basis = Some((provider.to_string(), model.to_string()));
        }
        let Some((local, measured)) = sample.filter(|(local, measured)| {
            *local > 0 && *local <= i64::MAX as u64 && *measured > 0 && *measured <= i64::MAX as u64
        }) else {
            self.samples.clear();
            return;
        };
        self.samples.push_back((local, measured));
        if self.samples.len() > 4 {
            self.samples.pop_front();
        }
    }

    pub(crate) fn estimate(&self, provider: &str, model: &str, local: i64) -> i64 {
        // A bare scalar has no representation identity. Only prepared prompts
        // can reuse production calibration, including downward calibration.
        if self.covered_prompt.is_some() {
            return local;
        }
        self.estimate_ratio(provider, model, local)
    }

    fn estimate_ratio(&self, provider: &str, model: &str, local: i64) -> i64 {
        if self
            .basis
            .as_ref()
            .is_none_or(|(p, m)| p != provider || m != model)
        {
            return local;
        }
        let Some(&(denominator, numerator)) = self.samples.iter().max_by(|(al, ap), (bl, bp)| {
            (u128::from(*ap) * u128::from(*bl)).cmp(&(u128::from(*bp) * u128::from(*al)))
        }) else {
            return local;
        };
        let adjusted =
            (local.max(0) as u128 * u128::from(numerator)).div_ceil(u128::from(denominator));
        i64::try_from(adjusted).unwrap_or(i64::MAX)
    }
}

/// Usage of the latest compaction request, with the stored cumulative and
/// conversation usage it was counted into. Compaction never becomes the
/// context baseline, yet it stays the latest request until either changes.
struct CompactionReceipt {
    total_token_usage: TokenUsage,
    last_token_usage: TokenUsage,
    usage: TokenUsage,
}

/// Persistent, session-scoped state previously stored directly on `Session`.
pub(crate) struct SessionState {
    pub(crate) session_configuration: SessionConfiguration,
    pub(crate) history: ContextManager,
    rate_limits: HashMap<String, RateLimitSnapshot>,
    active_rate_limit_id: Option<String>,
    rate_limit_account: Option<crate::responses_retry::AccountChangeIdentity>,
    /// Latest compaction request usage; see `published_token_info`.
    latest_compaction_receipt: Option<CompactionReceipt>,
    /// Usage and rate limits carried by the last published `TokenCount`.
    last_published_token_count: Option<(Option<TokenUsageInfo>, Option<RateLimitSnapshot>)>,
    pub(crate) prompt_token_calibration: PromptTokenCalibration,
    /// Estimated retirable tokens re-sent by each request checked since history
    /// was last rewritten; see `retire_consumed_tool_outputs_before_limit`.
    pub(crate) proactive_retirement_carried_tokens: u64,
    pub(crate) mcp_dependency_prompted: HashSet<String>,
    pub(crate) additional_context: AdditionalContextStore,
    /// Settings used by the latest regular user turn, used for turn-to-turn
    /// model handling on subsequent regular turns (including full-context
    /// reinjection after resume or `/compact`).
    previous_turn_settings: Option<PreviousTurnSettings>,
    /// Runtime accounting state for the active auto-compaction window.
    auto_compact_window: AutoCompactWindow,
    /// Startup prewarmed session prepared during session initialization.
    pub(crate) startup_prewarm: Option<SessionStartupPrewarmHandle>,
    /// Transport-only preconnect scheduled before MCP/tool initialization.
    pub(crate) startup_transport: Option<SessionStartupTransportHandle>,
    pub(crate) current_time_reminder: CurrentTimeReminderState,
    pub(crate) active_connector_selection: HashSet<String>,
    pub(crate) pending_session_start_sources: VecDeque<codex_hooks::SessionStartSource>,
    granted_permissions_by_approval_scope_id: HashMap<String, UriAdditionalPermissionProfile>,
    next_turn_is_first: bool,
    pending_context_baseline: Option<ContextBaselineCandidate>,
}

impl SessionState {

    pub(crate) fn new_with_auto_compact_window_ids(
        session_configuration: SessionConfiguration,
        auto_compact_window_ids: AutoCompactWindowIds,
    ) -> Self {
        let history = ContextManager::new();
        Self {
            session_configuration,
            history,
            rate_limits: HashMap::new(),
            active_rate_limit_id: None,
            rate_limit_account: None,
            latest_compaction_receipt: None,
            last_published_token_count: None,
            prompt_token_calibration: PromptTokenCalibration::default(),
            proactive_retirement_carried_tokens: 0,
            mcp_dependency_prompted: HashSet::new(),
            additional_context: AdditionalContextStore::default(),
            previous_turn_settings: None,
            auto_compact_window: AutoCompactWindow::new_with_ids(auto_compact_window_ids),
            startup_prewarm: None,
            startup_transport: None,
            current_time_reminder: CurrentTimeReminderState::default(),
            active_connector_selection: HashSet::new(),
            pending_session_start_sources: VecDeque::new(),
            granted_permissions_by_approval_scope_id: HashMap::new(),
            next_turn_is_first: true,
            pending_context_baseline: None,
        }
    }

    // History helpers
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator,
        I::Item: std::ops::Deref<Target = ResponseItem>,
    {
        self.history.record_items(items, policy);
    }

    pub(crate) fn previous_turn_settings(&self) -> Option<PreviousTurnSettings> {
        self.previous_turn_settings.clone()
    }
    pub(crate) fn set_previous_turn_settings(
        &mut self,
        previous_turn_settings: Option<PreviousTurnSettings>,
    ) {
        self.previous_turn_settings = previous_turn_settings;
    }

    pub(crate) fn set_next_turn_is_first(&mut self, value: bool) {
        self.next_turn_is_first = value;
    }

    pub(crate) fn take_next_turn_is_first(&mut self) -> bool {
        let is_first_turn = self.next_turn_is_first;
        self.next_turn_is_first = false;
        is_first_turn
    }

    pub(crate) fn clone_history(&self) -> ContextManager {
        self.history.clone()
    }

    pub(crate) fn set_tool_history_state(&mut self, state: ToolHistoryState) {
        self.history.set_tool_history_state(state);
    }

    pub(crate) fn tool_history_state(&self) -> ToolHistoryState {
        self.history.tool_history_state()
    }

    pub(crate) fn tool_history_state_ref(&self) -> &ToolHistoryState {
        self.history.tool_history_state_ref()
    }

    pub(crate) fn apply_tool_history_mutation(
        &mut self,
        mutation: &crate::tool_history::ToolHistoryMutation,
    ) -> bool {
        self.history.apply_tool_history_mutation(mutation)
    }

    pub(crate) fn mark_tool_history_consumed_with_delta(
        &mut self,
        input: &[ResponseItem],
        generation: ModelGenerationId,
    ) -> std::collections::BTreeSet<String> {
        self.history
            .mark_tool_history_consumed_with_delta(input, generation)
    }

    pub(crate) fn replace_history(
        &mut self,
        items: Vec<ResponseItem>,
        reference_context_item: Option<TurnContextItem>,
    ) {
        self.additional_context.reconcile_history(&items);
        self.history.replace(items);
        self.history
            .set_reference_context_item(reference_context_item);
        self.auto_compact_window.clear_prefill();
        // Compaction and rollback change the prompt composition. Ratios measured
        // against discarded history must not inflate the replacement checkpoint.
        self.prompt_token_calibration = PromptTokenCalibration::default();
        // The replacement already rewrote the prompt, so carrying restarts.
        self.proactive_retirement_carried_tokens = 0;
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.history.set_token_info(info);
    }

    pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>) {
        self.history.set_reference_context_item(item);
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        self.history.reference_context_item()
    }

    pub(crate) fn has_reference_context_item(&self) -> bool {
        self.history.has_reference_context_item()
    }

    pub(crate) fn stage_context_baseline(&mut self, candidate: ContextBaselineCandidate) {
        self.pending_context_baseline = Some(candidate);
    }

    pub(crate) fn pending_context_baseline(&self) -> Option<ContextBaselineCandidate> {
        self.pending_context_baseline.clone()
    }

    pub(crate) fn pending_context_baseline_ref(&self) -> Option<&ContextBaselineCandidate> {
        self.pending_context_baseline.as_ref()
    }

    pub(crate) fn pending_context_baseline_mut(&mut self) -> Option<&mut ContextBaselineCandidate> {
        self.pending_context_baseline.as_mut()
    }

    pub(crate) fn clear_pending_context_baseline(&mut self) {
        self.pending_context_baseline = None;
    }

    // Token/rate limit helpers
    pub(crate) fn update_token_info_from_usage(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
    ) {
        self.history.update_token_info(usage, model_context_window);
    }

    pub(crate) fn ensure_auto_compact_window_server_prefill_from_usage(
        &mut self,
        usage: &TokenUsage,
    ) {
        self.auto_compact_window
            .ensure_server_observed_prefill_from_usage(usage);
    }

    pub(crate) fn set_auto_compact_window_estimated_prefill(&mut self, tokens: i64) {
        self.auto_compact_window.set_estimated_prefill(tokens);
    }

    pub(crate) fn auto_compact_window_snapshot(&self) -> AutoCompactWindowSnapshot {
        self.auto_compact_window.snapshot()
    }

    pub(crate) fn auto_compact_window_server_observed_prefill(&self) -> Option<i64> {
        self.auto_compact_window
            .server_observed_prefill_input_tokens()
    }

    pub(crate) fn auto_compact_window_number(&self) -> u64 {
        self.auto_compact_window.window_number()
    }

    pub(crate) fn auto_compact_window_ids(&self) -> AutoCompactWindowIds {
        self.auto_compact_window.ids()
    }

    pub(crate) fn restore_auto_compact_window(
        &mut self,
        window_number: u64,
        ids: AutoCompactWindowIds,
    ) {
        self.auto_compact_window.restore(window_number, ids);
    }


    pub(crate) fn next_auto_compact_window(&self) -> (u64, AutoCompactWindowIds) {
        self.auto_compact_window.clone().advance()
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.history.token_info()
    }

    /// Records compaction usage after it was added to the cumulative total
    /// without replacing the context baseline.
    pub(crate) fn record_compaction_receipt(&mut self, usage: &TokenUsage) {
        self.latest_compaction_receipt = self.token_info().map(|info| CompactionReceipt {
            total_token_usage: info.total_token_usage,
            last_token_usage: info.last_token_usage,
            usage: usage.clone(),
        });
    }

    /// Usage as published to clients, rollouts, and usage contributors. History
    /// keeps the conversation receipt as its context baseline, but a compaction
    /// is reported as the latest request until another receipt is recorded.
    pub(crate) fn published_token_info(&self) -> Option<TokenUsageInfo> {
        let mut info = self.token_info()?;
        if let Some(receipt) = &self.latest_compaction_receipt
            && receipt.total_token_usage == info.total_token_usage
            && receipt.last_token_usage == info.last_token_usage
        {
            // Keep the conversation's context size independent of the receipt.
            info.estimated_context_tokens = Some(info.context_tokens());
            info.last_token_usage = receipt.usage.clone();
        }
        Some(info)
    }

    pub(crate) fn bind_rate_limit_account(
        &mut self,
        account: Option<crate::responses_retry::AccountChangeIdentity>,
    ) {
        if self.rate_limit_account != account {
            self.rate_limits.clear();
            self.active_rate_limit_id = None;
            self.rate_limit_account = account;
        }
    }

    pub(crate) fn set_rate_limits(&mut self, mut snapshot: RateLimitSnapshot) {
        let limit_id = snapshot.limit_id.as_deref().map(str::trim)
            .filter(|id| !id.is_empty()).unwrap_or("codex").to_ascii_lowercase().replace('-', "_");
        snapshot.limit_id = Some(limit_id.clone());
        // Credits are account-wide; bucket windows and policy fields are not.
        let credits = snapshot.credits.clone().or_else(|| self.active_rate_limit_id
            .as_ref().and_then(|id| self.rate_limits.get(id)).and_then(|prior| prior.credits.clone()));
        let mut snapshot = merge_rate_limit_fields(self.rate_limits.get(&limit_id), snapshot);
        snapshot.credits = credits.clone();
        for prior in self.rate_limits.values_mut() {
            prior.credits = credits.clone();
        }
        self.rate_limits.insert(limit_id.clone(), snapshot);
        self.active_rate_limit_id = Some(limit_id);
    }

    /// The snapshot a `TokenCount` publishes; see `published_token_info`.
    pub(crate) fn token_info_and_rate_limits(
        &self,
    ) -> (Option<TokenUsageInfo>, Option<RateLimitSnapshot>) {
        (self.published_token_info(), self.active_rate_limit_id.as_ref()
            .and_then(|id| self.rate_limits.get(id)).cloned())
    }

    /// Whether `event` exactly repeats the last published `TokenCount`.
    pub(crate) fn repeats_published_token_count(&self, event: &TokenCountEvent) -> bool {
        self.last_published_token_count
            .as_ref()
            .is_some_and(|(info, rate_limits)| {
                *info == event.info && *rate_limits == event.rate_limits
            })
    }

    pub(crate) fn record_published_token_count(&mut self, event: &TokenCountEvent) {
        self.last_published_token_count = Some((event.info.clone(), event.rate_limits.clone()));
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        self.history.set_token_usage_full(context_window);
    }

    pub(crate) fn record_mcp_dependency_prompted<I>(&mut self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.mcp_dependency_prompted.extend(names);
    }

    pub(crate) fn mcp_dependency_prompted(&self) -> HashSet<String> {
        self.mcp_dependency_prompted.clone()
    }

    pub(crate) fn set_session_startup_prewarm(
        &mut self,
        startup_prewarm: SessionStartupPrewarmHandle,
    ) {
        self.startup_prewarm = Some(startup_prewarm);
    }

    pub(crate) fn take_session_startup_prewarm(&mut self) -> Option<SessionStartupPrewarmHandle> {
        self.startup_prewarm.take()
    }

    pub(crate) fn set_session_startup_transport(
        &mut self,
        startup_transport: SessionStartupTransportHandle,
    ) {
        self.startup_transport = Some(startup_transport);
    }

    pub(crate) fn take_session_startup_transport(
        &mut self,
    ) -> Option<SessionStartupTransportHandle> {
        self.startup_transport.take()
    }

    // Adds connector IDs to the active set and returns the merged selection.
    pub(crate) fn merge_connector_selection<I>(&mut self, connector_ids: I) -> HashSet<String>
    where
        I: IntoIterator<Item = String>,
    {
        self.active_connector_selection.extend(connector_ids);
        self.active_connector_selection.clone()
    }

    // Returns the current connector selection tracked on session state.
    pub(crate) fn get_connector_selection(&self) -> HashSet<String> {
        self.active_connector_selection.clone()
    }

    // Removes all currently tracked connector selections.
    pub(crate) fn clear_connector_selection(&mut self) {
        self.active_connector_selection.clear();
    }

    pub(crate) fn queue_pending_session_start_source(
        &mut self,
        value: codex_hooks::SessionStartSource,
    ) {
        self.pending_session_start_sources.push_back(value);
    }

    pub(crate) fn take_pending_session_start_source(
        &mut self,
    ) -> Option<codex_hooks::SessionStartSource> {
        self.pending_session_start_sources.pop_front()
    }

    pub(crate) fn record_granted_permissions(
        &mut self,
        approval_scope_id: &str,
        permissions: UriAdditionalPermissionProfile,
    ) {
        let granted_permissions = merge_uri_permission_profiles(
            self.granted_permissions_by_approval_scope_id
                .get(approval_scope_id),
            Some(&permissions),
        );
        if let Some(granted_permissions) = granted_permissions {
            self.granted_permissions_by_approval_scope_id
                .insert(approval_scope_id.to_string(), granted_permissions);
        }
    }

    pub(crate) fn granted_permissions(
        &self,
        approval_scope_id: &str,
    ) -> Option<UriAdditionalPermissionProfile> {
        self.granted_permissions_by_approval_scope_id
            .get(approval_scope_id)
            .cloned()
    }
}

// Sometimes new snapshots don't include credits or plan information.
// Preserve those from the previous snapshot when missing. For `limit_id`, treat
// missing values as the default `"codex"` bucket.
fn merge_rate_limit_fields(
    previous: Option<&RateLimitSnapshot>,
    mut snapshot: RateLimitSnapshot,
) -> RateLimitSnapshot {
    if snapshot.limit_id.is_none() {
        snapshot.limit_id = Some("codex".to_string());
    }
    // Missing optional windows do not revoke an independently reported window.
    if let Some(prior) = previous.filter(|prior| prior.limit_id == snapshot.limit_id) {
        if snapshot.primary.is_none() {
            snapshot.primary = prior.primary.clone();
        }
        if snapshot.secondary.is_none() {
            snapshot.secondary = prior.secondary.clone();
        }
    }
    if snapshot.credits.is_none() {
        snapshot.credits = previous.and_then(|prior| prior.credits.clone());
    }
    if snapshot.individual_limit.is_none() {
        snapshot.individual_limit = previous.and_then(|prior| prior.individual_limit.clone());
    }

    if snapshot.spend_control_reached.is_none() {
        snapshot.spend_control_reached = previous.and_then(|prior| prior.spend_control_reached);
    }
    if snapshot.plan_type.is_none() {
        snapshot.plan_type = previous.and_then(|prior| prior.plan_type);
    }
    snapshot
}

