use std::sync::Arc;
use std::sync::OnceLock;

use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::compact::compaction_status_from_result;
use crate::compact::insert_initial_context_before_last_real_user_or_summary;
use crate::compact_model_fallback::record_model_fallback;
use crate::context::world_state::WorldState;
use crate::context_manager::ContextManager;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout_trace::CompactionCheckpointTracePayload;

#[path = "compact_remote_request.rs"]
mod request;
use request::RemoteCompactAttempt;
use request::run_remote_compact_attempt;

const CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE: &str =
    "Output exceeded the available model context and was truncated";

pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    fallback_step_context: Option<Arc<StepContext>>,
    turn_state: Arc<OnceLock<String>>,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
) -> CodexResult<()> {
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Auto,
        reason,
        CompactionImplementation::ResponsesCompact,
        phase,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        fallback_step_context.as_ref(),
        Some(turn_state),
        initial_context_injection,
        compaction_metadata,
    )
    .await?;
    Ok(())
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    // Standalone compaction is its own request boundary, so it captures a fresh step.
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await;
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;

    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionImplementation::ResponsesCompact,
        CompactionPhase::StandaloneTurn,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        /*fallback_step_context*/ None,
        /*turn_state*/ None,
        InitialContextInjection::DoNotInject,
        compaction_metadata,
    )
    .await?;
    Ok(())
}

async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    turn_state: Option<Arc<OnceLock<String>>>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let trigger = compaction_metadata.trigger();
    let reason = compaction_metadata.reason();
    let implementation = compaction_metadata.implementation();
    let phase = compaction_metadata.phase();
    let mut analytics_details = CompactionAnalyticsDetails {
        active_context_tokens_before: Some(sess.get_total_token_usage().await),
        ..Default::default()
    };
    let attempt = CompactionAnalyticsAttempt::begin(
        sess.as_ref(),
        turn_context.as_ref(),
        trigger,
        reason,
        implementation,
        phase,
    )
    .await;
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => {
            let error = CodexErr::TurnAborted;
            attempt
                .track(
                    sess.as_ref(),
                    codex_analytics::CompactionStatus::Interrupted,
                    Some(&error),
                    analytics_details,
                )
                .await;
            return Err(error);
        }
    }
    let result = run_remote_compact_task_inner_impl(
        sess,
        step_context,
        fallback_step_context,
        turn_state,
        initial_context_injection,
        compaction_metadata,
        &mut analytics_details,
    )
    .await;
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if result.is_ok() {
        let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
        if let PostCompactHookOutcome::Stopped = post_compact_outcome {
            attempt
                .track(sess.as_ref(), status, codex_error, analytics_details)
                .await;
            return Err(CodexErr::TurnAborted);
        }
    }
    attempt
        .track(sess.as_ref(), status, codex_error, analytics_details)
        .await;
    if let Err(err) = result {
        sess.track_turn_codex_error(turn_context, &err);
        let event = EventMsg::Error(
            err.to_error_event(Some("Error running remote compact task".to_string())),
        );
        sess.send_event(turn_context, event).await;
        return Err(err);
    }
    Ok(())
}

