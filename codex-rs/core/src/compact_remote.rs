use std::sync::Arc;
use std::sync::OnceLock;

use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::compact::compaction_status_from_result;
use crate::compact::insert_compaction_initial_context;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::context::world_state::WorldStateSnapshot;
use crate::context_manager::ContextManager;
use crate::context_manager::estimate_item_token_count;
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
use codex_utils_output_truncation::approx_token_count;
use std::collections::HashSet;

#[path = "compact_remote_request.rs"]
mod request;
use request::RemoteCompactAttempt;
use request::run_remote_compact_attempt;

const CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE: &str =
    "Output exceeded the available model context and was truncated";
const REMOTE_COMPACTION_TOOL_RECEIPT_MAX_TOKENS: usize = 2_000;
const REMOTE_COMPACTION_TOOL_RECEIPT_MAX_ITEMS: usize = 32;

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
    let world_state = Arc::new(sess.build_world_state_for_step(step_context.as_ref()).await);
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        /*fallback_step_context*/ None,
        /*turn_state*/ None,
        InitialContextInjection::AtStart(world_state),
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
        PreCompactHookOutcome::Stopped { reason } => {
            crate::hook_runtime::emit_hook_stop_reason(
                sess,
                turn_context,
                "PreCompact",
                reason.as_deref(),
            )
            .await;
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
        let recovery_summary = sess
            .services
            .task_evidence
            .compaction_recovery_summary()
            .await;
        let post_compact_outcome =
            run_post_compact_hooks(sess, turn_context, trigger, Some(&recovery_summary)).await;
        if let PostCompactHookOutcome::Stopped { reason } = post_compact_outcome {
            crate::hook_runtime::emit_hook_stop_reason(
                sess,
                turn_context,
                "PostCompact",
                reason.as_deref(),
            )
            .await;
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
    turn_context.turn_timing_state.begin_compaction_generation();
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
            if !should_retry_with_current_model(&error) {
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
            turn_context.turn_timing_state.record_model_fallback();
            let fallback_result = run_remote_compact_attempt(
                sess,
                fallback_step_context,
                turn_state,
                &fallback_compaction_trace,
                compaction_metadata,
                analytics_details,
            )
            .await;
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
                Err(fallback_error) => {
                    return Err(CodexErr::Fatal(format!(
                        "remote compaction failed with the previous model: {error}; retry with the current model also failed: {fallback_error}"
                    )));
                }
            }
        }
    };
    let RemoteCompactAttempt {
        new_history,
        trace_input_history,
    } = attempt;
    let (new_window_number, new_window_ids) = sess.advance_auto_compact_window().await;
    let (new_history, world_state_baseline, fragment_digests) = process_compacted_history(
        sess.as_ref(),
        compaction_turn_context.as_ref(),
        new_history,
        &initial_context_injection,
    )
    .await;

    let reference_context_item = match &initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::AtStart(_) => Some(compaction_turn_context.to_turn_context_item()),
        #[cfg(test)]
        InitialContextInjection::BeforeLastUserMessage(_) => {
            Some(compaction_turn_context.to_turn_context_item())
        }
    };
    let compacted_item = CompactedItem {
        message: String::new(),
        replacement_history: Some(new_history.clone()),
        window_number: Some(new_window_number),
        first_window_id: Some(new_window_ids.first_window_id.to_string()),
        previous_window_id: new_window_ids.previous_window_id.map(|id| id.to_string()),
        window_id: Some(new_window_ids.window_id.to_string()),
    };
    // Install is the semantic boundary where the compact endpoint's output becomes live
    // thread history. Keep it distinct from the later inference request so the reducer can
    // still represent repeated developer/context prefix items exactly as the model saw them.
    if let Some(trace_input_history) = trace_input_history {
        compaction_trace.record_installed(&CompactionCheckpointTracePayload {
            input_history: &trace_input_history,
            replacement_history: &new_history,
        });
    }
    sess.replace_compacted_history(
        compaction_turn_context.as_ref(),
        new_history,
        reference_context_item,
        world_state_baseline,
        fragment_digests,
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
) -> (
    Vec<ResponseItem>,
    Option<WorldStateSnapshot>,
    Vec<codex_protocol::protocol::ContextFragmentDigest>,
) {
    // Preserve the caller-selected replacement ordering. The default `AtStart` path retains the
    // cacheable prompt prefix while still leaving the summary or compaction item last.
    let (initial_context, world_state_baseline, fragment_digests) =
        build_compaction_initial_context(sess, turn_context, initial_context_injection).await;

    compacted_history = bounded_remote_compacted_history(compacted_history);
    (
        insert_compaction_initial_context(
            compacted_history,
            initial_context,
            initial_context_injection,
        ),
        world_state_baseline,
        fragment_digests,
    )
}

/// Returns whether an item from remote compaction output should be preserved.
///
/// Called while processing the model-provided compacted transcript, before we
/// append fresh canonical context from the current session.
///
/// Raw messages are consumed evidence after the remote endpoint has produced an opaque
/// compaction item. Only that item and recoverable tool receipts remain eligible; fresh current
/// instructions and durable task state are injected separately by the caller.
pub(crate) fn should_keep_compacted_history_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
    )
}

