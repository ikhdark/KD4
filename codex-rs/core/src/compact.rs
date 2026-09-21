use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use crate::Prompt;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::context::is_legacy_compaction_warning_fragment;
use crate::context::is_startup_contextual_user_fragment;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::hook_runtime::run_post_compact_hook_gate;
use crate::hook_runtime::run_pre_compact_hook_gate;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_retryable_response_stream_error;
#[cfg(test)]
use crate::session::PreviousTurnSettings;
use crate::session::session::Session;
use crate::session::turn::get_last_assistant_message_from_turn;
use crate::session::turn_context::TurnContext;
use crate::stable_context::StableContextTarget;
use crate::stable_context::project_stable_context;
use crate::tools::command_output_artifact::CanonicalOutputArtifact;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use codex_analytics::CodexCompactionEvent;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionStatus;
use codex_analytics::CompactionStrategy;
use codex_analytics::CompactionTrigger;
use codex_analytics::now_unix_seconds;
use codex_protocol::ResponseItemId;
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
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::CanonicalToolResult;
use codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use codex_utils_string::TokenCountEstimate;
use futures::prelude::*;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use codex_model_provider_info::ModelProviderInfo;

pub use codex_prompts::COMPACTION_BASE_INSTRUCTIONS;
pub use codex_prompts::INCREMENTAL_SUMMARIZATION_PROMPT;
pub use codex_prompts::SUMMARIZATION_PROMPT;
pub use codex_prompts::SUMMARY_PREFIX;
const COMPACTION_SUMMARY_ITEM_ID_PREFIX: &str = "msg_compaction_summary_";
const COMPACTION_SUMMARY_ITEM_ID_BASE: &str = "msg_compaction_summary";
const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 16_000;
const COMPACT_AGENT_MESSAGE_MAX_TOKENS: usize = 8_000;
const COMPACT_TASK_STATE_MAX_TOKENS: usize = 2_400;
const COMPACT_UNSTRUCTURED_UPDATE_MAX_TOKENS: usize = 1_000;
// Keep introductory prose subordinate to the structured task-state sections.
const COMPACT_PREAMBLE_MAX_TOKENS: usize = 300;
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
    (EVIDENCE_HEADING, 500),
    (NEXT_ACTION_HEADING, 250),
];
pub(crate) const MAX_RETAINED_USER_IMAGES: usize = 8;
pub(crate) const MAX_RETAINED_USER_IMAGE_BYTES: usize =
    MAX_PROMPT_IMAGE_SOURCE_BYTES / 3 * 4 + 4096;
pub(crate) const COMPACT_IMAGE_OMISSION_MARKER: &str =
    "[codex-local-compaction omitted user images: limits exceeded]";
const COMPACT_TEXT_OMISSION_MARKER: &str = "codex_local_compaction_text_omission";

#[derive(Clone, Debug, Serialize)]
struct CompactionTextOmissionReceiptV1 {
    version: u8,
    kind: &'static str,
    role: &'static str,
    source_item_id: Option<String>,
    source_index: usize,
    turn_id: Option<String>,
    original_tokens: usize,
    retained_tokens: usize,
    omitted_tokens: usize,
    unresolved: bool,
}

/// Controls whether compaction replacement history must include initial context.
///
/// Pre-turn/manual compaction variants use `AtStart` so the next request retains the same
/// cacheable initial-context prefix instead of appending a fresh copy after the summary.
///
/// The test-only `BeforeLastUserMessage` variant preserves coverage for legacy replacement-history
/// ordering. `AtStart` keeps the summary or compaction item last while preserving the stable prompt
/// prefix in production.
///
/// `DoNotInject` is likewise test-only: every production compaction path now restores initial
/// context, capturing a world-state snapshot when the caller did not already have one. It is
/// retained so tests can exercise compaction without building that snapshot.
#[derive(Clone, Debug)]
pub(crate) enum InitialContextInjection {
    AtStart(Arc<WorldState>),
    #[cfg(test)]
    BeforeLastUserMessage(Arc<WorldState>),
    #[cfg(test)]
    DoNotInject,
}

pub(crate) async fn build_compaction_initial_context(
    sess: &Session,
    turn_context: &TurnContext,
    initial_context_injection: &InitialContextInjection,
) -> (
    Vec<ResponseItem>,
    Option<WorldStateSnapshot>,
    Vec<codex_protocol::protocol::ContextFragmentDigest>,
) {
    // Return the rendered state with its items so history and its baseline stay identical.
    match initial_context_injection {
        InitialContextInjection::AtStart(world_state) => {
            let (items, delivered_snapshot, fragment_digests) = sess
                .build_initial_context_with_world_state_and_provenance(
                    turn_context,
                    world_state.as_ref(),
                )
                .await;
            (items, Some(delivered_snapshot), fragment_digests)
        }
        #[cfg(test)]
        InitialContextInjection::BeforeLastUserMessage(world_state) => {
            let (items, delivered_snapshot, fragment_digests) = sess
                .build_initial_context_with_world_state_and_provenance(
                    turn_context,
                    world_state.as_ref(),
                )
                .await;
            (items, Some(delivered_snapshot), fragment_digests)
        }
        #[cfg(test)]
        InitialContextInjection::DoNotInject => (Vec::new(), None, Vec::new()),
    }
}

pub(crate) fn should_use_remote_compact_task(
    provider: &ModelProviderInfo,
    compact_prompt: Option<&str>,
) -> bool {
    provider.supports_remote_compaction() && compact_prompt.is_none()
}