async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    turn_state: Option<Arc<OnceLock<String>>>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let context_compaction_item = ContextCompactionItem::new();
    let compaction_id = context_compaction_item.id.clone();
    // Use the UI compaction item ID as the trace compaction ID so protocol lifecycle events,
    // endpoint attempts, and the installed history checkpoint all have one join key.
    let compaction_trace = sess.services.rollout_thread_trace.compaction_trace_context(
        turn_context.sub_id.as_str(),
        compaction_id.as_str(),
        turn_context.model_info.slug.as_str(),
        turn_context.provider.info().name.as_str(),
    );
    let compaction_item = TurnItem::ContextCompaction(context_compaction_item);
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let attempt = run_remote_compact_attempt(
        sess,
        step_context,
        turn_state.clone(),
        &compaction_trace,
        compaction_metadata,
        analytics_details,
    )
    .await;
    let (attempt, compaction_turn_context) = match attempt {
        Ok(attempt) => (attempt, turn_context),
        Err(error) => {
            let Some(fallback_step_context) = fallback_step_context else {
                return Err(error);
            };
            if !matches!(&error, CodexErr::InvalidRequest(_)) {
                return Err(error);
            }
            let fallback_turn_context = &fallback_step_context.turn;
            let fallback_compaction_trace =
                sess.services.rollout_thread_trace.compaction_trace_context(
                    fallback_turn_context.sub_id.as_str(),
                    compaction_id.as_str(),
                    fallback_turn_context.model_info.slug.as_str(),
                    fallback_turn_context.provider.info().name.as_str(),
                );
            let fallback_result = run_remote_compact_attempt(
                sess,
                fallback_step_context,
                turn_state,
                &fallback_compaction_trace,
                compaction_metadata,
                analytics_details,
            )
            .await;
            turn_context.turn_timing_state.record_model_fallback();
            record_model_fallback(
                &sess.services.session_telemetry,
                turn_context.model_info.slug.as_str(),
                fallback_turn_context.model_info.slug.as_str(),
                compaction_metadata.reason(),
                compaction_metadata.implementation(),
                fallback_result.as_ref().err(),
            );
            match fallback_result {
                Ok(attempt) => (attempt, fallback_turn_context),
                Err(_) => return Err(error),
            }
        }
    };
    let RemoteCompactAttempt {
        new_history,
        trace_input_history,
    } = attempt;
    let (new_window_number, new_window_ids) = sess.advance_auto_compact_window().await;
    let (new_history, world_state_baseline) = process_compacted_history(
        sess.as_ref(),
        compaction_turn_context.as_ref(),
        new_history,
        &initial_context_injection,
    )
    .await;

    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage(_) => {
            Some(compaction_turn_context.to_turn_context_item())
        }
    };
    let compacted_item = CompactedItem {
        message: String::new(),
        replacement_history: None,
        window_number: Some(new_window_number),
        first_window_id: Some(new_window_ids.first_window_id.to_string()),
        previous_window_id: new_window_ids.previous_window_id.map(|id| id.to_string()),
        window_id: Some(new_window_ids.window_id.to_string()),
    };
    // Install is the semantic boundary where the compact endpoint's output becomes live
    // thread history. Keep it distinct from the later inference request so the reducer can
    // still represent repeated developer/context prefix items exactly as the model saw them.
    compaction_trace.record_installed(&CompactionCheckpointTracePayload {
        input_history: &trace_input_history,
        replacement_history: &new_history,
    });
    sess.replace_compacted_history(
        compaction_turn_context.as_ref(),
        new_history,
        reference_context_item,
        world_state_baseline,
        compacted_item,
    )
    .await;
    sess.recompute_token_usage(compaction_turn_context).await;

    sess.emit_turn_item_completed(compaction_turn_context, compaction_item)
        .await;
    Ok(())
}

pub(crate) async fn process_compacted_history(
    sess: &Session,
    turn_context: &TurnContext,
    mut compacted_history: Vec<ResponseItem>,
    initial_context_injection: &InitialContextInjection,
) -> (Vec<ResponseItem>, Option<Arc<WorldState>>) {
    // Mid-turn compaction is the only path that must inject initial context above the last user
    // message in the replacement history. Pre-turn compaction instead injects context after the
    // compaction item, but mid-turn compaction keeps the compaction item last for model training.
    let (initial_context, world_state_baseline) =
        build_compaction_initial_context(sess, turn_context, initial_context_injection).await;

    compacted_history.retain(should_keep_compacted_history_item);
    (
        insert_initial_context_before_last_real_user_or_summary(compacted_history, initial_context),
        world_state_baseline,
    )
}

/// Returns whether an item from remote compaction output should be preserved.
///
/// Called while processing the model-provided compacted transcript, before we
/// append fresh canonical context from the current session.
///
/// We drop:
/// - `developer` messages because remote output can include stale/duplicated
///   instruction content.
/// - non-user-content `user` messages (session prefix/instruction wrappers),
///   while preserving real user messages and persisted hook prompts.
///
/// This intentionally keeps:
/// - `assistant` messages (future remote compaction models may emit them)
/// - `user`-role warnings that parse as `TurnItem::UserMessage` and compaction-generated summary
///   messages. Legacy warning fragments are filtered by `parse_turn_item` before they reach this
///   check.
pub(crate) fn should_keep_compacted_history_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } if role == "developer" => false,
        ResponseItem::Message { role, .. } if role == "user" => {
            matches!(
                crate::event_mapping::parse_turn_item(item),
                Some(TurnItem::UserMessage(_) | TurnItem::HookPrompt(_))
            )
        }
        ResponseItem::Message { role, .. } if role == "assistant" => true,
        ResponseItem::Message { .. } => false,
        ResponseItem::AgentMessage { .. } => true,
        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::CompactionTrigger { .. } => false,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Other => false,
    }
}

