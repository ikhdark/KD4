use std::sync::Arc;

use super::RemoteCompactionV2Output;
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
use crate::session::turn::prepare_sampling_prompt_for_client;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::CompactionTraceContext;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(super) struct RemoteCompactV2Attempt {
    pub(super) trace_input_history: Option<Vec<ResponseItem>>,
    pub(super) prompt_input: Vec<ResponseItem>,
    pub(super) compaction_output: ResponseItem,
    pub(super) token_usage: Option<TokenUsage>,
    pub(super) stable_context_fingerprint: [u8; 32],
    /// Keeps a session created for standalone compaction alive through lifecycle completion.
    pub(super) owned_client_session: Option<ModelClientSession>,
}

pub(super) async fn run_remote_compact_v2_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    compaction_trace: &CompactionTraceContext,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    cancellation_token: &CancellationToken,
) -> CodexResult<RemoteCompactV2Attempt> {
    let turn_context = &step_context.turn;
    let mut history = sess.clone_history().await;
    let base_instructions = sess.get_base_instructions().await;
    let tool_router = built_tools(
        sess.as_ref(),
        step_context.as_ref(),
        &[],
        cancellation_token,
    )
    .await?;
    let mut owned_client_session = None;
    let client_session = match client_session {
        Some(client_session) => client_session,
        None => owned_client_session.insert(sess.services.model_client.new_session()),
    };
    let mut prepared = prepare_sampling_prompt_for_client(
        history.clone(),
        turn_context,
        client_session,
        sess.services.git_workspace.as_ref(),
    )
    .await;
    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window_for_prompt(
            &mut history,
            turn_context.as_ref(),
            &base_instructions,
            Some(prepared.items()),
        );
    if rewritten_outputs > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            rewritten_outputs,
            "rewrote history outputs before remote compaction v2"
        );
        prepared = prepare_sampling_prompt_for_client(
            history.clone(),
            turn_context,
            client_session,
            sess.services.git_workspace.as_ref(),
        )
        .await;
    }
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

    let trace_input_history = compaction_trace
        .is_enabled()
        .then(|| history.raw_items().to_vec());
    let mut prompt = build_projected_prompt(
        sess.as_ref(),
        &prepared,
        &tool_router,
        step_context.as_ref(),
        base_instructions,
    );
    prompt.output_schema = None;
    prompt.output_schema_strict = true;
    append_compaction_trigger(&mut prompt);
    let stable_context_fingerprint = prompt.stable_context_manifest.fingerprint();

    let window_id = sess.current_window_id().await;
    let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
        sess.installation_id.clone(),
        window_id,
        CodexResponsesRequestKind::Compaction(compaction_metadata),
    );
    let compaction_output_result = run_remote_compaction_request_v2(
        sess,
        turn_context.as_ref(),
        client_session,
        &prompt,
        &responses_metadata,
        compaction_trace,
        cancellation_token,
    )
    .await;
    let RemoteCompactionV2Output {
        compaction_output,
        token_usage,
    } = compaction_output_result?;
    let mut prompt_input = prompt.input.to_vec();
    let Some(ResponseItem::CompactionTrigger {}) = prompt_input.pop() else {
        unreachable!("remote compaction v2 prompt must end with its synthetic trigger");
    };
    Ok(RemoteCompactV2Attempt {
        trace_input_history,
        prompt_input,
        compaction_output,
        token_usage,
        stable_context_fingerprint,
        owned_client_session,
    })
}

fn append_compaction_trigger(prompt: &mut Prompt) {
    for input in [
        &mut prompt.input,
        &mut prompt.stable_context_fallback_input,
        &mut prompt.tool_history_fallback_input,
        &mut prompt.stable_context_tool_history_fallback_input,
    ] {
        let mut items = input.to_vec();
        items.push(ResponseItem::CompactionTrigger {});
        *input = items.into();
    }
}