fn incremental_summarization_prompt(compact_prompt: Option<&str>) -> &str {
    compact_prompt.unwrap_or(INCREMENTAL_SUMMARIZATION_PROMPT)
}

enum CompactionClientSession<'a, T> {
    Reused(&'a mut T),
    Owned(T),
}

impl<T> AsMut<T> for CompactionClientSession<'_, T> {
    fn as_mut(&mut self) -> &mut T {
        match self {
            Self::Reused(value) => value,
            Self::Owned(value) => value,
        }
    }
}

fn reuse_or_create_compaction_client_session<'a, T>(
    reusable: Option<&'a mut T>,
    create: impl FnOnce() -> T,
) -> CompactionClientSession<'a, T> {
    match reusable {
        Some(value) => CompactionClientSession::Reused(value),
        None => CompactionClientSession::Owned(create()),
    }
}

pub(crate) struct InlineAutoCompactReuse<'a> {
    pub(crate) client_session: &'a mut ModelClientSession,
    pub(crate) prefetched_workspace_identity:
        Option<&'a Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "Preserve explicit compaction context, publication options, and cancellation"
)]
pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    reuse: InlineAutoCompactReuse<'_>,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let InlineAutoCompactReuse {
        client_session,
        prefetched_workspace_identity,
    } = reuse;
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
        Some(client_session),
        prefetched_workspace_identity,
        input,
        initial_context_injection,
        CompactionTrigger::Auto,
        reason,
        phase,
        emit_error_event,
        cancellation_token,
    )
    .await?;
    Ok(())
}

