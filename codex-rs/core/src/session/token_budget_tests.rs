use super::*;
use crate::session::context_window::projected_context_window_token_status;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelTokenBudgetConfig;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn enable(turn: &mut TurnContext) {
    let config = Arc::make_mut(&mut turn.config);
    config.features.enable(Feature::TokenBudget).unwrap();
    config.token_budget_startup_config = None;
    config.token_budget = Some(TokenBudgetConfig {
        reminder_threshold_tokens: Some(10),
        reminder_message_template: "remaining {n_remaining}".into(),
        auto_compact_fallback_prompt: Some("save notes now".into()),
        auto_compact_fallback_buffer_tokens: Some(20),
        ..Default::default()
    });
}

fn history_text(items: &[ResponseItem]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|part| match part {
                        ContentItem::InputText { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn reminders_are_opt_in_and_once_per_window() {
    let (session, mut turn) = make_session_and_context().await;
    let initial = session.clone_history().await.raw_items().len();
    maybe_record(&session, &turn, Some(0), true).await.unwrap();
    assert_eq!(session.clone_history().await.raw_items().len(), initial);
    enable(&mut turn);
    maybe_record(&session, &turn, None, true).await.unwrap();
    maybe_record(&session, &turn, Some(11), true).await.unwrap();
    assert_eq!(session.clone_history().await.raw_items().len(), initial);
    maybe_record(&session, &turn, Some(10), true).await.unwrap();
    maybe_record(&session, &turn, Some(0), false).await.unwrap();
    let text = history_text(session.clone_history().await.raw_items());
    assert_eq!(text.matches("remaining 10").count(), 1);
    assert!(!text.contains("save notes now"));
    for _ in 0..2 {
        maybe_record(&session, &turn, Some(0), true).await.unwrap();
    }
    assert_eq!(
        history_text(session.clone_history().await.raw_items())
            .matches("save notes now")
            .count(),
        1
    );
    {
        let mut state = session.state.lock().await;
        let (number, ids) = state.next_auto_compact_window();
        state.restore_auto_compact_window(number, ids);
    }
    maybe_record(&session, &turn, Some(5), true).await.unwrap();
    assert!(history_text(session.clone_history().await.raw_items()).contains("remaining 5"));
}

#[tokio::test]
async fn fallback_buffer_never_exceeds_physical_window_in_either_scope() {
    let (session, mut turn) = make_session_and_context().await;
    enable(&mut turn);
    turn.model_info.context_window = Some(110);
    turn.model_info.effective_context_window_percent = 100;
    turn.model_info.auto_compact_token_limit = Some(100);
    for scope in [
        AutoCompactTokenLimitScope::Total,
        AutoCompactTokenLimitScope::BodyAfterPrefix,
    ] {
        Arc::make_mut(&mut turn.config).model_auto_compact_token_limit_scope = scope;
        let status = projected_context_window_token_status(&session, &turn, 100, 100).await;
        assert_eq!(status.base_window_tokens_remaining, Some(0));
        assert!(!status.token_limit_reached);
        let status = projected_context_window_token_status(&session, &turn, 110, 105).await;
        assert!(status.full_context_window_limit_reached);
        assert!(status.token_limit_reached);
    }
}

#[tokio::test]
async fn fresh_window_preserves_environment_and_cancellation_preserves_history() {
    let (session, mut turn) = make_session_and_context().await;
    enable(&mut turn);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step = StepContext::for_test(Arc::clone(&turn));
    let world = Arc::new(session.build_world_state_for_step(&step).await);
    maybe_record(&session, &turn, Some(0), true).await.unwrap();
    let history_before = session.clone_history().await.raw_items().to_vec();
    let ids_before = session.state.lock().await.auto_compact_window_ids();
    let cwd_before = turn.cwd().clone();
    session.request_new_context_window().await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let result = crate::compact_token_budget::run_inline_auto_compact_task(
        Arc::clone(&session),
        Arc::clone(&step),
        crate::compact::InitialContextInjection::AtStart(Arc::clone(&world)),
        &cancelled,
    )
    .await;
    assert!(matches!(
        result,
        Err(codex_protocol::error::CodexErr::TurnAborted)
    ));
    assert_eq!(session.clone_history().await.raw_items(), history_before);
    assert_eq!(
        session.state.lock().await.auto_compact_window_ids(),
        ids_before
    );
    session
        .start_new_context_window(&step, world)
        .await
        .unwrap();
    let ids_after = session.state.lock().await.auto_compact_window_ids();
    assert_ne!(ids_after.window_id, ids_before.window_id);
    assert_eq!(ids_after.first_window_id, ids_before.first_window_id);
    assert_eq!(ids_after.previous_window_id, Some(ids_before.window_id));
    assert!(!session.new_context_window_requested().await);
    assert_eq!(turn.cwd(), &cwd_before);
    let text = history_text(session.clone_history().await.raw_items());
    assert!(!text.contains("save notes now"));
    assert!(text.contains(&format!(
        "Current context window id: {}",
        ids_after.window_id
    )));
    assert!(text.contains(&format!(
        "Previous context window id: {}",
        ids_before.window_id
    )));
}

#[tokio::test]
async fn model_switch_uses_original_preferences_and_fresh_child_restores_activation() {
    let (_, mut turn) = make_session_and_context().await;
    let config = Arc::make_mut(&mut turn.config);
    config.token_budget_startup_config = None;
    config.features.disable(Feature::TokenBudget).unwrap();
    config.token_budget = None;
    config.prepare_token_budget_for_startup().unwrap();
    config.features.enable(Feature::TokenBudget).unwrap();
    config.token_budget = Some(TokenBudgetConfig {
        use_history_notes_extension: true,
        ..Default::default()
    });
    let mut model = turn.model_info.clone();
    model.model_messages = Some(ModelMessages {
        token_budget: Some(ModelTokenBudgetConfig {
            enabled: false,
            use_history_notes_extension: false,
            reminder_threshold_tokens: 10,
            reminder_message_template: "model reminder".into(),
            guidance_message: "model guidance".into(),
            auto_compact_fallback_prompt: "model fallback".into(),
            auto_compact_fallback_buffer_tokens: 20,
        }),
        instructions_template: None,
        instructions_variables: None,
        approvals: None,
    });
    config.token_budget = resolve_for_model(config, &model);
    assert_eq!(
        config
            .token_budget
            .as_ref()
            .unwrap()
            .guidance_message
            .as_deref(),
        Some("model guidance")
    );
    model.model_messages = None;
    config.token_budget = resolve_for_model(config, &model);
    let resolved = config.token_budget.as_ref().unwrap();
    assert_eq!(resolved.guidance_message, None);
    assert!(resolved.use_history_notes_extension);
    config.prepare_token_budget_for_startup().unwrap();
    assert!(!config.features.enabled(Feature::TokenBudget));
    assert_eq!(config.token_budget, None);
}
