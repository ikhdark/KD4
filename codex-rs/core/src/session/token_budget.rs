use super::session::Session;
use super::turn_context::TurnContext;
use crate::config::Config;
use crate::config::TokenBudgetConfig;
use crate::config::resolve_token_budget_config;
use crate::context::ContextualUserFragment;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::account::PlanType;
use codex_protocol::auth::AuthMode;
use codex_protocol::openai_models::ModelInfo;

fn experimental_context_is_eligible(auth_mode: AuthMode, plan_type: Option<PlanType>) -> bool {
    auth_mode == AuthMode::Chatgpt
        && matches!(
            plan_type,
            Some(PlanType::Plus | PlanType::Pro | PlanType::ProLite | PlanType::ProMax)
        )
}

impl Session {
    pub(crate) async fn start_new_context_window(
        self: &std::sync::Arc<Self>,
        step_context: &std::sync::Arc<super::step_context::StepContext>,
        world_state: std::sync::Arc<crate::context::world_state::WorldState>,
    ) -> codex_protocol::error::Result<()> {
        let turn = &step_context.turn;
        let (items, snapshot, digests) = self
            .build_initial_context_with_world_state_and_provenance(turn, &world_state)
            .await;
        self.replace_compacted_history(
            turn,
            items,
            Some(turn.to_turn_context_item_async().await),
            Some(snapshot),
            digests,
            codex_protocol::protocol::CompactedItem {
                message: String::new(),
                replacement_history: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            },
        )
        .await?;
        self.recompute_token_usage(turn).await;
        Ok(())
    }

    pub(crate) async fn request_new_context_window(&self) {
        self.state.lock().await.new_context_window_requested = true;
    }

    pub(crate) async fn new_context_window_requested(&self) -> bool {
        self.state.lock().await.new_context_window_requested
    }
}

pub(super) fn update_window_metadata(
    items: &mut Vec<codex_protocol::models::ResponseItem>,
    turn: &TurnContext,
    ids: crate::state::AutoCompactWindowIds,
) {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    let metadata = crate::context::TokenBudgetContext::new(
        turn.session_source
            .get_agent_path()
            .unwrap_or_else(codex_protocol::AgentPath::root),
        ids.first_window_id,
        ids.previous_window_id,
        ids.window_id,
        turn.config
            .token_budget
            .as_ref()
            .and_then(|config| config.guidance_message.as_deref())
            .map(|guidance| crate::context::ContextWindowGuidance::new(guidance).render()),
    );
    for item in items.iter_mut() {
        if let ResponseItem::Message { role, content, .. } = item {
            if role != "developer" {
                continue;
            }
            for part in content {
                if let ContentItem::InputText { text } = part
                    && let Some(start) = text.find("<context_window>")
                    && let Some(end) = text[start..].find("</context_window>")
                {
                    text.replace_range(
                        start..start + end + "</context_window>".len(),
                        &metadata.render(),
                    );
                    return;
                }
            }
        }
    }
    items.push(ContextualUserFragment::into(metadata));
}

pub(super) fn apply_experimental_context(
    config: &mut Config,
    auth: Option<&CodexAuth>,
    starting_model: &ModelInfo,
) -> std::io::Result<()> {
    let provider = &config.model_provider;
    if !config.features.enabled(Feature::ContextManagement)
        || !starting_model.supports_experimental_context
        || !provider.is_openai()
        || provider
            .base_url
            .as_deref()
            .is_some_and(|url| !url.trim_end_matches('/').ends_with("/backend-api/codex"))
        || !provider.requires_openai_auth
        || provider.env_key.is_some()
        || provider.experimental_bearer_token.is_some()
        || provider.auth.is_some()
        || provider.aws.is_some()
        || !auth.is_some_and(|auth| {
            experimental_context_is_eligible(auth.auth_mode(), auth.account_plan_type())
        })
        || config.features.enable(Feature::TokenBudget).is_err()
        || !config.features.enabled(Feature::TokenBudget)
    {
        return Ok(());
    }

    if config.token_budget.is_none() {
        let config_toml = config
            .config_layer_stack
            .effective_config()
            .try_into()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        config.token_budget = resolve_token_budget_config(&config_toml, &config.features)?;
    }

    config
        .token_budget
        .get_or_insert_default()
        .use_history_notes_extension = true;
    Ok(())
}

/// Detects explicit preferences before model defaults are applied to the turn config.
pub(super) fn has_explicit_settings(config: &Config) -> bool {
    config
        .config_layer_stack
        .effective_config()
        .get("features")
        .and_then(|features| features.get("token_budget"))
        .and_then(|token_budget| token_budget.as_table())
        .is_some_and(|settings| {
            settings
                .keys()
                .any(|key| !matches!(key.as_str(), "enabled" | "use_history_notes_extension"))
        })
        || configured_preferences(config).is_some_and(|token_budget| {
            let mut settings = token_budget.clone();
            settings.use_history_notes_extension = false;
            settings != TokenBudgetConfig::default()
        })
}

fn configured_preferences(config: &Config) -> Option<&TokenBudgetConfig> {
    match config.token_budget_startup_config.as_ref() {
        Some(snapshot) => snapshot.configured_token_budget(),
        None => config.token_budget.as_ref(),
    }
}

pub(super) fn resolve_for_model(
    config: &Config,
    model_info: &ModelInfo,
) -> Option<TokenBudgetConfig> {
    if !config.features.enabled(Feature::TokenBudget) {
        return None;
    }
    let mut configured = configured_preferences(config).cloned().unwrap_or_default();
    configured.use_history_notes_extension = config
        .token_budget
        .as_ref()
        .is_some_and(|effective| effective.use_history_notes_extension);
    resolve_token_budget(
        Some(&configured),
        !has_explicit_settings(config),
        model_info,
    )
}