pub(crate) async fn run_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await?;
    let world_state = Arc::new(sess.build_world_state_for_step(step_context.as_ref()).await);
    run_compact_task_inner(
        sess.clone(),
        turn_context,
        // A standalone compaction turn has no in-flight sampling session to inherit.
        /*client_session*/
        None,
        /*prefetched_workspace_identity*/ None,
        input,
        InitialContextInjection::AtStart(world_state),
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionPhase::StandaloneTurn,
        /*emit_error_event*/ true,
        cancellation_token,
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_compact_task_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    client_session: Option<&mut ModelClientSession>,
    prefetched_workspace_identity: Option<&Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
    trigger: CompactionTrigger,
    reason: CompactionReason,
    phase: CompactionPhase,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
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
    if run_pre_compact_hook_gate(&sess, &turn_context, trigger).await {
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
    let mut analytics_details = CompactionAnalyticsDetails::default();
    let result = run_compact_task_inner_impl(
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        client_session,
        prefetched_workspace_identity,
        input,
        initial_context_injection,
        compaction_metadata,
        &mut analytics_details,
        emit_error_event,
        cancellation_token,
    )
    .await;
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if let Ok(summary) = &result
        && run_post_compact_hook_gate(&sess, &turn_context, trigger, Some(summary)).await
    {
        attempt
            .track(sess.as_ref(), status, codex_error, analytics_details)
            .await;
        return Err(CodexErr::TurnAborted);
    }
    attempt
        .track(sess.as_ref(), status, codex_error, analytics_details)
        .await;
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn run_compact_task_inner_impl(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    client_session: Option<&mut ModelClientSession>,
    prefetched_workspace_identity: Option<&Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    mut input: Vec<UserInput>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<String> {
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(&turn_context, &compaction_item)
        .await;
    let mut history = sess.clone_history().await;
    let (
        mut unresolved_history,
        retained_image_count,
        omitted_image_count,
        omitted_user_text,
        omitted_text,
    ) = build_bounded_unresolved_input_history(history.raw_items());
    analytics_details.retained_image_count = Some(retained_image_count);
    let text_recovery_sidecar =
        match persist_compaction_text_recovery(sess.as_ref(), history.raw_items(), omitted_text)
            .await
        {
            Ok(sidecar) => sidecar,
            Err(error) => {
                sess.track_turn_codex_error(turn_context.as_ref(), &error);
                if emit_error_event {
                    sess.send_event(&turn_context, EventMsg::Error(error.to_error_event(None)))
                        .await;
                }
                return Err(error);
            }
        };
    let previous_summary = latest_summary_message(history.raw_items()).map(str::to_string);
    let reuse_previous_summary = previous_summary.is_some()
        && can_reuse_previous_summary(history.raw_items(), omitted_user_text);
    if previous_summary.is_some() && !reuse_previous_summary {
        input.push(UserInput::Text {
            text: incremental_summarization_prompt(turn_context.config.compact_prompt.as_deref())
                .to_string(),
            text_elements: Vec::new(),
        });
    }
    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);
    history.record_items(
        &[initial_input_for_turn.into()],
        turn_context.model_info.truncation_policy.into(),
    );

    let workspace_identity = workspace_identity_for_compaction(
        sess.as_ref(),
        turn_context.as_ref(),
        prefetched_workspace_identity,
    )
    .await;
    let tool_history = history.tool_history_state();
    let turn_input = history.for_compaction_prompt_with_completed_tool_projection(
        &turn_context.model_info.input_modalities,
        workspace_identity.as_ref(),
    );
    let turn_input = strip_compaction_startup_envelopes(turn_input);
    let artifact_pin_payload = tool_history.artifact_pin_payload_for_items(&turn_input);
    let summary_text_result = if reuse_previous_summary {
        validated_compaction_summary(previous_summary.as_deref(), "", false)
    } else {
        let base_instructions = BaseInstructions {
            text: COMPACTION_BASE_INSTRUCTIONS.trim().to_string(),
        };
        let max_retries = turn_context.provider.info().stream_max_retries();
        // Reuse the turn's live session when compaction runs inside a turn so the warm transport,
        // sticky routing, and websocket incremental-request state carry over instead of forcing a
        // second handshake while the turn's own connection sits idle. A standalone compaction turn
        // has no such session and opens its own, which then publishes normally on drop.
        //
        // The caller reaches this point only after its response stream has closed: `run_turn`
        // compacts from the post-sampling branch, and pre-sampling compaction runs before any
        // stream is opened. Retries inside this compact turn keep sharing the one session.
        let mut client_session = reuse_or_create_compaction_client_session(client_session, || {
            sess.services.model_client.new_session()
        });
        let client_session = client_session.as_mut();
        let window_id = sess.current_window_id().await;
        let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
            sess.installation_id.clone(),
            window_id,
            CodexResponsesRequestKind::Compaction(compaction_metadata),
        );

        let mut prompt = Prompt {
            input: turn_input.into(),
            base_instructions,
            ..Default::default()
        };
        turn_context.turn_timing_state.begin_compaction_generation();
        let mut retry_state = ResponsesStreamRetryState::default();
        let mut retried_invalid_summary = false;
        loop {
            let attempt_result = drain_to_completed(
                &sess,
                turn_context.as_ref(),
                &mut *client_session,
                &responses_metadata,
                &prompt,
                cancellation_token,
            )
            .await;

            match attempt_result {
                Ok(output) => {
                    sess.update_token_usage_info(
                        turn_context.as_ref(),
                        output.token_usage.as_ref(),
                    )
                    .await?;
                    let summary_suffix =
                        get_last_assistant_message_from_turn(&output.items).unwrap_or_default();
                    match validated_compaction_summary(
                        previous_summary.as_deref(),
                        &summary_suffix,
                        true,
                    ) {
                        Ok(summary_text) => {
                            // Model output is tentative until its checkpoint is semantically valid.
                            sess.record_conversation_items(turn_context.as_ref(), &output.items)
                                .await?;
                            break Ok(summary_text);
                        }
                        Err(error) if !retried_invalid_summary => {
                            // Keep rejected output out of durable and live history. Give the
                            // model one corrective attempt, without replaying an unbounded
                            // malformed answer into the already-full compaction request.
                            retried_invalid_summary = true;
                            let correction = ResponseItem::from(ResponseInputItem::from(vec![
                                UserInput::Text {
                                    text: format!(
                                        "The previous compaction handoff was rejected: {error}. Regenerate the handoff with every required checkpoint section and a non-empty body for each section."
                                    ),
                                    text_elements: Vec::new(),
                                },
                            ]));
                            let mut corrected_input = prompt.input.to_vec();
                            corrected_input.push(correction);
                            prompt.input = corrected_input.into();
                            turn_context.turn_timing_state.record_model_retry();
                        }
                        Err(error) => break Err(error),
                    }
                }
                Err(err @ (CodexErr::Interrupted | CodexErr::TurnAborted)) => {
                    return Err(err);
                }
                Err(e @ CodexErr::ContextWindowExceeded) => {
                    sess.set_total_tokens_full(turn_context.as_ref()).await;
                    sess.track_turn_codex_error(turn_context.as_ref(), &e);
                    if emit_error_event {
                        let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                        sess.send_event(&turn_context, event).await;
                    }
                    return Err(e);
                }
                Err(e) if !e.is_retryable() => {
                    sess.track_turn_codex_error(turn_context.as_ref(), &e);
                    if emit_error_event {
                        let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                        sess.send_event(&turn_context, event).await;
                    }
                    return Err(e);
                }
                Err(e) => {
                    if let Err(e) = handle_retryable_response_stream_error(
                        &mut retry_state,
                        max_retries,
                        e,
                        &mut *client_session,
                        sess.as_ref(),
                        turn_context.as_ref(),
                        ResponsesStreamRequest::LocalCompaction,
                        cancellation_token,
                    )
                    .await
                    {
                        sess.track_turn_codex_error(turn_context.as_ref(), &e);
                        if emit_error_event {
                            let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                            sess.send_event(&turn_context, event).await;
                        }
                        return Err(e);
                    }
                    turn_context.turn_timing_state.record_model_retry();
                }
            }
        }
    };
    let summary_text = match summary_text_result {
        Ok(summary_text) => summary_text,
        Err(error) => {
            sess.track_turn_codex_error(turn_context.as_ref(), &error);
            if emit_error_event {
                let event = EventMsg::Error(error.to_error_event(/*message_prefix*/ None));
                sess.send_event(&turn_context, event).await;
            }
            return Err(error);
        }
    };
    // The summary and durable world state own consumed continuation state. Preserve only the
    // exact input tail that no model-generated item has consumed yet, in its original order.
    let mut summary_for_history = summary_text.clone();
    if omitted_image_count > 0 {
        summary_for_history.push_str("\n\n");
        summary_for_history.push_str(&compaction_image_omission_marker(omitted_image_count));
    }
    let mut summary_item =
        compaction_summary_item_with_artifact_pins(summary_for_history, artifact_pin_payload);
    if let Some(text) = text_recovery_sidecar
        && let ResponseItem::Message { content, .. } = &mut summary_item
    {
        content.push(ContentItem::InputText { text });
    }
    unresolved_history.push(summary_item);
    let mut new_history = unresolved_history;
    if let Some(summary_item) = new_history.last_mut() {
        // This replacement history skips `record_conversation_items`; only the appended summary
        // belongs to this compaction turn.
        summary_item.set_turn_id_if_missing(&turn_context.sub_id);
    }
    let (initial_context, world_state_baseline, fragment_digests) =
        build_compaction_initial_context(
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
        #[cfg(test)]
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::AtStart(_) => {
            Some(turn_context.to_turn_context_item_async().await)
        }
        #[cfg(test)]
        InitialContextInjection::BeforeLastUserMessage(_) => {
            Some(turn_context.to_turn_context_item_async().await)
        }
    };
    let compacted_item = CompactedItem {
        message: summary_text.clone(),
        // Persist the exact new eviction shape. Older records with `None` still use the legacy
        // reconstruction path that retained raw user messages.
        replacement_history: Some(new_history.clone()),
        // The ordered history commit assigns and publishes the next window.
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    };
    sess.replace_compacted_history(
        &turn_context,
        new_history,
        reference_context_item,
        world_state_baseline,
        fragment_digests,
        compacted_item,
    )
    .await?;
    sess.recompute_token_usage(&turn_context).await;

    sess.emit_turn_item_completed(&turn_context, compaction_item)
        .await;
    let warning = EventMsg::Warning(WarningEvent {
        message: "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.".to_string(),
    });
    sess.send_event(&turn_context, warning).await;
    Ok(summary_text)
}

async fn workspace_identity_for_compaction(
    sess: &Session,
    turn_context: &TurnContext,
    prefetched_workspace_identity: Option<&Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
) -> Option<crate::git_workspace::WorkspaceEvidenceIdentity> {
    match prefetched_workspace_identity {
        // `Some(None)` is an authoritative capture of a non-Git workspace and must suppress a
        // second repository probe just as an ordinary identity does.
        Some(workspace_identity) => workspace_identity.clone(),
        None => {
            sess.services
                .git_workspace
                .workspace_evidence_for_turn(turn_context, turn_context.config.cwd.as_path())
                .await
                .identity
        }
    }
}

pub(crate) fn strip_compaction_startup_envelopes(
    items: impl Into<Arc<[ResponseItem]>>,
) -> Vec<ResponseItem> {
    project_stable_context(items.into(), StableContextTarget::Sampling)
        .items
        .iter()
        .cloned()
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
    summary.lines().any(|line| {
        let line = line.trim();
        COMPACTION_SECTIONS
            .iter()
            .any(|(heading, _)| line == *heading)
    })
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

    // Older/custom compaction models may return a non-empty free-form handoff. Preserve that
    // backward-compatible path; once a model opts into the structured checkpoint format, enforce
    // its completeness so a partially emitted section set cannot silently discard task state.
    if !has_compaction_section(summary_suffix) {
        return Ok(());
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
    let populated_sections = compaction_section_bodies(&complete_summary);
    let missing = COMPACTION_SECTIONS
        .iter()
        .zip(populated_sections)
        .filter_map(|((heading, _), populated)| (!populated).then_some(*heading))
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

fn validated_compaction_summary(
    previous_summary: Option<&str>,
    summary_suffix: &str,
    validate_suffix: bool,
) -> CodexResult<String> {
    if validate_suffix {
        validate_generated_compaction_summary(previous_summary, summary_suffix)?;
    }
    let summary_text = bounded_task_state_summary(previous_summary, summary_suffix);
    if has_compaction_section(&summary_text) {
        validate_generated_compaction_summary(None, &summary_text)?;
    }
    Ok(summary_text)
}

fn has_nonempty_compaction_section(summary: &str) -> bool {
    compaction_section_bodies(summary)
        .into_iter()
        .any(|populated| populated)
}

fn compaction_section_bodies(summary: &str) -> [bool; COMPACTION_SECTIONS.len()] {
    let mut populated = [false; COMPACTION_SECTIONS.len()];
    let mut current = None;
    for line in summary.lines() {
        let trimmed = line.trim();
        if let Some(index) = COMPACTION_SECTIONS
            .iter()
            .position(|(heading, _)| trimmed == *heading)
        {
            current = Some(index);
        } else if !trimmed.is_empty()
            && let Some(index) = current
        {
            // Incremental checkpoints may repeat a heading. An earlier empty
            // occurrence must not hide a later body for the same section.
            populated[index] = true;
        }
    }
    populated
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
        .map(|(heading, budget)| (*heading, *budget, Vec::<Vec<&str>>::new()))
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
                    body.push(line);
                }
            }
            None => preamble.push(line),
        }
    }

    let sections = sections
        .into_iter()
        .filter(|(_, _, updates)| !updates.is_empty())
        .map(|(heading, budget, updates)| {
            let updates = updates
                .into_iter()
                .map(|lines| {
                    let body = lines.join("\n");
                    let tokens = approx_token_count(body.trim());
                    (body, tokens)
                })
                .collect::<Vec<_>>();
            (heading, budget, updates)
        })
        .collect::<Vec<_>>();
    let mut bodies = vec!["[truncated]".to_string(); sections.len()];
    let mut bounded_preamble = String::new();
    let headings = sections
        .iter()
        .map(|(heading, _, _)| TokenCountEstimate::new(heading))
        .collect::<Vec<_>>();
    let mut body_costs = bodies
        .iter()
        .map(|body| TokenCountEstimate::new(body))
        .collect::<Vec<_>>();
    let summary_cost = |preamble: Option<TokenCountEstimate>, costs: &[TokenCountEstimate]| {
        let mut parts = headings
            .iter()
            .zip(costs)
            .map(|(heading, body)| heading.then(*body, 1));
        let first = preamble.unwrap_or_else(|| parts.next().unwrap_or_default());
        parts
            .fold(first, |total, part| total.then(part, 2))
            .tokens()
    };
    if summary_cost(None, &body_costs) > max_tokens {
        // A syntactically complete handoff has an irreducible minimum. Callers use a
        // substantially larger bound, but preserve the typed structure if a future caller
        // supplies an impossible limit instead of cutting a heading or body mid-field.
        return render_structured_compaction(&bounded_preamble, &sections, &bodies);
    }

    let full_preamble = preamble.join("\n");
    let mut low = 0usize;
    let mut high = COMPACT_PREAMBLE_MAX_TOKENS.min(max_tokens);
    while low < high {
        let candidate_budget = low.saturating_add(high).saturating_add(1) / 2;
        let candidate = truncate_text_to_token_ceiling(&full_preamble, candidate_budget);
        let cost = (!candidate.trim().is_empty()).then(|| TokenCountEstimate::new(&candidate));
        if summary_cost(cost, &body_costs) <= max_tokens {
            low = candidate_budget;
            bounded_preamble = candidate;
        } else {
            high = candidate_budget.saturating_sub(1);
        }
    }

    let preamble_cost =
        (!bounded_preamble.trim().is_empty()).then(|| TokenCountEstimate::new(&bounded_preamble));
    for (index, (heading, configured_budget, updates)) in sections.iter().enumerate() {
        let mut low = 0usize;
        let mut high = (*configured_budget).min(max_tokens);
        let mut selected_cost = body_costs[index];
        while low < high {
            let candidate_budget = low.saturating_add(high).saturating_add(1) / 2;
            let candidate = if *heading == GOAL_HEADING {
                retain_goal_boundary_updates(updates, candidate_budget)
            } else {
                retain_newest_section_updates(updates, candidate_budget)
            };
            let candidate = if candidate.trim().is_empty() {
                "[truncated]".to_string()
            } else {
                candidate
            };
            body_costs[index] = TokenCountEstimate::new(&candidate);
            if summary_cost(preamble_cost, &body_costs) <= max_tokens {
                low = candidate_budget;
                bodies[index] = candidate;
                selected_cost = body_costs[index];
            } else {
                high = candidate_budget.saturating_sub(1);
            }
        }
        body_costs[index] = selected_cost;
    }

    render_structured_compaction(&bounded_preamble, &sections, &bodies)
}

