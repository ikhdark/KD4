use std::sync::Arc;

use super::RemoteCompactionV2Output;
use super::prepare_v2_retained_input_source;
use super::run_remote_compaction_request_v2;
use crate::Prompt;
use crate::client::ModelClientSession;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact_remote::trim_function_call_history_to_fit_context_window_for_prompt;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn::build_projected_prompt;
use crate::session::turn::built_tools;
use crate::session::turn::prepare_sampling_prompt_with_workspace_identity;
use crate::session::turn::prompt_token_estimates;
use crate::session::turn::with_sampling_workspace_identity;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(super) struct RemoteCompactV2Attempt {
    pub(super) retained_input: crate::compact::TaskInputCheckpoint,
    pub(super) compaction_output: ResponseItem,
    pub(super) token_usage: Option<TokenUsage>,
    /// Keeps a session created for standalone compaction alive through lifecycle completion.
    pub(super) owned_client_session: Option<ModelClientSession>,
}

pub(super) async fn run_remote_compact_v2_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    cancellation_token: &CancellationToken,
) -> CodexResult<RemoteCompactV2Attempt> {
    let turn_context = &step_context.turn;
    let mut history = sess.clone_history().await;
    let base_instructions = turn_context.base_instructions.as_ref().clone();
    let tool_router = built_tools(sess.as_ref(), step_context, &[], cancellation_token).await?;
    let mut owned_client_session = None;
    let client_session = match client_session {
        Some(client_session) => client_session,
        None => owned_client_session.insert(sess.services.model_client.new_session()),
    };
    let build_prompt = |prepared: &crate::context_manager::PreparedPromptInput| {
        let mut prompt = build_projected_prompt(
            sess.as_ref(),
            prepared,
            &tool_router,
            step_context.as_ref(),
            base_instructions.clone(),
        );
        prompt.output_schema = None;
        prompt.output_schema_strict = true;
        append_compaction_trigger(&mut prompt);
        prompt
    };
    // Only tool-search schemas can be rewritten by this trimmer. Size those
    // before observing the workspace: preliminary sizing does not authorize
    // source-evidence reuse and must not freeze an early workspace identity.
    // Without that intervening observation, the authoritative pass below is
    // sufficient; do not build and size the same request twice.
    let preliminary_deleted_tokens = if history.requires_workspace_evidence_validation()
        && history.raw_items().iter()
            .any(|item| matches!(item, ResponseItem::ToolSearchOutput { .. }))
    {
        let prepared = history
            .clone()
            .prepare_for_prompt(&turn_context.model_info.input_modalities);
        let sizing_prompt = build_prompt(&prepared);
        let measured_input = largest_compaction_request_input(&sizing_prompt);
        let tool_tokens =
            i64::try_from(sizing_prompt.tools.serialized().len().div_ceil(4)).unwrap_or(i64::MAX);
        let (rewritten_outputs, deleted_tokens) =
            trim_function_call_history_to_fit_context_window_for_prompt(
                &mut history,
                turn_context.as_ref(),
                &base_instructions,
                Some(&measured_input),
                tool_tokens,
            );
        if rewritten_outputs > 0 {
            info!(
                turn_id = %turn_context.sub_id,
                rewritten_outputs,
                "rewrote history outputs while sizing remote compaction v2"
            );
        }
        deleted_tokens
    } else {
        0
    };
    let (history, mut prompt, final_deleted_tokens) = with_sampling_workspace_identity(
        history,
        turn_context,
        sess.services.git_workspace.as_ref(),
        |mut history, workspace_identity| {
            let project = |history| {
                let prepared = prepare_sampling_prompt_with_workspace_identity(
                    history,
                    turn_context,
                    workspace_identity,
                    sess.services.git_workspace.as_ref(),
                );
                build_prompt(&prepared)
            };
            let mut prompt = project(history.clone());
            let measured_input = largest_compaction_request_input(&prompt);
            let tool_tokens =
                i64::try_from(prompt.tools.serialized().len().div_ceil(4)).unwrap_or(i64::MAX);
            let (rewritten_outputs, estimated_deleted_tokens) =
                trim_function_call_history_to_fit_context_window_for_prompt(
                    &mut history,
                    turn_context.as_ref(),
                    &base_instructions,
                    Some(&measured_input),
                    tool_tokens,
                );
            if rewritten_outputs > 0 {
                info!(
                    turn_id = %turn_context.sub_id,
                    rewritten_outputs,
                    "rewrote history outputs before remote compaction v2"
                );
                // Freshness notices can make the authoritative request larger
                // than the preliminary estimate. Recheck that exact projection;
                // only tool-search schemas change, with no intervening await.
                prompt = project(history.clone());
            }
            (history, prompt, estimated_deleted_tokens)
        },
    )
    .await;
    let estimated_deleted_tokens = preliminary_deleted_tokens.saturating_add(final_deleted_tokens);
    if estimated_deleted_tokens > 0 {
        let max_local_deleted_tokens = sess
            .estimated_tokens_after_last_model_generated_item()
            .await;
        analytics_details.active_context_tokens_before = analytics_details
            .active_context_tokens_before
            .map(|active_context_tokens_before| {
                active_context_tokens_before
                    .saturating_sub(estimated_deleted_tokens.min(max_local_deleted_tokens))
            });
    }

    // Trimming is best effort, not a fit receipt. Required text may still be
    // oversized; do not spend a compaction request that cannot fit either. The
    // byte estimate overstates recorded text-heavy prompts by 11-45 % (557,192
    // tokens for a measured 442,400), so refuse only when sampling admission's
    // provider-calibrated estimate does not fit either.
    let largest_input = largest_compaction_request_input(&prompt);
    if let Some(limit) = turn_context.model_context_window()
        && prompt.budget_tokens_for_input(&largest_input) >= limit
        && prompt_token_estimates(sess, turn_context, &prompt)
            .await
            .calibrated_tokens
            >= limit
    {
        return Err(codex_protocol::error::CodexErr::ContextWindowExceeded);
    }


    // Required recovery must be available before spending a provider request.
    // Use source IDs, not the lossy sampling projection or synthetic trigger.
    // Retention is awaited to completion: cancellation must not detach writes.
    let retained_input = prepare_v2_retained_input_source(sess, history.raw_items()).await?;
    {
        let current_history = sess.clone_history().await;
        let mut reference_items = current_history.raw_items().to_vec();
        reference_items.extend(retained_input.items.iter().cloned());
        if let Some(plan) = crate::compact::retained_plan_context(sess).await? {
            reference_items.push(plan);
        }
        // Installation rechecks the live plan and pins after the provider wait.
        // Existing content-addressed retention reuses unchanged recovery bytes.
        sess.compaction_artifact_pins(&current_history.tool_history_state(), &reference_items)
            .await?;
    }
    if cancellation_token.is_cancelled() {
        return Err(codex_protocol::error::CodexErr::TurnAborted);
    }

    let window_id = sess.current_window_id().await;
    let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
        sess.installation_id.clone(),
        window_id,
        CodexResponsesRequestKind::Compaction(compaction_metadata),
    );
    prompt.stream_progress_timeout = Some(super::remote_compaction_progress_timeout(
        turn_context.provider.info(),
    ));
    let compaction_output_result = run_remote_compaction_request_v2(
        sess,
        turn_context.as_ref(),
        client_session,
        &prompt,
        &responses_metadata,
        cancellation_token,
    )
    .await;
    let RemoteCompactionV2Output {
        compaction_output,
        token_usage,
    } = compaction_output_result?;
    Ok(RemoteCompactV2Attempt {
        retained_input,
        compaction_output,
        token_usage,
        owned_client_session,
    })
}

