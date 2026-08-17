use super::session::Session;
use super::turn_context::TurnContext;
use codex_protocol::config_types::AutoCompactTokenLimitScope;

#[derive(Debug)]
pub(crate) struct ContextWindowTokenStatus {
    pub(crate) active_context_tokens: i64,
    // Usage counted against `model_auto_compact_token_limit` for the current scope.
    pub(crate) auto_compact_scope_tokens: i64,
    pub(crate) auto_compact_scope_limit: Option<i64>,
    pub(crate) full_context_window_limit: Option<i64>,
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
    context_window_token_status_for_pressure(
        sess,
        turn_context,
        active_context_tokens,
        active_context_tokens,
        None,
    )
    .await
}

pub(crate) async fn projected_context_window_token_status(
    sess: &Session,
    turn_context: &TurnContext,
    projected_context_tokens: i64,
    projected_auto_compact_scope_tokens: i64,
) -> ContextWindowTokenStatus {
    let active_context_tokens = sess.get_total_token_usage().await;
    context_window_token_status_for_pressure(
        sess,
        turn_context,
        active_context_tokens,
        projected_context_tokens.max(0),
        Some(projected_auto_compact_scope_tokens.max(0)),
    )
    .await
}

async fn context_window_token_status_for_pressure(
    sess: &Session,
    turn_context: &TurnContext,
    active_context_tokens: i64,
    pressure_context_tokens: i64,
    projected_auto_compact_scope_tokens: Option<i64>,
) -> ContextWindowTokenStatus {
    let (auto_compact_scope_tokens, auto_compact_scope_limit, body_window) =
        match turn_context.config.model_auto_compact_token_limit_scope {
            AutoCompactTokenLimitScope::Total => (
                projected_auto_compact_scope_tokens.unwrap_or(pressure_context_tokens),
                turn_context
                    .config
                    .model_auto_compact_token_limit
                    .or_else(|| turn_context.model_info.auto_compact_token_limit()),
                None,
            ),
            AutoCompactTokenLimitScope::BodyAfterPrefix => {
                let window = sess.auto_compact_window_snapshot().await;
                let baseline = window.prefill_input_tokens.unwrap_or(active_context_tokens);

                let scope_limit = turn_context
                    .config
                    .model_auto_compact_token_limit
                    .or_else(|| turn_context.model_info.auto_compact_token_limit());
                let full_context_window_limit = turn_context.model_context_window();

                (
                    projected_auto_compact_scope_tokens
                        .unwrap_or_else(|| pressure_context_tokens.saturating_sub(baseline)),
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
        full_context_window_limit.is_some_and(|limit| pressure_context_tokens >= limit);
    let soft_limit_reached =
        auto_compact_scope_limit.is_some_and(|limit| auto_compact_scope_tokens >= limit);
    let token_limit_reached = soft_limit_reached || full_context_window_limit_reached;

    ContextWindowTokenStatus {
        active_context_tokens,
        auto_compact_scope_tokens,
        auto_compact_scope_limit,
        full_context_window_limit,
        auto_compact_window_prefill_tokens,
        full_context_window_limit_reached,
        token_limit_reached,
    }
}