fn bounded_remote_compacted_history(items: Vec<ResponseItem>) -> Vec<ResponseItem> {
    let mut retained_indices = HashSet::new();
    let mut remaining_tokens = REMOTE_COMPACTION_TOOL_RECEIPT_MAX_TOKENS;
    let mut retained_tool_items = 0usize;
    let mut retained_compaction = false;

    for (index, item) in items.iter().enumerate().rev() {
        match item {
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                if !retained_compaction =>
            {
                retained_compaction = true;
                retained_indices.insert(index);
            }
            item if should_keep_compacted_history_item(item) => {
                let Some(group) = complete_tool_receipt_indices(&items, index) else {
                    continue;
                };
                if !group
                    .iter()
                    .any(|index| item_has_recoverable_artifact_reference(&items[*index]))
                {
                    continue;
                }
                if group.iter().any(|index| retained_indices.contains(index)) {
                    continue;
                }
                if retained_tool_items.saturating_add(group.len())
                    > REMOTE_COMPACTION_TOOL_RECEIPT_MAX_ITEMS
                {
                    continue;
                }
                let tokens = group.iter().fold(0usize, |total, index| {
                    let item_tokens =
                        usize::try_from(estimate_item_token_count(&items[*index]).max(1))
                            .unwrap_or(usize::MAX);
                    total.saturating_add(item_tokens)
                });
                if tokens <= remaining_tokens {
                    retained_tool_items = retained_tool_items.saturating_add(group.len());
                    remaining_tokens = remaining_tokens.saturating_sub(tokens);
                    retained_indices.extend(group);
                }
            }
            _ => {}
        }
    }

    items
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| retained_indices.contains(&index).then_some(item))
        .collect()
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ToolReceiptKind {
    Function,
    Custom,
    Search,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ToolReceiptSide {
    Call,
    Output,
}

fn complete_tool_receipt_indices(items: &[ResponseItem], index: usize) -> Option<Vec<usize>> {
    let (kind, side, call_id) = tool_receipt_identity(&items[index])?;
    let counterpart_side = match side {
        ToolReceiptSide::Call => ToolReceiptSide::Output,
        ToolReceiptSide::Output => ToolReceiptSide::Call,
    };
    let counterpart = items
        .iter()
        .enumerate()
        .find_map(|(candidate_index, item)| {
            let (candidate_kind, candidate_side, candidate_call_id) = tool_receipt_identity(item)?;
            (candidate_kind == kind
                && candidate_side == counterpart_side
                && candidate_call_id == call_id)
                .then_some(candidate_index)
        })?;
    let mut group = vec![index, counterpart];
    group.sort_unstable();
    group.dedup();
    Some(group)
}

fn item_has_recoverable_artifact_reference(item: &ResponseItem) -> bool {
    let text = match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.body.to_text(),
        _ => None,
    };
    text.is_some_and(|text| has_recoverable_artifact_reference(&text))
}

fn has_recoverable_artifact_reference(text: &str) -> bool {
    let lowercase = text.to_ascii_lowercase();
    let has_recovery_cue = lowercase.contains("read_tool_output")
        || lowercase.contains("raw output artifact:")
        || lowercase.contains("raw_output_artifact_id")
        || lowercase.contains("available as artifact")
        || lowercase.contains("retained as artifact");
    has_recovery_cue
        && text
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
            .any(|candidate| uuid::Uuid::parse_str(candidate).is_ok())
}

fn tool_receipt_identity(item: &ResponseItem) -> Option<(ToolReceiptKind, ToolReceiptSide, &str)> {
    match item {
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::FunctionCall { call_id, .. } => {
            Some((ToolReceiptKind::Function, ToolReceiptSide::Call, call_id))
        }
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            Some((ToolReceiptKind::Function, ToolReceiptSide::Output, call_id))
        }
        ResponseItem::CustomToolCall { call_id, .. } => {
            Some((ToolReceiptKind::Custom, ToolReceiptSide::Call, call_id))
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            Some((ToolReceiptKind::Custom, ToolReceiptSide::Output, call_id))
        }
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => Some((ToolReceiptKind::Search, ToolReceiptSide::Call, call_id)),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some((ToolReceiptKind::Search, ToolReceiptSide::Output, call_id)),
        _ => None,
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
    let item_token_counts = history
        .raw_items()
        .iter()
        .map(estimate_item_token_count)
        .collect::<Vec<_>>();
    let mut estimated_tokens =
        i128::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i128::MAX);
    for item_tokens in &item_token_counts {
        estimated_tokens = estimated_tokens.saturating_add(i128::from(*item_tokens));
    }
    let estimated_tokens_before = estimated_tokens;
    let context_window = i128::from(context_window);
    let mut replacements = Vec::new();

    for index in (0..history.raw_items().len()).rev() {
        if estimated_tokens <= context_window {
            break;
        }
        let Some(rewritten_item) = history
            .raw_items()
            .get(index)
            .and_then(rewritten_output_for_context_window)
        else {
            break;
        };
        let rewritten_tokens = estimate_item_token_count(&rewritten_item);
        estimated_tokens = estimated_tokens
            .saturating_sub(i128::from(item_token_counts[index]))
            .saturating_add(i128::from(rewritten_tokens));
        replacements.push((index, rewritten_item));
    }

    let rewritten_outputs = replacements.len();
    if rewritten_outputs > 0 {
        let mut items = history.raw_items().to_vec();
        for (index, rewritten_item) in replacements {
            items[index] = rewritten_item;
        }
        history.replace(items);
    }

    let estimated_deleted_tokens = estimated_tokens_before.saturating_sub(estimated_tokens);
    let estimated_deleted_tokens = i64::try_from(estimated_deleted_tokens).unwrap_or_else(|_| {
        if estimated_deleted_tokens.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    });
    (rewritten_outputs, estimated_deleted_tokens)
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
            id,
            call_id,
            status,
            execution,
            omitted_result_count,
            internal_chat_message_metadata_passthrough: metadata,
            ..
        } => ResponseItem::ToolSearchOutput {
            id: id.clone(),
            call_id: call_id.clone(),
            status: status.clone(),
            execution: execution.clone(),
            tools: Vec::new(),
            omitted_result_count: *omitted_result_count,
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
#[path = "compact_remote_tests.rs"]
mod tests;
