use std::sync::Arc;
use std::time::Instant;

use crate::Prompt;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::context::is_legacy_compaction_warning_fragment;
use crate::context::is_startup_contextual_user_fragment;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::handle_retryable_response_stream_error;
#[cfg(test)]
use crate::session::PreviousTurnSettings;
use crate::session::session::Session;
use crate::session::turn::get_last_assistant_message_from_turn;
use crate::session::turn_context::TurnContext;
use codex_analytics::CodexCompactionEvent;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionStatus;
use codex_analytics::CompactionStrategy;
use codex_analytics::CompactionTrigger;
use codex_analytics::now_unix_seconds;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout_trace::InferenceTraceContext;
use codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use futures::prelude::*;

use codex_model_provider_info::ModelProviderInfo;

pub use codex_prompts::COMPACTION_BASE_INSTRUCTIONS;
pub use codex_prompts::INCREMENTAL_SUMMARIZATION_PROMPT;
pub use codex_prompts::SUMMARIZATION_PROMPT;
pub use codex_prompts::SUMMARY_PREFIX;
const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 4_000;
const COMPACT_AGENT_MESSAGE_MAX_TOKENS: usize = 4_000;
const COMPACT_TASK_STATE_MAX_TOKENS: usize = 1_800;
const COMPACT_UNSTRUCTURED_UPDATE_MAX_TOKENS: usize = 1_000;
const GOAL_HEADING: &str = "## Goal";
const CURRENT_STATE_HEADING: &str = "## Current state";
const COMPLETED_WORK_HEADING: &str = "## Completed work";
const UNRESOLVED_WORK_HEADING: &str = "## Unresolved work";
const EVIDENCE_HEADING: &str = "## Evidence";
const NEXT_ACTION_HEADING: &str = "## Next action";
const COMPACTION_SECTIONS: [(&str, usize); 6] = [
    (GOAL_HEADING, 250),
    (CURRENT_STATE_HEADING, 350),
    (COMPLETED_WORK_HEADING, 250),
    (UNRESOLVED_WORK_HEADING, 350),
    (EVIDENCE_HEADING, 300),
    (NEXT_ACTION_HEADING, 150),
];
pub(crate) const MAX_RETAINED_USER_IMAGES: usize = 8;
pub(crate) const MAX_RETAINED_USER_IMAGE_BYTES: usize =
    MAX_PROMPT_IMAGE_SOURCE_BYTES / 3 * 4 + 4096;
pub(crate) const COMPACT_IMAGE_OMISSION_MARKER: &str =
    "[codex-local-compaction omitted user images: limits exceeded]";

/// Controls whether compaction replacement history must include initial context.
///
/// Pre-turn/manual compaction variants use `AtStart` so the next request retains the same
/// cacheable initial-context prefix instead of appending a fresh copy after the summary.
///
/// The test-only `BeforeLastUserMessage` variant preserves coverage for legacy replacement-history
/// ordering. `AtStart` keeps the summary or compaction item last while preserving the stable prompt
/// prefix in production.
#[derive(Debug)]
pub(crate) enum InitialContextInjection {
    AtStart(Arc<WorldState>),
    #[cfg(test)]
    BeforeLastUserMessage(Arc<WorldState>),
    DoNotInject,
}

pub(crate) async fn build_compaction_initial_context(
    sess: &Session,
    turn_context: &TurnContext,
    initial_context_injection: &InitialContextInjection,
) -> (Vec<ResponseItem>, Option<WorldStateSnapshot>) {
    // Return the rendered state with its items so history and its baseline stay identical.
    match initial_context_injection {
        InitialContextInjection::AtStart(world_state) => {
            let (items, delivered_snapshot) = sess
                .build_initial_context_with_world_state_and_snapshot(
                    turn_context,
                    world_state.as_ref(),
                )
                .await;
            (items, Some(delivered_snapshot))
        }
        #[cfg(test)]
        InitialContextInjection::BeforeLastUserMessage(world_state) => {
            let (items, delivered_snapshot) = sess
                .build_initial_context_with_world_state_and_snapshot(
                    turn_context,
                    world_state.as_ref(),
                )
                .await;
            (items, Some(delivered_snapshot))
        }
        InitialContextInjection::DoNotInject => (Vec::new(), None),
    }
}

pub(crate) fn should_use_remote_compact_task(provider: &ModelProviderInfo) -> bool {
    provider.supports_remote_compaction()
}

pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
) -> CodexResult<()> {
    let prompt = turn_context
        .config
        .compact_prompt
        .as_deref()
        .unwrap_or(SUMMARIZATION_PROMPT)
        .to_string();
    let input = vec![UserInput::Text {
        text: prompt,
        // Compaction prompt is synthesized; no UI element ranges to preserve.
        text_elements: Vec::new(),
    }];

    run_compact_task_inner(
        sess,
        turn_context,
        input,
        initial_context_injection,
        CompactionTrigger::Auto,
        reason,
        phase,
    )
    .await?;
    Ok(())
}