type CompactionSection<'a> = (&'a str, usize, Vec<(String, usize)>);

fn render_structured_compaction(
    preamble: &str,
    sections: &[CompactionSection<'_>],
    bodies: &[String],
) -> String {
    let mut rendered = String::new();
    if !preamble.trim().is_empty() {
        rendered.push_str(preamble);
    }
    for ((heading, _, _), body) in sections.iter().zip(bodies) {
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(heading);
        rendered.push('\n');
        rendered.push_str(body);
    }
    rendered
}

fn retain_newest_section_updates(updates: &[(String, usize)], max_tokens: usize) -> String {
    let mut retained = Vec::new();
    let mut remaining = max_tokens;
    let separator_tokens = approx_token_count("\n\n");
    for (update, tokens) in updates.iter().rev() {
        if remaining == 0 {
            break;
        }
        let update = update.trim();
        if update.is_empty() {
            continue;
        }
        let separator_tokens = usize::from(!retained.is_empty()) * separator_tokens;
        if remaining <= separator_tokens {
            break;
        }
        remaining = remaining.saturating_sub(separator_tokens);
        if *tokens <= remaining {
            remaining = remaining.saturating_sub(*tokens);
            retained.push(std::borrow::Cow::Borrowed(update));
        } else {
            let truncated = truncate_text_to_token_ceiling(update, remaining);
            if !truncated.is_empty() {
                retained.push(std::borrow::Cow::Owned(truncated));
            }
            break;
        }
    }
    retained.reverse();
    retained.join("\n\n")
}

/// Keeps the original goal/constraints and the latest goal revision in agreement.
/// Intermediate status belongs in the other checkpoint sections and may be evicted.
fn retain_goal_boundary_updates(updates: &[(String, usize)], max_tokens: usize) -> String {
    let mut nonempty = updates
        .iter()
        .map(|(body, _)| body.trim())
        .filter(|body| !body.is_empty());
    let Some(oldest) = nonempty.next() else {
        return String::new();
    };
    let Some(newest) = nonempty.next_back() else {
        return truncate_text_to_token_ceiling(oldest, max_tokens);
    };

    let separator = "\n\n";
    let separator_tokens = approx_token_count(separator);
    if max_tokens <= separator_tokens {
        return truncate_text_to_token_ceiling(newest, max_tokens);
    }
    let payload_budget = max_tokens.saturating_sub(separator_tokens);
    let newest = truncate_text_to_token_ceiling(newest, payload_budget.div_ceil(2));
    let oldest_budget = payload_budget.saturating_sub(approx_token_count(&newest));
    let oldest = truncate_text_to_token_ceiling(oldest, oldest_budget);
    if oldest.is_empty() {
        return truncate_text_to_token_ceiling(newest.as_str(), max_tokens);
    }
    if newest.is_empty() {
        return truncate_text_to_token_ceiling(oldest.as_str(), max_tokens);
    }
    format!("{oldest}{separator}{newest}")
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
    source_item_id: Option<String>,
    content: Vec<UserInput>,
    internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
}

pub(crate) fn collect_user_messages(items: &[ResponseItem]) -> Vec<CompactedUserMessage> {
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message {
                id,
                role,
                content,
                internal_chat_message_metadata_passthrough,
                ..
            } if role == "user" => {
                if is_compaction_summary_item(item)
                    || (item.turn_id().is_none()
                        && content.iter().any(is_legacy_compaction_warning_fragment)
                        && crate::event_mapping::is_contextual_user_message_content(content))
                {
                    return None;
                }
                let content = crate::event_mapping::parse_user_message_content(content).content;
                Some(CompactedUserMessage {
                    source_item_id: id.as_ref().map(ToString::to_string),
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
    let base_start = items
        .iter()
        .rposition(is_compaction_model_generated_item)
        .map_or(0, |index| index.saturating_add(1));
    let pending_output_ids = items[base_start..]
        .iter()
        .filter_map(compaction_output_call_id)
        .collect::<BTreeSet<_>>();
    let start = items[..base_start]
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            compaction_call_id(item)
                .filter(|call_id| pending_output_ids.contains(call_id))
                .map(|_| index)
        })
        .min()
        .unwrap_or(base_start);
    strip_compaction_startup_envelopes(items[start..].to_vec())
}

fn compaction_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id),
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => Some(call_id),
        _ => None,
    }
}