pub(crate) fn trim_function_call_history_to_fit_context_window(
    history: &mut ContextManager,
    turn_context: &TurnContext,
    base_instructions: &BaseInstructions,
) -> (usize, i64) {
    let Some(context_window) = turn_context.model_context_window() else {
        return (0, 0);
    };
    trim_function_call_history_to_fit_context_window_with_costs(
        history,
        context_window,
        base_instructions,
        proven_rewrite_costs,
    )
}

fn trim_function_call_history_to_fit_context_window_with_costs(
    history: &mut ContextManager,
    context_window: i64,
    base_instructions: &BaseInstructions,
    rewrite_costs: fn(&ResponseItem, &ResponseItem) -> Option<(u128, u128)>,
) -> (usize, i64) {
    let base_cost =
        crate::context_manager::estimate_base_instruction_token_count(base_instructions);
    let mut exact_total = u128::try_from(base_cost).unwrap_or_default();
    for item in history.raw_items() {
        exact_total = exact_total.saturating_add(
            u128::try_from(crate::context_manager::estimate_item_token_count(item))
                .unwrap_or_default(),
        );
    }

    let mut replacements = Vec::new();
    let mut estimated_deleted_tokens = 0i64;
    for index in (0..history.raw_items().len()).rev() {
        let estimated_tokens_before = clamped_token_total(exact_total);
        if estimated_tokens_before <= context_window {
            break;
        }
        let item = &history.raw_items()[index];
        let Some(rewritten_item) = rewritten_output_for_context_window(item) else {
            break;
        };
        let Some((old_cost, new_cost)) = rewrite_costs(item, &rewritten_item) else {
            return trim_function_call_history_to_fit_context_window_legacy(
                history,
                context_window,
                base_instructions,
            );
        };
        exact_total = exact_total
            .saturating_sub(old_cost)
            .saturating_add(new_cost);
        let estimated_tokens_after = clamped_token_total(exact_total);
        estimated_deleted_tokens = estimated_deleted_tokens
            .saturating_add(estimated_tokens_before.saturating_sub(estimated_tokens_after));
        replacements.push((index, rewritten_item));
    }

    if replacements.is_empty() {
        return (0, 0);
    }
    let rewritten_outputs = replacements.len();
    let mut items = history.raw_items().to_vec();
    for (index, replacement) in replacements {
        items[index] = replacement;
    }
    history.replace_with_rewrite_count(items, rewritten_outputs);
    (rewritten_outputs, estimated_deleted_tokens)
}

fn trim_function_call_history_to_fit_context_window_legacy(
    history: &mut ContextManager,
    context_window: i64,
    base_instructions: &BaseInstructions,
) -> (usize, i64) {
    let mut rewritten_outputs = 0usize;
    let mut estimated_deleted_tokens = 0i64;
    let item_count = history.raw_items().len();

    for index in (0..item_count).rev() {
        let Some(estimated_tokens_before) =
            history.estimate_token_count_with_base_instructions(base_instructions)
        else {
            break;
        };
        if estimated_tokens_before <= context_window {
            break;
        }
        let Some(rewritten_item) = history
            .raw_items()
            .get(index)
            .and_then(rewritten_output_for_context_window)
        else {
            break;
        };
        let mut items = history.raw_items().to_vec();
        items[index] = rewritten_item;
        history.replace(items);
        let estimated_tokens_after = history
            .estimate_token_count_with_base_instructions(base_instructions)
            .unwrap_or_default();
        rewritten_outputs += 1;
        estimated_deleted_tokens = estimated_deleted_tokens
            .saturating_add(estimated_tokens_before.saturating_sub(estimated_tokens_after));
    }

    (rewritten_outputs, estimated_deleted_tokens)
}

fn proven_rewrite_costs(original: &ResponseItem, rewritten: &ResponseItem) -> Option<(u128, u128)> {
    if !matches!(
        original,
        ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
    ) {
        return None;
    }
    let old_cost = crate::context_manager::estimate_item_token_count(original);
    let new_cost = crate::context_manager::estimate_item_token_count(rewritten);
    Some((
        u128::try_from(old_cost).unwrap_or_default(),
        u128::try_from(new_cost).unwrap_or_default(),
    ))
}

fn clamped_token_total(total: u128) -> i64 {
    i64::try_from(total).unwrap_or(i64::MAX)
}

