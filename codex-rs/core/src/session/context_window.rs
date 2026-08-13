use super::session::Session;
use super::turn_context::TurnContext;
use crate::context_manager::ContextManager;
use crate::stable_context::StableContextTarget;
use codex_features::Feature;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use sha2::Digest;
use sha2::Sha256;

pub(crate) const KD4_SOFT_WORKING_SET_LIMIT: i64 = 72_000;
pub(crate) const KD4_HARD_OPERATING_LIMIT: i64 = 80_000;

#[derive(Debug)]
pub(crate) struct ContextWindowTokenStatus {
    /// Legacy active-context estimate retained for unrelated context behavior.
    pub(crate) active_context_tokens: i64,
    /// Final locally prepared/projected history plus base instructions.
    pub(crate) local_projected_occupancy: i64,
    /// Best trustworthy estimate of the state the provider will carry.
    pub(crate) effective_provider_occupancy: Option<i64>,
    pub(crate) effective_estimate_basis: &'static str,
    pub(crate) context_identity: String,
    pub(crate) soft_floor_receipt_active: bool,
    // Usage counted against `model_auto_compact_token_limit` for the current scope.
    pub(crate) auto_compact_scope_tokens: i64,
    pub(crate) auto_compact_scope_limit: Option<i64>,
    pub(crate) full_context_window_limit: Option<i64>,
    pub(crate) tokens_until_compaction: Option<i64>,
    pub(crate) auto_compact_window_prefill_tokens: Option<i64>,
    pub(crate) full_context_window_limit_reached: bool,
    pub(crate) token_limit_reached: bool,
}

struct BodyAfterPrefixWindowStatus {
    full_context_window_limit: Option<i64>,
    auto_compact_window_prefill_tokens: Option<i64>,
}