fn compaction_output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id),
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some(call_id),
        _ => None,
    }
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

fn compaction_image_omission_marker(count: usize) -> String {
    format!("{COMPACT_IMAGE_OMISSION_MARKER} Omitted image count: {count}.")
}

#[cfg(test)]
pub(crate) fn build_unresolved_user_history(items: &[ResponseItem]) -> (Vec<ResponseItem>, usize) {
    let (history, retained_images, _) = build_unresolved_input_checkpoint(items);
    (history, retained_images)
}

pub(crate) fn build_unresolved_input_checkpoint(
    items: &[ResponseItem],
) -> (Vec<ResponseItem>, usize, bool) {
    let (mut history, retained_image_count, omitted_image_count, omitted_text, _) =
        build_bounded_unresolved_input_history(items);
    if omitted_image_count > 0 {
        history.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: compaction_image_omission_marker(omitted_image_count),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        });
    }
    (history, retained_image_count, omitted_text)
}

fn build_bounded_unresolved_input_history(
    items: &[ResponseItem],
) -> (Vec<ResponseItem>, usize, usize, bool, bool) {
    let unresolved = unresolved_compaction_items(items);
    let (user_source_indices, messages): (Vec<_>, Vec<_>) = unresolved
        .iter()
        .enumerate()
        .flat_map(|(index, item)| {
            collect_user_messages(std::slice::from_ref(item))
                .into_iter()
                .map(move |message| (index, message))
        })
        .unzip();
    let (user_items, retained_image_count, omitted_image_count, selected_user_indices) =
        append_bounded_user_messages(
            Vec::new(),
            &messages,
            COMPACT_USER_MESSAGE_MAX_TOKENS,
            MAX_RETAINED_USER_IMAGES,
            MAX_RETAINED_USER_IMAGE_BYTES,
        );
    let (agent_items, agent_source_indices) =
        append_bounded_agent_messages_with_indices(&unresolved, COMPACT_AGENT_MESSAGE_MAX_TOKENS);
    let selected_agent_items = agent_source_indices
        .iter()
        .copied()
        .zip(agent_items.iter().cloned())
        .collect::<Vec<_>>();

    let selected_user_source_indices = selected_user_indices
        .iter()
        .map(|user_index| user_source_indices[*user_index])
        .collect::<Vec<_>>();
    let mut indexed_items = selected_user_source_indices
        .iter()
        .copied()
        .zip(user_items)
        .chain(selected_agent_items.iter().cloned())
        .collect::<Vec<_>>();

    let retained_tokens_by_index = indexed_items
        .iter()
        .map(|(index, item)| {
            (
                *index,
                response_item_text_tokens(item).saturating_add(agent_message_text_tokens(item)),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut omission_receipts = Vec::new();
    let mut omitted_user_text = false;
    for (user_index, message) in messages.iter().enumerate() {
        let source_index = user_source_indices[user_index];
        let original_tokens = compacted_user_message_text_tokens(message);
        let retained_tokens = retained_tokens_by_index
            .get(&source_index)
            .copied()
            .unwrap_or(0);
        if retained_tokens < original_tokens {
            omitted_user_text = true;
            omission_receipts.push((
                source_index,
                compaction_text_omission_receipt(
                    "user",
                    message.source_item_id.clone(),
                    source_index,
                    message
                        .internal_chat_message_metadata_passthrough
                        .as_ref()
                        .and_then(|metadata| metadata.turn_id.clone()),
                    original_tokens,
                    retained_tokens,
                ),
            ));
        }
    }

    for (source_index, item) in unresolved.iter().enumerate() {
        let ResponseItem::AgentMessage {
            id,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            continue;
        };
        let original_tokens = agent_message_text_tokens(item);
        let retained_tokens = retained_tokens_by_index
            .get(&source_index)
            .copied()
            .unwrap_or(0);
        if retained_tokens < original_tokens {
            omission_receipts.push((
                source_index,
                compaction_text_omission_receipt(
                    "agent",
                    id.as_ref().map(ToString::to_string),
                    source_index,
                    internal_chat_message_metadata_passthrough
                        .as_ref()
                        .and_then(|metadata| metadata.turn_id.clone()),
                    original_tokens,
                    retained_tokens,
                ),
            ));
        }
    }

    // Detailed provenance is useful, but its envelope must also fit a fixed
    // budget. Exact text remains recoverable from the mandatory sidecar.
    let omitted_text = !omission_receipts.is_empty();
    let mut receipt_tokens = 0usize;
    let mut omitted_receipt_count = 0usize;
    let mut first_omitted_index = None;
    for (index, receipt) in omission_receipts {
        let tokens = response_item_text_tokens(&receipt);
        if receipt_tokens.saturating_add(tokens) <= 1024 {
            receipt_tokens += tokens;
            indexed_items.push((index, receipt));
        } else {
            omitted_receipt_count += 1;
            first_omitted_index.get_or_insert(index);
        }
    }
    if let Some(index) = first_omitted_index {
        indexed_items.push((
            index,
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: serde_json::json!({
                        "kind": COMPACT_TEXT_OMISSION_MARKER,
                        "unresolved": true,
                        "additional_omitted_messages": omitted_receipt_count,
                        "instruction": "Recover exact text from the compaction recovery artifact.",
                    })
                    .to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        ));
    }
    let pending_output_ids = unresolved
        .iter()
        .filter_map(compaction_output_call_id)
        .collect::<std::collections::HashSet<_>>();
    for (source_index, item) in unresolved.iter().enumerate() {
        let Some(call_id) = compaction_call_id(item).or_else(|| compaction_output_call_id(item))
        else {
            continue;
        };
        let has_pending_output = pending_output_ids.contains(call_id);
        if has_pending_output {
            indexed_items.push((source_index, item.clone()));
        }
    }
    indexed_items.sort_by_key(|(index, _)| *index);
    let history = indexed_items.into_iter().map(|(_, item)| item).collect();
    (
        history,
        retained_image_count,
        omitted_image_count,
        omitted_user_text,
        omitted_text,
    )
}

fn compacted_user_message_text_tokens(message: &CompactedUserMessage) -> usize {
    message
        .content
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(approx_token_count(text)),
            _ => None,
        })
        .fold(0usize, usize::saturating_add)
}

fn response_item_text_tokens(item: &ResponseItem) -> usize {
    match item {
        ResponseItem::Message { content, .. } => content
            .iter()
            .filter_map(|item| match item {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(approx_token_count(text))
                }
                ContentItem::InputImage { .. } => None,
            })
            .fold(0usize, usize::saturating_add),
        _ => 0,
    }
}