fn rewritten_output_for_context_window(item: &ResponseItem) -> Option<ResponseItem> {
    Some(match item {
        ResponseItem::FunctionCallOutput {
            id,
            call_id,
            output,
            internal_chat_message_metadata_passthrough: metadata,
        } => ResponseItem::FunctionCallOutput {
            id: id.clone(),
            call_id: call_id.clone(),
            output: truncated_output_payload(output),
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
            output: truncated_output_payload(output),
            internal_chat_message_metadata_passthrough: metadata.clone(),
        },
        ResponseItem::ToolSearchOutput {
            call_id,
            status,
            execution,
            internal_chat_message_metadata_passthrough: metadata,
            ..
        } => ResponseItem::ToolSearchOutput {
            id: item.id().map(str::to_string),
            call_id: call_id.clone(),
            status: status.clone(),
            execution: execution.clone(),
            tools: Vec::new(),
            internal_chat_message_metadata_passthrough: metadata.clone(),
        },
        _ => return None,
    })
}

fn truncated_output_payload(output: &FunctionCallOutputPayload) -> FunctionCallOutputPayload {
    FunctionCallOutputPayload {
        body: FunctionCallOutputBody::Text(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string()),
        success: output.success,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn text_output(text: &str) -> FunctionCallOutputPayload {
        FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(text.to_string()),
            success: Some(true),
        }
    }

    fn mixed_history() -> ContextManager {
        let large = "tool output ".repeat(512);
        let items = vec![
            ResponseItem::FunctionCallOutput {
                id: Some("prefix-output".to_string()),
                call_id: "prefix-call".to_string(),
                output: text_output(&large),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Other,
            ResponseItem::CustomToolCallOutput {
                id: Some("custom-output".to_string()),
                call_id: "custom-call".to_string(),
                name: Some("custom".to_string()),
                output: text_output(&large),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ToolSearchOutput {
                id: Some("search-output".to_string()),
                call_id: Some("search-call".to_string()),
                status: "completed".to_string(),
                execution: "server".to_string(),
                tools: vec![json!({"name": "tool", "description": large})],
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: Some("function-output".to_string()),
                call_id: "function-call".to_string(),
                output: text_output(&large),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let mut history = ContextManager::new();
        history.replace(items);
        history
    }

    #[test]
    fn linear_compaction_matches_legacy_for_proven_variants_and_budgets() {
        let base_instructions = BaseInstructions {
            text: "base instructions".to_string(),
        };
        let history = mixed_history();
        let total = history
            .estimate_token_count_with_base_instructions(&base_instructions)
            .expect("history estimate");

        for context_window in [total, total.saturating_sub(1), total / 2, 0] {
            let mut linear = history.clone();
            let mut legacy = history.clone();

            let linear_result = trim_function_call_history_to_fit_context_window_with_costs(
                &mut linear,
                context_window,
                &base_instructions,
                proven_rewrite_costs,
            );
            let legacy_result = trim_function_call_history_to_fit_context_window_legacy(
                &mut legacy,
                context_window,
                &base_instructions,
            );

            assert_eq!(linear_result, legacy_result, "budget {context_window}");
            assert_eq!(
                linear.raw_items(),
                legacy.raw_items(),
                "budget {context_window}"
            );
            assert_eq!(
                linear.history_version(),
                legacy.history_version(),
                "budget {context_window}"
            );
        }
    }

    #[test]
    fn unproven_rewrite_falls_back_against_untouched_history() {
        fn reject_custom_output_costs(
            original: &ResponseItem,
            rewritten: &ResponseItem,
        ) -> Option<(u128, u128)> {
            if matches!(original, ResponseItem::CustomToolCallOutput { .. }) {
                None
            } else {
                proven_rewrite_costs(original, rewritten)
            }
        }

        let base_instructions = BaseInstructions {
            text: "base instructions".to_string(),
        };
        let mut actual = mixed_history();
        let mut expected = actual.clone();

        let actual_result = trim_function_call_history_to_fit_context_window_with_costs(
            &mut actual,
            0,
            &base_instructions,
            reject_custom_output_costs,
        );
        let expected_result = trim_function_call_history_to_fit_context_window_legacy(
            &mut expected,
            0,
            &base_instructions,
        );

        assert_eq!(actual_result, expected_result);
        assert_eq!(actual.raw_items(), expected.raw_items());
        assert_eq!(actual.history_version(), expected.history_version());
    }

    #[test]
    fn linear_compaction_matches_legacy_at_saturation_boundaries() {
        assert_eq!(clamped_token_total(u128::MAX), i64::MAX);

        let base_instructions = BaseInstructions {
            text: "base instructions".to_string(),
        };
        let items = mixed_history().into_raw_items();
        let mut linear = ContextManager::new();
        linear.replace_with_rewrite_count(items.clone(), usize::MAX);
        let mut legacy = ContextManager::new();
        legacy.replace_with_rewrite_count(items, usize::MAX);

        let linear_result = trim_function_call_history_to_fit_context_window_with_costs(
            &mut linear,
            0,
            &base_instructions,
            proven_rewrite_costs,
        );
        let legacy_result = trim_function_call_history_to_fit_context_window_legacy(
            &mut legacy,
            0,
            &base_instructions,
        );

        assert_eq!(linear_result, legacy_result);
        assert_eq!(linear.raw_items(), legacy.raw_items());
        assert_eq!(linear.history_version(), u64::MAX);
        assert_eq!(linear.history_version(), legacy.history_version());
    }

    #[test]
    fn linear_compaction_request_body_matches_legacy_and_wire_golden() {
        let large = "tool output ".repeat(512);
        let items = vec![
            ResponseItem::FunctionCall {
                id: Some("function-call-item".to_string()),
                name: "function_tool".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "function-call".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCall {
                id: Some("custom-call-item".to_string()),
                status: None,
                call_id: "custom-call".to_string(),
                name: "custom_tool".to_string(),
                namespace: None,
                input: "input".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ToolSearchCall {
                id: Some("search-call-item".to_string()),
                call_id: Some("search-call".to_string()),
                status: Some("completed".to_string()),
                execution: "server".to_string(),
                arguments: json!({ "query": "tool" }),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCallOutput {
                id: Some("custom-output".to_string()),
                call_id: "custom-call".to_string(),
                name: Some("custom_tool".to_string()),
                output: text_output(&large),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ToolSearchOutput {
                id: Some("search-output".to_string()),
                call_id: Some("search-call".to_string()),
                status: "completed".to_string(),
                execution: "server".to_string(),
                tools: vec![json!({ "name": "tool", "description": large })],
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: Some("function-output".to_string()),
                call_id: "function-call".to_string(),
                output: text_output(&large),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let mut linear = ContextManager::new();
        linear.replace(items.clone());
        let mut legacy = ContextManager::new();
        legacy.replace(items);
        let base_instructions = BaseInstructions {
            text: "base instructions".to_string(),
        };

        let linear_result = trim_function_call_history_to_fit_context_window_with_costs(
            &mut linear,
            0,
            &base_instructions,
            proven_rewrite_costs,
        );
        let legacy_result = trim_function_call_history_to_fit_context_window_legacy(
            &mut legacy,
            0,
            &base_instructions,
        );
        assert_eq!(linear_result, legacy_result);

        let request_body = |history: ContextManager| {
            let input = history.for_prompt(&[codex_protocol::openai_models::InputModality::Text]);
            serde_json::to_value(codex_api::CompactionInput {
                model: "test-model",
                input: &input,
                instructions: &base_instructions.text,
                tools: None,
                parallel_tool_calls: false,
                reasoning: None,
                service_tier: None,
                prompt_cache_key: None,
                text: None,
            })
            .expect("serialize compaction request")
        };
        let linear_body = request_body(linear);
        let legacy_body = request_body(legacy);

        assert_eq!(linear_body, legacy_body);
        assert_eq!(
            linear_body,
            json!({
                "model": "test-model",
                "input": [
                    {
                        "type": "function_call",
                        "id": "function-call-item",
                        "name": "function_tool",
                        "arguments": "{}",
                        "call_id": "function-call"
                    },
                    {
                        "type": "custom_tool_call",
                        "id": "custom-call-item",
                        "call_id": "custom-call",
                        "name": "custom_tool",
                        "input": "input"
                    },
                    {
                        "type": "tool_search_call",
                        "id": "search-call-item",
                        "call_id": "search-call",
                        "status": "completed",
                        "execution": "server",
                        "arguments": { "query": "tool" }
                    },
                    {
                        "type": "custom_tool_call_output",
                        "id": "custom-output",
                        "call_id": "custom-call",
                        "name": "custom_tool",
                        "output": CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE
                    },
                    {
                        "type": "tool_search_output",
                        "id": "search-output",
                        "call_id": "search-call",
                        "status": "completed",
                        "execution": "server",
                        "tools": []
                    },
                    {
                        "type": "function_call_output",
                        "id": "function-output",
                        "call_id": "function-call",
                        "output": CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE
                    }
                ],
                "instructions": "base instructions",
                "parallel_tool_calls": false
            })
        );
    }
}