pub(crate) async fn run_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await;
    let world_state = Arc::new(sess.build_world_state_for_step(step_context.as_ref()).await);
    run_compact_task_inner(
        sess.clone(),
        turn_context,
        input,
        InitialContextInjection::AtStart(world_state),
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionPhase::StandaloneTurn,
    )
    .await?;
    Ok(())
}

async fn run_compact_task_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
    trigger: CompactionTrigger,
    reason: CompactionReason,
    phase: CompactionPhase,
) -> CodexResult<()> {
    let compaction_metadata =
        CompactionTurnMetadata::new(trigger, reason, CompactionImplementation::Responses, phase);
    let attempt = CompactionAnalyticsAttempt::begin(
        sess.as_ref(),
        turn_context.as_ref(),
        trigger,
        reason,
        CompactionImplementation::Responses,
        phase,
    )
    .await;
    let pre_compact_outcome = run_pre_compact_hooks(&sess, &turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped { reason } => {
            crate::hook_runtime::emit_hook_stop_reason(
                &sess,
                &turn_context,
                "PreCompact",
                reason.as_deref(),
            )
            .await;
            let error = CodexErr::TurnAborted;
            attempt
                .track(
                    sess.as_ref(),
                    CompactionStatus::Interrupted,
                    Some(&error),
                    CompactionAnalyticsDetails::default(),
                )
                .await;
            return Err(error);
        }
    }
    let result = run_compact_task_inner_impl(
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        input,
        initial_context_injection,
        compaction_metadata,
    )
    .await;
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if let Ok(summary) = &result {
        let post_compact_outcome =
            run_post_compact_hooks(&sess, &turn_context, trigger, Some(summary)).await;
        if let PostCompactHookOutcome::Stopped { reason } = post_compact_outcome {
            crate::hook_runtime::emit_hook_stop_reason(
                &sess,
                &turn_context,
                "PostCompact",
                reason.as_deref(),
            )
            .await;
            attempt
                .track(
                    sess.as_ref(),
                    status,
                    codex_error,
                    CompactionAnalyticsDetails::default(),
                )
                .await;
            return Err(CodexErr::TurnAborted);
        }
    }
    attempt
        .track(
            sess.as_ref(),
            status,
            codex_error,
            CompactionAnalyticsDetails::default(),
        )
        .await;
    result.map(|_| ())
}

