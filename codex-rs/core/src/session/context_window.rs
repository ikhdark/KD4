use super::session::Session;
use super::turn_context::TurnContext;
use codex_protocol::config_types::AutoCompactTokenLimitScope;

pub(crate) const LOCAL_CONTEXT_WINDOW_LIMIT: i64 = 272_000;

#[derive(Debug)]
pub(crate) struct ContextWindowTokenStatus {
    // Full active context usage, independent of the configured auto-compact scope.
    pub(crate) active_context_tokens: i64,
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
    let active_context_tokens = sess.get_total_token_usage().await;

    let (auto_compact_scope_tokens, auto_compact_scope_limit, body_window) =
        match turn_context.config.model_auto_compact_token_limit_scope {
            AutoCompactTokenLimitScope::Total => (
                active_context_tokens,
                effective_auto_compact_token_limit(turn_context),
                None,
            ),
            AutoCompactTokenLimitScope::BodyAfterPrefix => {
                let window = sess.auto_compact_window_snapshot().await;
                let baseline = window.prefill_input_tokens.unwrap_or(active_context_tokens);

                let scope_limit = effective_auto_compact_token_limit(turn_context);
                let full_context_window_limit =
                    body_after_prefix_full_context_limit(turn_context.model_context_window());

                (
                    active_context_tokens.saturating_sub(baseline),
                    scope_limit,
                    Some(BodyAfterPrefixWindowStatus {
                        full_context_window_limit,
                        auto_compact_window_prefill_tokens: window.prefill_input_tokens,
                    }),
                )
            }
        };

    let full_context_window_limit = body_window
        .as_ref()
        .and_then(|window| window.full_context_window_limit);
    let auto_compact_window_prefill_tokens = body_window
        .as_ref()
        .and_then(|window| window.auto_compact_window_prefill_tokens);

    let full_context_window_limit_reached =
        full_context_window_limit.is_some_and(|full_context_window_limit| {
            active_context_tokens >= full_context_window_limit
        });
    let token_limit_reached = auto_compact_scope_limit
        .is_some_and(|limit| auto_compact_scope_tokens >= limit)
        || full_context_window_limit_reached;

    let auto_compact_scope_remaining = auto_compact_scope_limit
        .map(|limit| limit.saturating_sub(auto_compact_scope_tokens).max(0));
    let full_context_remaining =
        full_context_window_limit.map(|limit| limit.saturating_sub(active_context_tokens).max(0));
    let tokens_until_compaction = match (auto_compact_scope_remaining, full_context_remaining) {
        (Some(scope_remaining), Some(full_remaining)) => Some(scope_remaining.min(full_remaining)),
        (scope_remaining, full_remaining) => scope_remaining.or(full_remaining),
    };

    ContextWindowTokenStatus {
        active_context_tokens,
        auto_compact_scope_tokens,
        auto_compact_scope_limit,
        full_context_window_limit,
        tokens_until_compaction,
        auto_compact_window_prefill_tokens,
        full_context_window_limit_reached,
        token_limit_reached,
    }
}

pub(crate) fn estimated_prompt_reaches_hard_limit(
    turn_context: &TurnContext,
    estimated_tokens: Option<i64>,
) -> bool {
    let hard_limit = prompt_hard_limit(turn_context.model_context_window());
    estimated_tokens.is_some_and(|estimated| estimated >= hard_limit)
}

fn prompt_hard_limit(model_context_window: Option<i64>) -> i64 {
    model_context_window
        .filter(|limit| *limit > 0)
        .map_or(LOCAL_CONTEXT_WINDOW_LIMIT, |limit| {
            limit.min(LOCAL_CONTEXT_WINDOW_LIMIT)
        })
}

fn body_after_prefix_full_context_limit(model_context_window: Option<i64>) -> Option<i64> {
    Some(prompt_hard_limit(model_context_window))
}

fn effective_auto_compact_token_limit(turn_context: &TurnContext) -> Option<i64> {
    Some(effective_auto_compact_token_limit_value(turn_context))
}

fn effective_auto_compact_token_limit_value(turn_context: &TurnContext) -> i64 {
    effective_auto_compact_token_limit_from_values(
        turn_context.config.model_auto_compact_token_limit,
        turn_context.model_info.auto_compact_token_limit(),
        turn_context.model_context_window(),
    )
}

fn effective_auto_compact_token_limit_from_values(
    configured_limit: Option<i64>,
    model_auto_compact_limit: Option<i64>,
    model_context_window: Option<i64>,
) -> i64 {
    [
        Some(LOCAL_CONTEXT_WINDOW_LIMIT),
        configured_limit,
        model_auto_compact_limit,
        model_context_window,
    ]
    .into_iter()
    .flatten()
    .filter(|limit| *limit > 0)
    .min()
    .unwrap_or(LOCAL_CONTEXT_WINDOW_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_limit_uses_smallest_explicit_or_local_limit() {
        assert_eq!(
            effective_auto_compact_token_limit_from_values(None, None, None),
            272_000
        );
        assert_eq!(
            effective_auto_compact_token_limit_from_values(None, None, Some(64_000)),
            64_000
        );
        assert_eq!(
            effective_auto_compact_token_limit_from_values(None, Some(50_000), Some(64_000)),
            50_000
        );
        assert_eq!(
            effective_auto_compact_token_limit_from_values(Some(120_000), None, Some(64_000)),
            64_000
        );
        assert_eq!(
            effective_auto_compact_token_limit_from_values(Some(0), Some(-1), Some(200_000)),
            200_000
        );
        assert_eq!(
            effective_auto_compact_token_limit_from_values(Some(0), Some(-1), Some(300_000)),
            272_000
        );
    }

    #[test]
    fn prompt_hard_limit_excludes_soft_auto_compact_limits() {
        assert_eq!(prompt_hard_limit(None), 272_000);
        assert_eq!(prompt_hard_limit(Some(64_000)), 64_000);
        assert_eq!(prompt_hard_limit(Some(200_000)), 200_000);
        assert_eq!(prompt_hard_limit(Some(300_000)), 272_000);
        assert_eq!(prompt_hard_limit(Some(0)), 272_000);
    }

    #[test]
    fn body_after_prefix_full_limit_matches_prompt_hard_limit() {
        assert_eq!(
            body_after_prefix_full_context_limit(Some(300_000)),
            Some(272_000)
        );
        assert_eq!(
            body_after_prefix_full_context_limit(Some(64_000)),
            Some(64_000)
        );
        assert_eq!(body_after_prefix_full_context_limit(None), Some(272_000));
    }
}