pub(crate) async fn context_window_token_status(
    sess: &Session,
    turn_context: &TurnContext,
) -> ContextWindowTokenStatus {
    let history = sess.clone_history().await;
    let base_instructions = sess.get_base_instructions().await;
    let prepared = history.clone().prepare_for_sampling_prompt(
        &turn_context.model_info.input_modalities,
        StableContextTarget::Sampling,
    );
    let local_projected_occupancy =
        ContextManager::estimate_items_token_count_with_base_instructions(
            prepared.items(),
            &base_instructions,
        )
        .unwrap_or(i64::MAX);
    let active_context_tokens = sess.get_total_token_usage().await;
    let kd4_budget_enabled = turn_context.config.features.enabled(Feature::TokenBudget)
        && sess.services.task_evidence.allows_kd4_completion();

    let server_usage = history.token_info().map(|info| info.last_token_usage);
    let has_server_baseline = server_usage.as_ref().is_some_and(|usage| {
        usage.input_tokens > 0
            || usage.output_tokens > 0
            || usage.reasoning_output_tokens > 0
            || usage.total_tokens > 0
    });
    let (effective_provider_occupancy, effective_estimate_basis) =
        if !history.contains_model_generated_item() {
            (
                Some(local_projected_occupancy),
                "stateless_or_rebased_local",
            )
        } else if has_server_baseline {
            (
                Some(active_context_tokens.max(local_projected_occupancy)),
                "server_baseline_plus_known_delta",
            )
        } else {
            (None, "provider_baseline_missing_or_stale")
        };
    let effective_estimate_complete = effective_provider_occupancy.is_some();
    let context_identity = occupancy_identity(
        prepared.items(),
        &base_instructions.text,
        turn_context,
        sess.services.planning_generation(),
        server_usage.as_ref(),
    );
    let soft_floor_receipt_active =
        kd4_budget_enabled && sess.soft_floor_receipt_matches(&context_identity).await;

    let budget_occupancy = if kd4_budget_enabled {
        effective_provider_occupancy.unwrap_or(i64::MAX)
    } else {
        active_context_tokens
    };
    let (auto_compact_scope_tokens, mut auto_compact_scope_limit, body_window) =
        match turn_context.config.model_auto_compact_token_limit_scope {
            AutoCompactTokenLimitScope::Total => (
                budget_occupancy,
                turn_context
                    .config
                    .model_auto_compact_token_limit
                    .or_else(|| turn_context.model_info.auto_compact_token_limit()),
                None,
            ),
            AutoCompactTokenLimitScope::BodyAfterPrefix => {
                let window = sess.auto_compact_window_snapshot().await;
                let baseline = window.prefill_input_tokens.unwrap_or(budget_occupancy);

                let scope_limit = turn_context
                    .config
                    .model_auto_compact_token_limit
                    .or_else(|| turn_context.model_info.auto_compact_token_limit());
                let full_context_window_limit = turn_context.model_context_window();

                (
                    budget_occupancy.saturating_sub(baseline),
                    scope_limit,
                    Some(BodyAfterPrefixWindowStatus {
                        full_context_window_limit,
                        auto_compact_window_prefill_tokens: window.prefill_input_tokens,
                    }),
                )
            }
        };
    if kd4_budget_enabled
        && matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::Total
        )
    {
        auto_compact_scope_limit = Some(
            auto_compact_scope_limit
                .unwrap_or(KD4_SOFT_WORKING_SET_LIMIT)
                .min(KD4_SOFT_WORKING_SET_LIMIT),
        );
    }

    let full_context_window_limit = body_window
        .as_ref()
        .and_then(|window| window.full_context_window_limit);
    let auto_compact_window_prefill_tokens = body_window
        .as_ref()
        .and_then(|window| window.auto_compact_window_prefill_tokens);
    let full_context_window_limit_reached =
        full_context_window_limit.is_some_and(|limit| active_context_tokens >= limit);
    let hard_operating_limit = kd4_budget_enabled.then_some(KD4_HARD_OPERATING_LIMIT);
    let hard_operating_limit_reached = hard_operating_limit
        .zip(effective_provider_occupancy)
        .is_some_and(|(limit, occupancy)| occupancy >= limit);
    let soft_limit_reached = auto_compact_scope_limit
        .is_some_and(|limit| auto_compact_scope_tokens >= limit)
        && !soft_floor_receipt_active;
    let token_limit_reached = soft_limit_reached
        || full_context_window_limit_reached
        || hard_operating_limit_reached
        || (kd4_budget_enabled && !effective_estimate_complete);

    let auto_compact_scope_remaining = auto_compact_scope_limit
        .map(|limit| limit.saturating_sub(auto_compact_scope_tokens).max(0));
    let full_context_remaining =
        full_context_window_limit.map(|limit| limit.saturating_sub(active_context_tokens).max(0));
    let hard_remaining = hard_operating_limit.and_then(|limit| {
        effective_provider_occupancy.map(|occupancy| limit.saturating_sub(occupancy).max(0))
    });
    let tokens_until_compaction = [
        auto_compact_scope_remaining,
        full_context_remaining,
        hard_remaining,
    ]
    .into_iter()
    .flatten()
    .min();

    ContextWindowTokenStatus {
        active_context_tokens,
        local_projected_occupancy,
        effective_provider_occupancy,
        effective_estimate_basis,
        context_identity,
        soft_floor_receipt_active,
        auto_compact_scope_tokens,
        auto_compact_scope_limit,
        full_context_window_limit,
        tokens_until_compaction,
        auto_compact_window_prefill_tokens,
        full_context_window_limit_reached,
        token_limit_reached,
    }
}

fn occupancy_identity(
    items: &[codex_protocol::models::ResponseItem],
    base_instructions: &str,
    turn_context: &TurnContext,
    planning_generation: u64,
    server_usage: Option<&codex_protocol::protocol::TokenUsage>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"codex.kd4.soft-floor-receipt.v1");
    hasher.update(planning_generation.to_be_bytes());
    hasher.update(base_instructions.as_bytes());
    hasher.update(turn_context.model_info.slug.as_bytes());
    hasher.update(format!("{:?}", turn_context.collaboration_mode).as_bytes());
    if let Some(server_usage) = server_usage {
        for value in [
            server_usage.input_tokens,
            server_usage.cached_input_tokens,
            server_usage.output_tokens,
            server_usage.reasoning_output_tokens,
            server_usage.total_tokens,
        ] {
            hasher.update(value.to_be_bytes());
        }
    }
    if let Ok(encoded) = serde_json::to_vec(items) {
        hasher.update(encoded);
    }
    format!("{:x}", hasher.finalize())
}