fn largest_compaction_request_input(prompt: &Prompt) -> Arc<[ResponseItem]> {
    // Transport fallback still needs the largest complete representation, but
    // shared representations need only one estimate.
    let mut measured: Vec<(&Arc<[ResponseItem]>, i64)> = Vec::with_capacity(4);
    for input in [
        &prompt.input,
        &prompt.stable_context_fallback_input,
        &prompt.tool_history_fallback_input,
        &prompt.stable_context_tool_history_fallback_input,
    ] {
        let tokens = measured
            .iter()
            .find(|(previous, _)| Arc::ptr_eq(previous, input))
            .map(|(_, tokens)| *tokens)
            .unwrap_or_else(|| {
                input
                    .iter()
                    .map(crate::context_manager::estimate_item_token_count)
                    .fold(0_i64, i64::saturating_add)
            });
        measured.push((input, tokens));
    }
    measured
        .into_iter()
        .max_by_key(|(_, tokens)| *tokens)
        .map(|(input, _)| Arc::clone(input))
        .unwrap_or_else(|| Arc::clone(&prompt.input))
}

fn append_compaction_trigger(prompt: &mut Prompt) {
    prompt.prompt_provenance = prompt.prompt_provenance.with_appended_items(
        &[ResponseItem::CompactionTrigger {}],
        &prompt.stable_context_manifest,
    );
    // The suffix changes History even though the projected prefix is unchanged.
    prompt.digests.history = None;
    // Preserve sharing established by prompt projection. Pointer identity is
    // sufficient; do not compare large distinct transcripts for equality.
    type SharedInput = Arc<[ResponseItem]>;
    let mut appended: Vec<(SharedInput, SharedInput)> = Vec::with_capacity(4);
    for input in [
        &mut prompt.input,
        &mut prompt.stable_context_fallback_input,
        &mut prompt.tool_history_fallback_input,
        &mut prompt.stable_context_tool_history_fallback_input,
    ] {
        if let Some((_, replacement)) = appended
            .iter()
            .find(|(original, _)| Arc::ptr_eq(original, input))
        {
            *input = Arc::clone(replacement);
        } else {
            let mut items = input.to_vec();
            items.push(ResponseItem::CompactionTrigger {});
            let replacement: Arc<[ResponseItem]> = items.into();
            appended.push((Arc::clone(input), Arc::clone(&replacement)));
            *input = replacement;
        }
    }
}