fn agent_message_text_tokens(item: &ResponseItem) -> usize {
    let ResponseItem::AgentMessage { content, .. } = item else {
        return 0;
    };
    content
        .iter()
        .map(|item| match item {
            AgentMessageInputContent::InputText { text } => approx_token_count(text),
            AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                approx_token_count(encrypted_content)
            }
        })
        .fold(0usize, usize::saturating_add)
}

fn compaction_text_omission_receipt(
    role: &'static str,
    source_item_id: Option<String>,
    source_index: usize,
    turn_id: Option<String>,
    original_tokens: usize,
    retained_tokens: usize,
) -> ResponseItem {
    let receipt = CompactionTextOmissionReceiptV1 {
        version: 1,
        kind: COMPACT_TEXT_OMISSION_MARKER,
        role,
        source_item_id,
        source_index,
        turn_id,
        original_tokens,
        retained_tokens,
        omitted_tokens: original_tokens.saturating_sub(retained_tokens),
        unresolved: true,
    };
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: match serde_json::to_string(&receipt) {
                Ok(text) => text,
                Err(error) => unreachable!("compaction omission receipt must serialize: {error}"),
            },
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) async fn persist_compaction_text_recovery(
    sess: &Session,
    source_items: &[ResponseItem],
    omitted_text: bool,
) -> CodexResult<Option<String>> {
    let Some(canonical) = compaction_text_recovery_canonical(source_items, omitted_text) else {
        return Ok(None);
    };
    let codex_home = sess.codex_home().await;
    let artifact = create_canonical_output_artifact(
        codex_home.as_path(),
        &sess.thread_id().to_string(),
        &canonical,
    )
    .await;
    if !artifact.complete {
        return Err(CodexErr::Fatal(
            "Compaction could not preserve exact unresolved text; original history was retained."
                .to_string(),
        ));
    }
    compaction_text_recovery_sidecar(&canonical, &artifact)
        .map(Some)
        .ok_or_else(|| {
            CodexErr::Fatal(
                "Compaction recovery artifact is unavailable; original history was retained."
                    .to_string(),
            )
        })
}