async fn run_compact_task_inner_impl(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    mut input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
) -> CodexResult<String> {
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(&turn_context, &compaction_item)
        .await;
    let mut history = sess.clone_history().await;
    let (mut unresolved_history, _retained_image_count, omitted_images) =
        build_bounded_unresolved_input_history(history.raw_items());
    let previous_summary = latest_summary_message(history.raw_items()).map(str::to_string);
    let reuse_previous_summary = previous_summary.is_some()
        && history_after_latest_summary_is_user_only(history.raw_items());
    if previous_summary.is_some() && !reuse_previous_summary {
        input.push(UserInput::Text {
            text: INCREMENTAL_SUMMARIZATION_PROMPT.to_string(),
            text_elements: Vec::new(),
        });
    }
    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);
    history.record_items(
        &[initial_input_for_turn.into()],
        turn_context.model_info.truncation_policy.into(),
    );

    let base_instructions = BaseInstructions {
        text: COMPACTION_BASE_INSTRUCTIONS.trim().to_string(),
    };
    let max_retries = turn_context.provider.info().stream_max_retries();
    let mut client_session = sess.services.model_client.new_session();
    // Reuse one client session so turn-scoped state (sticky routing, websocket incremental
    // request tracking)
    // survives retries within this compact turn.
    let window_id = sess.current_window_id().await;
    let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
        sess.installation_id.clone(),
        window_id,
        CodexResponsesRequestKind::Compaction(compaction_metadata),
    );

    let workspace_identity = sess
        .services
        .git_workspace
        .workspace_evidence_identity(turn_context.config.cwd.as_path())
        .await;
    let turn_input = history
        .clone()
        .for_compaction_prompt_with_completed_tool_projection(
            &turn_context.model_info.input_modalities,
            workspace_identity.as_ref(),
        );
    let turn_input = strip_compaction_startup_envelopes(turn_input);
    let prompt = Prompt {
        input: turn_input.into(),
        base_instructions: base_instructions.clone(),
        ..Default::default()
    };
    if !reuse_previous_summary {
        turn_context.turn_timing_state.begin_compaction_generation();
        let mut retries = 0;
        loop {
            let attempt_result = drain_to_completed(
                &sess,
                turn_context.as_ref(),
                &mut client_session,
                &responses_metadata,
                &prompt,
            )
            .await;

            match attempt_result {
                Ok(()) => {
                    break;
                }
                Err(err @ (CodexErr::Interrupted | CodexErr::TurnAborted)) => {
                    return Err(err);
                }
                Err(e @ CodexErr::ContextWindowExceeded) => {
                    sess.set_total_tokens_full(turn_context.as_ref()).await;
                    sess.track_turn_codex_error(turn_context.as_ref(), &e);
                    let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                    sess.send_event(&turn_context, event).await;
                    return Err(e);
                }
                Err(e) if !e.is_retryable() => {
                    sess.track_turn_codex_error(turn_context.as_ref(), &e);
                    let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                    sess.send_event(&turn_context, event).await;
                    return Err(e);
                }
                Err(e) => {
                    if let Err(e) = handle_retryable_response_stream_error(
                        &mut retries,
                        max_retries,
                        e,
                        &mut client_session,
                        sess.as_ref(),
                        turn_context.as_ref(),
                        ResponsesStreamRequest::LocalCompaction,
                    )
                    .await
                    {
                        sess.track_turn_codex_error(turn_context.as_ref(), &e);
                        let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                        sess.send_event(&turn_context, event).await;
                        return Err(e);
                    }
                    turn_context.turn_timing_state.record_model_retry();
                }
            }
        }
    }

    let history_snapshot = if reuse_previous_summary {
        history
    } else {
        sess.clone_history().await
    };
    let history_items = history_snapshot.raw_items();
    let summary_suffix = if reuse_previous_summary {
        String::new()
    } else {
        get_last_assistant_message_from_turn(history_items).unwrap_or_default()
    };
    if !reuse_previous_summary {
        validate_generated_compaction_summary(previous_summary.as_deref(), &summary_suffix)?;
    }
    let summary_text = bounded_task_state_summary(previous_summary.as_deref(), &summary_suffix);
    // The summary and durable world state own consumed continuation state. Preserve only the
    // exact input tail that no model-generated item has consumed yet, in its original order.
    let mut summary_for_history = summary_text.clone();
    if omitted_images {
        summary_for_history.push_str("\n\n");
        summary_for_history.push_str(COMPACT_IMAGE_OMISSION_MARKER);
    }
    unresolved_history.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: summary_for_history,
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    let mut new_history = unresolved_history;
    if let Some(summary_item) = new_history.last_mut() {
        // This replacement history skips `record_conversation_items`; only the appended summary
        // belongs to this compaction turn.
        summary_item.set_turn_id_if_missing(&turn_context.sub_id);
    }
    let (window_number, window_ids) = sess.advance_auto_compact_window().await;

    let (initial_context, world_state_baseline) = build_compaction_initial_context(
        sess.as_ref(),
        turn_context.as_ref(),
        &initial_context_injection,
    )
    .await;
    if !initial_context.is_empty() {
        new_history = insert_compaction_initial_context(
            new_history,
            initial_context,
            &initial_context_injection,
        );
    }
    let reference_context_item = match &initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::AtStart(_) => Some(turn_context.to_turn_context_item()),
        #[cfg(test)]
        InitialContextInjection::BeforeLastUserMessage(_) => {
            Some(turn_context.to_turn_context_item())
        }
    };
    let compacted_item = CompactedItem {
        message: summary_text.clone(),
        // Persist the exact new eviction shape. Older records with `None` still use the legacy
        // reconstruction path that retained raw user messages.
        replacement_history: Some(new_history.clone()),
        window_number: Some(window_number),
        first_window_id: Some(window_ids.first_window_id.to_string()),
        previous_window_id: window_ids.previous_window_id.map(|id| id.to_string()),
        window_id: Some(window_ids.window_id.to_string()),
    };
    sess.replace_compacted_history(
        turn_context.as_ref(),
        new_history,
        reference_context_item,
        world_state_baseline,
        compacted_item,
    )
    .await;
    sess.recompute_token_usage(&turn_context).await;

    sess.emit_turn_item_completed(&turn_context, compaction_item)
        .await;
    let warning = EventMsg::Warning(WarningEvent {
        message: "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.".to_string(),
    });
    sess.send_event(&turn_context, warning).await;
    Ok(summary_text)
}

pub(crate) fn strip_compaction_startup_envelopes(items: Vec<ResponseItem>) -> Vec<ResponseItem> {
    items
        .into_iter()
        .filter_map(|item| match item {
            ResponseItem::Message {
                id,
                role,
                mut content,
                phase,
                internal_chat_message_metadata_passthrough,
            } if role == "user" => {
                content.retain(|part| !is_startup_contextual_user_fragment(part));
                (!content.is_empty()).then_some(ResponseItem::Message {
                    id,
                    role,
                    content,
                    phase,
                    internal_chat_message_metadata_passthrough,
                })
            }
            item => Some(item),
        })
        .collect()
}

