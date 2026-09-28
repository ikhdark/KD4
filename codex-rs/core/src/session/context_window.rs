use super::session::Session;
use super::turn_context::TurnContext;
use crate::client_common::Prompt;
use crate::config::TokenBudgetConfig;
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
    pub(crate) base_window_tokens_remaining: Option<i64>,
}

struct BodyAfterPrefixWindowStatus {
    full_context_window_limit: Option<i64>,
    auto_compact_window_prefill_tokens: Option<i64>,
}

/// A checkpoint invalidates the old server usage as a next-request pressure
/// estimate. Count the prepared request, including every possible transport
/// fallback, without rewriting actual usage or the window's prefill baseline.
pub(super) async fn checkpoint_prompt_requires_compaction(
    sess: &Session,
    turn: &TurnContext,
    prompt: &Prompt,
) -> bool {
    if turn
        .config
        .features
        .enabled(codex_features::Feature::TokenBudget)
        && sess.new_context_window_requested().await
    {
        return true;
    }
    // Retiring a prefill result invalidates body-after-prefix credit. Unknown
    // resume baselines must also conservatively compact rather than undercount.
    if matches!(
        turn.config.model_auto_compact_token_limit_scope,
        AutoCompactTokenLimitScope::BodyAfterPrefix
    ) && !sess.state.lock().await.checkpoint_preserves_prefill()
    {
        return true;
    }
    let pressure = checkpoint_prompt_token_pressure(prompt);
    let scope_pressure = match turn.config.model_auto_compact_token_limit_scope {
        AutoCompactTokenLimitScope::Total => pressure,
        AutoCompactTokenLimitScope::BodyAfterPrefix => pressure
            .saturating_sub(
                sess.auto_compact_window_snapshot()
                    .await
                    .prefill_input_tokens
                    .unwrap_or(0),
            )
            .max(0),
    };
    turn.model_context_window()
        .is_some_and(|limit| pressure >= limit)
        || projected_context_window_token_status(sess, turn, pressure, scope_pressure)
            .await
            .token_limit_reached
}

fn checkpoint_prompt_token_pressure(prompt: &Prompt) -> i64 {
    let bytes_to_tokens = |bytes: usize| i64::try_from(bytes.div_ceil(4)).unwrap_or(i64::MAX);
    let mut inputs = Vec::new();
    let mut input_tokens = 0;
    for input in [
        &prompt.input,
        &prompt.stable_context_fallback_input,
        &prompt.tool_history_fallback_input,
        &prompt.stable_context_tool_history_fallback_input,
    ] {
        if inputs
            .iter()
            .any(|prior| std::sync::Arc::ptr_eq(prior, input))
        {
            continue;
        }
        inputs.push(std::sync::Arc::clone(input));
        // Reuse the history estimator, including its image cache. Count all
        // reasoning still present in this request, including all-turns models.
        let tokens = input
            .iter()
            .map(crate::context_manager::estimate_item_token_count)
            .fold(0i64, i64::saturating_add);
        input_tokens = input_tokens.max(tokens);
    }
    let measured = input_tokens
        .saturating_add(bytes_to_tokens(prompt.base_instructions.text.len()))
        .saturating_add(bytes_to_tokens(prompt.tools.serialized().len()))
        .saturating_add(
            prompt
                .output_schema
                .as_ref()
                .map_or(0, |schema| bytes_to_tokens(schema.to_string().len())),
        );
    // The heuristic needs framing/error headroom; genuine provider overflows
    // still follow the existing bounded recovery path.
    measured.saturating_add((measured / 10).max(1024))
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
                turn_context.model_info.auto_compact_token_limit(),
                None,
            ),
            AutoCompactTokenLimitScope::BodyAfterPrefix => {
                let window = sess.auto_compact_window_snapshot().await;
                let baseline = window.prefill_input_tokens.unwrap_or(active_context_tokens);

                let scope_limit = turn_context.model_info.auto_compact_token_limit();
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
    let token_budget_enabled = turn_context
        .config
        .features
        .enabled(codex_features::Feature::TokenBudget);
    let full_context_window_limit = body_window
        .as_ref()
        .and_then(|window| window.full_context_window_limit)
        .or_else(|| {
            token_budget_enabled
                .then(|| turn_context.model_context_window())
                .flatten()
        });
    let auto_compact_window_prefill_tokens = body_window
        .as_ref()
        .and_then(|window| window.auto_compact_window_prefill_tokens);
    let full_context_window_limit_reached =
        full_context_window_limit.is_some_and(|limit| pressure_context_tokens >= limit);
    let fallback_buffer = if token_budget_enabled {
        turn_context
            .config
            .token_budget
            .as_ref()
            .map_or(0, TokenBudgetConfig::fallback_buffer_tokens)
    } else {
        0
    };
    let soft_limit_reached = auto_compact_scope_limit
        .is_some_and(|limit| auto_compact_scope_tokens >= limit.saturating_add(fallback_buffer));
    let token_limit_reached = soft_limit_reached || full_context_window_limit_reached;
    let base_window_tokens_remaining = remaining_tokens(
        auto_compact_scope_limit.map(|limit| limit.saturating_sub(auto_compact_scope_tokens)),
        full_context_window_limit.map(|limit| limit.saturating_sub(pressure_context_tokens)),
    );

    ContextWindowTokenStatus {
        active_context_tokens,
        auto_compact_scope_tokens,
        auto_compact_scope_limit,
        full_context_window_limit,
        auto_compact_window_prefill_tokens,
        full_context_window_limit_reached,
        token_limit_reached,
        base_window_tokens_remaining,
    }
}

fn remaining_tokens(soft: Option<i64>, physical: Option<i64>) -> Option<i64> {
    match (soft, physical) {
        (Some(soft), Some(physical)) => Some(soft.min(physical).max(0)),
        (Some(tokens), None) | (None, Some(tokens)) => Some(tokens.max(0)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;

    #[test]
    fn checkpoint_pressure_counts_fallbacks_and_non_history_content() {
        let mut prompt = Prompt::default();
        let without_content = checkpoint_prompt_token_pressure(&prompt);
        prompt.base_instructions.text.push_str(&"i".repeat(4000));
        prompt.output_schema = Some(serde_json::json!({"description": "s".repeat(4000)}));
        assert!(checkpoint_prompt_token_pressure(&prompt) >= without_content + 2000);
        prompt.tool_history_fallback_input = vec![ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "e".repeat(400_000),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }]
        .into();
        assert!(checkpoint_prompt_token_pressure(&prompt) >= 110_000);
        assert!(
            prompt.input.is_empty(),
            "the distinct fallback, not primary input, creates pressure"
        );
    }
}