fn compaction_text_recovery_canonical(
    source_items: &[ResponseItem],
    omitted_text: bool,
) -> Option<CanonicalToolResult> {
    if !omitted_text {
        return None;
    }
    let exact_items = unresolved_compaction_items(source_items)
        .into_iter()
        .filter(|item| {
            matches!(
                item,
                ResponseItem::Message { .. } | ResponseItem::AgentMessage { .. }
            )
        })
        .collect::<Vec<_>>();
    Some(CanonicalToolResult::json(serde_json::json!({
        "version": 1,
        "kind": "local_compaction_text_recovery",
        "instruction": "Recover exact unresolved text with read_tool_output and a json_pointer under /items; do not ask the user to repeat it.",
        "items": exact_items,
    })))
}

fn compaction_text_recovery_sidecar(
    canonical: &CanonicalToolResult,
    artifact: &CanonicalOutputArtifact,
) -> Option<String> {
    let artifact_id = artifact.artifact_id()?;
    serde_json::to_string(&serde_json::json!({
        "version": 1,
        "kind": "local_compaction_text_recovery",
        "artifact_id": artifact_id,
        "canonical_sha256": canonical.sha256,
        "canonical_bytes": canonical.exact_bytes,
        "complete": artifact.complete,
        "unavailable_ranges": artifact.unavailable_ranges,
        "instruction": "Use read_tool_output with this artifact_id and json_pointer selectors under /items to recover exact omitted unresolved text; do not rerun work or ask the user to repeat it."
    }))
    .ok()
}