fn bounded_task_state_summary(previous_summary: Option<&str>, summary_suffix: &str) -> String {
    let summary_suffix = summary_suffix.trim();
    match (previous_summary, summary_suffix.is_empty()) {
        (Some(previous_summary), true) => {
            truncate_compaction_summary(previous_summary, COMPACT_TASK_STATE_MAX_TOKENS)
        }
        (Some(previous_summary), false) if has_compaction_section(summary_suffix) => {
            truncate_compaction_summary(
                &format!("{previous_summary}\n\n{summary_suffix}"),
                COMPACT_TASK_STATE_MAX_TOKENS,
            )
        }
        (Some(previous_summary), false) => retain_unstructured_incremental_update(
            previous_summary,
            summary_suffix,
            COMPACT_TASK_STATE_MAX_TOKENS,
        ),
        (None, _) => truncate_compaction_summary(
            &format!("{SUMMARY_PREFIX}\n{summary_suffix}"),
            COMPACT_TASK_STATE_MAX_TOKENS,
        ),
    }
}

fn has_compaction_section(summary: &str) -> bool {
    COMPACTION_SECTIONS
        .iter()
        .any(|(heading, _)| summary.lines().any(|line| line.trim() == *heading))
}

fn validate_generated_compaction_summary(
    previous_summary: Option<&str>,
    summary_suffix: &str,
) -> CodexResult<()> {
    let summary_suffix = summary_suffix.trim();
    if summary_suffix.is_empty() {
        return Err(CodexErr::Fatal(
            "compaction completed without a checkpoint handoff".to_string(),
        ));
    }

    if previous_summary.is_some() && !has_nonempty_compaction_section(summary_suffix) {
        return Err(CodexErr::Fatal(
            "incremental compaction handoff did not contain a recognized non-empty checkpoint section"
                .to_string(),
        ));
    }

    let complete_summary = match previous_summary {
        Some(previous_summary) => format!("{previous_summary}\n\n{summary_suffix}"),
        None => summary_suffix.to_string(),
    };
    let missing = COMPACTION_SECTIONS
        .iter()
        .filter_map(|(heading, _)| {
            (!section_has_nonempty_body(&complete_summary, heading)).then_some(*heading)
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(CodexErr::Fatal(format!(
            "compaction handoff is incomplete; missing non-empty sections: {}",
            missing.join(", ")
        )))
    }
}

fn has_nonempty_compaction_section(summary: &str) -> bool {
    COMPACTION_SECTIONS
        .iter()
        .any(|(heading, _)| section_has_nonempty_body(summary, heading))
}

fn section_has_nonempty_body(summary: &str, target_heading: &str) -> bool {
    let mut in_target = false;
    for line in summary.lines() {
        let trimmed = line.trim();
        if COMPACTION_SECTIONS
            .iter()
            .any(|(heading, _)| trimmed == *heading)
        {
            if in_target && trimmed != target_heading {
                return false;
            }
            in_target = trimmed == target_heading;
            continue;
        }
        if in_target && !trimmed.is_empty() {
            return true;
        }
    }
    false
}

fn retain_unstructured_incremental_update(
    previous_summary: &str,
    summary_suffix: &str,
    max_tokens: usize,
) -> String {
    let update = truncate_text_to_token_ceiling(
        &format!("\n\n{summary_suffix}"),
        COMPACT_UNSTRUCTURED_UPDATE_MAX_TOKENS.min(max_tokens),
    );
    let previous_budget = max_tokens.saturating_sub(approx_token_count(&update));
    truncate_text_to_token_ceiling(
        &format!(
            "{}{}",
            truncate_text_to_token_ceiling(previous_summary, previous_budget),
            update
        ),
        max_tokens,
    )
}

fn truncate_compaction_summary(summary: &str, max_tokens: usize) -> String {
    if !has_compaction_section(summary) {
        return truncate_text_to_token_ceiling(summary, max_tokens);
    }

    let mut preamble = Vec::new();
    let mut sections = COMPACTION_SECTIONS
        .iter()
        .map(|(heading, budget)| (*heading, *budget, Vec::<Vec<String>>::new()))
        .collect::<Vec<_>>();
    let mut current = None;
    for line in summary.lines() {
        if let Some(index) = COMPACTION_SECTIONS
            .iter()
            .position(|(heading, _)| line.trim() == *heading)
        {
            current = Some(index);
            sections[index].2.push(Vec::new());
            continue;
        }
        match current {
            Some(index) => {
                if let Some(body) = sections[index].2.last_mut() {
                    body.push(line.to_string());
                }
            }
            None => preamble.push(line.to_string()),
        }
    }

    let mut rendered = vec![truncate_text_to_token_ceiling(&preamble.join("\n"), 300)];
    for (heading, budget, updates) in sections {
        if updates.is_empty() {
            continue;
        }
        let heading_tokens = approx_token_count(heading);
        let body = retain_newest_section_updates(&updates, budget.saturating_sub(heading_tokens));
        rendered.push(format!("{heading}\n{body}"));
    }
    truncate_text_to_token_ceiling(&rendered.join("\n\n"), max_tokens)
}

fn retain_newest_section_updates(updates: &[Vec<String>], max_tokens: usize) -> String {
    let mut retained = Vec::new();
    let mut remaining = max_tokens;
    for update in updates.iter().rev() {
        if remaining == 0 {
            break;
        }
        let update = update.join("\n").trim().to_string();
        if update.is_empty() {
            continue;
        }
        let separator_tokens = usize::from(!retained.is_empty()) * approx_token_count("\n\n");
        if remaining <= separator_tokens {
            break;
        }
        remaining = remaining.saturating_sub(separator_tokens);
        let tokens = approx_token_count(&update);
        if tokens <= remaining {
            remaining = remaining.saturating_sub(tokens);
            retained.push(update);
        } else {
            let truncated = truncate_text_to_token_ceiling(&update, remaining);
            if !truncated.is_empty() {
                retained.push(truncated);
            }
            break;
        }
    }
    retained.reverse();
    retained.join("\n\n")
}

pub(crate) struct CompactionAnalyticsAttempt {
    thread_id: String,
    turn_id: String,
    trigger: CompactionTrigger,
    reason: CompactionReason,
    implementation: CompactionImplementation,
    phase: CompactionPhase,
    active_context_tokens_before: i64,
    started_at: u64,
    start_instant: Instant,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct CompactionAnalyticsDetails {
    pub(crate) active_context_tokens_before: Option<i64>,
    pub(crate) retained_image_count: Option<usize>,
    pub(crate) compaction_summary_tokens: Option<i64>,
    pub(crate) cached_input_tokens: Option<i64>,
}

impl CompactionAnalyticsAttempt {
    pub(crate) async fn begin(
        sess: &Session,
        turn_context: &TurnContext,
        trigger: CompactionTrigger,
        reason: CompactionReason,
        implementation: CompactionImplementation,
        phase: CompactionPhase,
    ) -> Self {
        let active_context_tokens_before = sess.get_total_token_usage().await;
        Self {
            thread_id: sess.thread_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            trigger,
            reason,
            implementation,
            phase,
            active_context_tokens_before,
            started_at: now_unix_seconds(),
            start_instant: Instant::now(),
        }
    }

    pub(crate) async fn track(
        self,
        sess: &Session,
        status: CompactionStatus,
        codex_error: Option<&CodexErr>,
        details: CompactionAnalyticsDetails,
    ) {
        let CompactionAnalyticsDetails {
            active_context_tokens_before,
            retained_image_count,
            compaction_summary_tokens,
            cached_input_tokens,
        } = details;
        let active_context_tokens_before =
            active_context_tokens_before.unwrap_or(self.active_context_tokens_before);
        let active_context_tokens_after = sess.get_total_token_usage().await;
        sess.services
            .analytics_events_client
            .track_compaction(CodexCompactionEvent {
                thread_id: self.thread_id,
                turn_id: self.turn_id,
                trigger: self.trigger,
                reason: self.reason,
                implementation: self.implementation,
                phase: self.phase,
                strategy: CompactionStrategy::Memento,
                status,
                codex_error_kind: codex_error.map(Into::into),
                codex_error_http_status_code: codex_error
                    .and_then(CodexErr::http_status_code_value),
                active_context_tokens_before,
                active_context_tokens_after,
                retained_image_count,
                compaction_summary_tokens,
                cached_input_tokens,
                started_at: self.started_at,
                completed_at: now_unix_seconds(),
                duration_ms: Some(
                    u64::try_from(self.start_instant.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
            });
    }
}

pub(crate) fn compaction_status_from_result<T>(result: &CodexResult<T>) -> CompactionStatus {
    match result {
        Ok(_) => CompactionStatus::Completed,
        Err(CodexErr::Interrupted | CodexErr::TurnAborted) => CompactionStatus::Interrupted,
        Err(_) => CompactionStatus::Failed,
    }
}

pub fn content_items_to_text(content: &[ContentItem]) -> Option<String> {
    let mut pieces = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if !text.is_empty() {
                    pieces.push(text.as_str());
                }
            }
            ContentItem::InputImage { .. } => {}
        }
    }
    if pieces.is_empty() {
        None
    } else {
        Some(pieces.join("\n"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompactedUserMessage {
    content: Vec<UserInput>,
    internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
}

pub(crate) fn collect_user_messages(items: &[ResponseItem]) -> Vec<CompactedUserMessage> {
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message {
                role,
                content,
                internal_chat_message_metadata_passthrough,
                ..
            } if role == "user" => {
                let message = content_items_to_text(content).unwrap_or_default();
                if is_summary_message(&message)
                    || content.iter().any(is_legacy_compaction_warning_fragment)
                {
                    return None;
                }
                let content = crate::event_mapping::parse_user_message_content(content).content;
                Some(CompactedUserMessage {
                    content,
                    internal_chat_message_metadata_passthrough:
                        internal_chat_message_metadata_passthrough.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn collect_unresolved_user_messages(
    items: &[ResponseItem],
) -> Vec<CompactedUserMessage> {
    let unresolved = unresolved_compaction_items(items);
    collect_user_messages(&unresolved)
}

#[cfg(test)]
pub(crate) fn collect_unresolved_agent_messages(items: &[ResponseItem]) -> Vec<ResponseItem> {
    let unresolved = unresolved_compaction_items(items);
    append_bounded_agent_messages_with_indices(&unresolved, COMPACT_AGENT_MESSAGE_MAX_TOKENS).0
}

fn unresolved_compaction_items(items: &[ResponseItem]) -> Vec<ResponseItem> {
    let start = items
        .iter()
        .rposition(is_compaction_model_generated_item)
        .map_or(0, |index| index.saturating_add(1));
    strip_compaction_startup_envelopes(items[start..].to_vec())
}

fn append_bounded_agent_messages_with_indices(
    items: &[ResponseItem],
    max_tokens: usize,
) -> (Vec<ResponseItem>, Vec<usize>) {
    let mut selected = Vec::new();
    let mut selected_indices = Vec::new();
    let mut remaining = max_tokens;
    for (index, item) in items.iter().enumerate().rev() {
        let ResponseItem::AgentMessage {
            id,
            author,
            recipient,
            content,
            internal_chat_message_metadata_passthrough,
        } = item
        else {
            continue;
        };
        let mut bounded_content = Vec::new();
        for part in content {
            match part {
                AgentMessageInputContent::InputText { text }
                    if remaining > 0 && !text.is_empty() =>
                {
                    let tokens = approx_token_count(text);
                    let text = if tokens <= remaining {
                        remaining = remaining.saturating_sub(tokens);
                        text.clone()
                    } else {
                        let text = truncate_text_to_token_ceiling(text, remaining);
                        remaining = 0;
                        text
                    };
                    if !text.is_empty() {
                        bounded_content.push(AgentMessageInputContent::InputText { text });
                    }
                }
                AgentMessageInputContent::EncryptedContent { encrypted_content }
                    if remaining > 0 && !encrypted_content.is_empty() =>
                {
                    let tokens = approx_token_count(encrypted_content);
                    if tokens <= remaining {
                        remaining = remaining.saturating_sub(tokens);
                        bounded_content.push(AgentMessageInputContent::EncryptedContent {
                            encrypted_content: encrypted_content.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        if !bounded_content.is_empty() {
            selected_indices.push(index);
            selected.push(ResponseItem::AgentMessage {
                id: id.clone(),
                author: author.clone(),
                recipient: recipient.clone(),
                content: bounded_content,
                internal_chat_message_metadata_passthrough:
                    internal_chat_message_metadata_passthrough.clone(),
            });
        }
        if remaining == 0 {
            break;
        }
    }
    selected.reverse();
    selected_indices.reverse();
    (selected, selected_indices)
}

fn is_compaction_model_generated_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role == "assistant",
        ResponseItem::Reasoning { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::CompactionTrigger { .. }
        | ResponseItem::AdditionalTools { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Other => false,
    }
}

pub(crate) fn build_unresolved_user_history(items: &[ResponseItem]) -> (Vec<ResponseItem>, usize) {
    let (mut history, retained_image_count, omitted_images) =
        build_bounded_unresolved_input_history(items);
    if omitted_images {
        history.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: COMPACT_IMAGE_OMISSION_MARKER.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        });
    }
    (history, retained_image_count)
}

fn build_bounded_unresolved_input_history(
    items: &[ResponseItem],
) -> (Vec<ResponseItem>, usize, bool) {
    let unresolved = unresolved_compaction_items(items);
    let messages = collect_user_messages(&unresolved);
    let user_source_indices = unresolved
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (!collect_user_messages(std::slice::from_ref(item)).is_empty()).then_some(index)
        })
        .collect::<Vec<_>>();
    let (user_items, retained_image_count, omitted_images, selected_user_indices) =
        append_bounded_user_messages(
            Vec::new(),
            &messages,
            COMPACT_USER_MESSAGE_MAX_TOKENS,
            MAX_RETAINED_USER_IMAGES,
            MAX_RETAINED_USER_IMAGE_BYTES,
        );
    let (agent_items, agent_source_indices) =
        append_bounded_agent_messages_with_indices(&unresolved, COMPACT_AGENT_MESSAGE_MAX_TOKENS);

    let mut indexed_items = selected_user_indices
        .into_iter()
        .zip(user_items)
        .map(|(user_index, item)| (user_source_indices[user_index], item))
        .chain(agent_source_indices.into_iter().zip(agent_items))
        .collect::<Vec<_>>();
    indexed_items.sort_by_key(|(index, _)| *index);
    let history = indexed_items.into_iter().map(|(_, item)| item).collect();
    (history, retained_image_count, omitted_images)
}

pub(crate) fn is_summary_message(message: &str) -> bool {
    message.starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

fn latest_summary_message(items: &[ResponseItem]) -> Option<&str> {
    items.iter().rev().find_map(|item| match item {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            content.iter().find_map(|item| match item {
                ContentItem::InputText { text } if is_summary_message(text) => Some(text.as_str()),
                _ => None,
            })
        }
        _ => None,
    })
}

fn history_after_latest_summary_is_user_only(items: &[ResponseItem]) -> bool {
    let Some(summary_index) = items.iter().rposition(|item| match item {
        ResponseItem::Message { role, content, .. } if role == "user" => content.iter().any(
            |content| matches!(content, ContentItem::InputText { text } if is_summary_message(text)),
        ),
        _ => false,
    }) else {
        return false;
    };
    items[summary_index + 1..]
        .iter()
        .all(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
}

pub(crate) fn insert_compaction_initial_context(
    compacted_history: Vec<ResponseItem>,
    mut initial_context: Vec<ResponseItem>,
    initial_context_injection: &InitialContextInjection,
) -> Vec<ResponseItem> {
    match initial_context_injection {
        InitialContextInjection::AtStart(_) => {
            initial_context.extend(compacted_history);
            initial_context
        }
        #[cfg(test)]
        InitialContextInjection::BeforeLastUserMessage(_) => {
            insert_initial_context_before_last_real_user_or_summary(
                compacted_history,
                initial_context,
            )
        }
        InitialContextInjection::DoNotInject => compacted_history,
    }
}

/// Inserts canonical initial context into compacted replacement history at the
/// model-expected boundary.
///
/// Placement rules:
/// - Prefer immediately before the last real user message.
/// - If no real user messages remain, insert before the compaction summary so
///   the summary stays last.
/// - If there are no user messages, insert before the last compaction item so
///   that item remains last (remote compaction may return only compaction items).
/// - If there are no user messages or compaction items, append the context.
#[cfg(test)]
pub(crate) fn insert_initial_context_before_last_real_user_or_summary(
    mut compacted_history: Vec<ResponseItem>,
    initial_context: Vec<ResponseItem>,
) -> Vec<ResponseItem> {
    let mut last_user_or_summary_index = None;
    let mut last_real_user_index = None;
    for (i, item) in compacted_history.iter().enumerate().rev() {
        let Some(TurnItem::UserMessage(user)) = crate::event_mapping::parse_turn_item(item) else {
            continue;
        };
        // Compaction summaries are encoded as user messages, so track both:
        // the last real user message (preferred insertion point) and the last
        // user-message-like item (fallback summary insertion point).
        last_user_or_summary_index.get_or_insert(i);
        if !is_summary_message(&user.message()) {
            last_real_user_index = Some(i);
            break;
        }
    }
    let last_compaction_index = compacted_history
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, item)| {
            matches!(
                item,
                ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
            )
            .then_some(i)
        });
    let insertion_index = last_real_user_index
        .or(last_user_or_summary_index)
        .or(last_compaction_index);

    // Re-inject canonical context from the current session since we stripped it
    // from the pre-compaction history. Prefer placing it before the last real
    // user message; if there is no real user message left, place it before the
    // summary or compaction item so the compaction item remains last.
    if let Some(insertion_index) = insertion_index {
        compacted_history.splice(insertion_index..insertion_index, initial_context);
    } else {
        compacted_history.extend(initial_context);
    }

    compacted_history
}

pub(crate) fn build_compacted_history(
    initial_context: Vec<ResponseItem>,
    user_messages: &[CompactedUserMessage],
    summary_text: &str,
) -> Vec<ResponseItem> {
    build_compacted_history_with_limit(
        initial_context,
        user_messages,
        summary_text,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
    )
}

fn build_compacted_history_with_limit(
    history: Vec<ResponseItem>,
    user_messages: &[CompactedUserMessage],
    summary_text: &str,
    max_tokens: usize,
) -> Vec<ResponseItem> {
    build_compacted_history_with_limits(
        history,
        user_messages,
        summary_text,
        max_tokens,
        MAX_RETAINED_USER_IMAGES,
        MAX_RETAINED_USER_IMAGE_BYTES,
    )
}

fn build_compacted_history_with_limits(
    history: Vec<ResponseItem>,
    user_messages: &[CompactedUserMessage],
    summary_text: &str,
    max_tokens: usize,
    max_images: usize,
    max_image_bytes: usize,
) -> Vec<ResponseItem> {
    let (mut history, _retained_image_count, omitted_images, _selected_indices) =
        append_bounded_user_messages(
            history,
            user_messages,
            max_tokens,
            max_images,
            max_image_bytes,
        );

    let mut summary_text = if summary_text.is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.to_string()
    };
    if omitted_images {
        summary_text.push_str("\n\n");
        summary_text.push_str(COMPACT_IMAGE_OMISSION_MARKER);
    }

    history.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text: summary_text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });

    history
}

fn append_bounded_user_messages(
    mut history: Vec<ResponseItem>,
    user_messages: &[CompactedUserMessage],
    max_tokens: usize,
    max_images: usize,
    max_image_bytes: usize,
) -> (Vec<ResponseItem>, usize, bool, Vec<usize>) {
    let mut selected_messages: Vec<(usize, CompactedUserMessage)> = Vec::new();
    let mut remaining = max_tokens;
    let mut retained_image_count = 0usize;
    let mut retained_image_bytes = 0usize;
    let mut omitted_images = false;
    for (index, message) in user_messages.iter().enumerate().rev() {
        let mut content = Vec::new();
        for item in &message.content {
            match item {
                UserInput::Text { text, .. } if remaining > 0 && !text.is_empty() => {
                    let tokens = approx_token_count(text);
                    if tokens <= remaining {
                        content.push(UserInput::Text {
                            text: text.clone(),
                            text_elements: Vec::new(),
                        });
                        remaining = remaining.saturating_sub(tokens);
                    } else {
                        let truncated = truncate_text_to_token_ceiling(text, remaining);
                        if !truncated.is_empty() {
                            content.push(UserInput::Text {
                                text: truncated,
                                text_elements: Vec::new(),
                            });
                        }
                        remaining = 0;
                    }
                }
                UserInput::Image { image_url, detail } => {
                    let next_bytes = retained_image_bytes.saturating_add(image_url.len());
                    if retained_image_count < max_images && next_bytes <= max_image_bytes {
                        content.push(UserInput::Image {
                            image_url: image_url.clone(),
                            detail: *detail,
                        });
                        retained_image_count = retained_image_count.saturating_add(1);
                        retained_image_bytes = next_bytes;
                    } else {
                        omitted_images = true;
                    }
                }
                _ => {}
            }
        }
        if !content.is_empty() {
            selected_messages.push((
                index,
                CompactedUserMessage {
                    content,
                    internal_chat_message_metadata_passthrough: message
                        .internal_chat_message_metadata_passthrough
                        .clone(),
                },
            ));
        }
    }
    selected_messages.reverse();

    for (_, message) in &selected_messages {
        let content = message
            .content
            .iter()
            .filter_map(|item| match item {
                UserInput::Text { text, .. } => Some(ContentItem::InputText { text: text.clone() }),
                UserInput::Image { image_url, detail } => Some(ContentItem::InputImage {
                    image_url: image_url.clone(),
                    detail: *detail,
                }),
                _ => None,
            })
            .collect();
        history.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content,
            phase: None,
            internal_chat_message_metadata_passthrough: message
                .internal_chat_message_metadata_passthrough
                .clone(),
        });
    }

    let selected_indices = selected_messages
        .into_iter()
        .map(|(index, _)| index)
        .collect();
    (
        history,
        retained_image_count,
        omitted_images,
        selected_indices,
    )
}

async fn drain_to_completed(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    responses_metadata: &CodexResponsesMetadata,
    prompt: &Prompt,
) -> CodexResult<()> {
    let model_request_timing_guard = turn_context.turn_timing_state.begin_model_request_wait();
    let stream_result = client_session
        .stream(
            prompt,
            &turn_context.model_info,
            &turn_context.session_telemetry,
            crate::client::request_effort_for_model(
                &turn_context.model_info,
                turn_context.reasoning_effort.clone(),
            ),
            turn_context.reasoning_summary,
            turn_context.config.service_tier.clone(),
            responses_metadata,
            // Rollout tracing currently models remote compaction only; local compaction streams
            // are left untraced until the reducer has a first-class local compaction lifecycle.
            &InferenceTraceContext::disabled(),
        )
        .await;
    drop(model_request_timing_guard);
    let mut stream = stream_result?;
    loop {
        let model_stream_wait_timing_guard =
            turn_context.turn_timing_state.begin_model_stream_wait();
        let maybe_event = stream.next().await;
        drop(model_stream_wait_timing_guard);
        let Some(event) = maybe_event else {
            return Err(CodexErr::Stream(
                "stream closed before response.completed".into(),
                None,
            ));
        };
        let _model_stream_processing_timing_guard = turn_context
            .turn_timing_state
            .begin_model_stream_processing();
        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                sess.record_conversation_items(turn_context, std::slice::from_ref(&item))
                    .await;
            }
            Ok(ResponseEvent::ServerReasoningIncluded(included)) => {
                sess.set_server_reasoning_included(included).await;
            }
            Ok(ResponseEvent::RateLimits(snapshot)) => {
                sess.update_rate_limits(turn_context, snapshot).await;
            }
            Ok(ResponseEvent::Completed { token_usage, .. }) => {
                sess.update_token_usage_info(turn_context, token_usage.as_ref())
                    .await?;
                return Ok(());
            }
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
#[path = "compact_tests.rs"]
mod tests;
