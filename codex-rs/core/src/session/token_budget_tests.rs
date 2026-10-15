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

fn install_recovery_tools(session: &mut Session) {
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.tool_contributor(Arc::new(crate::session::tests::TokenBudgetRecoveryTools));
    session.services.extensions = Arc::new(builder.build());
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

// Exact pre-correction reset sequence retained only for paired lifecycle
// measurement. Unlike a mutation of shared production source, this cannot
// affect another session's build or calls.
async fn legacy_fresh_window_reference(
    session: &Arc<Session>,
    step: &Arc<StepContext>,
    world: &Arc<crate::context::world_state::WorldState>,
) {
    let turn = &step.turn;
    let history = session.clone_history().await;
    let source = history
        .raw_items()
        .iter()
        .filter(|item| {
            !matches!(item,
                ResponseItem::Message { id: Some(id), role, .. }
                    if role == "developer" && id.as_str().starts_with("msg_token_budget_")
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let (retained, _, omitted_text) = crate::compact::build_task_input_checkpoint(&source);
    let recovery =
        crate::compact::persist_task_compaction_text_recovery(session, &source, omitted_text)
            .await
            .unwrap();
    let (mut items, snapshot, digests) = session
        .build_initial_context_with_world_state_and_provenance(turn, world)
        .await;
    items.extend(retained);
    if let Some(text) = recovery {
        items.push(crate::compact::compaction_context_message(text));
    }
    session
        .replace_compacted_history(
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
        .await
        .unwrap();
    session.recompute_token_usage(turn).await;
}

#[tokio::test]
#[ignore = "paired native reset lifecycle measurement; no wall-clock assertion"]
async fn benchmark_audit21_fresh_window_pipeline() {
    use codex_protocol::plan_tool::{PlanItemArg, StepStatus, UpdatePlanArgs};
    let mut samples = [Vec::new(), Vec::new()];
    for round in 0..7 {
        for candidate in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let (mut session, mut turn) = make_session_and_context().await;
            install_recovery_tools(&mut session);
            enable(&mut turn);
            let session = Arc::new(session);
            let turn = Arc::new(turn);
            let plan = |step: &str| UpdatePlanArgs {
                explanation: None,
                plan: vec![PlanItemArg {
                    step: step.into(),
                    status: StepStatus::Pending,
                }],
            };
            session
                .services
                .plan_store
                .restore(Some(plan("obsolete obligation")))
                .await;
            let world =
                crate::session::tests::build_world_state_from_turn_context(&session, &turn).await;
            let initial = session
                .build_initial_context_with_world_state(&turn, &world)
                .await;
            session
                .record_conversation_items(&turn, &initial)
                .await
                .unwrap();
            session
                .services
                .plan_store
                .restore(Some(plan("current obligation")))
                .await;
            let step = StepContext::for_test(Arc::clone(&turn));
            let world = Arc::new(session.build_world_state_for_step(&step).await);
            let started = std::time::Instant::now();
            if candidate {
                session
                    .start_new_context_window(&step, world)
                    .await
                    .unwrap();
            } else {
                legacy_fresh_window_reference(&session, &step, &world).await;
            }
            let history = session.clone_history().await;
            let projection = crate::stable_context::project_stable_context(
                history.raw_items().to_vec().into(),
                crate::stable_context::StableContextTarget::Sampling,
            );
            samples[usize::from(candidate)].push(started.elapsed().as_micros());
            let text = history_text(&projection.items);
            assert_eq!(text.contains("current obligation"), candidate);
            assert_eq!(text.contains("obsolete obligation"), !candidate);
        }
    }
    for values in &mut samples {
        values.sort_unstable();
    }
    eprintln!(
        "audit21_native_reset samples=7 baseline_median_us={} candidate_median_us={} baseline_correct=0 candidate_correct=7",
        samples[0][3], samples[1][3]
    );
}

#[tokio::test]
async fn reminders_are_opt_in_and_once_per_window() {
    let (mut session, mut turn) = make_session_and_context().await;
    install_recovery_tools(&mut session);
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

#[test]
fn recovery_tools_require_both_notes_and_history() {
    let tools = crate::session::tests::token_budget_recovery_tool_stubs();
    assert!(recovery_tools_registered(&tools));
    assert!(!recovery_tools_registered(&[]));
    for tool in &tools {
        assert!(!recovery_tools_registered(&[Arc::clone(tool)]));
        assert!(!recovery_tools_registered(&[
            Arc::clone(tool),
            Arc::clone(tool),
        ]));
    }
}

#[tokio::test]
async fn recovery_prompts_require_recovery_tools() {
    for recovery_tools in [false, true] {
        let (mut session, mut turn) = make_session_and_context().await;
        if recovery_tools {
            install_recovery_tools(&mut session);
        }
        enable(&mut turn);
        Arc::make_mut(&mut turn.config)
            .token_budget
            .as_mut()
            .unwrap()
            .guidance_message = Some("write notes before the reset".into());
        let turn = Arc::new(turn);
        let world =
            crate::session::tests::build_world_state_from_turn_context(&session, &turn).await;
        let initial = session
            .build_initial_context_with_world_state(&turn, &world)
            .await;
        assert_eq!(
            history_text(&initial).contains("write notes before the reset"),
            recovery_tools
        );
        maybe_record(&session, &turn, Some(0), true).await.unwrap();
        let text = history_text(session.clone_history().await.raw_items());
        assert_eq!(text.contains("remaining 0"), recovery_tools);
        assert_eq!(text.contains("save notes now"), recovery_tools);
    }
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
    let (mut session, mut turn) = make_session_and_context().await;
    install_recovery_tools(&mut session);
    enable(&mut turn);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step = StepContext::for_test(Arc::clone(&turn));
    let world = Arc::new(session.build_world_state_for_step(&step).await);
    let request = codex_protocol::models::ResponseItem::from(
        codex_protocol::models::ResponseInputItem::Message {
            role: "user".to_string(),
            phase: None,
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: "Current accepted request: do not modify X.".to_string(),
            }],
        },
    );
    session
        .record_conversation_items(&turn, &[request])
        .await
        .unwrap();
    maybe_record(&session, &turn, Some(0), true).await.unwrap();
    let history_before = session.clone_history().await.raw_items().to_vec();
    let ids_before = session.state.lock().await.auto_compact_window_ids();
    // The old window never held an environment block, so only the rebuild can supply one.
    assert!(!history_text(&history_before).contains("<environment_context>"));
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
    let text = history_text(session.clone_history().await.raw_items());
    let environment_cwd = step.environments.turn_environments[0]
        .cwd()
        .inferred_native_path_string();
    assert!(
        text.contains("<environment_context>")
            && text.contains(&format!("<cwd>{environment_cwd}</cwd>")),
        "{text}"
    );
    assert!(!text.contains("save notes now"));
    assert!(text.contains("Current accepted request: do not modify X."));
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
#[tokio::test]
async fn fresh_window_replaces_stale_task_state_from_the_plan_owner() {
    use codex_protocol::plan_tool::{PlanItemArg, StepStatus, UpdatePlanArgs};

    let (mut session, mut turn) = make_session_and_context().await;
    install_recovery_tools(&mut session);
    enable(&mut turn);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let plan = |step: &str| UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            step: step.into(),
            status: StepStatus::Pending,
        }],
    };
    session
        .services
        .plan_store
        .restore(Some(plan("obsolete obligation")))
        .await;
    let old_world =
        crate::session::tests::build_world_state_from_turn_context(&session, &turn).await;
    let old_context = session
        .build_initial_context_with_world_state(&turn, &old_world)
        .await;
    assert!(history_text(&old_context).contains("obsolete obligation"));
    session
        .record_conversation_items(&turn, &old_context)
        .await
        .unwrap();
    let request = ResponseItem::from(codex_protocol::models::ResponseInputItem::Message {
        role: "user".into(),
        phase: None,
        content: vec![ContentItem::InputText {
            text: "Preserve my literal <codex_task_state>example</codex_task_state>.".into(),
        }],
    });
    session
        .record_conversation_items(&turn, &[request])
        .await
        .unwrap();
    session
        .services
        .plan_store
        .restore(Some(plan("current obligation")))
        .await;
    let step = StepContext::for_test(Arc::clone(&turn));
    let world = Arc::new(session.build_world_state_for_step(&step).await);
    // Sensitivity check using the former assembly operation, with the same
    // real plan/world producers and final sampling projector as the reset.
    let source = session.clone_history().await;
    let (retained, _, _) = crate::compact::build_task_input_checkpoint(source.raw_items());
    let mut legacy = session
        .build_initial_context_with_world_state(&turn, &world)
        .await;
    legacy.extend(retained);
    let legacy = crate::stable_context::project_stable_context(
        legacy.into(),
        crate::stable_context::StableContextTarget::Sampling,
    );
    let legacy_text = history_text(&legacy.items);
    assert!(legacy_text.contains("obsolete obligation"));
    assert!(!legacy_text.contains("current obligation"));
    let started = std::time::Instant::now();
    session
        .start_new_context_window(&step, world)
        .await
        .unwrap();
    eprintln!(
        "fresh_window_task_state_reset_elapsed={:?}",
        started.elapsed()
    );
    let history = session.clone_history().await;
    let text = history_text(history.raw_items());
    assert!(!text.contains("obsolete obligation"), "{text}");
    assert!(text.contains("current obligation"), "{text}");
    assert!(text.contains("Preserve my literal <codex_task_state>example</codex_task_state>."));
    let trusted = history
        .raw_items()
        .iter()
        .filter(|item| crate::stable_context::is_trusted_stable_context_item(item))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        history_text(&trusted).matches("<codex_task_state>").count(),
        1
    );
    let projected = crate::stable_context::project_stable_context(
        history.raw_items().to_vec().into(),
        crate::stable_context::StableContextTarget::Sampling,
    );
    assert!(history_text(&projected.items).contains("current obligation"));
    assert!(!history_text(&projected.items).contains("obsolete obligation"));
}