pub(crate) fn is_summary_message(message: &str) -> bool {
    message.starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

fn compaction_summary_item(summary_text: String) -> ResponseItem {
    compaction_summary_item_with_artifact_pins(summary_text, None)
}

fn compaction_summary_item_with_artifact_pins(
    summary_text: String,
    artifact_pin_payload: Option<String>,
) -> ResponseItem {
    let mut content = vec![ContentItem::InputText { text: summary_text }];
    if let Some(text) = artifact_pin_payload {
        content.push(ContentItem::InputText { text });
    }
    ResponseItem::Message {
        id: Some(ResponseItemId::new(COMPACTION_SUMMARY_ITEM_ID_BASE)),
        role: "user".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn compaction_summary_text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message {
        id: Some(id),
        role,
        content,
        ..
    } = item
    else {
        return None;
    };
    if role != "user" || !id.as_str().starts_with(COMPACTION_SUMMARY_ITEM_ID_PREFIX) {
        return None;
    }
    content.iter().find_map(|content| match content {
        ContentItem::InputText { text } if is_summary_message(text) => Some(text.as_str()),
        _ => None,
    })
}

fn is_compaction_summary_item(item: &ResponseItem) -> bool {
    compaction_summary_text(item).is_some()
}

fn latest_summary_message(items: &[ResponseItem]) -> Option<&str> {
    items.iter().rev().find_map(compaction_summary_text)
}

fn history_after_latest_summary_is_user_only(items: &[ResponseItem]) -> bool {
    let Some(summary_index) = items.iter().rposition(is_compaction_summary_item) else {
        return false;
    };
    items[summary_index + 1..]
        .iter()
        .all(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
}

fn can_reuse_previous_summary(items: &[ResponseItem], omitted_user_text: bool) -> bool {
    !omitted_user_text && history_after_latest_summary_is_user_only(items)
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
        #[cfg(test)]
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
        if is_compaction_summary_item(item) {
            last_user_or_summary_index.get_or_insert(i);
            continue;
        }
        let Some(TurnItem::UserMessage(_)) = crate::event_mapping::parse_turn_item(item) else {
            continue;
        };
        // Compaction summaries are encoded as user messages, so track both:
        // the last real user message (preferred insertion point) and the last
        // user-message-like item (fallback summary insertion point).
        last_user_or_summary_index.get_or_insert(i);
        last_real_user_index = Some(i);
        break;
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
    let (mut history, _retained_image_count, omitted_image_count, _selected_indices) =
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
    if omitted_image_count > 0 {
        summary_text.push_str("\n\n");
        summary_text.push_str(&compaction_image_omission_marker(omitted_image_count));
    }

    history.push(compaction_summary_item(summary_text));

    history
}

fn append_bounded_user_messages(
    mut history: Vec<ResponseItem>,
    user_messages: &[CompactedUserMessage],
    max_tokens: usize,
    max_images: usize,
    max_image_bytes: usize,
) -> (Vec<ResponseItem>, usize, usize, Vec<usize>) {
    let mut selected_messages: Vec<(usize, CompactedUserMessage)> = Vec::new();
    let mut remaining = max_tokens;
    let mut retained_image_count = 0usize;
    let mut retained_image_bytes = 0usize;
    let mut omitted_image_count = 0usize;
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
                        omitted_image_count += 1;
                    }
                }
                _ => {}
            }
        }
        if !content.is_empty() {
            selected_messages.push((
                index,
                CompactedUserMessage {
                    source_item_id: message.source_item_id.clone(),
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
        omitted_image_count,
        selected_indices,
    )
}

#[derive(Default)]
struct LocalCompactionAccumulator {
    items: Vec<ResponseItem>,
}

impl LocalCompactionAccumulator {
    fn record_output(&mut self, item: ResponseItem) {
        self.items.push(item);
    }

    fn complete(self, token_usage: Option<TokenUsage>) -> LocalCompactionOutput {
        LocalCompactionOutput {
            items: self.items,
            token_usage,
        }
    }
}

struct LocalCompactionOutput {
    items: Vec<ResponseItem>,
    token_usage: Option<TokenUsage>,
}

async fn drain_to_completed(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    responses_metadata: &CodexResponsesMetadata,
    prompt: &Prompt,
    cancellation_token: &CancellationToken,
) -> CodexResult<LocalCompactionOutput> {
    let model_request_timing_guard = turn_context.turn_timing_state.begin_model_request_wait();
    let inference_trace_context = InferenceTraceContext::disabled();
    let stream_result = tokio::select! {
        _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
        result = client_session.stream(
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
            &inference_trace_context,
        ) => result,
    };
    drop(model_request_timing_guard);
    let mut stream = stream_result?;
    let mut accumulator = LocalCompactionAccumulator::default();
    loop {
        let model_stream_wait_timing_guard =
            turn_context.turn_timing_state.begin_model_stream_wait();
        let maybe_event = tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            event = stream.next() => event,
        };
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
                accumulator.record_output(item);
            }
            Ok(ResponseEvent::ServerReasoningIncluded(included)) => {
                sess.set_server_reasoning_included(included).await;
            }
            Ok(ResponseEvent::RateLimits(snapshot)) => {
                sess.update_rate_limits(turn_context, snapshot).await;
            }
            Ok(ResponseEvent::Completed { token_usage, .. }) => {
                return Ok(accumulator.complete(token_usage));
            }
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
#[path = "compact_tests.rs"]
mod tests;