/// Resolves user-configured token-budget preferences against the current model's defaults.
pub(super) fn resolve_token_budget(
    configured_token_budget: Option<&TokenBudgetConfig>,
    use_model_defaults: bool,
    model_info: &ModelInfo,
) -> Option<TokenBudgetConfig> {
    if !use_model_defaults {
        return configured_token_budget.cloned();
    }

    let Some(model_defaults) = model_info
        .model_messages
        .as_ref()
        .and_then(|messages| messages.token_budget.as_ref())
    else {
        return configured_token_budget.cloned();
    };

    let token_budget = TokenBudgetConfig {
        use_history_notes_extension: configured_token_budget
            .is_some_and(|token_budget| token_budget.use_history_notes_extension),
        reminder_threshold_tokens: Some(model_defaults.reminder_threshold_tokens),
        reminder_message_template: model_defaults.reminder_message_template.clone(),
        guidance_message: Some(model_defaults.guidance_message.clone()),
        auto_compact_fallback_prompt: Some(model_defaults.auto_compact_fallback_prompt.clone()),
        auto_compact_fallback_buffer_tokens: Some(
            model_defaults.auto_compact_fallback_buffer_tokens,
        ),
    };

    if let Err(error) = token_budget.validate() {
        tracing::warn!(
            model = %model_info.slug,
            %error,
            "ignoring invalid model-owned token-budget defaults"
        );
        return configured_token_budget.cloned();
    }

    Some(token_budget)
}

/// Applies model activation defaults before thread extensions are initialized.
pub(super) fn apply_model_defaults(config: &mut Config, model_info: &ModelInfo) {
    let Some(model_defaults) = model_info
        .model_messages
        .as_ref()
        .and_then(|messages| messages.token_budget.as_ref())
    else {
        return;
    };
    if !model_defaults.enabled {
        return;
    }

    let has_explicit_config = config.token_budget.is_some()
        || config
            .config_layer_stack
            .effective_config()
            .get("features")
            .and_then(|features| features.get("token_budget"))
            .is_some();
    if has_explicit_config {
        return;
    }

    if config.features.enable(Feature::TokenBudget).is_err() {
        return;
    }
    // Managed requirements can pin the feature off even when enable() succeeds.
    if !config.features.enabled(Feature::TokenBudget) {
        return;
    }

    // Keep prompts unresolved so later turns and model switches use their own defaults.
    config.token_budget = Some(TokenBudgetConfig {
        use_history_notes_extension: model_defaults.use_history_notes_extension,
        ..TokenBudgetConfig::default()
    });
}

pub(super) async fn maybe_record(
    sess: &Session,
    turn_context: &TurnContext,
    base_window_tokens_remaining: Option<i64>,
    allow_auto_compact_fallback: bool,
) -> std::io::Result<()> {
    if !turn_context.config.features.enabled(Feature::TokenBudget) {
        return Ok(());
    }
    let Some(base_window_tokens_remaining) = base_window_tokens_remaining else {
        return Ok(());
    };

    let Some(config) = turn_context.config.token_budget.as_ref() else {
        return Ok(());
    };

    if config
        .reminder_threshold_tokens
        .is_some_and(|threshold| base_window_tokens_remaining <= threshold)
    {
        let reminder_due = {
            let mut state = sess.state.lock().await;
            state.claim_token_budget_reminder()
        };
        if reminder_due {
            let response_item =
                ContextualUserFragment::into(crate::context::TokenBudgetReminder::new(
                    &config.reminder_message_template,
                    base_window_tokens_remaining,
                ));
            if let Err(error) = sess
                .record_conversation_items(turn_context, std::slice::from_ref(&response_item))
                .await
            {
                sess.state.lock().await.release_token_budget_reminder();
                return Err(error);
            }
        }
    }

    if !allow_auto_compact_fallback || base_window_tokens_remaining != 0 {
        return Ok(());
    }
    let Some(prompt) = config.auto_compact_fallback_prompt.as_deref() else {
        return Ok(());
    };

    let fallback_due = {
        let mut state = sess.state.lock().await;
        state.claim_auto_compact_fallback()
    };
    if !fallback_due {
        return Ok(());
    }

    let response_item =
        ContextualUserFragment::into(crate::context::AutoCompactFallbackPrompt::new(prompt));
    if let Err(error) = sess
        .record_conversation_items(turn_context, std::slice::from_ref(&response_item))
        .await
    {
        sess.state.lock().await.release_auto_compact_fallback();
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
#[path = "token_budget_tests.rs"]
mod runtime_tests;

#[cfg(test)]
mod tests {
    use super::experimental_context_is_eligible;
    use codex_protocol::account::PlanType;
    use codex_protocol::auth::AuthMode;

    #[test]
    fn experimental_context_requires_eligible_chatgpt_subscription() {
        for (auth_mode, plan_type, expected) in [
            (AuthMode::Chatgpt, PlanType::Plus, true),
            (AuthMode::Chatgpt, PlanType::Pro, true),
            (AuthMode::Chatgpt, PlanType::ProLite, true),
            (AuthMode::Chatgpt, PlanType::ProMax, true),
            (AuthMode::Chatgpt, PlanType::Free, false),
            (AuthMode::Chatgpt, PlanType::Enterprise, false),
            (AuthMode::ApiKey, PlanType::Pro, false),
        ] {
            assert_eq!(
                experimental_context_is_eligible(auth_mode, Some(plan_type)),
                expected
            );
        }
    }
}
